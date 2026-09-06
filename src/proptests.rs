//! Property tests for the parsers that read attacker-controlled bytes: request
//! paths, query strings, Host headers, tar entry names, tenant files and
//! compiled module source.
//! None may panic, and nothing path-shaped may climb out of the tenant.

use proptest::prelude::*;
use serde_json::{Map, Value, json};

use crate::http::archive::entry_path;
use crate::http::preview::percent_decode;
use crate::http::tenant_from_host;
use crate::resolve::{import_map, join, normalize, package_name, strip_jsonc, tsconfig_paths, urlenc};
use crate::store::{clean_path, valid_id};
use crate::transform::content::{parse_markdown, slugify, split_frontmatter, yaml_to_json};
use crate::transform::css::rewrite_relative;
use crate::transform::{Config, Engine, Kind, glob, js, mdx, svg_size};

#[rustfmt::skip]
const TOKENS: &[&str] = &[
    "/", "//", "\\", ".", "..", "/../", "/./", "src/pages", "index", ".astro", "public/", "@/",
    "%", "%2", "%2e", "%2E%2e", "%2F", "%c3", "%A9", "%FF", "%00", "%+1", "%-1", "%%",
    "\"", "'", "`", "\\\"", "\\\\", "(", ")", "[", "]", "{", "}", ",", ":", ";", "=", "&", "?", "#", "*", "!", "@", "-", "_", "<", ">",
    " ", "\t", "\n", "\r", "\r\n", "\0", "\u{1}", "\u{b}", "\u{7f}", "\u{85}", "\u{a0}", "\u{feff}", "\u{200b}", "\u{2028}",
    "é", "€", "𝄞", "字", "\u{301}", "\u{fffd}", "İ",
    "true", "false", "null", "0", "1", "-1", "1e999", "inf", "NaN", "px",
    "/*", "*/", "---", "...", "<svg", "<svg ", "width=", "height=", "viewBox=", "url(", "@import", "import.meta.glob(",
    "eager", "query", "import", "as", "raw", "url", "type=", "index=", "v=", "style", "script",
    "compilerOptions", "baseUrl", "paths", "imports", "&a", "*a", "<<", "!!binary", "- ", "? ", "|", "localhost",
];

const QUOTES: &[&str] = &["", "\"", "'"];

fn tokens() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(TOKENS), 0..24).prop_map(|v| v.concat())
}

fn unicode() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..48).prop_map(String::from_iter)
}

fn short() -> impl Strategy<Value = String> {
    prop_oneof![4 => tokens(), 1 => unicode(), 1 => "[ -~]{0,48}"]
}

/// A chunk repeated up to 64 KiB plus a tail: the shape that exposes a quadratic scan or an off-by-one at the end.
fn long() -> impl Strategy<Value = String> {
    (prop::sample::select(TOKENS), tokens(), 0usize..=65536, short()).prop_map(|(head, rest, len, tail)| {
        let chunk = format!("{head}{rest}");
        chunk.repeat(len / chunk.len()) + &tail
    })
}

fn input() -> impl Strategy<Value = String> {
    prop_oneof![7 => short(), 1 => long()]
}

fn jsonc() -> impl Strategy<Value = (String, Value)> {
    (short(), short(), short()).prop_map(|(key, val, comment)| {
        let line = comment.replace('\n', " ");
        let block = comment.replace('*', " ");
        let (k, v) = (Value::String(key.clone()), Value::String(val.clone()));
        let text = format!("{{ // {line}\n /* {block} */ {k}: [ {v}, /* {block} */ ], // {line}\n }}");
        (text, json!({ key: [val] }))
    })
}

fn tsconfig() -> impl Strategy<Value = String> {
    (short(), prop::collection::vec((short(), short()), 0..4)).prop_map(|(base, paths)| {
        let paths: Map<String, Value> = paths.into_iter().map(|(k, v)| (k, json!([v]))).collect();
        json!({ "compilerOptions": { "baseUrl": base, "paths": paths } }).to_string()
    })
}

fn yaml() -> impl Strategy<Value = String> {
    prop::collection::vec((short(), short()), 0..6).prop_map(|pairs| pairs.iter().map(|(k, v)| format!("{k}: {v}\n")).collect())
}

fn markdown() -> impl Strategy<Value = String> {
    (yaml(), short()).prop_map(|(fm, body)| format!("---\n{fm}---\n{body}"))
}

fn mdx() -> impl Strategy<Value = String> {
    (yaml(), short(), short())
        .prop_map(|(fm, heading, text)| format!("---\n{fm}---\nimport X from \"./x.astro\";\n\n## {heading}\n\n<X>{text}</X>\n\n{text}\n"))
}

fn svg() -> impl Strategy<Value = String> {
    (short(), short(), short()).prop_map(|(w, h, vb)| format!("<svg xmlns=\"x\" width={w} height=\"{h}\" viewBox='{vb}'>"))
}

fn css() -> impl Strategy<Value = String> {
    (short(), short(), prop::sample::select(QUOTES))
        .prop_map(|(a, b, q)| format!("@import {q}{a}{q}; .x{{background:url({q}{b}{q})}} @import url({a})"))
}

/// The stack `Engine::build` gives every parser; running one here on the 2 MiB test-thread stack
/// aborts the whole test binary instead of failing a case.
fn parser_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    let engine = Engine::new(Config::default(), std::sync::Arc::new(crate::metrics::Metrics::default()));
    engine.on_parser_stack(f).expect("compiler thread")
}

fn assert_inside_tenant(path: &str) -> Result<(), TestCaseError> {
    prop_assert!(!path.starts_with('/'), "absolute path {path:?}");
    prop_assert!(path.is_empty() || path.split('/').all(|s| !s.is_empty() && s != "." && s != ".."), "dot segment in {path:?}");
    Ok(())
}

fn longest_key_first(pairs: &[(String, String)]) -> bool {
    pairs.windows(2).all(|w| w[0].0.len() >= w[1].0.len())
}

/// The paths `rewrite_relative` put after `/__sl/raw/`, cut at the delimiter that closes the construct they sit in.
fn rewritten_paths(out: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    for (i, _) in out.match_indices("/__sl/raw/") {
        let (before, rest) = (&out[..i], &out[i + "/__sl/raw/".len()..]);
        let quote = before.chars().last().filter(|c| *c == '"' || *c == '\'');
        let path = if before.trim_end_matches(['"', '\'']).ends_with("url(") {
            let value = &rest[..rest.find(')').unwrap_or(rest.len())];
            quote.and_then(|q| value.strip_suffix(q)).unwrap_or(value)
        } else {
            &rest[..quote.and_then(|q| rest.find(q)).unwrap_or(rest.len())]
        };
        paths.push(path);
    }
    paths
}

proptest! {
    #[test]
    fn clean_path_never_escapes(raw in input()) {
        for candidate in [raw.clone(), percent_decode(&raw)] {
            if let Some(p) = clean_path(&candidate) {
                prop_assert!(!p.is_empty() && !p.ends_with('/') && !p.contains('\0') && !p.contains('\\'), "{p:?}");
                assert_inside_tenant(&p)?;
                let again = clean_path(&p);
                prop_assert_eq!(again.as_deref(), Some(p.as_str()));
            }
        }
    }

    #[test]
    fn tar_entry_names_never_escape_the_tenant(raw in input()) {
        if let Some(p) = entry_path(&raw) {
            assert_inside_tenant(&p)?;
            prop_assert!(!raw.starts_with('/'), "absolute entry name {raw:?} was accepted");
            let again = entry_path(&p);
            prop_assert_eq!(again.as_deref(), Some(p.as_str()));
        }
    }

    #[test]
    fn valid_id_is_a_dns_label(id in input()) {
        if valid_id(&id) {
            prop_assert!(id.is_ascii() && (1..=63).contains(&id.len()) && !id.starts_with('-') && !id.ends_with('-'));
        }
    }

    #[test]
    fn tenant_from_host_yields_only_valid_ids(host in input(), domain in input()) {
        if let Some(id) = tenant_from_host(&host, &domain) {
            prop_assert!(valid_id(&id));
            prop_assert_eq!(format!("{id}.{domain}"), host);
        }
    }

    #[test]
    fn valid_ids_survive_the_host_round_trip(id in "[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?", domain in input()) {
        prop_assert!(valid_id(&id));
        prop_assert_eq!(tenant_from_host(&format!("{id}.{domain}"), &domain), Some(id));
    }

    #[test]
    fn normalize_and_join_never_climb(dir in input(), rel in input()) {
        let n = normalize(&rel);
        assert_inside_tenant(&n)?;
        prop_assert_eq!(normalize(&n), n);
        let j = join(&dir, &rel);
        assert_inside_tenant(&j)?;
        prop_assert!(j.len() <= dir.len() + rel.len() + 1);
    }

    #[test]
    fn package_name_splits_without_loss(spec in input()) {
        let (name, subpath) = package_name(&spec);
        prop_assert!(subpath.is_empty() || subpath.starts_with('/'));
        prop_assert_eq!(format!("{name}{subpath}"), spec);
    }

    #[test]
    fn strip_jsonc_never_grows(text in input()) {
        prop_assert!(strip_jsonc(&text).len() <= text.len());
    }

    #[test]
    fn strip_jsonc_keeps_the_value((text, value) in jsonc()) {
        let stripped = strip_jsonc(&text);
        let parsed: Value = serde_json::from_str(&stripped).map_err(|e| TestCaseError::fail(format!("{e} in {stripped:?}")))?;
        prop_assert_eq!(parsed, value);
    }

    #[test]
    fn tsconfig_targets_stay_inside_the_tenant(text in prop_oneof![input(), tsconfig()]) {
        // Junk that does not parse is refused rather than read as no aliases; what it does parse to has to hold.
        let Ok(aliases) = tsconfig_paths(&text) else { return Ok(()) };
        for (key, target) in &aliases {
            prop_assert!(!key.ends_with('*'));
            assert_inside_tenant(target.strip_suffix('/').unwrap_or(target))?;
        }
        prop_assert!(longest_key_first(&aliases));
    }

    #[test]
    fn import_map_keeps_every_string_entry(entries in prop::collection::vec((short(), short()), 0..4), junk in input()) {
        let map: Map<String, Value> = entries.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
        let mut want: Vec<(String, String)> = map.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect();
        let mut got = import_map(&json!({ "imports": map, "site": junk }).to_string()).unwrap();
        prop_assert!(longest_key_first(&got));
        got.sort();
        want.sort();
        prop_assert_eq!(got, want);
        if let Ok(m) = import_map(&junk) {
            prop_assert!(longest_key_first(&m));
        }
    }

    #[test]
    fn glob_spans_are_in_bounds(code in input()) {
        let mut last = 0;
        for g in glob::find(&code) {
            prop_assert!(last <= g.start && g.start < g.end && g.end <= code.len(), "span {}..{} of {} bytes", g.start, g.end, code.len());
            prop_assert!(code.is_char_boundary(g.start) && code.is_char_boundary(g.end));
            prop_assert!(code[g.start..].starts_with("import.meta.glob("));
            let closes = matches!(code.as_bytes()[g.end - 1], b')' | b']' | b'}');
            prop_assert!(closes, "span ends in {:?}", &code[g.start..g.end]);
            prop_assert!(!g.patterns.is_empty());
            last = g.end;
        }
    }

    #[test]
    fn glob_options_round_trip(
        pattern in "[^\"'`\\\\\\x00-\\x1f]{0,40}",
        import in "[A-Za-z_$][A-Za-z0-9_$]{0,10}",
        query in "[^\"'`\\\\\\x00-\\x1f]{0,20}",
        eager in any::<bool>(),
        pad in "[a-z0-9 ;=\\n]{0,48}",
    ) {
        let call = format!("import.meta.glob(\"{pattern}\", {{ eager: {eager}, import: '{import}', query: `{query}` }})");
        let code = format!("{pad}const m = {call};{pad}");
        let found = glob::find(&code);
        prop_assert_eq!(found.len(), 1);
        let g = &found[0];
        prop_assert_eq!(&code[g.start..g.end], call.as_str());
        prop_assert_eq!(&g.patterns, &vec![pattern]);
        prop_assert_eq!(g.eager, eager);
        prop_assert_eq!(g.import.as_deref(), Some(import.as_str()));
        prop_assert_eq!(g.query.as_deref(), Some(query.trim_start_matches('?')).filter(|q| !q.is_empty()));
    }

    #[test]
    fn percent_decode_never_grows(s in input()) {
        let out = percent_decode(&s);
        prop_assert!(out.len() <= s.len());
        if !s.contains('%') {
            prop_assert_eq!(out, s);
        }
    }

    #[test]
    fn percent_decode_inverts_urlenc(s in input()) {
        prop_assert_eq!(percent_decode(&urlenc(&s)), s);
    }

    #[test]
    fn frontmatter_split_partitions_the_source(src in prop_oneof![input(), markdown()]) {
        let (fm, body) = split_frontmatter(&src);
        let s = src.strip_prefix('\u{feff}').unwrap_or(&src);
        prop_assert!(s.ends_with(body));
        if let Some(fm) = fm {
            prop_assert!(s.starts_with("---") && s.contains(fm));
            prop_assert!(fm.len() + body.len() + 7 <= s.len());
        }
        // Frontmatter that is not a YAML mapping is refused; whatever is accepted is still an object.
        if let Ok(md) = parse_markdown(&src) {
            prop_assert!(md.frontmatter.is_object());
            prop_assert_eq!(md.body.as_str(), body);
        }
    }

    #[test]
    fn mdx_page_module_is_a_module(src in prop_oneof![input(), markdown(), mdx()]) {
        // Nested blockquotes recurse in the MDX parser, so run it where the daemon runs it.
        if let Ok(code) = parser_stack(|| mdx::page_module("src/pages/x.mdx", &src)) {
            prop_assert!(js::scan_checked(&code).is_some(), "generated module does not parse:\n{code}");
            prop_assert!(code.contains("\nexport const frontmatter = {"), "no frontmatter export in:\n{code}");
            prop_assert!(code.contains("\n__astro_tag_component__(Content, \"astro:jsx\");\n"), "Content not tagged in:\n{code}");
        }
    }

    #[test]
    fn yaml_to_json_is_an_object_whenever_it_answers(yaml in prop_oneof![input(), yaml()]) {
        if let Ok(v) = yaml_to_json(&yaml) {
            prop_assert!(v.is_object());
        }
    }

    #[test]
    fn slugs_are_clean(text in input()) {
        let slug = slugify(&text);
        prop_assert!(!slug.starts_with('-') && !slug.ends_with('-') && !slug.contains("--"), "{slug:?}");
        // `İ` lowercases to `i` plus a combining dot, which is not alphanumeric
        prop_assert!(slug.chars().all(|c| c == '-' || c.is_alphanumeric() || c == '\u{307}'), "{slug:?}");
    }

    #[test]
    fn svg_size_is_bounded(svg in prop_oneof![input(), svg()]) {
        let (w, h) = svg_size(&svg);
        prop_assert!(w <= u32::MAX as usize && h <= u32::MAX as usize, "{w}x{h}");
    }

    #[test]
    fn svg_size_reads_quoted_dimensions(w in any::<u32>(), h in any::<u32>(), gap in " {1,3}") {
        let want = (w as usize, h as usize);
        prop_assert_eq!(svg_size(&format!("<svg{gap}width=\"{w}px\"{gap}height='{h}'>")), want);
        prop_assert_eq!(svg_size(&format!("<svg{gap}viewBox=\"0 0 {w} {h}\">")), want);
        prop_assert_eq!(svg_size(&format!("<svg{gap}viewBox=\"0,0,{w},{h}\"{gap}width=\"{w}\">")), want);
    }

    #[test]
    fn css_rewrites_stay_inside_the_tenant(css in prop_oneof![input(), css()], dir in short()) {
        prop_assume!(!css.contains("/__sl/raw/"));
        let out = rewrite_relative(&css, &dir);
        if !css.contains("url(") && !css.contains("@import") {
            prop_assert_eq!(&out, &css);
        }
        for path in rewritten_paths(&out) {
            assert_inside_tenant(path)?;
        }
    }

    #[test]
    fn kind_from_query_is_total(query in input()) {
        let kind = Kind::from_query(&query);
        let flagged = query.split('&').any(|p| p == "raw" || p == "url");
        prop_assert_eq!(flagged, matches!(kind, Kind::Raw | Kind::Url));
    }
}
