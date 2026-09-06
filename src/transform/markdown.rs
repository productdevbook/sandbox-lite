use super::content::parse_markdown;
use super::{BuildError, Diag, json_str};

pub fn page_module(path: &str, source: &str) -> Result<String, BuildError> {
    let md = parse_markdown(source).map_err(|text| {
        let diag = Diag { severity: "error".into(), text: text.clone(), hint: String::new(), file: path.to_string(), line: 1, column: 1 };
        BuildError::compile(format!("{path}: {text}"), vec![diag])
    })?;
    let layout = md.frontmatter.get("layout").and_then(|l| l.as_str()).map(|s| s.to_string());
    let url = page_url(path);
    let mut code = String::with_capacity(md.html.len() + 1024);
    code.push_str("import { createComponent, render, renderComponent, unescapeHTML } from \"/__sl/astro.js\";\n");
    match &layout {
        Some(l) => code.push_str(&format!("import Layout from {};\n", json_str(l))),
        None => code.push_str("const Layout = undefined;\n"),
    }
    code.push_str(&format!("export const frontmatter = {};\n", md.frontmatter));
    code.push_str(&format!("export const file = {};\n", json_str(path)));
    code.push_str(&format!("export const url = {};\n", json_str(&url)));
    code.push_str(&format!("const headings = {};\n", serde_json::to_string(&md.headings).unwrap_or_else(|_| "[]".into())));
    code.push_str(&format!("const html = {};\n", json_str(&md.html)));
    code.push_str(&format!("const raw = {};\n", json_str(&md.body)));
    code.push_str("export function getHeadings() { return headings; }\n");
    code.push_str("export function rawContent() { return raw; }\n");
    code.push_str("export function compiledContent() { return html; }\n");
    code.push_str(&format!(
        "export const Content = createComponent((result, props, slots) => render`${{unescapeHTML(html)}}`, {});\n",
        json_str(&format!("{path}:Content"))
    ));
    code.push_str(&format!(
        "export default createComponent((result, props, slots) => Layout\n  ? render`${{renderComponent(result, \"Layout\", Layout, {{ frontmatter, headings, url, file, rawContent, compiledContent, ...props }}, {{ default: () => render`${{unescapeHTML(html)}}` }})}}`\n  : render`${{unescapeHTML(html)}}`, {});\n",
        json_str(path)
    ));
    Ok(code)
}

pub(super) fn page_url(path: &str) -> String {
    let rest = path.strip_prefix("src/pages/").unwrap_or(path);
    let stem = rest.rsplit_once('.').map(|(s, _)| s).unwrap_or(rest);
    let stem = stem.strip_suffix("/index").or_else(|| (stem == "index").then_some("")).unwrap_or(stem);
    format!("/{stem}")
}
