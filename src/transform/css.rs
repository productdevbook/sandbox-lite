use super::json_str;
use crate::resolve::join;

pub fn to_module(key: &str, css: &str, importer_dir: &str) -> String {
    let css = rewrite_relative(css, importer_dir);
    format!("const css = {};\n(globalThis.__sl_css ||= new Map()).set({}, css);\nexport default css;\n", json_str(&css), json_str(key))
}

fn is_relative(v: &str) -> bool {
    v.starts_with("./") || v.starts_with("../")
}

/// Relative `url()` and `@import` targets are rewritten to absolute daemon URLs,
/// because the CSS ends up in a `<style>` tag whose base URL is the page, not the file.
pub fn rewrite_relative(css: &str, dir: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut i = 0;
    let bytes = css.as_bytes();
    while i < bytes.len() {
        if css[i..].starts_with("url(") {
            let start = i + 4;
            let Some(close) = css[start..].find(')') else {
                out.push_str(&css[i..]);
                break;
            };
            let raw = &css[start..start + close];
            let trimmed = raw.trim();
            let (quote, value) = strip_quotes(trimmed);
            out.push_str("url(");
            out.push_str(&rewritten(value, quote, dir));
            out.push(')');
            i = start + close + 1;
        } else if css[i..].starts_with("@import") {
            out.push_str("@import");
            i += 7;
            let ws_end = css[i..].find(|c: char| !c.is_whitespace()).map(|n| i + n).unwrap_or(bytes.len());
            out.push_str(&css[i..ws_end]);
            i = ws_end;
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let q = bytes[i] as char;
                if let Some(end) = css[i + 1..].find(q) {
                    let value = &css[i + 1..i + 1 + end];
                    out.push_str(&rewritten(value, Some(q), dir));
                    i = i + 1 + end + 1;
                }
            }
        } else {
            let ch = css[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn strip_quotes(v: &str) -> (Option<char>, &str) {
    if v.len() >= 2 && (v.starts_with('"') && v.ends_with('"') || v.starts_with('\'') && v.ends_with('\'')) {
        (Some(v.chars().next().unwrap()), &v[1..v.len() - 1])
    } else {
        (None, v)
    }
}

fn rewritten(value: &str, quote: Option<char>, dir: &str) -> String {
    let target = if is_relative(value) { format!("/__sl/raw/{}", join(dir, value)) } else { value.to_string() };
    match quote {
        Some(q) => format!("{q}{target}{q}"),
        None => target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_only_relative_targets() {
        let css = r#"@import "tailwindcss"; @import './base.css'; .a{background:url(../img/x.png)} .b{background:url("/fonts/a.woff2")} .c{background:url(data:x)}"#;
        let out = rewrite_relative(css, "src/styles");
        assert!(out.contains(r#"@import "tailwindcss""#));
        assert!(out.contains("@import '/__sl/raw/src/styles/base.css'"));
        assert!(out.contains("url(/__sl/raw/src/img/x.png)"));
        assert!(out.contains(r#"url("/fonts/a.woff2")"#));
        assert!(out.contains("url(data:x)"));
    }
}
