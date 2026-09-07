//! TypeScript out of the `<script>` blocks of a `.svelte` file, and what the daemon can tell about
//! a `.vue` file's script without compiling it.
//!
//! The browser compiles the SFC and imports the result as a blob module, so TypeScript anywhere in
//! the script is a syntax error there. `svelte/compiler` refuses to parse it at all, so every
//! `.svelte` block that says `lang="ts"` goes through the same oxc transform as a `.ts` file here,
//! and the block keeps its other attributes.
//!
//! A `.vue` file is served untouched instead. `@vue/compiler-sfc` reads `defineProps<Props>()` and
//! the rest of the type-driven macros out of the source to build the runtime declaration, so
//! stripping first would hand it a component with no props; the loader posts the script it
//! generates back to `POST /__sl/strip-ts` and the types come off there. What stays here is what
//! needs no browser: the block still has to parse, and a macro whose type comes from another module
//! is refused, because resolving that type needs a file system the browser does not have.

use std::ops::Range;

use super::{BuildError, Diag, js};

/// A macro whose only argument is a type, which `@vue/compiler-sfc` reads out of the source.
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
pub fn strip_types(path: &str, source: &str) -> Result<String, BuildError> {
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for block in blocks(source) {
        let Some(lang) = block.attr("lang") else { continue };
        if !is_ts(&lang.value) {
            continue;
        }
        let content = &source[block.content.clone()];
        let code = js::transform_sfc_script(path, &padded(source, &block))?;
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

fn is_ts(lang: &str) -> bool {
    matches!(lang.trim().to_ascii_lowercase().as_str(), "ts" | "typescript")
}

/// A block's text with blank lines instead of the markup above it, so oxc's diagnostics carry the
/// line the block holds in the file.
fn padded(source: &str, block: &Block) -> String {
    let newlines = "\n".repeat(source[..block.content.start].matches('\n').count());
    format!("{newlines}{}", &source[block.content.clone()])
}

/// Everything the daemon can say about a `.vue` file's script without a browser: that its
/// TypeScript parses, and that no type-driven macro reads its type from another module. The file
/// itself is served untouched — the loader compiles it and posts the result to `/__sl/strip-ts`.
pub fn check_vue(path: &str, source: &str) -> Result<(), BuildError> {
    let typed: Vec<Block> = blocks(source).into_iter().filter(|b| b.attr("lang").is_some_and(|l| is_ts(&l.value))).collect();
    let mut imported: Vec<String> = Vec::new();
    for block in &typed {
        imported.extend(js::check_sfc_script(path, &padded(source, block))?);
    }
    // A `<script setup>` macro may name a type the plain `<script>` block imported, so the names
    // are collected from the whole file before any block is judged.
    for block in &typed {
        refuse_imported_type(path, source, block, &imported)?;
    }
    Ok(())
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

/// `@vue/compiler-sfc` resolves a macro's type argument itself, and reaching a type in another
/// module means reading that module — which in the browser it cannot do: it answers "No fs option
/// provided to `compileScript` in non-Node environment". Refuse here instead, where the diagnostic
/// carries the line and `sandbox-lite check` sees it too.
fn refuse_imported_type(path: &str, source: &str, block: &Block, imported: &[String]) -> Result<(), BuildError> {
    let code = &source[block.content.clone()];
    for (offset, macro_name, root) in type_only_macros(code) {
        let Some(root) = root.filter(|r| imported.iter().any(|name| name.as_str() == *r)) else { continue };
        let text = format!("{macro_name}<{root}>() reads {root} from another module");
        let hint = format!(
            "@vue/compiler-sfc resolves that type in the browser, which has no file system: declare {root} in this file, or use {}",
            runtime_form(macro_name)
        );
        let (line, column) = js::line_col(source, block.content.start + offset);
        let diag = Diag { severity: "error".into(), text: text.clone(), hint, file: path.to_string(), line, column };
        return Err(BuildError::compile(format!("{path}: {text}"), vec![diag]));
    }
    Ok(())
}

fn runtime_form(macro_name: &str) -> &'static str {
    match macro_name {
        "defineProps" => "defineProps({ label: { type: String, required: true } })",
        "defineEmits" => "defineEmits([\"change\"])",
        "defineModel" => "defineModel({ type: String })",
        _ => "defineSlots()",
    }
}

/// Every `defineProps<…>`-style call outside a string or a comment: its offset, its name, and the
/// name at the root of its type argument when the argument is one — `Props` in `defineProps<Props>`
/// and in `defineProps<Props<Row>>`, and nothing for an inline `{ … }`, a union or an intersection,
/// which `@vue/compiler-sfc` reads without leaving the file.
fn type_only_macros(code: &str) -> Vec<(usize, &'static str, Option<&str>)> {
    let b = code.as_bytes();
    let mut out = Vec::new();
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
                    out.push((start, name, type_root(code, j + 1)));
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// The identifier a type argument starts with, when the whole argument is that identifier or an
/// instantiation of it.
fn type_root(code: &str, from: usize) -> Option<&str> {
    let b = code.as_bytes();
    let mut i = from;
    while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
        i += 1;
    }
    let start = i;
    if !b.get(i).is_some_and(|c| is_ident(*c) && !c.is_ascii_digit()) {
        return None;
    }
    while b.get(i).is_some_and(|c| is_ident(*c)) {
        i += 1;
    }
    let name = &code[start..i];
    while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
        i += 1;
    }
    matches!(b.get(i).copied(), Some(b'>' | b'<')).then_some(name)
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
    use super::{check_vue, strip_types};

    const VUE: &str = r#"<script setup lang="ts">
import { ref } from "vue";
import Badge from "./Badge.vue";

interface Props {
  rows: Row[];
}

interface Row {
  id: number;
}

const props = withDefaults(defineProps<Props>(), { rows: () => [] });
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
  import Badge from "./Badge.svelte";

  interface Props {
    label: string;
    start?: number;
  }

  let { label, start = 0 }: Props = $props();
  let delta: number = $state(0);
</script>

<h1>{label}{start}{delta}</h1>
<Badge />
"#;

    fn line_of(source: &str, needle: &str) -> usize {
        source[..source.find(needle).unwrap_or_else(|| panic!("{needle} missing from\n{source}"))].matches('\n').count() + 1
    }

    /// Issue #52: what `@vue/compiler-sfc` reads to build the runtime props is exactly what
    /// stripping first took away, so the file reaches the browser whole.
    #[test]
    fn a_vue_file_keeps_the_types_its_macros_are_declared_with() {
        check_vue("src/components/Table.vue", VUE).unwrap();
    }

    #[test]
    fn a_vue_macro_whose_type_is_declared_in_the_file_is_served() {
        for arg in ["Props", "{ n: number }", "Props<Row>", "Row | Props"] {
            let source =
                format!("<script setup lang=\"ts\">\ninterface Props {{ n: number }}\nconst p = defineProps<{arg}>();\n</script>\n");
            check_vue("src/components/C.vue", &source).unwrap_or_else(|e| panic!("{arg}: {}", e.message));
        }
    }

    /// The browser's compiler has no file system, so a type it must open another module to read is
    /// the one thing the round trip cannot serve. Refuse it here, at its line.
    #[test]
    fn a_vue_macro_whose_type_comes_from_another_module_is_refused() {
        let source = "<script setup lang=\"ts\">\nimport type { Props } from \"./types\";\nconst p = defineProps<Props>();\n</script>\n";
        let err = check_vue("src/components/C.vue", source).unwrap_err();
        assert!(err.message.contains("defineProps<Props>() reads Props from another module"), "{}", err.message);
        assert_eq!(err.diagnostics[0].line, 3);
        assert!(err.diagnostics[0].hint.contains("no file system"), "{:?}", err.diagnostics);
    }

    #[test]
    fn an_imported_type_a_macro_does_not_name_is_not_a_refusal() {
        for source in [
            "<script setup lang=\"ts\">\nimport type { Row } from \"./types\";\nconst p = defineProps<{ rows: Row[] }>();\n</script>\n",
            "<script setup lang=\"ts\">\nimport { ref } from \"vue\";\ninterface Props { n: number }\nconst p = defineProps<Props>();\nconst o = ref(0);\n</script>\n",
        ] {
            check_vue("src/components/C.vue", source).unwrap_or_else(|e| panic!("{source}\n{}", e.message));
        }
    }

    /// A type in another block of the same file is still one module, and `<script setup>` may name
    /// what the plain `<script>` imported.
    #[test]
    fn an_import_in_the_other_script_block_counts() {
        let source = "<script lang=\"ts\">\nimport type { Props } from \"./types\";\n</script>\n\n<script setup lang=\"ts\">\nconst p = defineProps<Props>();\n</script>\n";
        let err = check_vue("src/components/C.vue", source).unwrap_err();
        assert_eq!(err.diagnostics[0].line, 6, "{:?}", err.diagnostics);
    }

    /// `generic="…"` compiles to a component whose type parameter survives only in the types oxc
    /// removes, so stripping after `compileScript` is what makes it work.
    #[test]
    fn a_generic_vue_component_is_served() {
        let source = "<script setup lang=\"ts\" generic=\"T extends Item<string>\">\nconst p = defineProps<{ items: T[] }>();\n</script>\n";
        check_vue("src/components/C.vue", source).unwrap();
    }

    #[test]
    fn a_macro_named_in_a_comment_is_not_a_refusal() {
        let source = "<script setup lang=\"ts\">\nimport type { Props } from \"./types\";\n// defineProps<Props>() would need the file it is declared in\nconst n: number = 1;\n</script>\n";
        assert!(check_vue("src/components/C.vue", source).is_ok());
    }

    #[test]
    fn a_vue_type_error_is_reported_at_its_line_in_the_file() {
        let source = "<template>\n  <p>hi</p>\n</template>\n\n<script setup lang=\"ts\">\nconst n: number = ;\n</script>\n";
        let err = check_vue("src/components/C.vue", source).unwrap_err();
        assert_eq!(err.diagnostics[0].file, "src/components/C.vue");
        assert_eq!(err.diagnostics[0].line, 6, "{:?}", err.diagnostics);
    }

    #[test]
    fn a_vue_block_without_lang_is_not_read_as_typescript() {
        let source = "<script setup>\nconst p = defineProps<Props>();\n</script>\n";
        assert!(check_vue("src/components/C.vue", source).is_ok());
    }

    #[test]
    fn both_svelte_script_blocks_are_stripped() {
        let out = strip_types("src/components/Counter.svelte", SVELTE).unwrap();
        assert!(out.contains(r#"<script context="module">"#), "{out}");
        assert!(out.contains("<script>\n"), "{out}");
        assert!(!out.contains("lang="), "{out}");
        assert!(!out.contains("interface Props"), "{out}");
        assert!(out.contains("export const kind = \"counter\""), "{out}");
        assert!(out.contains("let { label, start = 0 } = $props()"), "{out}");
        assert!(out.contains("let delta = $state(0)"), "{out}");
    }

    #[test]
    fn an_import_only_the_markup_uses_survives() {
        let out = strip_types("src/components/Counter.svelte", SVELTE).unwrap();
        assert!(out.contains(r#"import Badge from "./Badge.svelte""#), "{out}");
    }

    #[test]
    fn the_markup_below_a_svelte_block_keeps_its_line_numbers() {
        let out = strip_types("src/components/C.svelte", SVELTE).unwrap();
        assert_eq!(line_of(&out, "<h1>"), line_of(SVELTE, "<h1>"), "{out}");
    }

    #[test]
    fn a_svelte_block_without_lang_is_left_alone() {
        let source = "<script>\nconst n = 1;\n</script>\n";
        assert_eq!(strip_types("src/components/C.svelte", source).unwrap(), source);
    }

    #[test]
    fn a_svelte_type_error_is_reported_at_its_line_in_the_file() {
        let source = "<h1>hi</h1>\n\n<script lang=\"ts\">\nconst n: number = ;\n</script>\n";
        let err = strip_types("src/components/C.svelte", source).unwrap_err();
        assert_eq!(err.diagnostics[0].file, "src/components/C.svelte");
        assert_eq!(err.diagnostics[0].line, 4, "{:?}", err.diagnostics);
    }
}
