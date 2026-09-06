pub mod astro;
pub mod content;
pub mod css;
pub mod glob;
pub mod js;
pub mod markdown;
pub mod mdx;
pub mod scss;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use xxhash_rust::xxh3::xxh3_128;

use crate::resolve::{Resolver, dirname};
use crate::store::Tenant;

#[derive(Serialize, Clone, Debug)]
pub struct Diag {
    pub severity: String,
    pub text: String,
    pub hint: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug)]
pub struct BuildError {
    pub status: u16,
    pub message: String,
    pub diagnostics: Vec<Diag>,
}

impl BuildError {
    pub fn not_found(path: &str) -> BuildError {
        BuildError { status: 404, message: format!("{path}: not found"), diagnostics: vec![] }
    }

    pub fn compile(message: String, diagnostics: Vec<Diag>) -> BuildError {
        BuildError { status: 500, message, diagnostics }
    }
}

pub struct Built {
    pub body: String,
    pub content_type: &'static str,
    pub specs: Vec<js::SpecRef>,
    pub component_paths: Vec<js::SpecRef>,
    pub globs: Vec<glob::GlobRef>,
    pub warnings: Vec<Diag>,
    pub env_banner: bool,
    pub css: Vec<String>,
    pub scripts: Vec<astro::Script>,
    pub islands: usize,
}

impl Built {
    fn js(body: String) -> Built {
        Built {
            body,
            content_type: JS,
            specs: vec![],
            component_paths: vec![],
            globs: vec![],
            warnings: vec![],
            env_banner: false,
            css: vec![],
            scripts: vec![],
            islands: 0,
        }
    }

    fn retained_bytes(&self) -> usize {
        let scripts: usize = self
            .scripts
            .iter()
            .map(|s| match s {
                astro::Script::Inline(code) | astro::Script::External(code) => code.len(),
            })
            .sum();
        self.body.len() + self.css.iter().map(String::len).sum::<usize>() + scripts
    }

    fn scanned(body: String) -> Built {
        let specs = js::scan(&body);
        let globs = glob::find(&body);
        Built { specs, globs, env_banner: true, ..Built::js(body) }
    }
}

pub const JS: &str = "text/javascript; charset=utf-8";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Module,
    Style(usize),
    Script(usize),
    Raw,
    Url,
}

impl Kind {
    pub fn from_query(query: &str) -> Kind {
        let mut ty = "";
        let mut index = 0usize;
        for part in query.split('&') {
            match part {
                "raw" => return Kind::Raw,
                "url" => return Kind::Url,
                _ => {}
            }
            if let Some(v) = part.strip_prefix("type=") {
                ty = v;
            } else if let Some(v) = part.strip_prefix("index=") {
                index = v.parse().unwrap_or(0);
            }
        }
        match ty {
            "style" => Kind::Style(index),
            "script" => Kind::Script(index),
            _ => Kind::Module,
        }
    }

    fn tag(self) -> String {
        match self {
            Kind::Module => "m".into(),
            Kind::Style(i) => format!("s{i}"),
            Kind::Script(i) => format!("j{i}"),
            Kind::Raw => "r".into(),
            Kind::Url => "u".into(),
        }
    }
}

pub struct Config {
    pub cdn: String,
    pub cache_bytes: usize,
    pub sass_timeout: Duration,
}

impl Default for Config {
    fn default() -> Config {
        Config { cdn: "https://esm.sh".into(), cache_bytes: 64 << 20, sass_timeout: Duration::from_millis(scss::DEFAULT_TIMEOUT_MS) }
    }
}

#[derive(Serialize, Clone, Copy)]
pub struct CacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub hits: u64,
    pub misses: u64,
}

struct Cache {
    map: HashMap<u128, Arc<Built>>,
    order: VecDeque<u128>,
    bytes: usize,
    hits: u64,
    misses: u64,
}

pub struct Engine {
    pub cfg: Config,
    cache: Mutex<Cache>,
    sass: scss::Sass,
}

impl Engine {
    pub fn new(cfg: Config) -> Engine {
        let sass = scss::Sass::new(cfg.sass_timeout);
        Engine { cfg, cache: Mutex::new(Cache { map: HashMap::new(), order: VecDeque::new(), bytes: 0, hits: 0, misses: 0 }), sass }
    }

    pub fn stats(&self) -> CacheStats {
        let c = self.cache.lock().unwrap();
        CacheStats { entries: c.map.len(), bytes: c.bytes, hits: c.hits, misses: c.misses }
    }

    pub fn sass_stats(&self) -> scss::SassStats {
        self.sass.stats()
    }

    fn cached(&self, key: u128) -> Option<Arc<Built>> {
        let mut c = self.cache.lock().unwrap();
        match c.map.get(&key).cloned() {
            Some(b) => {
                c.hits += 1;
                Some(b)
            }
            None => {
                c.misses += 1;
                None
            }
        }
    }

    fn insert(&self, key: u128, built: Arc<Built>) {
        let mut c = self.cache.lock().unwrap();
        if c.map.contains_key(&key) {
            return;
        }
        c.bytes += built.retained_bytes();
        c.map.insert(key, built);
        c.order.push_back(key);
        while c.bytes > self.cfg.cache_bytes {
            let Some(old) = c.order.pop_front() else { break };
            if let Some(b) = c.map.remove(&old) {
                c.bytes -= b.retained_bytes();
            }
        }
    }

    pub fn build(&self, tenant: &Tenant, path: &str, kind: Kind) -> Result<Arc<Built>, BuildError> {
        let data = tenant.read(path).ok_or_else(|| BuildError::not_found(path))?;
        let site = crate::resolve::tenant_site(tenant);
        let mut h = Vec::with_capacity(data.len() + path.len() + 32);
        h.extend_from_slice(kind.tag().as_bytes());
        h.push(0);
        h.extend_from_slice(path.as_bytes());
        h.push(0);
        h.extend_from_slice(site.as_deref().unwrap_or("").as_bytes());
        h.push(0);
        if scss::is_sass_path(path) || (path.ends_with(".astro") && scss::uses_sass(&String::from_utf8_lossy(&data))) {
            h.extend_from_slice(&scss::fingerprint(tenant).to_le_bytes());
        }
        h.extend_from_slice(&data);
        let key = xxh3_128(&h);
        if let Some(b) = self.cached(key) {
            return Ok(b);
        }
        let built = Arc::new(self.compile(tenant, path, kind, &data, site.as_deref())?);
        self.insert(key, built.clone());
        Ok(built)
    }

    fn compile(&self, tenant: &Tenant, path: &str, kind: Kind, data: &[u8], site: Option<&str>) -> Result<Built, BuildError> {
        let text = || String::from_utf8_lossy(data).into_owned();
        match kind {
            Kind::Url => Ok(Built::js(format!("export default {};\n", json_str(&format!("/__sl/raw/{path}"))))),
            Kind::Raw => Ok(Built::js(format!("export default {};\n", json_str(&text())))),
            Kind::Style(i) => {
                let m = self.build(tenant, path, Kind::Module)?;
                let css = m.css.get(i).ok_or_else(|| BuildError::not_found(&format!("{path}?style={i}")))?;
                Ok(Built::js(css::to_module(&format!("{path}?{i}"), css, dirname(path))))
            }
            Kind::Script(i) => {
                let m = self.build(tenant, path, Kind::Module)?;
                let script = m.scripts.get(i).ok_or_else(|| BuildError::not_found(&format!("{path}?script={i}")))?;
                match script {
                    astro::Script::Inline(code) => {
                        let out = js::transform(&format!("{path}.{i}.ts"), code)?;
                        Ok(Built::scanned(out))
                    }
                    astro::Script::External(src) => Ok(Built::scanned(format!("import {};\n", json_str(src)))),
                }
            }
            Kind::Module => {
                let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("").to_ascii_lowercase();
                match ext.as_str() {
                    "astro" => {
                        let dir = dirname(path);
                        let preprocess = |lang: &str, src: &str| self.sass.compile(tenant, dir, src, lang == "sass");
                        let out = astro::compile(path, &text(), site, &preprocess)?;
                        let specs = js::scan(&out.code);
                        let globs = glob::find(&out.code);
                        Ok(Built {
                            body: out.code,
                            content_type: JS,
                            specs,
                            component_paths: out.component_paths,
                            globs,
                            warnings: out.warnings,
                            env_banner: true,
                            css: out.css,
                            scripts: out.scripts,
                            islands: out.islands,
                        })
                    }
                    "ts" | "tsx" | "jsx" | "mts" => Ok(Built::scanned(js::transform(path, &text())?)),
                    "js" | "mjs" => {
                        let src = text();
                        match js::scan_checked(&src) {
                            Some(specs) => Ok(Built { specs, env_banner: true, ..Built::js(src) }),
                            None => Ok(Built::scanned(js::transform(path, &src)?)),
                        }
                    }
                    "css" => Ok(Built::js(css::to_module(path, &text(), dirname(path)))),
                    "scss" | "sass" => {
                        let compiled = self.sass.compile(tenant, dirname(path), &text(), ext == "sass").map_err(|e| {
                            BuildError::compile(
                                format!("{path}: {e}"),
                                vec![Diag {
                                    severity: "error".into(),
                                    text: e,
                                    hint: String::new(),
                                    file: path.to_string(),
                                    line: 0,
                                    column: 0,
                                }],
                            )
                        })?;
                        Ok(Built::js(css::to_module(path, &compiled, dirname(path))))
                    }
                    "vue" | "svelte" => Ok(Built::js(sfc_loader(&ext, path, &text()))),
                    "json" => Ok(Built::js(format!("export default JSON.parse({});\n", json_str(&text())))),
                    "md" => Ok(Built::scanned(markdown::page_module(path, &text()))),
                    "mdx" => Ok(Built::scanned(mdx::page_module(path, &text())?)),
                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" | "ico" | "bmp" | "tiff" => {
                        let (w, h) = if ext == "svg" {
                            svg_size(&text())
                        } else {
                            imagesize::blob_size(data).map(|s| (s.width, s.height)).unwrap_or((0, 0))
                        };
                        let format = if ext == "jpeg" { "jpg".to_string() } else { ext.clone() };
                        Ok(Built::js(format!(
                            "export default {{ src: {}, width: {w}, height: {h}, format: {}, fsPath: {} }};\n",
                            json_str(&format!("/__sl/raw/{path}")),
                            json_str(&format),
                            json_str(path)
                        )))
                    }
                    _ => Ok(Built::js(format!("export default {};\n", json_str(&format!("/__sl/raw/{path}"))))),
                }
            }
        }
    }

    pub fn serve(&self, tenant: &Tenant, path: &str, kind: Kind, resolver: &Resolver) -> Result<(String, &'static str), BuildError> {
        let built = self.build(tenant, path, kind)?;
        let mut edits: Vec<(usize, usize, String)> =
            built.specs.iter().chain(&built.component_paths).map(|s| (s.start, s.end, resolver.resolve(path, &s.spec))).collect();
        let mut hoisted = String::new();
        for (i, g) in built.globs.iter().enumerate() {
            let expansion = glob::expand(tenant, path, g, i, resolver.version())
                .map_err(|e| BuildError::compile(format!("{path}: import.meta.glob: {e}"), vec![]))?;
            hoisted.push_str(&expansion.imports);
            edits.push((g.start, g.end, expansion.literal));
        }
        edits.sort_by_key(|e| e.0);
        let mut out = String::with_capacity(built.body.len() + hoisted.len() + 512);
        if built.env_banner {
            out.push_str("import.meta.env = globalThis.__sl_env || {};\n");
        }
        out.push_str(&hoisted);
        let mut last = 0usize;
        for (start, end, replacement) in edits {
            if start < last {
                continue;
            }
            out.push_str(&built.body[last..start]);
            out.push_str(&replacement);
            last = end;
        }
        out.push_str(&built.body[last..]);
        Ok((out, built.content_type))
    }
}

/// No Rust compiler exists for `.vue` or `.svelte`, so the file is served as a module that
/// compiles its source in the browser and re-exports the component.
fn sfc_loader(ext: &str, path: &str, source: &str) -> String {
    format!(
        "import {{ compileComponent }} from \"/__sl/shim/{ext}-loader.js\";\nexport default await compileComponent({}, {}, import.meta.url);\n",
        json_str(source),
        json_str(path)
    )
}

pub(crate) fn svg_size(svg: &str) -> (usize, usize) {
    let Some(open) = svg.find("<svg") else { return (0, 0) };
    let tag = &svg[open..svg[open..].find('>').map(|i| open + i).unwrap_or(svg.len())];
    let length = |v: &str| v.trim().trim_end_matches("px").parse().ok().and_then(pixels);
    match (svg_attr(tag, "width").and_then(length), svg_attr(tag, "height").and_then(length)) {
        (Some(w), Some(h)) => (w, h),
        _ => {
            let vb = svg_attr(tag, "viewBox").unwrap_or_default();
            let parts: Vec<f64> = vb.split([' ', ',']).filter_map(|p| p.trim().parse().ok()).collect();
            match parts[..] {
                [_, _, w, h] => (pixels(w).unwrap_or(0), pixels(h).unwrap_or(0)),
                _ => (0, 0),
            }
        }
    }
}

fn svg_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let i = tag.find(&format!(" {name}="))?;
    let rest = &tag[i + name.len() + 2..];
    let quote = rest.chars().next().filter(|q| *q == '"' || *q == '\'')?;
    let end = rest[1..].find(quote)?;
    Some(&rest[1..1 + end])
}

/// `inf`, `NaN`, `1e999` and negatives parse as f64 but are not sizes an image can have.
fn pixels(n: f64) -> Option<usize> {
    (0.0..=u32::MAX as f64).contains(&n).then(|| n.round() as usize)
}

pub fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

pub fn is_source(path: &str) -> bool {
    matches!(
        path.rsplit_once('.').map(|(_, e)| e).unwrap_or(""),
        "astro" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "mts" | "md" | "mdx" | "vue" | "svelte"
    ) && path.starts_with("src/")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{Config, Engine, Kind, svg_size};
    use crate::resolve::Resolver;
    use crate::store::{Base, Store, Tenant};

    const PAGE: &str = "---\nimport { Chart } from \"some-widgets\";\n---\n<Chart client:load />\n";
    const PAGE_PATH: &str = "src/pages/index.astro";

    fn tenant(files: &[(&str, &str)]) -> Arc<Tenant> {
        static N: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!("sandbox-lite-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::new(None, u64::MAX);
        store.add_base(Base::load("b", &root).unwrap());
        std::fs::remove_dir(&root).unwrap();
        let tenant = store.create_tenant("t", "b").unwrap();
        for (path, body) in files {
            tenant.write(path, body.as_bytes().to_vec()).unwrap();
        }
        tenant
    }

    fn engine() -> Engine {
        Engine::new(Config { cache_bytes: 1 << 20, ..Config::default() })
    }

    fn served(engine: &Engine, tenant: &Tenant) -> String {
        engine.serve(tenant, PAGE_PATH, Kind::Module, &Resolver::new(tenant, "https://esm.sh", 7)).unwrap().0
    }

    #[test]
    fn a_package_island_path_becomes_the_pinned_cdn_url() {
        let t = tenant(&[("package.json", r#"{"dependencies":{"some-widgets":"^1.2.3"}}"#), (PAGE_PATH, PAGE)]);
        let out = served(&engine(), &t);
        assert!(out.contains(r#""client:component-path": "https://esm.sh/some-widgets@^1.2.3""#), "{out}");
        assert!(out.contains(r#""client:component-export": "Chart""#), "{out}");
        assert!(out.contains(r#"import { Chart } from "https://esm.sh/some-widgets@^1.2.3""#), "{out}");
    }

    #[test]
    fn a_client_only_package_island_resolves_at_serve_time() {
        let t = tenant(&[
            ("package.json", r#"{"dependencies":{"some-widgets":"1.0.0"}}"#),
            (PAGE_PATH, "---\nimport Widget from \"some-widgets\";\n---\n<Widget client:only=\"react\" />\n"),
        ]);
        let out = served(&engine(), &t);
        assert!(out.contains(r#""client:component-path": "https://esm.sh/some-widgets@1.0.0""#), "{out}");
        assert!(out.contains(r#""client:component-export": "default""#), "{out}");
    }

    #[test]
    fn an_aliased_island_path_becomes_the_tenants_own_module() {
        let t = tenant(&[
            ("tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@c/*":["src/components/*"]}}}"#),
            ("src/components/Card.tsx", "export default () => null;\n"),
            (PAGE_PATH, "---\nimport Card from \"@c/Card.tsx\";\n---\n<Card client:load />\n"),
        ]);
        let out = served(&engine(), &t);
        assert!(out.contains(r#""client:component-path": "/__sl/m/src/components/Card.tsx?v=7""#), "{out}");
    }

    #[test]
    fn one_cached_compile_serves_each_tenant_its_own_pin() {
        let a = tenant(&[("package.json", r#"{"dependencies":{"some-widgets":"1.0.0"}}"#), (PAGE_PATH, PAGE)]);
        let b = tenant(&[("package.json", r#"{"dependencies":{"some-widgets":"2.0.0"}}"#), (PAGE_PATH, PAGE)]);
        let engine = engine();
        assert!(served(&engine, &a).contains("https://esm.sh/some-widgets@1.0.0"));
        assert!(served(&engine, &b).contains("https://esm.sh/some-widgets@2.0.0"));
        assert_eq!(engine.stats().misses, 1, "package.json is not in the cache key, so both tenants share one compile");
    }

    #[test]
    fn svg_size_survives_junk_attributes() {
        assert_eq!(svg_size("<svg width=€ height=\"1\">"), (0, 0));
        assert_eq!(svg_size("<svg width=\"inf\" height=\"1e999\">"), (0, 0));
        assert_eq!(svg_size("<svg viewBox=\"0 0 NaN -1\">"), (0, 0));
        assert_eq!(svg_size("<svg width=\"24px\" height='16'>"), (24, 16));
        assert_eq!(svg_size("<svg width=\"x\" viewBox=\"0,0,100.4,50.5\">"), (100, 51));
    }
}
