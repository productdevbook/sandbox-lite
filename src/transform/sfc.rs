//! TypeScript out of the `<script>` blocks of a `.vue` or `.svelte` file.
//!
//! The browser compiles the SFC and imports the result as a blob module, so TypeScript anywhere in
//! the script is a syntax error there: `svelte/compiler` refuses to parse it, and
//! `@vue/compiler-sfc` copies it into its output untouched. Every block that says `lang="ts"` goes
//! through the same oxc transform as a `.ts` file, and the block keeps its other attributes.

use std::ops::Range;

use super::{BuildError, Diag, js};

/// A macro whose only argument is a type, so stripping types ahead of `@vue/compiler-sfc` would
/// leave it with nothing to compile.
const TYPE_ONLY_MACROS: [&str; 4] = ["defineProps", "defineEmits", "defineModel", "defineSlots"];

struct Attr {
    name: String,
    value: String,
    start: usize,
    end: usize,
}

struct Block {
    attrs: Vec<Attr>,
    content: Range<usize>,
}

impl Block {
    fn attr(&self, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.name == name)
    }
}

/// Rewrites every `<script lang="ts">` block of `source` to JavaScript.
pub fn strip_types(ext: &str, path: &str, source: &str) -> Result<String, BuildError> {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for block in blocks(source) {
        let Some(lang) = block.attr("lang") else { continue };
        if !matches!(lang.value.trim().to_ascii_lowercase().as_str(), "ts" | "typescript") {
            continue;
        }
        if ext == "vue" {
            refuse_type_only(path, source, &block)?;
        }
        let content = &source[block.content.clone()];
        // Blank lines instead of the markup above the block, so oxc's diagnostics carry file lines.
        let padded = format!("{}{content}", "\n".repeat(source[..block.content.start].matches('\n').count()));
        let code = js::transform_sfc_script(path, &padded)?;
        let mut start = lang.start;
        if matches!(source.as_bytes().get(start.wrapping_sub(1)).copied(), Some(b' ' | b'\t')) {
            start -= 1;
        }
        edits.push((start, lang.end, String::new()));
        edits.push((block.content.start, block.content.end, relined(content, &code)));
    }
    if edits.is_empty() {
        return Ok(source.to_string());
    }
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(source.len());
    let mut last = 0usize;
    for (start, end, replacement) in edits {
        out.push_str(&source[last..start]);
        out.push_str(&replacement);
        last = end;
    }
    out.push_str(&source[last..]);
    Ok(out)
}

/// The compiled script, on as many lines as the block it replaces, so the SFC compiler's own
/// diagnostics still point at the right line of the template and the styles below it.
fn relined(content: &str, code: &str) -> String {
    let body = content.find(|c: char| !c.is_whitespace()).unwrap_or(content.len());
    let lead = match content[..body].rfind('\n') {
        Some(i) => &content[..=i],
        None => "",
    };
    let want = content.matches('\n').count();
    let have = lead.matches('\n').count() + code.matches('\n').count();
    format!("{lead}{code}{}", "\n".repeat(want.saturating_sub(have)))
}

/// `@vue/compiler-sfc` turns a type into a runtime declaration, and it reads that type from the
/// source we are about to strip. Refuse rather than hand back a component with no props.
fn refuse_type_only(path: &str, source: &str, block: &Block) -> Result<(), BuildError> {
    let (offset, text, hint) = if let Some(generic) = block.attr("generic") {
        (
            generic.start,
            "<script setup generic> compiles to TypeScript the browser cannot run".to_string(),
            "drop the generic attribute: the preview strips types before @vue/compiler-sfc sees them".to_string(),
        )
    } else if let Some((offset, macro_name)) = type_only_macro(&source[block.content.clone()]) {
        (
            block.content.start + offset,
            format!("{macro_name}<T>() needs a type the preview has already stripped"),
            format!("declare them at runtime instead: {}", runtime_form(macro_name)),
        )
    } else {
        return Ok(());
    };
    let (line, column) = js::line_col(source, offset);
    let diag = Diag { severity: "error".into(), text: text.clone(), hint, file: path.to_string(), line, column };
    Err(BuildError::compile(format!("{path}: {text}"), vec![diag]))
}

fn runtime_form(macro_name: &str) -> &'static str {
    match macro_name {
        "defineProps" => "defineProps({ label: { type: String, required: true } })",
        "defineEmits" => "defineEmits([\"change\"])",
        "defineModel" => "defineModel({ type: String })",
        _ => "defineSlots()",
    }
}

/// Offset of the first `defineProps<…>`-style call outside a string or a comment.
fn type_only_macro(code: &str) -> Option<(usize, &'static str)> {
    let b = code.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1).copied() == Some(b'/') => i = code[i..].find('\n').map(|k| i + k + 1).unwrap_or(b.len()),
            b'/' if b.get(i + 1).copied() == Some(b'*') => i = code[i + 2..].find("*/").map(|k| i + 4 + k).unwrap_or(b.len()),
            b'"' | b'\'' | b'`' => i = string_end(code, i),
            c if is_ident(c) && !c.is_ascii_digit() => {
                let start = i;
                while i < b.len() && is_ident(b[i]) {
                    i += 1;
                }
                let word = &code[start..i];
                let mut j = i;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if b.get(j).copied() == Some(b'<')
                    && let Some(name) = TYPE_ONLY_MACROS.iter().find(|m| **m == word).copied()
                {
                    return Some((start, name));
                }
            }
            _ => i += 1,
        }
    }
    None
}

fn string_end(code: &str, open: usize) -> usize {
    let b = code.as_bytes();
    let quote = b[open];
    let mut i = open + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// Every `<script>` element in the file, in source order. A `<script>` nested in markup is one of
/// them, but only a block that says `lang="ts"` is ever rewritten.
fn blocks(source: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(rel) = source[i..].find('<') {
        let open = i + rel;
        i = open + 1;
        if !is_tag(source, i, "script") {
            continue;
        }
        let Some((attrs, content_start, self_closing)) = attributes(source, i + "script".len()) else { break };
        i = content_start;
        if self_closing {
            continue;
        }
        let Some((content_end, after)) = close_tag(source, content_start) else { break };
        out.push(Block { attrs, content: content_start..content_end });
        i = after;
    }
    out
}

fn is_tag(source: &str, at: usize, name: &str) -> bool {
    source.get(at..at + name.len()).is_some_and(|s| s.eq_ignore_ascii_case(name))
        && !source.as_bytes().get(at + name.len()).is_some_and(|c| is_ident(*c) || *c == b'-')
}

/// The tag's attributes, the offset just past its `>`, and whether it closed itself. Quoted values
/// are read as such: `generic="T extends Item<string>"` holds a `>` that does not end the tag.
fn attributes(source: &str, from: usize) -> Option<(Vec<Attr>, usize, bool)> {
    let b = source.as_bytes();
    let mut attrs = Vec::new();
    let mut i = from;
    loop {
        while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
            i += 1;
        }
        match b.get(i).copied()? {
            b'>' => return Some((attrs, i + 1, false)),
            b'/' if b.get(i + 1).copied() == Some(b'>') => return Some((attrs, i + 2, true)),
            b'/' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let name_start = i;
        while b.get(i).is_some_and(|c| !c.is_ascii_whitespace() && !matches!(*c, b'=' | b'>' | b'/')) {
            i += 1;
        }
        if i == name_start {
            return None;
        }
        let name = source[name_start..i].to_ascii_lowercase();
        let mut end = i;
        let mut value = String::new();
        let mut j = i;
        while b.get(j).is_some_and(|c| c.is_ascii_whitespace()) {
            j += 1;
        }
        if b.get(j).copied() == Some(b'=') {
            j += 1;
            while b.get(j).is_some_and(|c| c.is_ascii_whitespace()) {
                j += 1;
            }
            match b.get(j).copied() {
                Some(quote @ (b'"' | b'\'')) => {
                    let start = j + 1;
                    let close = start + source[start..].find(quote as char)?;
                    value = source[start..close].to_string();
                    end = close + 1;
                }
                Some(_) => {
                    let start = j;
                    while b.get(j).is_some_and(|c| !c.is_ascii_whitespace() && *c != b'>') {
                        j += 1;
                    }
                    value = source[start..j].to_string();
                    end = j;
                }
                None => return None,
            }
            i = end;
        }
        attrs.push(Attr { name, value, start: name_start, end });
    }
}

/// Start of `</script>` and the offset just past it. A script's text ends at the first one of
/// these, which is why `</script>` cannot appear inside a script anywhere.
fn close_tag(source: &str, from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    loop {
        let open = i + source[i..].find('<')?;
        if source[open + 1..].starts_with('/') && is_tag(source, open + 2, "script") {
            let end = open + source[open..].find('>')? + 1;
            return Some((open, end));
        }
        i = open + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::strip_types;

    const VUE: &str = r#"<script setup lang="ts">
import { ref, type PropType } from "vue";
import Badge from "./Badge.vue";

interface Row {
  id: number;
}

const props = defineProps({
  rows: { type: Array as PropType<Row[]>, required: true },
});

const open = ref<boolean>(false);
</script>

<template>
  <Badge v-if="open" :count="props.rows.length" />
</template>

<style>
.badge {
  color: red;
}
</style>
"#;

    const SVELTE: &str = r#"<script context="module" lang="ts">
  export const kind: string = "counter";
</script>

<script lang="ts">
  interface Props {
    label: string;
    start?: number;
  }

  let { label, start = 0 }: Props = $props();
  let delta: number = $state(0);
</script>

<h1>{label}{start}{delta}</h1>
"#;

    fn line_of(source: &str, needle: &str) -> usize {
        source[..source.find(needle).unwrap_or_else(|| panic!("{needle} missing from\n{source}"))].matches('\n').count() + 1
    }

    #[test]
    fn a_vue_script_setup_block_loses_its_types_and_keeps_its_attributes() {
        let out = strip_types("vue", "src/components/Table.vue", VUE).unwrap();
        assert!(out.contains("<script setup>"), "{out}");
        assert!(!out.contains("lang="), "{out}");
        assert!(!out.contains("interface Row"), "{out}");
        assert!(!out.contains("PropType"), "{out}");
        assert!(out.contains("ref(false)"), "{out}");
        assert!(out.contains("type: Array,"), "{out}");
        assert!(out.contains("<template>"), "the markup is untouched: {out}");
        assert!(out.contains("color: red;"), "the styles are untouched: {out}");
    }

    #[test]
    fn an_import_only_the_template_uses_survives() {
        let out = strip_types("vue", "src/components/Table.vue", VUE).unwrap();
        assert!(out.contains(r#"import Badge from "./Badge.vue""#), "{out}");
    }

    #[test]
    fn both_svelte_script_blocks_are_stripped() {
        let out = strip_types("svelte", "src/components/Counter.svelte", SVELTE).unwrap();
        assert!(out.contains(r#"<script context="module">"#), "{out}");
        assert!(out.contains("<script>\n"), "{out}");
        assert!(!out.contains("lang="), "{out}");
        assert!(!out.contains("interface Props"), "{out}");
        assert!(out.contains("export const kind = \"counter\""), "{out}");
        assert!(out.contains("let { label, start = 0 } = $props()"), "{out}");
        assert!(out.contains("let delta = $state(0)"), "{out}");
    }

    #[test]
    fn the_markup_below_a_block_keeps_its_line_numbers() {
        for (ext, source, needle) in [("vue", VUE, "<style>"), ("svelte", SVELTE, "<h1>")] {
            let out = strip_types(ext, "src/components/C.vue", source).unwrap();
            assert_eq!(line_of(&out, needle), line_of(source, needle), "{ext}:\n{out}");
        }
    }

    #[test]
    fn a_block_without_lang_is_left_alone() {
        let source = "<script setup>\nconst n = 1 as const;\n</script>\n";
        assert_eq!(strip_types("vue", "src/components/C.vue", source).unwrap(), source);
    }

    #[test]
    fn a_type_error_is_reported_at_its_line_in_the_file() {
        let source = "<template>\n  <p>hi</p>\n</template>\n\n<script setup lang=\"ts\">\nconst n: number = ;\n</script>\n";
        let err = strip_types("vue", "src/components/C.vue", source).unwrap_err();
        assert_eq!(err.diagnostics[0].file, "src/components/C.vue");
        assert_eq!(err.diagnostics[0].line, 6, "{:?}", err.diagnostics);
    }

    #[test]
    fn a_type_only_vue_macro_is_refused_by_name() {
        let source = "<script setup lang=\"ts\">\ninterface Props { n: number }\nconst props = defineProps<Props>();\n</script>\n";
        let err = strip_types("vue", "src/components/C.vue", source).unwrap_err();
        assert!(err.message.contains("defineProps<T>()"), "{}", err.message);
        assert_eq!(err.diagnostics[0].line, 3);
        assert!(err.diagnostics[0].hint.contains("defineProps({"), "{:?}", err.diagnostics);
    }

    #[test]
    fn a_generic_vue_component_is_refused() {
        let source = "<script setup lang=\"ts\" generic=\"T extends Item<string>\">\nconst n = 1;\n</script>\n";
        let err = strip_types("vue", "src/components/C.vue", source).unwrap_err();
        assert!(err.message.contains("generic"), "{}", err.message);
    }

    #[test]
    fn a_macro_named_in_a_comment_is_not_a_refusal() {
        let source = "<script setup lang=\"ts\">\n// defineProps<Props>() is not supported\nconst n: number = 1;\n</script>\n";
        assert!(strip_types("vue", "src/components/C.vue", source).is_ok());
    }

    #[test]
    fn a_svelte_type_annotation_is_not_a_vue_macro() {
        let source = "<script lang=\"ts\">\n  let props: Props<number> = $props();\n</script>\n";
        assert!(strip_types("svelte", "src/components/C.svelte", source).is_ok());
    }
}
