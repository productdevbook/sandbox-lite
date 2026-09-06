pub mod astro;
pub mod content;
pub mod css;
pub mod glob;
pub mod js;
pub mod markdown;
pub mod scss;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

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
            globs: vec![],
            warnings: vec![],
            env_banner: false,
            css: vec![],
            scripts: vec![],
            islands: 0,
        }
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
}

impl Engine {
    pub fn new(cfg: Config) -> Engine {
        Engine { cfg, cache: Mutex::new(Cache { map: HashMap::new(), order: VecDeque::new(), bytes: 0, hits: 0, misses: 0 }) }
    }

    pub fn stats(&self) -> CacheStats {
        let c = self.cache.lock().unwrap();
        CacheStats { entries: c.map.len(), bytes: c.bytes, hits: c.hits, misses: c.misses }
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
        c.bytes += built.body.len();
        c.map.insert(key, built);
        c.order.push_back(key);
        while c.bytes > self.cfg.cache_bytes {
            let Some(old) = c.order.pop_front() else { break };
            if let Some(b) = c.map.remove(&old) {
                c.bytes -= b.body.len();
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
                        let preprocess = |lang: &str, src: &str| scss::compile(tenant, dir, src, lang == "sass");
                        let out = astro::compile(path, &text(), site, &preprocess)?;
                        let specs = js::scan(&out.code);
                        let globs = glob::find(&out.code);
                        Ok(Built {
                            body: out.code,
                            content_type: JS,
                            specs,
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
                        let compiled = scss::compile(tenant, dirname(path), &text(), ext == "sass").map_err(|e| {
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
                    "json" => Ok(Built::js(format!("export default JSON.parse({});\n", json_str(&text())))),
                    "md" => Ok(Built::scanned(markdown::page_module(path, &text()))),
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
            built.specs.iter().map(|s| (s.start, s.end, resolver.resolve(path, &s.spec))).collect();
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

fn svg_size(svg: &str) -> (usize, usize) {
    let Some(open) = svg.find("<svg") else { return (0, 0) };
    let tag = &svg[open..svg[open..].find('>').map(|i| open + i).unwrap_or(svg.len())];
    let attr = |name: &str| -> Option<String> {
        let i = tag.find(&format!(" {name}="))?;
        let rest = &tag[i + name.len() + 2..];
        let q = rest.chars().next()?;
        let end = rest[1..].find(q)?;
        Some(rest[1..1 + end].to_string())
    };
    let num = |v: String| v.trim().trim_end_matches("px").parse::<f64>().ok().map(|n| n.round() as usize);
    match (attr("width").and_then(num), attr("height").and_then(num)) {
        (Some(w), Some(h)) => (w, h),
        _ => {
            let vb = attr("viewBox").unwrap_or_default();
            let parts: Vec<f64> = vb.split([' ', ',']).filter_map(|p| p.trim().parse().ok()).collect();
            if parts.len() == 4 { (parts[2].round() as usize, parts[3].round() as usize) } else { (0, 0) }
        }
    }
}

pub fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

pub fn is_source(path: &str) -> bool {
    matches!(path.rsplit_once('.').map(|(_, e)| e).unwrap_or(""), "astro" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "mts" | "md")
        && path.starts_with("src/")
}
