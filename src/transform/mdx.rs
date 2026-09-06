use satteri_arena::mdx_types::Place;
use satteri_arena::{Arena, Hast, StringRef};
use satteri_ast::hast::codec::{decode_element_prop, decode_element_prop_count, decode_element_tag, encode_element_data};
use satteri_ast::hast::{HastNodeType, mdast_arena_to_hast_arena};
use satteri_ast::shared::PROP_STRING;
use satteri_mdxjs::{ElementAttributeNameCase, JsxRuntime, Options};
use satteri_pulldown_cmark::{MDX_OPTIONS, strip_leading_bom};
use serde_json::{Map, Value};

use super::content::{Heading, Slugs, slugify, split_frontmatter, yaml_to_json};
use super::js::{self, line_col};
use super::markdown::page_url;
use super::{BuildError, Diag, json_str};

pub struct Compiled {
    pub code: String,
    pub frontmatter: Value,
    pub headings: Vec<Heading>,
}

/// Compiles MDX to a module that imports `astro/jsx-runtime`, with ids on headings like Astro's processor.
pub fn compile(path: &str, source: &str) -> Result<Compiled, BuildError> {
    let source = strip_leading_bom(source);
    let (fm, body) = split_frontmatter(source);
    let frontmatter = match fm.map(yaml_to_json).transpose() {
        Ok(fm) => fm.unwrap_or_else(|| Value::Object(Map::new())),
        Err(reason) => return Err(error(path, &reason, 1, 1)),
    };
    // Blank lines stand in for the frontmatter so positions in diagnostics match the file.
    let text = format!("{}{body}", "\n".repeat(source[..source.len() - body.len()].matches('\n').count()));
    let (mdast, errors) = satteri_pulldown_cmark::parse(&text, MDX_OPTIONS);
    if let Some((offset, reason)) = errors.first() {
        let (line, column) = line_col(&text, *offset);
        return Err(error(path, reason, line, column));
    }
    let mut hast = mdast_arena_to_hast_arena(&mdast);
    hast.mdx = true;
    let mut headings = Vec::new();
    collect_headings(&mut hast, 0, &frontmatter, &mut headings, &mut Slugs::default());
    let options = Options {
        jsx_runtime: Some(JsxRuntime::Automatic),
        jsx_import_source: Some("astro".into()),
        element_attribute_name_case: ElementAttributeNameCase::Html,
        filepath: Some(path.to_string()),
        ..Options::default()
    };
    let code = satteri_mdxjs::compile_hast_arena(&hast, &options).map_err(|m| {
        let (line, column) = match m.place.as_deref() {
            Some(Place::Point(p)) => (p.line as u32, p.column as u32),
            Some(Place::Position(p)) => (p.start.line as u32, p.start.column as u32),
            None => (0, 0),
        };
        error(path, &m.reason, line, column)
    })?;
    Ok(Compiled { code, frontmatter, headings })
}

fn error(path: &str, text: &str, line: u32, column: u32) -> BuildError {
    let diag = Diag { severity: "error".into(), text: text.to_string(), hint: String::new(), file: path.to_string(), line, column };
    BuildError::compile(format!("{path}: {text}"), vec![diag])
}

fn collect_headings(hast: &mut Arena<Hast>, id: u32, frontmatter: &Value, out: &mut Vec<Heading>, slugs: &mut Slugs) {
    if hast.get_node(id).node_type == HastNodeType::Element as u8 {
        let data = hast.get_type_data(id);
        let tag = decode_element_tag(data);
        let depth = match hast.get_str(tag).as_bytes() {
            [b'h', d @ b'1'..=b'6'] => Some(d - b'0'),
            _ => None,
        };
        if let Some(depth) = depth {
            let props: Vec<(StringRef, u8, StringRef)> =
                (0..decode_element_prop_count(data)).map(|i| decode_element_prop(data, i)).collect();
            let existing =
                props.iter().find(|(name, _, _)| hast.get_str(*name) == "id").map(|(_, _, value)| hast.get_str(*value).to_string());
            let mut text = String::new();
            heading_text(hast, id, frontmatter, &mut text);
            let slug = match &existing {
                Some(id) => {
                    slugs.claim(id);
                    id.clone()
                }
                None => slugs.unique(slugify(&text)),
            };
            if existing.is_none() {
                let mut props = props;
                props.push((hast.alloc_string("id"), PROP_STRING, hast.alloc_string(&slug)));
                hast.set_type_data(id, &encode_element_data(tag, &props));
            }
            out.push(Heading { depth, slug, text });
        }
    }
    for child in hast.get_children(id).to_vec() {
        collect_headings(hast, child, frontmatter, out, slugs);
    }
}

fn heading_text(hast: &Arena<Hast>, id: u32, frontmatter: &Value, out: &mut String) {
    let value = |hast: &Arena<Hast>| hast.get_str(StringRef::from_bytes(&hast.get_type_data(id)[..8])).to_string();
    match HastNodeType::from_u8(hast.get_node(id).node_type) {
        Some(HastNodeType::Text) => out.push_str(&value(hast)),
        Some(HastNodeType::MdxTextExpression | HastNodeType::MdxFlowExpression) => {
            let expr = value(hast);
            out.push_str(frontmatter_string(expr.trim(), frontmatter).unwrap_or(&expr));
        }
        _ => {
            for child in hast.get_children(id) {
                heading_text(hast, *child, frontmatter, out);
            }
        }
    }
}

/// `frontmatter.a.b` or `frontmatter["a"][0]` looked up in the frontmatter, when it names a string.
fn frontmatter_string<'a>(expr: &str, frontmatter: &'a Value) -> Option<&'a str> {
    let mut rest = expr.strip_prefix("frontmatter")?;
    let mut value = frontmatter;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let key = if let Some(after) = rest.strip_prefix('.') {
            let end = after.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(after.len());
            rest = &after[end..];
            after[..end].to_string()
        } else {
            let after = rest.strip_prefix('[')?;
            let end = after.find(']')?;
            rest = &after[end + 1..];
            after[..end].trim_matches(|c| c == '"' || c == '\'').to_string()
        };
        value = match value {
            Value::Object(map) => map.get(&key)?,
            Value::Array(items) => items.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    value.as_str()
}

/// The page module `@astrojs/mdx` would produce: `frontmatter`, `file`, `url`, `getHeadings`, the `layout`
/// wrapper, and a default export tagged for the `astro:jsx` renderer.
pub fn page_module(path: &str, source: &str) -> Result<String, BuildError> {
    let compiled = compile(path, source)?;
    let exports = js::exports(&compiled.code);
    let mut code = compiled.code.replace("\nexport default MDXContent;\n", "\n");
    code.push_str(&format!("export const frontmatter = {};\n", compiled.frontmatter));
    if !exports.iter().any(|e| e == "file") {
        code.push_str(&format!("export const file = {};\n", json_str(path)));
    }
    if !exports.iter().any(|e| e == "url") {
        code.push_str(&format!("export const url = {};\n", json_str(&page_url(path))));
    }
    code.push_str(&format!(
        "export function getHeadings() {{ return {}; }}\n",
        serde_json::to_string(&compiled.headings).unwrap_or_else(|_| "[]".into())
    ));
    if let Some(layout) = compiled.frontmatter.get("layout").and_then(|l| l.as_str()) {
        code = code.replacen("function MDXContent(", "function __OriginalMDXContent__(", 1);
        code.push_str(&format!(
            "import {{ jsx as __astro_layout_jsx__ }} from \"astro/jsx-runtime\";\n\
             import __astro_layout_component__ from {};\n\
             function MDXContent(props) {{\n  \
               const content = __OriginalMDXContent__(props);\n  \
               const {{ layout, ...frontmatterContent }} = frontmatter;\n  \
               frontmatterContent.file = file;\n  \
               frontmatterContent.url = url;\n  \
               return __astro_layout_jsx__(__astro_layout_component__, {{ file, url, content: frontmatterContent, frontmatter: frontmatterContent, headings: getHeadings(), \"server:root\": true, children: content }});\n\
             }}\n",
            json_str(layout)
        ));
    }
    let components = if exports.iter().any(|e| e == "components") { ", ...components" } else { "" };
    code.push_str(&format!(
        "import {{ Fragment as __sl_Fragment }} from \"astro/jsx-runtime\";\n\
         import {{ __astro_tag_component__ }} from \"/__sl/astro.js\";\n\
         export const Content = (props = {{}}) => MDXContent({{ ...props, components: {{ Fragment: __sl_Fragment{components}, ...props.components }} }});\n\
         export default Content;\n\
         Content[Symbol.for(\"mdx-component\")] = true;\n\
         Content[Symbol.for(\"astro.needsHeadRendering\")] = !frontmatter.layout;\n\
         Content.moduleId = {};\n\
         __astro_tag_component__(Content, \"astro:jsx\");\n",
        json_str(path)
    ));
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO: &str = "---\nlayout: ../layouts/Markdown.astro\ntitle: Hello MDX\n---\nimport Card from \"../components/Card.astro\";\n\n## {frontmatter.title}\n\n<Card title=\"x\" />\n\n## Second\n\n## Second\n";

    #[test]
    fn compiles_to_astro_jsx() {
        let out = page_module("src/pages/demo.mdx", DEMO).unwrap();
        assert!(out.contains("from \"astro/jsx-runtime\""));
        assert!(out.contains("import Card from \"../components/Card.astro\""));
        assert!(out.contains("export const frontmatter = {\"layout\":\"../layouts/Markdown.astro\",\"title\":\"Hello MDX\"};"));
        assert!(out.contains("export const file = \"src/pages/demo.mdx\";"));
        assert!(out.contains("export const url = \"/demo\";"));
        assert!(out.contains("import __astro_layout_component__ from \"../layouts/Markdown.astro\";"));
        assert!(out.contains("function __OriginalMDXContent__("));
        assert!(!out.contains("export default MDXContent;"));
        assert!(out.contains("export default Content;"));
        assert!(out.contains("__astro_tag_component__(Content, \"astro:jsx\");"));
    }

    #[test]
    fn headings_get_ids_and_are_listed() {
        let c = compile("src/pages/demo.mdx", DEMO).unwrap();
        let slugs: Vec<&str> = c.headings.iter().map(|h| h.slug.as_str()).collect();
        assert_eq!(slugs, ["hello-mdx", "second", "second-1"]);
        assert_eq!(c.headings[0].text, "Hello MDX");
        assert_eq!(c.headings[0].depth, 2);
        assert!(c.code.contains("\"hello-mdx\""));
        assert!(c.code.contains("\"second-1\""));
    }

    #[test]
    fn keeps_user_exports() {
        let out = page_module("src/pages/x.mdx", "export const url = \"/custom\";\nexport const components = {};\n\n# Hi\n").unwrap();
        assert_eq!(out.matches("export const url").count(), 1);
        assert!(out.contains("Fragment: __sl_Fragment, ...components, ...props.components"));
    }

    #[test]
    fn reports_positions_past_the_frontmatter() {
        let Err(err) = compile("src/pages/bad.mdx", "---\ntitle: x\n---\n\n<Card\n") else { panic!("unclosed tag compiled") };
        assert_eq!(err.diagnostics[0].line, 5);
        assert_eq!(err.diagnostics[0].file, "src/pages/bad.mdx");
    }

    /// Issue #63: assigning ids by rescanning the headings already assigned was O(n^3), so 25.6 KB
    /// of `# a` took 115 s of one core in release — and the 64 KiB source cap admits 2.56x that,
    /// about forty minutes. Nothing else bounded it: `# a\n` has no bracket and no blockquote
    /// marker, so it passes the nesting pre-scan, and it is far under the size cap.
    #[test]
    fn repeated_headings_are_not_cubic() {
        let src = "# a\n".repeat(6400);
        let t = std::time::Instant::now();
        let c = compile("x.mdx", &src).unwrap();
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "took {:?}", t.elapsed());
        assert_eq!(c.headings.len(), 6400);
        assert_eq!(c.headings[6399].slug, "a-6399");
    }
}
