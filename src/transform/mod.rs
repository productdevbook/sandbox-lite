pub mod astro;
pub mod content;
pub mod css;
pub mod glob;
pub mod js;
pub mod markdown;
pub mod mdx;
pub mod scss;
pub mod sfc;

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use xxhash_rust::xxh3::xxh3_128;

use crate::metrics::Metrics;
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

    pub fn busy(message: String) -> BuildError {
        BuildError { status: 503, message, diagnostics: vec![] }
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

/// oxc, astro_codegen, satteri-mdxjs and grass are recursive-descent, so a source byte can cost a
/// stack frame and a stack overflow aborts the process (`panic = "abort"`). The two shapes measured
/// past 1 KiB of stack per source byte both nest on characters `nesting_depth` counts, so what is
/// left for the stack to absorb is the recursion that scan cannot see — chained unary `-` through
/// oxc and `<<` through astro_codegen, both around 300 bytes per source byte in a debug build. The
/// stack is sized from the source cap rather than fixed, so raising the cap raises it too.
const STACK_PER_SOURCE_BYTE: usize = 4000;
const MIN_PARSER_STACK: usize = 16 << 20;

/// Three orders of magnitude past what hand-written source nests to. Two parsers cost far more than
/// the rest per level — oxc about 2 KiB and the MDX blockquote reader about 3.7 KiB, and the latter
/// is also quadratic in time, so 64 KiB of `>` takes a quarter of an hour whether or not the stack
/// holds. Both recurse on characters a scan can count, so they are bounded before they are called.
const MAX_NESTING_DEPTH: usize = 2000;

/// How deep the parsers will recurse, counted on the bytes themselves: unclosed `(`, `[` and `{`,
/// and runs of markdown blockquote markers. It does not skip strings or comments, so a literal full
/// of brackets reads as nesting — nothing written by hand comes near the limit either way.
fn nesting_depth(source: &[u8]) -> usize {
    let (mut open, mut quote, mut max) = (0usize, 0usize, 0usize);
    for &b in source {
        match b {
            b'(' | b'[' | b'{' => open += 1,
            b')' | b']' | b'}' => open = open.saturating_sub(1),
            b'>' => quote += 1,
            b' ' | b'\t' => continue,
            _ => quote = 0,
        }
        max = max.max(open).max(quote);
    }
    max
}

/// Extensions whose module build hands the source to one of those parsers — `.vue` and `.svelte`
/// among them, since `sfc::strip_types` runs oxc over their `<script lang="ts">` blocks. `md` is
/// out: markdown never overflowed at any size the cap allows, and long articles are legitimate.
/// `css` and `json` are out too — they are embedded in a JS module as a string, never parsed.
fn parses_source(ext: &str) -> bool {
    matches!(ext, "astro" | "ts" | "tsx" | "jsx" | "mts" | "js" | "mjs" | "mdx" | "scss" | "sass" | "vue" | "svelte")
}

thread_local! {
    static ON_PARSER_STACK: Cell<bool> = const { Cell::new(false) };
    /// Set while a compile that holds a permit runs, so the module build a `Style`/`Script` compile
    /// makes from inside it does not queue for a second one.
    static COMPILING: Cell<bool> = const { Cell::new(false) };
    /// Counted per calling thread so parallel tests cannot see each other's spawns.
    #[cfg(test)]
    static SPAWNED: Cell<usize> = const { Cell::new(0) };
}

/// Runs `f` on a thread with a stack the parsers cannot walk off. The reservation is virtual
/// address space; only the pages a compile actually touches become resident. A `?type=style` or
/// `?type=script` build asks for the module first, so the flag keeps that inner build on the stack
/// this one already reserved instead of reserving a second one.
fn with_big_stack<T: Send>(stack_bytes: usize, f: impl FnOnce() -> T + Send) -> std::io::Result<T> {
    if ON_PARSER_STACK.get() {
        return Ok(f());
    }
    std::thread::scope(|scope| {
        let body = || {
            ON_PARSER_STACK.set(true);
            f()
        };
        let handle = std::thread::Builder::new().stack_size(stack_bytes).spawn_scoped(scope, body)?;
        #[cfg(test)]
        SPAWNED.set(SPAWNED.get() + 1);
        Ok(handle.join().unwrap_or_else(|payload| std::panic::resume_unwind(payload)))
    })
}

fn refused(path: &str, text: String, hint: &str) -> BuildError {
    BuildError::compile(
        format!("{path}: {text}"),
        vec![Diag { severity: "error".into(), text, hint: hint.to_string(), file: path.to_string(), line: 0, column: 0 }],
    )
}

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
    pub max_source_bytes: usize,
    pub sass_timeout: Duration,
    pub max_compiles: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            cdn: "https://esm.sh".into(),
            cache_bytes: 64 << 20,
            max_source_bytes: 64 << 10,
            sass_timeout: Duration::from_millis(scss::DEFAULT_TIMEOUT_MS),
            max_compiles: default_max_compiles(),
        }
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

/// How long a compile waits for a permit before its request is refused. Long enough to absorb the
/// burst of module requests one page load makes, short enough that a saturated daemon says so
/// instead of holding the connection open.
const COMPILE_QUEUE_WAIT: Duration = Duration::from_secs(5);

/// Compiles that may run at once when nothing says otherwise. A compile is CPU-bound and holds a
/// `parser_stack_bytes()` reservation while it runs, so more of them than cores buys queueing.
pub fn default_max_compiles() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

#[derive(Serialize, Clone, Copy)]
pub struct CompileStats {
    pub running: usize,
    pub queued: usize,
    pub limit: usize,
    pub refused: u64,
}

#[derive(Default)]
struct GateState {
    running: usize,
    queued: usize,
}

/// One permit per compile that may run at once. Every cache miss reserves `parser_stack_bytes()` of
/// address space on a thread of its own and tokio's blocking pool would let 512 of those exist
/// together, so without this the ceiling is the OS rather than a decision.
struct Gate {
    limit: usize,
    wait: Duration,
    state: Mutex<GateState>,
    free: Condvar,
    refused: AtomicU64,
}

struct Permit<'a>(&'a Gate);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.running -= 1;
        drop(state);
        self.0.free.notify_one();
    }
}

impl Gate {
    fn new(limit: usize, wait: Duration) -> Gate {
        Gate { limit, wait, state: Mutex::new(GateState::default()), free: Condvar::new(), refused: AtomicU64::new(0) }
    }

    fn stats(&self) -> CompileStats {
        let state = self.state.lock().unwrap();
        CompileStats { running: state.running, queued: state.queued, limit: self.limit, refused: self.refused.load(Ordering::Relaxed) }
    }

    /// `None` when this thread is already compiling under a permit: a `?type=style` build asks for
    /// the module from inside `compile`, and a second permit would deadlock once the gate is full.
    fn enter(&self) -> Result<Option<Permit<'_>>, String> {
        if COMPILING.get() {
            return Ok(None);
        }
        let mut state = self.state.lock().unwrap();
        if state.running >= self.limit {
            let deadline = Instant::now() + self.wait;
            state.queued += 1;
            loop {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    state.queued -= 1;
                    drop(state);
                    self.refused.fetch_add(1, Ordering::Relaxed);
                    let (n, ms) = (self.limit, self.wait.as_millis());
                    return Err(format!("{n} compiles are already running and this one waited {ms} ms for a slot"));
                };
                state = self.free.wait_timeout(state, left).unwrap().0;
                if state.running < self.limit {
                    state.queued -= 1;
                    break;
                }
            }
        }
        state.running += 1;
        Ok(Some(Permit(self)))
    }
}

/// Marks the thread running a permitted compile, and restores the flag on the way out so a thread
/// that builds again afterwards queues for a permit of its own.
struct Compiling(bool);

impl Compiling {
    fn enter() -> Compiling {
        Compiling(COMPILING.replace(true))
    }
}

impl Drop for Compiling {
    fn drop(&mut self) {
        COMPILING.set(self.0);
    }
}

pub struct Engine {
    pub cfg: Config,
    cache: Mutex<Cache>,
    sass: scss::Sass,
    gate: Gate,
    metrics: Arc<Metrics>,
}

impl Engine {
    pub fn new(cfg: Config, metrics: Arc<Metrics>) -> Engine {
        let sass = scss::Sass::new(cfg.sass_timeout);
        let gate = Gate::new(cfg.max_compiles.max(1), COMPILE_QUEUE_WAIT);
        Engine {
            cfg,
            cache: Mutex::new(Cache { map: HashMap::new(), order: VecDeque::new(), bytes: 0, hits: 0, misses: 0 }),
            sass,
            gate,
            metrics,
        }
    }

    pub fn parser_stack_bytes(&self) -> usize {
        self.cfg.max_source_bytes.saturating_mul(STACK_PER_SOURCE_BYTE).max(MIN_PARSER_STACK)
    }

    /// For the parsers reached outside `build` — the content config, which oxc walks.
    pub fn on_parser_stack<T: Send>(&self, f: impl FnOnce() -> T + Send) -> std::io::Result<T> {
        with_big_stack(self.parser_stack_bytes(), f)
    }

    pub fn stats(&self) -> CacheStats {
        let c = self.cache.lock().unwrap();
        CacheStats { entries: c.map.len(), bytes: c.bytes, hits: c.hits, misses: c.misses }
    }

    pub fn sass_stats(&self) -> scss::SassStats {
        self.sass.stats()
    }

    pub fn compile_stats(&self) -> CompileStats {
        self.gate.stats()
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
        let _permit = self.gate.enter().map_err(|e| BuildError::busy(format!("{path}: {e}")))?;
        let started = Instant::now();
        let spawned = self.on_parser_stack(|| {
            let _compiling = Compiling::enter();
            self.compile(tenant, path, kind, &data, site.as_deref())
        });
        self.metrics.compiled(kind, started.elapsed());
        let compiled = spawned.map_err(|e| BuildError::compile(format!("{path}: cannot start a compiler thread: {e}"), vec![]))?;
        let built = Arc::new(compiled?);
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
                if parses_source(&ext) {
                    let max = self.cfg.max_source_bytes;
                    if data.len() > max {
                        let text = format!("file too large to compile: {} KiB, limit {} KiB", data.len() / 1024, max / 1024);
                        return Err(refused(path, text, "raise --max-source-kb, or split the file"));
                    }
                    let depth = nesting_depth(data);
                    if depth > MAX_NESTING_DEPTH {
                        let text = format!("source nests {depth} deep, limit {MAX_NESTING_DEPTH}");
                        return Err(refused(path, text, "unbalanced brackets are the usual cause"));
                    }
                }
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
                    "vue" | "svelte" => Ok(Built::js(sfc_loader(&ext, path, &sfc::strip_types(&ext, path, &text())?))),
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
/// compiles its source in the browser and re-exports the component. The source it carries has
/// been through `sfc::strip_types`, so its `<script>` blocks are JavaScript.
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

    use super::*;
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
        Engine::new(Config { cache_bytes: 1 << 20, ..Config::default() }, Arc::new(crate::metrics::Metrics::default()))
    }

    fn served(engine: &Engine, tenant: &Tenant) -> String {
        engine.serve(tenant, PAGE_PATH, Kind::Module, &Resolver::new(tenant, "https://esm.sh", 7)).unwrap().0
    }

    const CAP: usize = 64 << 10;

    fn engine_capped(max_source_bytes: usize) -> Engine {
        Engine::new(capped(max_source_bytes), Arc::new(crate::metrics::Metrics::default()))
    }

    fn tenant_with(path: &str, source: &str) -> Arc<Tenant> {
        tenant(&[(path, source)])
    }

    /// The queue deadline is not a flag, so tests reach past `Engine::new` to shorten it.
    fn engine_gated(cfg: Config, limit: usize, wait: Duration) -> Engine {
        Engine { gate: Gate::new(limit, wait), ..Engine::new(cfg, Arc::new(crate::metrics::Metrics::default())) }
    }

    fn capped(max_source_bytes: usize) -> Config {
        Config { cache_bytes: 8 << 20, max_source_bytes, ..Config::default() }
    }

    fn build_err(r: Result<Arc<Built>, BuildError>) -> BuildError {
        match r {
            Ok(_) => panic!("expected a compile error"),
            Err(e) => e,
        }
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
    fn cap_rejects_oversized_parsed_sources() {
        let big = format!("export const x = [{}0];\n", "\"x\",".repeat(CAP / 4));
        let t = tenant_with("src/big.ts", &big);
        let e = build_err(engine_capped(CAP).build(&t, "src/big.ts", Kind::Module));
        assert_eq!(e.status, 500);
        assert!(e.message.contains("file too large to compile"), "{}", e.message);
        assert_eq!(e.diagnostics.len(), 1);
    }

    #[test]
    fn cap_leaves_unparsed_kinds_alone() {
        let t = tenant_with("src/big.json", &format!("[{}0]", "\"x\",".repeat(CAP / 4)));
        let e = engine_capped(CAP);
        assert!(e.build(&t, "src/big.json", Kind::Module).is_ok());
        assert!(e.build(&t, "src/big.json", Kind::Raw).is_ok());
        assert!(e.build(&t, "src/big.json", Kind::Url).is_ok());
    }

    #[test]
    fn cap_counts_bytes_not_characters() {
        let source = format!("export const x = \"{}\";\n", "é".repeat(CAP / 2));
        assert!(source.chars().count() < CAP && source.len() > CAP);
        let t = tenant_with("src/wide.ts", &source);
        assert!(engine_capped(CAP).build(&t, "src/wide.ts", Kind::Module).is_err());
    }

    /// Issue #25: on a default 2 MiB thread every one of these aborts the process. Bracket and
    /// blockquote nesting is refused by the pre-scan; the shapes it does not model reach the parsers
    /// on the big stack. Either way the build answers, and the process is alive for the next case.
    #[test]
    fn deep_nesting_answers_instead_of_aborting() {
        for (path, source) in [
            ("src/brackets.ts", format!("const x = {}1{}", "[".repeat(20_000), "]".repeat(20_000))),
            ("src/unclosed.ts", format!("const x = {}", "[".repeat(20_000))),
            ("src/quotes.mdx", ">".repeat(20_000)),
            ("src/braces.scss", "a{".repeat(20_000)),
            ("src/deep.vue", format!("<script lang=\"ts\">const x = {}</script>\n", "[".repeat(20_000))),
        ] {
            let t = tenant_with(path, &source);
            let e = build_err(engine_capped(CAP).build(&t, path, Kind::Module));
            assert!(e.message.contains("nests"), "{path}: {}", e.message);
        }
        for (path, source) in [
            ("src/unary.ts", "- ".repeat(20_000)),
            ("src/angles.astro", "<<".repeat(20_000)),
            ("src/divs.astro", format!("{}x{}", "<div>".repeat(2_000), "</div>".repeat(2_000))),
            ("src/jsx.mdx", format!("{}x{}", "<a>".repeat(2_000), "</a>".repeat(2_000))),
        ] {
            let t = tenant_with(path, &source);
            if let Err(e) = engine_capped(CAP).build(&t, path, Kind::Module) {
                assert!(!e.message.contains("too large") && !e.message.contains("nests"), "{path}: {}", e.message);
            }
        }
    }

    #[test]
    fn nesting_depth_counts_brackets_and_blockquotes() {
        assert_eq!(nesting_depth(b"const x = [[1], [2]];"), 2);
        assert_eq!(nesting_depth(b"f(a) f(b) f(c)"), 1);
        assert_eq!(nesting_depth(b"> > > quoted"), 3);
        assert_eq!(nesting_depth(b"a >> b >>> c"), 3);
        assert_eq!(nesting_depth(b"<div><span></span></div>"), 1);
        assert_eq!(nesting_depth(b"))))"), 0);
    }

    #[test]
    fn deeply_nested_astro_compiles() {
        let source = format!("{}hi{}", "<div>".repeat(1000), "</div>".repeat(1000));
        let t = tenant_with("src/nested.astro", &source);
        let built = engine_capped(CAP).build(&t, "src/nested.astro", Kind::Module).unwrap();
        assert!(built.body.contains("createComponent"));
    }

    /// `src/content.config.ts` goes to oxc from the content route, which does not go through `build`.
    #[test]
    fn a_content_config_past_either_limit_is_not_parsed() {
        let good = "export const collections = { posts: defineCollection({}) };";
        let t = tenant_with("src/content.config.ts", good);
        assert!(!crate::transform::content::config(&t, CAP).is_empty());
        for source in [&"a".repeat(CAP + 1), &"[".repeat(MAX_NESTING_DEPTH + 1)] {
            let t = tenant_with("src/content.config.ts", source);
            assert!(crate::transform::content::config(&t, CAP).is_empty());
        }
    }

    #[test]
    fn parser_stack_grows_with_the_cap() {
        assert_eq!(engine_capped(CAP).parser_stack_bytes(), CAP * STACK_PER_SOURCE_BYTE);
        assert_eq!(engine_capped(1).parser_stack_bytes(), MIN_PARSER_STACK);
        assert_eq!(engine_capped(usize::MAX).parser_stack_bytes(), usize::MAX);
    }

    /// A `?type=style` build asks for the module first. That inner build must not reserve a second
    /// big stack on top of the one the outer build is already running on.
    #[test]
    fn a_style_build_reserves_one_stack_not_two() {
        let t = tenant_with("src/styled.astro", "<style>p{color:red}</style><p>hi</p>");
        let e = engine_capped(CAP);
        SPAWNED.set(0);
        e.build(&t, "src/styled.astro", Kind::Style(0)).unwrap();
        assert_eq!(SPAWNED.get(), 1);
    }

    #[test]
    fn a_script_build_reserves_one_stack_not_two() {
        let t = tenant_with("src/scripted.astro", "<script>console.log(1)</script><p>hi</p>");
        let e = engine_capped(CAP);
        SPAWNED.set(0);
        e.build(&t, "src/scripted.astro", Kind::Script(0)).unwrap();
        assert_eq!(SPAWNED.get(), 1);
    }

    /// Issue #47: every compile reserves a stack of its own on a thread of its own, and tokio's
    /// blocking pool would let 512 of those exist together. Three concurrent compiles, two permits:
    /// the third waits for a slot and is refused at the deadline instead of reserving a third stack.
    #[test]
    fn concurrent_compiles_reserve_no_more_stacks_than_permits() {
        let bomb = |n: u32| format!("@for $i from 1 through {n} {{ .a-#{{$i}} {{ color: red }} }}");
        let (a, b, c) = (bomb(200_000), bomb(200_001), bomb(200_002));
        let t = tenant(&[("src/a.scss", a.as_str()), ("src/b.scss", b.as_str()), ("src/c.scss", c.as_str())]);
        let cfg = Config { sass_timeout: Duration::from_secs(2), ..capped(CAP) };
        let engine = engine_gated(cfg, 2, Duration::from_millis(50));
        std::thread::scope(|scope| {
            let busy = ["src/a.scss", "src/b.scss"].map(|path| {
                let (engine, t) = (&engine, &t);
                scope.spawn(move || {
                    SPAWNED.set(0);
                    let _ = engine.build(t, path, Kind::Module);
                    SPAWNED.get()
                })
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while engine.compile_stats().running < 2 && Instant::now() < deadline {
                std::thread::yield_now();
            }
            SPAWNED.set(0);
            let e = build_err(engine.build(&t, "src/c.scss", Kind::Module));
            assert_eq!(e.status, 503, "{}", e.message);
            assert_eq!(SPAWNED.get(), 0, "a refused compile reserves no stack");
            let stats = engine.compile_stats();
            assert_eq!((stats.limit, stats.queued, stats.refused), (2, 0, 1));
            let stacks: usize = busy.into_iter().map(|h| h.join().unwrap()).sum();
            assert_eq!(stacks, 2, "three concurrent compiles, two stacks");
        });
    }

    /// A `?type=style` build compiles the module from inside its own compile. That inner build runs
    /// under the permit its parent holds; asking for a second would never be answered.
    #[test]
    fn a_nested_build_takes_no_second_permit() {
        let t = tenant_with("src/styled.astro", "<style>p{color:red}</style><script>console.log(1)</script><p>hi</p>");
        for kind in [Kind::Style(0), Kind::Script(0)] {
            let engine = engine_gated(capped(CAP), 1, Duration::from_millis(50));
            engine.build(&t, "src/styled.astro", kind).unwrap();
            let stats = engine.compile_stats();
            assert_eq!((stats.running, stats.refused), (0, 0));
        }
    }

    /// A warm daemon serving cached modules must not queue: only a miss is a compile.
    #[test]
    fn a_cache_hit_takes_no_permit() {
        let t = tenant(&[("src/hit.ts", "export const x = 1;\n"), ("src/miss.ts", "export const y = 2;\n")]);
        let engine = engine_gated(capped(CAP), 1, Duration::from_millis(50));
        engine.build(&t, "src/hit.ts", Kind::Module).unwrap();
        let closed = Engine { gate: Gate::new(0, Duration::from_millis(50)), ..engine };
        assert!(closed.build(&t, "src/hit.ts", Kind::Module).is_ok());
        assert_eq!(build_err(closed.build(&t, "src/miss.ts", Kind::Module)).status, 503);
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
