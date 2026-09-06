use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::resolve::{dirname, join, normalize};
use crate::store::Tenant;

/// One `import.meta.glob(...)` call found in a module, with the byte range of the whole call.
#[derive(Clone, Debug)]
pub struct GlobRef {
    pub start: usize,
    pub end: usize,
    pub patterns: Vec<String>,
    pub eager: bool,
    pub import: Option<String>,
    pub query: Option<String>,
}

const CALL: &str = "import.meta.glob(";

pub fn find(code: &str) -> Vec<GlobRef> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = code[from..].find(CALL) {
        let start = from + i;
        let args_start = start + CALL.len();
        let Some(close) = matching_paren(code, args_start) else { break };
        if let Some(g) = parse_args(&code[args_start..close]) {
            out.push(GlobRef { start, end: close + 1, ..g });
        }
        from = close + 1;
    }
    out
}

fn matching_paren(code: &str, from: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut depth = 1;
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' | b'`' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_args(args: &str) -> Option<GlobRef> {
    let args = args.trim();
    let (patterns, rest) = if let Some(inner) = args.strip_prefix('[') {
        let close = inner.find(']')?;
        (string_literals(&inner[..close]), &inner[close + 1..])
    } else {
        let (s, len) = string_literal(args)?;
        (vec![s], &args[len..])
    };
    if patterns.is_empty() {
        return None;
    }
    let options = rest.trim_start().strip_prefix(',').unwrap_or("");
    let query = option_string(options, "query").or_else(|| {
        option_string(options, "as").map(|v| match v.as_str() {
            "raw" => "raw".to_string(),
            "url" => "url".to_string(),
            other => other.to_string(),
        })
    });
    Some(GlobRef {
        start: 0,
        end: 0,
        patterns,
        eager: option_bool(options, "eager"),
        import: option_string(options, "import"),
        query: query.map(|q| q.trim_start_matches('?').to_string()).filter(|q| !q.is_empty()),
    })
}

fn string_literal(s: &str) -> Option<(String, usize)> {
    let q = s.chars().next()?;
    if q != '"' && q != '\'' && q != '`' {
        return None;
    }
    let end = s[1..].find(q)?;
    Some((s[1..1 + end].to_string(), end + 2))
}

fn string_literals(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s.trim_start();
    while let Some((lit, len)) = string_literal(rest) {
        out.push(lit);
        rest = rest[len..].trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
    out
}

fn option_string(options: &str, key: &str) -> Option<String> {
    let i = find_key(options, key)?;
    string_literal(options[i..].trim_start()).map(|(s, _)| s)
}

fn option_bool(options: &str, key: &str) -> bool {
    find_key(options, key).is_some_and(|i| options[i..].trim_start().starts_with("true"))
}

fn find_key(options: &str, key: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = options[from..].find(key) {
        let at = from + i;
        let before_ok = at == 0 || !options.as_bytes()[at - 1].is_ascii_alphanumeric();
        let after = options[at + key.len()..].trim_start();
        if before_ok && after.starts_with(':') {
            return Some(at + key.len() + (options[at + key.len()..].len() - after.len()) + 1);
        }
        from = at + key.len();
    }
    None
}

pub struct Expansion {
    pub imports: String,
    pub literal: String,
}

pub fn expand(tenant: &Tenant, importer: &str, g: &GlobRef, index: usize, version: u64) -> Result<Expansion, String> {
    let dir = dirname(importer);
    let mut pos = GlobSetBuilder::new();
    let mut neg = GlobSetBuilder::new();
    let mut absolute_keys = false;
    for (i, p) in g.patterns.iter().enumerate() {
        let (negated, p) = match p.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, p.as_str()),
        };
        let abs = p.starts_with('/');
        let resolved = if abs { normalize(p) } else { join(dir, p) };
        let glob = GlobBuilder::new(&resolved).literal_separator(true).build().map_err(|e| e.to_string())?;
        if negated {
            neg.add(glob);
        } else {
            if i == 0 || !absolute_keys {
                absolute_keys = abs;
            }
            pos.add(glob);
        }
    }
    let pos: GlobSet = pos.build().map_err(|e| e.to_string())?;
    let neg: GlobSet = neg.build().map_err(|e| e.to_string())?;
    let mut files: Vec<String> = tenant.list().into_iter().map(|e| e.path).filter(|p| pos.is_match(p) && !neg.is_match(p)).collect();
    files.sort();
    let mut imports = String::new();
    let mut literal = String::from("{");
    for (i, file) in files.iter().enumerate() {
        let key = if absolute_keys { format!("/{file}") } else { relative(dir, file) };
        let url = match &g.query {
            Some(q) => format!("/__sl/m/{file}?{q}&v={version}"),
            None => format!("/__sl/m/{file}?v={version}"),
        };
        let key_json = serde_json::to_string(&key).unwrap_or_default();
        let url_json = serde_json::to_string(&url).unwrap_or_default();
        if g.eager {
            let ident = format!("__sl_glob{index}_{i}");
            match g.import.as_deref() {
                None => imports.push_str(&format!("import * as {ident} from {url_json};\n")),
                Some("default") => imports.push_str(&format!("import {ident} from {url_json};\n")),
                Some(name) => imports.push_str(&format!("import {{ {name} as {ident} }} from {url_json};\n")),
            }
            literal.push_str(&format!(" {key_json}: {ident},"));
        } else {
            match g.import.as_deref() {
                None => literal.push_str(&format!(" {key_json}: () => import({url_json}),")),
                Some(name) => literal.push_str(&format!(
                    " {key_json}: () => import({url_json}).then((m) => m[{}]),",
                    serde_json::to_string(name).unwrap_or_default()
                )),
            }
        }
    }
    literal.push_str(" }");
    Ok(Expansion { imports, literal })
}

fn relative(dir: &str, file: &str) -> String {
    if dir.is_empty() {
        return format!("./{file}");
    }
    let dir_parts: Vec<&str> = dir.split('/').collect();
    let file_parts: Vec<&str> = file.split('/').collect();
    let common = dir_parts.iter().zip(file_parts.iter()).take_while(|(a, b)| a == b).count();
    let ups = dir_parts.len() - common;
    let rest = file_parts[common..].join("/");
    if ups == 0 { format!("./{rest}") } else { format!("{}{rest}", "../".repeat(ups)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_calls_and_options() {
        let code = r#"const a = import.meta.glob("./posts/*.md", { eager: true });
const b = import.meta.glob(['/src/data/*.json', '!/src/data/private.json'], { import: 'default', query: '?raw' });
const c = import.meta.glob("./x/*.ts");"#;
        let g = find(code);
        assert_eq!(g.len(), 3);
        assert!(g[0].eager && g[0].patterns == vec!["./posts/*.md"]);
        assert_eq!(g[1].patterns.len(), 2);
        assert_eq!(g[1].import.as_deref(), Some("default"));
        assert_eq!(g[1].query.as_deref(), Some("raw"));
        assert!(!g[2].eager);
        assert_eq!(&code[g[0].start..g[0].end], r#"import.meta.glob("./posts/*.md", { eager: true })"#);
    }

    #[test]
    fn relative_keys() {
        assert_eq!(relative("src/pages", "src/pages/a.md"), "./a.md");
        assert_eq!(relative("src/pages", "src/content/b.md"), "../content/b.md");
        assert_eq!(relative("", "x.md"), "./x.md");
    }
}
