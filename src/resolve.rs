use crate::store::Tenant;

pub const SHIM_PREFIX: &str = "/__sl/shim/";
const EXTS: &[&str] = &["", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".mts", ".json", ".astro", ".md", ".mdx", ".vue", ".svelte"];
const INDEX: &[&str] = &["/index.ts", "/index.tsx", "/index.js", "/index.jsx", "/index.mjs", "/index.astro"];

fn read_config(tenant: &Tenant, path: &str) -> Result<Option<String>, String> {
    tenant.read_text(path).map_err(|e| format!("{path}: {e}"))
}

pub struct Resolver<'a> {
    tenant: &'a Tenant,
    cdn: &'a str,
    version: u64,
    aliases: Vec<(String, String)>,
    imports: Vec<(String, String)>,
    deps: Vec<(String, String)>,
}

impl<'a> Resolver<'a> {
    /// A config file the tenant does not have is no aliases and no import map. One it has that does
    /// not parse is refused: dropped, every specifier it would have redirected silently resolves to
    /// a CDN URL instead of the tenant's own file.
    pub fn new(tenant: &'a Tenant, cdn: &'a str, version: u64) -> Result<Self, String> {
        let aliases = read_config(tenant, "tsconfig.json")?.map(|t| tsconfig_paths(&t)).transpose()?.unwrap_or_default();
        let imports = read_config(tenant, "sandbox-lite.json")?.map(|t| import_map(&t)).transpose()?.unwrap_or_default();
        let deps = package_deps(tenant)?;
        Ok(Resolver { tenant, cdn, version, aliases, imports, deps })
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn resolve(&self, importer: &str, spec: &str) -> String {
        if is_external(spec) {
            return spec.to_string();
        }
        if let Some(rest) = spec.strip_prefix("astro:") {
            return shim_url(&format!("astro-{}", rest.replace('/', "-")));
        }
        if let Some(name) = astro_shim(spec) {
            return shim_url(name);
        }
        let (path_part, query) = split_query(spec);
        let candidate = if path_part.starts_with("./") || path_part.starts_with("../") || path_part == "." || path_part == ".." {
            Some(join(dirname(importer), path_part))
        } else if let Some(rest) = path_part.strip_prefix('/') {
            Some(normalize(rest))
        } else {
            self.alias(path_part).map(|t| normalize(&t))
        };
        if let Some(cand) = candidate {
            return match self.probe(&cand) {
                Some(found) => self.module_url(&found, query),
                None => missing_url(spec, importer),
            };
        }
        if let Some(url) = self.mapped(spec) {
            return url;
        }
        cdn_url(self.cdn, &self.deps, spec)
    }

    fn probe(&self, cand: &str) -> Option<String> {
        for ext in EXTS {
            let p = format!("{cand}{ext}");
            if self.tenant.exists(&p) {
                return Some(p);
            }
        }
        for idx in INDEX {
            let p = format!("{cand}{idx}");
            if self.tenant.exists(&p) {
                return Some(p);
            }
        }
        None
    }

    fn module_url(&self, path: &str, query: &str) -> String {
        if query.is_empty() { format!("/__sl/m/{path}?v={}", self.version) } else { format!("/__sl/m/{path}?{query}&v={}", self.version) }
    }

    fn alias(&self, spec: &str) -> Option<String> {
        for (key, target) in &self.aliases {
            if key.ends_with('/') {
                if let Some(rest) = spec.strip_prefix(key.as_str()) {
                    return Some(format!("{target}{rest}"));
                }
            } else if spec == key {
                return Some(target.clone());
            }
        }
        None
    }

    fn mapped(&self, spec: &str) -> Option<String> {
        for (key, target) in &self.imports {
            if key.ends_with('/') {
                if let Some(rest) = spec.strip_prefix(key.as_str()) {
                    return Some(format!("{target}{rest}"));
                }
            } else if spec == key {
                return Some(target.clone());
            }
        }
        None
    }
}

/// `react/jsx-runtime` becomes `https://esm.sh/react@^19.2.8/jsx-runtime` when package.json pins react.
pub fn cdn_url(cdn: &str, deps: &[(String, String)], spec: &str) -> String {
    let cdn = cdn.trim_end_matches('/');
    let (name, subpath) = package_name(spec);
    match deps.iter().find(|(n, _)| n == name) {
        Some((_, range)) if !range.is_empty() && !range.contains(':') && !range.starts_with("file") => {
            format!("{cdn}/{name}@{range}{subpath}")
        }
        _ => format!("{cdn}/{spec}"),
    }
}

pub fn package_name(spec: &str) -> (&str, &str) {
    let segments = if spec.starts_with('@') { 2 } else { 1 };
    let mut idx = 0;
    for (n, (i, _)) in spec.match_indices('/').enumerate() {
        if n + 1 == segments {
            idx = i;
            break;
        }
    }
    if idx == 0 { (spec, "") } else { (&spec[..idx], &spec[idx..]) }
}

pub fn package_deps(tenant: &Tenant) -> Result<Vec<(String, String)>, String> {
    let Some(text) = read_config(tenant, "package.json")? else { return Ok(vec![]) };
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("package.json: {e}"))?;
    let mut out = Vec::new();
    for key in ["dependencies", "devDependencies"] {
        if let Some(map) = json.get(key).and_then(|d| d.as_object()) {
            for (k, v) in map {
                if let Some(v) = v.as_str() {
                    out.push((k.clone(), v.trim_start_matches('=').to_string()));
                }
            }
        }
    }
    Ok(out)
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct RendererCfg {
    pub name: String,
    pub server: String,
    pub client: Option<String>,
}

/// Framework renderers the preview should register, from package.json integrations and `sandbox-lite.json`.
pub fn renderers(tenant: &Tenant, cdn: &str) -> Result<Vec<RendererCfg>, String> {
    let deps = package_deps(tenant)?;
    let range = |name: &str| deps.iter().find(|(n, _)| n == name).map(|(_, r)| r.clone());
    let mut out = Vec::new();
    if let Some(r) = range("@astrojs/react") {
        let react = range("react").unwrap_or_else(|| "latest".into());
        let dom = range("react-dom").unwrap_or_else(|| react.clone());
        out.push(RendererCfg {
            name: "@astrojs/react".into(),
            server: shim_url("renderer-react"),
            client: Some(format!("{}/@astrojs/react@{r}/client.js?deps=react@{react},react-dom@{dom}", cdn.trim_end_matches('/'))),
        });
    }
    if let Some(r) = range("@astrojs/preact") {
        let preact = range("preact").unwrap_or_else(|| "latest".into());
        out.push(RendererCfg {
            name: "@astrojs/preact".into(),
            server: shim_url("renderer-preact"),
            client: Some(format!("{}/@astrojs/preact@{r}/client.js?deps=preact@{preact}", cdn.trim_end_matches('/'))),
        });
    }
    // Vue and Svelte SFCs are compiled in the browser, so both entrypoints are shims of ours:
    // @astrojs/vue's client.js imports a Vite virtual module and @astrojs/svelte's is uncompiled runes.
    if range("@astrojs/vue").is_some() {
        out.push(RendererCfg {
            name: "@astrojs/vue".into(),
            server: shim_url("renderer-vue"),
            client: Some(shim_url("renderer-vue-client")),
        });
    }
    if range("@astrojs/svelte").is_some() {
        out.push(RendererCfg {
            name: "@astrojs/svelte".into(),
            server: shim_url("renderer-svelte"),
            client: Some(shim_url("renderer-svelte-client")),
        });
    }
    if let Some(json) = sandbox_lite_json(tenant)?
        && let Some(list) = json.get("renderers").and_then(|r| r.as_array())
    {
        for item in list {
            let (Some(name), Some(server)) = (item.get("name").and_then(|v| v.as_str()), item.get("server").and_then(|v| v.as_str()))
            else {
                continue;
            };
            out.retain(|r| r.name != name);
            out.push(RendererCfg {
                name: name.to_string(),
                server: server.to_string(),
                client: item.get("client").and_then(|v| v.as_str()).map(|s| s.to_string()),
            });
        }
    }
    Ok(out)
}

/// `@vue/compiler-sfc` must match the `vue` it compiles for, and an Astro project never lists it.
pub fn vue_compiler_url(tenant: &Tenant, cdn: &str) -> Result<String, String> {
    let deps = package_deps(tenant)?;
    let range = deps.iter().find(|(n, _)| n == "@vue/compiler-sfc").or_else(|| deps.iter().find(|(n, _)| n == "vue"));
    let spec = match range {
        Some((_, r)) if !r.is_empty() && !r.contains(':') && !r.starts_with("file") => format!("@vue/compiler-sfc@{r}"),
        _ => "@vue/compiler-sfc".to_string(),
    };
    Ok(format!("{}/{spec}", cdn.trim_end_matches('/')))
}

fn sandbox_lite_json(tenant: &Tenant) -> Result<Option<serde_json::Value>, String> {
    let Some(text) = read_config(tenant, "sandbox-lite.json")? else { return Ok(None) };
    serde_json::from_str(&strip_jsonc(&text)).map(Some).map_err(|e| format!("sandbox-lite.json: {e}"))
}

/// `Astro.site`, which the compiler bakes into every module. A `sandbox-lite.json` that does not
/// parse leaves it undefined, and a canonical URL or an RSS feed built from it is then wrong
/// without saying so.
pub fn tenant_site(tenant: &Tenant) -> Result<Option<String>, String> {
    let Some(json) = sandbox_lite_json(tenant)? else { return Ok(None) };
    Ok(json.get("site").and_then(|s| s.as_str()).map(|s| s.to_string()))
}

fn is_external(spec: &str) -> bool {
    spec.starts_with("http://")
        || spec.starts_with("https://")
        || spec.starts_with("//")
        || spec.starts_with("data:")
        || spec.starts_with("blob:")
        || spec.starts_with("/__sl/")
}

fn astro_shim(spec: &str) -> Option<&'static str> {
    match spec {
        "astro/components" => Some("astro-components"),
        "astro/zod" => Some("zod"),
        "astro/config" => Some("astro-config"),
        "astro/loaders" => Some("astro-loaders"),
        "astro/types" => Some("astro-types"),
        "astro/jsx-runtime" => Some("astro-jsx-runtime"),
        _ => None,
    }
}

pub fn shim_url(name: &str) -> String {
    format!("{SHIM_PREFIX}{name}.js")
}

fn missing_url(spec: &str, importer: &str) -> String {
    format!("/__sl/missing.js?spec={}&from={}", urlenc(spec), urlenc(importer))
}

pub fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'/' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn split_query(spec: &str) -> (&str, &str) {
    match spec.find('?') {
        Some(i) => (&spec[..i], &spec[i + 1..]),
        None => (spec, ""),
    }
}

pub fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

pub fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() { normalize(rel) } else { normalize(&format!("{dir}/{rel}")) }
}

pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

pub fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut kept, mut in_str) = (0, 0, false);
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            match c {
                b'\\' => i += 2,
                b'"' => {
                    in_str = false;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                out.push_str(&text[kept..i]);
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                kept = i;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                out.push_str(&text[kept..i]);
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                kept = i;
            }
            b'}' | b']' => {
                out.push_str(&text[kept..i]);
                let end = out.trim_end_matches([' ', '\t', '\n', '\r']).len();
                if end > 0 && out.as_bytes()[end - 1] == b',' {
                    out.remove(end - 1);
                }
                kept = i;
                i += 1;
            }
            _ => i += 1,
        }
    }
    out.push_str(&text[kept..]);
    out
}

pub(crate) fn tsconfig_paths(text: &str) -> Result<Vec<(String, String)>, String> {
    let json: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).map_err(|e| format!("tsconfig.json: {e}"))?;
    let Some(co) = json.get("compilerOptions") else { return Ok(vec![]) };
    let base_url = co.get("baseUrl").and_then(|b| b.as_str()).unwrap_or(".");
    let Some(paths) = co.get("paths").and_then(|p| p.as_object()) else { return Ok(vec![]) };
    let mut out = Vec::new();
    for (key, targets) in paths {
        let Some(target) = targets.as_array().and_then(|a| a.first()).and_then(|t| t.as_str()) else { continue };
        let key = key.trim_end_matches('*').to_string();
        let target = normalize(&join(&normalize(base_url), target.trim_end_matches('*')));
        let target = if key.ends_with('/') && !target.is_empty() { format!("{target}/") } else { target };
        out.push((key, target));
    }
    out.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    Ok(out)
}

pub(crate) fn import_map(text: &str) -> Result<Vec<(String, String)>, String> {
    let json: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).map_err(|e| format!("sandbox-lite.json: {e}"))?;
    let Some(imports) = json.get("imports").and_then(|p| p.as_object()) else { return Ok(vec![]) };
    let mut out: Vec<(String, String)> = imports.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect();
    out.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dot_segments() {
        assert_eq!(join("src/pages", "../layouts/Layout.astro"), "src/layouts/Layout.astro");
        assert_eq!(join("", "./x.ts"), "x.ts");
        assert_eq!(normalize("/src//a/./b.ts"), "src/a/b.ts");
    }

    #[test]
    fn strips_jsonc() {
        let t = "{\n // c\n \"a\": [1,2,], /* x */ \"b\": \"//not\",\n}";
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(t)).unwrap();
        assert_eq!(v["b"], "//not");
        assert_eq!(v["a"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn strip_jsonc_keeps_multibyte_text_and_drops_commas_before_comments() {
        let t = "{\n  \"site\": \"https://exämple.com\", // ünicode\n  \"x\": [\"字\", /* last */ ],\n}";
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(t)).unwrap();
        assert_eq!(v["site"], "https://exämple.com");
        assert_eq!(v["x"][0], "字");
        assert!(strip_jsonc(t).len() <= t.len());
    }

    #[test]
    fn versioned_cdn_urls() {
        let deps = vec![("react".to_string(), "^19.2.8".to_string()), ("@astrojs/react".to_string(), "^6.0.5".to_string())];
        assert_eq!(cdn_url("https://esm.sh/", &deps, "react/jsx-runtime"), "https://esm.sh/react@^19.2.8/jsx-runtime");
        assert_eq!(cdn_url("https://esm.sh", &deps, "@astrojs/react/client.js"), "https://esm.sh/@astrojs/react@^6.0.5/client.js");
        assert_eq!(cdn_url("https://esm.sh", &deps, "dayjs"), "https://esm.sh/dayjs");
        assert_eq!(package_name("@scope/pkg/sub/path"), ("@scope/pkg", "/sub/path"));
    }

    #[test]
    fn parses_tsconfig_aliases() {
        let t = r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"],"@utils":["./src/utils/index.ts"]}}}"#;
        let a = tsconfig_paths(t).unwrap();
        assert!(a.contains(&("@/".to_string(), "src/".to_string())));
        assert!(a.contains(&("@utils".to_string(), "src/utils/index.ts".to_string())));
    }

    /// Issue #48: dropped, an alias table that does not parse sends `@/lib/x` to the CDN, where it
    /// is a package that does not exist — with nothing said about the tsconfig that caused it.
    #[test]
    fn a_config_that_does_not_parse_is_an_error() {
        assert!(tsconfig_paths(r#"{"compilerOptions":{"paths":}"#).is_err());
        assert!(import_map("{ not json").is_err());
    }
}
