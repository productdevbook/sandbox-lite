pub mod astro;
pub mod content;
pub mod css;
pub mod glob;
pub mod js;
pub mod markdown;
pub mod mdx;
pub mod scss;
pub mod sfc;

use std::any::Any;
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use xxhash_rust::xxh3::{Xxh3Default, xxh3_128};

use crate::metrics::Metrics;
use crate::resolve::{Resolver, dirname};
use crate::store::{Tenant, UpdateKind};

#[derive(Serialize, Clone, Debug)]
pub struct Diag {
    pub severity: String,
    pub text: String,
    pub hint: String,
    pub file: String,
    pub line: u32,
    pub column: u32,
}

/// Clone so one compile can answer every caller that waited on it: the followers of an in-flight
/// build get the leader's failure, diagnostics and all, rather than compiling again to rediscover it.
#[derive(Debug, Clone)]
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

    /// A collection's JSON, cached in the same map a module build's is.
    fn json(body: String) -> Built {
        Built { content_type: JSON, ..Built::js(body) }
    }
}

pub const JS: &str = "text/javascript; charset=utf-8";
pub const JSON: &str = "application/json; charset=utf-8";

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
/// among them, since `sfc::check_vue` and `sfc::strip_types` run oxc over their
/// `<script lang="ts">` blocks. `md` is
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
    pub compile_timeout: Duration,
    pub max_compiles: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            cdn: "https://esm.sh".into(),
            cache_bytes: 64 << 20,
            max_source_bytes: 64 << 10,
            sass_timeout: Duration::from_millis(scss::DEFAULT_TIMEOUT_MS),
            compile_timeout: Duration::from_millis(DEFAULT_COMPILE_TIMEOUT_MS),
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
    /// Misses that waited on a compile of the same key already running instead of starting a second
    /// one, so `misses - coalesced` is what the daemon actually compiled.
    pub coalesced: u64,
}

struct Cache {
    map: HashMap<u128, Arc<Built>>,
    order: VecDeque<u128>,
    bytes: usize,
    hits: u64,
    misses: u64,
    coalesced: u64,
}

/// How long a compile waits for a permit before its request is refused. Long enough to absorb the
/// burst of module requests one page load makes, short enough that a saturated daemon says so
/// instead of holding the connection open.
const COMPILE_QUEUE_WAIT: Duration = Duration::from_secs(5);

/// How long one compile may run before the request gives up on it. A whole `.astro` file of the
/// example projects compiles in single-digit milliseconds, and the Sass inside one has its own
/// five-second deadline, so ten seconds is three orders of magnitude of headroom over anything
/// written by hand and still bounds what one request can hold.
pub const DEFAULT_COMPILE_TIMEOUT_MS: u64 = 10_000;

/// Compile threads that may be running past their deadline before compilation is refused outright.
/// Each one keeps a core and a `parser_stack_bytes()` reservation until it returns on its own, and
/// nothing can take those back, so past this the honest answer to a new compile is 503.
const MAX_RUNAWAY_COMPILES: usize = 8;

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
    pub runaway: usize,
    pub timeouts: u64,
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
    limit: AtomicUsize,
    wait: Duration,
    max_runaway: usize,
    state: Mutex<GateState>,
    free: Condvar,
    refused: AtomicU64,
    runaway: AtomicUsize,
    timeouts: AtomicU64,
}

/// A compile that is still running, and whether the request that started it has given up on it.
struct Flight<T> {
    done: Mutex<Option<Outcome<T>>>,
    ready: Condvar,
    state: AtomicU8,
}

/// What a compile thread hands back: what it built, or the panic payload to resume on the waiter.
type Outcome<T> = Result<Result<T, BuildError>, Box<dyn Any + Send>>;

const RUNNING: u8 = 0;
const RUNAWAY: u8 = 1;
const SETTLED: u8 = 2;

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
    fn new(limit: usize, wait: Duration, max_runaway: usize) -> Gate {
        Gate {
            limit: AtomicUsize::new(limit),
            wait,
            max_runaway,
            state: Mutex::new(GateState::default()),
            free: Condvar::new(),
            refused: AtomicU64::new(0),
            runaway: AtomicUsize::new(0),
            timeouts: AtomicU64::new(0),
        }
    }

    fn stats(&self) -> CompileStats {
        let state = self.state.lock().unwrap();
        CompileStats {
            running: state.running,
            queued: state.queued,
            limit: self.limit.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            runaway: self.runaway.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
        }
    }

    /// `None` when this thread is already compiling under a permit: a `?type=style` build asks for
    /// the module from inside `compile`, and a second permit would deadlock once the gate is full.
    fn enter(&self) -> Result<Option<Permit<'_>>, String> {
        if COMPILING.get() {
            return Ok(None);
        }
        let runaway = self.runaway.load(Ordering::Relaxed);
        if runaway >= self.max_runaway {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return Err(format!(
                "{runaway} compiles are still running past their deadline and cannot be interrupted; not starting another"
            ));
        }
        let limit = self.limit.load(Ordering::Relaxed);
        let mut state = self.state.lock().unwrap();
        if state.running >= limit {
            let deadline = Instant::now() + self.wait;
            state.queued += 1;
            loop {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    state.queued -= 1;
                    drop(state);
                    self.refused.fetch_add(1, Ordering::Relaxed);
                    let ms = self.wait.as_millis();
                    return Err(format!("{limit} compiles are already running and this one waited {ms} ms for a slot"));
                };
                state = self.free.wait_timeout(state, left).unwrap().0;
                if state.running < limit {
                    state.queued -= 1;
                    break;
                }
            }
        }
        state.running += 1;
        Ok(Some(Permit(self)))
    }

    /// A compile passed its deadline. The permit goes back to the pool the moment the request
    /// returns; the thread cannot be stopped, so it is counted here until it ends on its own. Both
    /// this and `settled` take the gate's lock, so a compile that finishes in the same instant as
    /// the deadline is either counted and uncounted or neither.
    fn overran(&self, state: &AtomicU8) {
        let _lock = self.state.lock().unwrap();
        if state.compare_exchange(RUNNING, RUNAWAY, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            self.runaway.fetch_add(1, Ordering::Relaxed);
            self.timeouts.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn settled(&self, state: &AtomicU8) {
        let _lock = self.state.lock().unwrap();
        if state.swap(SETTLED, Ordering::Relaxed) == RUNAWAY {
            self.runaway.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// A compile of one cache key that other callers may wait on. Issue #55: `build` looked the cache
/// up, queued for a permit and did not look again, so N simultaneous requests for one cold module
/// each compiled it. The results are identical — the cache is content-addressed — so what the herd
/// costs is CPU and permits, precisely when the daemon is busiest.
#[derive(Default)]
struct Inflight {
    done: Mutex<Option<Result<Arc<Built>, BuildError>>>,
    ready: Condvar,
}

/// What a caller asking for a key gets: the right to compile it and answer for it, the right to
/// compile it for itself alone, or the answer the compile already running for it gave.
enum Join<'a> {
    Lead(Lead<'a>),
    Solo,
    Waited(Result<Arc<Built>, BuildError>),
}

/// The one caller compiling a key. Its drop takes the key out of the map and wakes everyone
/// waiting, so a panic on the way through hands them an answer rather than leaving them there.
struct Lead<'a> {
    engine: &'a Engine,
    key: u128,
    flight: Arc<Inflight>,
}

impl Lead<'_> {
    fn settle(&self, outcome: &Result<Arc<Built>, BuildError>) {
        *self.flight.done.lock().unwrap() = Some(outcome.clone());
    }
}

impl Drop for Lead<'_> {
    fn drop(&mut self) {
        self.engine.inflight.lock().unwrap().remove(&self.key);
        let mut done = self.flight.done.lock().unwrap();
        if done.is_none() {
            *done = Some(Err(BuildError::busy("the compile this request was waiting on ended without an answer".into())));
        }
        drop(done);
        self.flight.ready.notify_all();
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

/// How many `(tenant, .astro path)` pairs keep a compiled-JS fingerprint. Past that the map is
/// emptied rather than evicted one by one: a forgotten entry costs a page reload, nothing more.
const LAST_JS_ENTRIES: usize = 4096;

/// A handle, not the engine itself: a compile that overruns its deadline is abandoned rather than
/// joined, so the thread running it must keep everything it may still touch alive.
#[derive(Clone)]
pub struct Engine(Arc<EngineInner>);

pub struct EngineInner {
    pub cfg: Config,
    cache: Mutex<Cache>,
    /// Cache keys being compiled right now, so the second caller for one waits instead of
    /// duplicating the compile and the permit it costs.
    inflight: Mutex<HashMap<u128, Arc<Inflight>>>,
    /// The JS each `.astro` module last compiled to, so a write can be told apart from an edit that
    /// only moved a `<style>` block. Written by every module build, read once per write.
    last_js: Mutex<HashMap<(String, String), u128>>,
    sass: scss::Sass,
    gate: Gate,
    metrics: Arc<Metrics>,
}

impl std::ops::Deref for Engine {
    type Target = EngineInner;

    fn deref(&self) -> &EngineInner {
        &self.0
    }
}

impl Engine {
    pub fn new(cfg: Config, metrics: Arc<Metrics>) -> Engine {
        let gate = Gate::new(cfg.max_compiles.max(1), COMPILE_QUEUE_WAIT, MAX_RUNAWAY_COMPILES);
        Engine::with_gate(cfg, metrics, gate)
    }

    /// An engine whose compile gate is built to order. Neither the queue deadline nor the runaway
    /// cap is a flag, and a limit of zero — a gate no compile can ever pass — is how a test says
    /// "every permit is busy" without holding one.
    #[cfg(test)]
    pub(crate) fn gated(cfg: Config, metrics: Arc<Metrics>, limit: usize, wait: Duration, max_runaway: usize) -> Engine {
        Engine::with_gate(cfg, metrics, Gate::new(limit, wait, max_runaway))
    }

    fn with_gate(cfg: Config, metrics: Arc<Metrics>, gate: Gate) -> Engine {
        let sass = scss::Sass::new(cfg.sass_timeout);
        Engine(Arc::new(EngineInner {
            cfg,
            cache: Mutex::new(Cache { map: HashMap::new(), order: VecDeque::new(), bytes: 0, hits: 0, misses: 0, coalesced: 0 }),
            inflight: Mutex::new(HashMap::new()),
            last_js: Mutex::new(HashMap::new()),
            sass,
            gate,
            metrics,
        }))
    }

    pub fn parser_stack_bytes(&self) -> usize {
        self.cfg.max_source_bytes.saturating_mul(STACK_PER_SOURCE_BYTE).max(MIN_PARSER_STACK)
    }

    pub fn stats(&self) -> CacheStats {
        let c = self.cache.lock().unwrap();
        CacheStats { entries: c.map.len(), bytes: c.bytes, hits: c.hits, misses: c.misses, coalesced: c.coalesced }
    }

    pub fn sass_stats(&self) -> scss::SassStats {
        self.sass.stats()
    }

    pub fn compile_stats(&self) -> CompileStats {
        self.gate.stats()
    }

    /// Either the right to compile `key`, or the answer the compile already running for it gave.
    /// The wait is bounded by what the leader is bounded by — its permit deadline and its compile
    /// deadline — and its failure is the follower's failure, so nobody is left holding the key.
    ///
    /// A thread that is already compiling under a permit never waits: the leader takes the key
    /// before it takes a permit, so it may be queueing for the very permit this thread is holding —
    /// a sweep, or the module build a `?type=style` compile makes from inside itself. Such a thread
    /// compiles the key for itself instead, which is the duplicate the cache has always tolerated.
    fn join(&self, key: u128) -> Join<'_> {
        if COMPILING.get() {
            return Join::Solo;
        }
        let mut map = self.inflight.lock().unwrap();
        let Some(running) = map.get(&key).cloned() else {
            let flight = Arc::<Inflight>::default();
            map.insert(key, flight.clone());
            return Join::Lead(Lead { engine: self, key, flight });
        };
        drop(map);
        self.cache.lock().unwrap().coalesced += 1;
        let mut done = running.done.lock().unwrap();
        loop {
            if let Some(outcome) = done.clone() {
                return Join::Waited(outcome);
            }
            done = running.ready.wait(done).unwrap();
        }
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

    /// Runs a whole-project compile — `check` — under a single permit taken once, held for the
    /// sweep and given back at the end. Issue #54: `check` builds every source file of a tenant, and
    /// a permit per file means that on a saturated daemon the error page waits the queue deadline
    /// once per file. A maintenance sweep should cost one place in the queue, not one per file.
    ///
    /// The per-file compile deadline is untouched: each build inside still runs on its own thread
    /// with its own wall clock, so one unfinishable file does not take the sweep with it.
    pub fn sweep<T>(&self, f: impl FnOnce() -> T) -> Result<T, BuildError> {
        let _permit = self.gate.enter().map_err(BuildError::busy)?;
        let _compiling = Compiling::enter();
        Ok(f())
    }

    /// A collection's `{entries, dates}` JSON, built the way a module is. Issue #90: this reads and
    /// parses every entry file of a collection — the one thing the daemon does that touches every
    /// file of a project — and it went through none of what bounds a compile, so a 200-entry blog
    /// cost 1.3 s of CPU per page view with nothing limiting how many ran at once.
    ///
    /// The key is the tenant's version rather than the bytes that went into the build: what the
    /// entries are is itself a question only the build can answer, and every write and every base
    /// reload bumps the version, so an edit anywhere invalidates the collection.
    pub fn collection(&self, tenant: &Arc<Tenant>, name: &str) -> Result<Arc<Built>, BuildError> {
        let mut h = Vec::with_capacity(tenant.id.len() + name.len() + 32);
        h.extend_from_slice(b"c\0");
        h.extend_from_slice(tenant.id.as_bytes());
        h.push(0);
        h.extend_from_slice(&tenant.version().to_le_bytes());
        h.extend_from_slice(name.as_bytes());
        let key = xxh3_128(&h);
        if let Some(b) = self.cached(key) {
            return Ok(b);
        }
        let lead = match self.join(key) {
            Join::Lead(lead) => Some(lead),
            Join::Solo => None,
            Join::Waited(outcome) => return outcome,
        };
        let outcome = self.collection_once(tenant, name, key);
        if let Some(lead) = lead {
            lead.settle(&outcome);
        }
        outcome
    }

    /// One permit for the whole collection, as `sweep` takes one for a whole project (#54): a page
    /// asking for two hundred entries should cost one place in the queue, not two hundred. Inside
    /// it is one `deadlined` thread, so an entry file nobody can parse in time is given up on
    /// rather than held open.
    fn collection_once(&self, tenant: &Arc<Tenant>, name: &str, key: u128) -> Result<Arc<Built>, BuildError> {
        let path = format!("content/{name}");
        let _permit = self.gate.enter().map_err(|e| BuildError::busy(format!("{path}: {e}")))?;
        let (t, collection, max) = (tenant.clone(), name.to_string(), self.cfg.max_source_bytes);
        let json = self.deadlined(&path, move || content::collection_json(&t, &collection, max).map(|v| v.to_string()))?;
        let built = Arc::new(Built::json(json));
        self.insert(key, built.clone());
        Ok(built)
    }

    pub fn build(&self, tenant: &Arc<Tenant>, path: &str, kind: Kind) -> Result<Arc<Built>, BuildError> {
        let data = tenant
            .read(path)
            .map_err(|e| BuildError::compile(format!("{path}: {e}"), vec![]))?
            .ok_or_else(|| BuildError::not_found(path))?;
        let built = self.build_bytes(tenant, path, kind, data)?;
        self.remember(tenant, path, kind, &built);
        Ok(built)
    }

    /// `build` of a source the tenant may not hold yet. The cache is content-addressed, so building
    /// the bytes of a write before it lands is the compile the next module request would have paid
    /// for anyway.
    fn build_bytes(&self, tenant: &Arc<Tenant>, path: &str, kind: Kind, data: Arc<[u8]>) -> Result<Arc<Built>, BuildError> {
        let site = crate::resolve::tenant_site(tenant).map_err(|e| BuildError::compile(e, vec![]))?;
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
        self.within_caps(path, kind, &data)?;
        let lead = match self.join(key) {
            Join::Lead(lead) => Some(lead),
            Join::Solo => None,
            Join::Waited(outcome) => return outcome,
        };
        let outcome = self.compile_once(tenant, path, kind, data, site, key);
        if let Some(lead) = lead {
            lead.settle(&outcome);
        }
        outcome
    }

    /// The compile itself, under a permit: the caller holds the key while this runs, so this is the
    /// only thread compiling it however many requests are waiting.
    fn compile_once(
        &self,
        tenant: &Arc<Tenant>,
        path: &str,
        kind: Kind,
        data: Arc<[u8]>,
        site: Option<String>,
        key: u128,
    ) -> Result<Arc<Built>, BuildError> {
        let _permit = self.gate.enter().map_err(|e| BuildError::busy(format!("{path}: {e}")))?;
        let started = Instant::now();
        let compiled = self.bounded(tenant, path, kind, data, site);
        self.metrics.compiled(kind, started.elapsed());
        let built = Arc::new(compiled?);
        self.insert(key, built.clone());
        Ok(built)
    }

    /// The size and nesting caps, before a compile takes a permit or a thread: a source that cannot
    /// be compiled has no business queueing for the right to try.
    fn within_caps(&self, path: &str, kind: Kind, data: &[u8]) -> Result<(), BuildError> {
        let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("").to_ascii_lowercase();
        if kind != Kind::Module || !parses_source(&ext) {
            return Ok(());
        }
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
        Ok(())
    }

    fn bounded(&self, tenant: &Arc<Tenant>, path: &str, kind: Kind, data: Arc<[u8]>, site: Option<String>) -> Result<Built, BuildError> {
        let (engine, t, owned) = (self.clone(), tenant.clone(), path.to_string());
        self.deadlined(path, move || engine.compile(&t, &owned, kind, &data, site.as_deref()))
    }

    /// One compile, on a thread of its own, with a wall-clock deadline. `astro_codegen`, oxc and
    /// satteri-mdxjs are synchronous and offer no cancellation, so a compile that overruns is
    /// abandoned rather than joined: the request is answered with a diagnostic, the gate permit goes
    /// back to the pool the moment this returns, and the thread is counted under `runaway` until it
    /// ends on its own. The thread outlives the request, so the work owns everything it may touch —
    /// an `Engine` handle, the tenant, the source bytes.
    ///
    /// A `?type=style` or `?type=script` build asks for its module from inside its own compile. That
    /// inner build runs here on the stack and under the deadline its parent already has, which is
    /// what keeps one request to one thread and one permit.
    fn deadlined<T: Send + 'static>(
        &self,
        path: &str,
        work: impl FnOnce() -> Result<T, BuildError> + Send + 'static,
    ) -> Result<T, BuildError> {
        if ON_PARSER_STACK.get() {
            let _compiling = Compiling::enter();
            return work();
        }
        let flight = Arc::new(Flight { done: Mutex::new(None), ready: Condvar::new(), state: AtomicU8::new(RUNNING) });
        let (engine, mine) = (self.clone(), flight.clone());
        let spawned = std::thread::Builder::new().name("compile".into()).stack_size(self.parser_stack_bytes()).spawn(move || {
            ON_PARSER_STACK.set(true);
            let _compiling = Compiling::enter();
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
            *mine.done.lock().unwrap() = Some(out);
            mine.ready.notify_all();
            engine.gate.settled(&mine.state);
        });
        if let Err(e) = spawned {
            return Err(BuildError::compile(format!("{path}: cannot start a compiler thread: {e}"), vec![]));
        }
        #[cfg(test)]
        SPAWNED.set(SPAWNED.get() + 1);
        let deadline = Instant::now() + self.cfg.compile_timeout;
        let mut done = flight.done.lock().unwrap();
        loop {
            if let Some(out) = done.take() {
                drop(done);
                return out.unwrap_or_else(|payload| std::panic::resume_unwind(payload));
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
            done = flight.ready.wait_timeout(done, left).unwrap().0;
        }
        drop(done);
        self.gate.overran(&flight.state);
        let ms = self.cfg.compile_timeout.as_millis();
        Err(refused(
            path,
            format!("compile did not finish within {ms} ms"),
            "the compilers cannot be interrupted, so the thread was left to run out; raise --compile-timeout-ms, or split the file",
        ))
    }

    /// Only ever called for content the tenant holds, so a fingerprint here is one a page could
    /// have loaded. A speculative build — the one `update_kind` runs on bytes not yet written —
    /// must not land in the map: were the write then refused, the next write would be compared
    /// against JS the browser never ran.
    fn remember(&self, tenant: &Tenant, path: &str, kind: Kind, built: &Built) {
        if kind != Kind::Module || !path.ends_with(".astro") {
            return;
        }
        let mut map = self.last_js.lock().unwrap();
        if map.len() >= LAST_JS_ENTRIES {
            map.clear();
        }
        map.insert((tenant.id.clone(), path.to_string()), module_fingerprint(built));
    }

    /// How the live-reload client should apply a write of `bytes` to `path`. A stylesheet swaps in
    /// place. An `.astro` file that compiles to the JS the last build produced moved only its
    /// `<style>` blocks, so those swap instead. Everything else reloads, and so does anything this
    /// cannot prove: an `.astro` file nothing has built yet, one that no longer compiles, one whose
    /// hoisted `<script>` changed.
    pub fn update_kind(&self, tenant: &Arc<Tenant>, path: &str, bytes: &[u8]) -> UpdateKind {
        if !path.ends_with(".astro") {
            return UpdateKind::from_path(path);
        }
        let before = self.last_js.lock().unwrap().get(&(tenant.id.clone(), path.to_string())).copied();
        let Some(before) = before else { return UpdateKind::Module };
        match self.build_bytes(tenant, path, Kind::Module, Arc::from(bytes)) {
            Ok(built) if module_fingerprint(&built) == before => UpdateKind::Style,
            _ => UpdateKind::Module,
        }
    }

    fn compile(&self, tenant: &Arc<Tenant>, path: &str, kind: Kind, data: &[u8], site: Option<&str>) -> Result<Built, BuildError> {
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
                    "vue" => {
                        let source = text();
                        sfc::check_vue(path, &source)?;
                        Ok(Built::js(sfc_loader("vue", path, &source)))
                    }
                    "svelte" => Ok(Built::js(sfc_loader("svelte", path, &sfc::strip_types(path, &text())?))),
                    "json" => Ok(Built::js(format!("export default JSON.parse({});\n", json_str(&text())))),
                    "md" => Ok(Built::scanned(markdown::page_module(path, &text())?)),
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

    pub fn serve(&self, tenant: &Arc<Tenant>, path: &str, kind: Kind, resolver: &Resolver) -> Result<(String, &'static str), BuildError> {
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

    /// TypeScript out of a script `@vue/compiler-sfc` generated in the browser (issue #52).
    ///
    /// The compiler reads `defineProps<Props>()` and the other type-driven macros out of the source
    /// to build the runtime declaration, so a `.vue` file reaches it with its types intact and what
    /// comes back here is its output. It is JavaScript the daemon never asked for, so it is held to
    /// what every compile is held to: a size cap, the nesting cap, a gate permit and the compile
    /// deadline.
    pub fn strip_ts(&self, path: &str, source: &str) -> Result<String, BuildError> {
        let max = self.cfg.max_source_bytes.saturating_mul(COMPILED_SCRIPT_GROWTH);
        if source.len() > max {
            let text = format!("compiled script too large to strip: {} KiB, limit {} KiB", source.len() / 1024, max / 1024);
            return Err(refused(path, text, "raise --max-source-kb, or split the component"));
        }
        let depth = nesting_depth(source.as_bytes());
        if depth > MAX_NESTING_DEPTH {
            let text = format!("compiled script nests {depth} deep, limit {MAX_NESTING_DEPTH}");
            return Err(refused(path, text, "unbalanced brackets are the usual cause"));
        }
        let _permit = self.gate.enter().map_err(|e| BuildError::busy(format!("{path}: {e}")))?;
        let started = Instant::now();
        let (owned, code) = (path.to_string(), source.to_string());
        let out = self.deadlined(path, move || js::transform_sfc_script(&owned, &code));
        self.metrics.compiled(Kind::Module, started.elapsed());
        out
    }
}

/// How much bigger than `--max-source-kb` a compiled script may be. `compileScript` emits the
/// script block plus the render function it inlines from the template, so it outgrows the block it
/// came from — 4.3x and 12.5x for the two components in `examples/vue` — but the cap it is measured
/// against is the whole file's, template and styles included, and against that the same two measure
/// 1.3x and 1.9x. Eight leaves room for a component that is nearly all script.
const COMPILED_SCRIPT_GROWTH: usize = 8;

/// Everything a component renders except its CSS: the module body, and the hoisted scripts, which
/// the body only names by index. Two builds with the same fingerprint differ in their `<style>`
/// blocks or not at all.
fn module_fingerprint(built: &Built) -> u128 {
    let mut h = Xxh3Default::new();
    h.update(built.body.as_bytes());
    for script in &built.scripts {
        let (tag, code) = match script {
            astro::Script::Inline(code) => (b"i", code),
            astro::Script::External(src) => (b"e", src),
        };
        h.update(tag);
        h.update(code.as_bytes());
    }
    h.digest128()
}

/// No Rust compiler exists for `.vue` or `.svelte`, so the file is served as a module that
/// compiles its source in the browser and re-exports the component. A `.svelte` file's source has
/// been through `sfc::strip_types`, so its `<script>` blocks are JavaScript; a `.vue` file's has
/// not, because `@vue/compiler-sfc` needs the types — the loader posts the script it generates to
/// `/__sl/strip-ts` instead.
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

/// Every engine a test builds comes from here: `Engine::new` and `Engine::with_gate` are named in
/// this block and nowhere under `#[cfg(test)] mod`, so an argument added to either is one edit.
#[cfg(test)]
impl Engine {
    pub fn for_tests() -> Engine {
        Engine::for_tests_with(Config { cache_bytes: 1 << 20, ..Config::default() })
    }

    pub fn for_tests_with(cfg: Config) -> Engine {
        Engine::new(cfg, Arc::new(Metrics::default()))
    }

    /// Neither the queue deadline nor the runaway cap is a flag, so tests build the gate directly.
    pub fn for_tests_gated(cfg: Config, limit: usize, wait: Duration, max_runaway: usize) -> Engine {
        Engine::with_gate(cfg, Arc::new(Metrics::default()), Gate::new(limit, wait, max_runaway))
    }

    /// So a fixture's state records into the same metrics as the engine inside it, as the daemon does.
    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::resolve::Resolver;
    use crate::store::{Base, Store, Tenant, UpdateKind};

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
            tenant.write(path, body.as_bytes().to_vec(), UpdateKind::from_path(path)).unwrap();
        }
        tenant
    }

    fn engine() -> Engine {
        Engine::for_tests()
    }

    fn served(engine: &Engine, tenant: &Arc<Tenant>) -> String {
        engine.serve(tenant, PAGE_PATH, Kind::Module, &Resolver::new(tenant, "https://esm.sh", 7).unwrap()).unwrap().0
    }

    const CAP: usize = 64 << 10;

    fn engine_capped(max_source_bytes: usize) -> Engine {
        Engine::for_tests_with(capped(max_source_bytes))
    }

    fn tenant_with(path: &str, source: &str) -> Arc<Tenant> {
        tenant(&[(path, source)])
    }

    fn engine_gated(cfg: Config, limit: usize, wait: Duration) -> Engine {
        engine_bounded(cfg, limit, wait, MAX_RUNAWAY_COMPILES)
    }

    fn engine_bounded(cfg: Config, limit: usize, wait: Duration, max_runaway: usize) -> Engine {
        Engine::for_tests_gated(cfg, limit, wait, max_runaway)
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

    const TYPED_SFC: &str = "<script setup lang=\"ts\">\ninterface Props { label: string }\nconst props = defineProps<Props>();\n</script>\n\n<template><p>{{ props.label }}</p></template>\n";

    /// Issue #52: the type is what `@vue/compiler-sfc` builds the runtime props out of, so the
    /// module the browser loads carries the file as written — `lang="ts"` and all.
    #[test]
    fn a_vue_module_carries_its_typescript_to_the_browser() {
        let t = tenant_with("src/components/Counter.vue", TYPED_SFC);
        let built = engine().build(&t, "src/components/Counter.vue", Kind::Module).unwrap();
        assert!(built.body.contains("vue-loader.js"), "{}", built.body);
        assert!(built.body.contains("defineProps<Props>()"), "{}", built.body);
        assert!(built.body.contains(r#"lang=\"ts\""#), "{}", built.body);
    }

    /// A `.svelte` file goes the other way: `svelte/compiler` cannot parse TypeScript at all.
    #[test]
    fn a_svelte_module_carries_javascript() {
        let source = "<script lang=\"ts\">\n  let n: number = 1;\n</script>\n\n<p>{n}</p>\n";
        let t = tenant_with("src/components/C.svelte", source);
        let built = engine().build(&t, "src/components/C.svelte", Kind::Module).unwrap();
        assert!(built.body.contains("let n = 1"), "{}", built.body);
        assert!(!built.body.contains("lang="), "{}", built.body);
    }

    #[test]
    fn strip_ts_removes_the_types_from_a_compiled_script() {
        let script = "const props: { label: string } = __props;\nexport default { setup(__props: any) { return () => props.label; } };\n";
        let out = engine().strip_ts("src/components/Counter.vue", script).unwrap();
        assert!(!out.contains(": any"), "{out}");
        assert!(!out.contains("{ label: string }"), "{out}");
        assert!(out.contains("__props"), "{out}");
    }

    #[test]
    fn strip_ts_refuses_a_script_past_the_cap_and_one_that_does_not_parse() {
        let engine = engine_capped(CAP);
        let big = "x".repeat(CAP * COMPILED_SCRIPT_GROWTH + 1);
        let e = engine.strip_ts("src/components/C.vue", &big).unwrap_err();
        assert!(e.message.contains("too large"), "{}", e.message);
        let e = engine.strip_ts("src/components/C.vue", "const n: number = ;\n").unwrap_err();
        assert_eq!(e.diagnostics[0].file, "src/components/C.vue");
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
    /// Issue #48: past either limit it is refused rather than read as no collections at all — a
    /// config nobody parsed is not a tenant whose collections live in the default directory.
    #[test]
    fn a_content_config_past_either_limit_is_refused() {
        let good = "export const collections = { posts: defineCollection({}) };";
        let t = tenant_with("src/content.config.ts", good);
        assert!(!crate::transform::content::config(&t, CAP).unwrap().1.is_empty());
        for source in [&"a".repeat(CAP + 1), &"[".repeat(MAX_NESTING_DEPTH + 1)] {
            let t = tenant_with("src/content.config.ts", source);
            let e = crate::transform::content::config(&t, CAP).unwrap_err();
            assert!(e.message.contains("over the"), "{}", e.message);
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
            assert_eq!((stats.limit, stats.queued, stats.refused, stats.runaway), (2, 0, 1, 0));
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
        engine.gate.limit.store(0, Ordering::Relaxed);
        assert!(engine.build(&t, "src/hit.ts", Kind::Module).is_ok());
        assert_eq!(build_err(engine.build(&t, "src/miss.ts", Kind::Module)).status, 503);
    }

    /// A stylesheet that grass will not finish inside the deadline. Sass has a deadline of its own,
    /// so this test keeps that one long: what is under test is the compile deadline around it.
    fn slow_source() -> String {
        "@for $i from 1 through 300000 { $unused: $i * 2; }".to_string()
    }

    /// Issue #63: one tenant's file must not remove a permit from the pool for as long as it runs.
    /// `astro_codegen`, oxc and satteri-mdxjs cannot be interrupted, so the thread is abandoned —
    /// but the permit comes back with the answer, and the next tenant compiles on it.
    #[test]
    fn a_compile_past_its_deadline_gives_its_permit_back() {
        let slow = tenant_with("src/slow.scss", &slow_source());
        let ordinary = tenant_with("src/page.astro", "<p>hi</p>\n");
        let cfg = Config { compile_timeout: Duration::from_millis(200), ..capped(CAP) };
        let engine = engine_gated(cfg, 1, Duration::from_millis(50));

        let started = Instant::now();
        let e = build_err(engine.build(&slow, "src/slow.scss", Kind::Module));
        assert!(e.message.contains("compile did not finish within 200 ms"), "{}", e.message);
        assert_eq!(e.diagnostics.len(), 1);
        assert!(started.elapsed() < Duration::from_secs(5), "the request waited {:?}", started.elapsed());

        let stats = engine.compile_stats();
        assert_eq!((stats.running, stats.runaway, stats.timeouts), (0, 1, 1), "the permit is back and the thread is counted");
        // The gate holds one permit and the runaway thread is still burning a core on it.
        assert!(engine.build(&ordinary, "src/page.astro", Kind::Module).is_ok());
    }

    /// Nothing can take a runaway's core back, so past the cap the honest answer is 503 rather than
    /// another abandoned thread.
    #[test]
    fn compilation_is_refused_once_too_many_runaways_pile_up() {
        let t = tenant(&[("src/a.scss", slow_source().as_str()), ("src/b.scss", &format!("{} // b", slow_source()))]);
        let cfg = Config { compile_timeout: Duration::from_millis(200), ..capped(CAP) };
        let engine = engine_bounded(cfg, 4, Duration::from_millis(50), 1);

        assert!(build_err(engine.build(&t, "src/a.scss", Kind::Module)).message.contains("did not finish"));
        assert_eq!(engine.compile_stats().runaway, 1);

        let e = build_err(engine.build(&t, "src/b.scss", Kind::Module));
        assert_eq!(e.status, 503);
        assert!(e.message.contains("past their deadline"), "{}", e.message);
        assert_eq!(engine.compile_stats().refused, 1);
    }

    /// Issue #55: `build` looked the cache up, queued for a permit and did not look again, so N
    /// simultaneous requests for one cold module each compiled it. Four ask at once; one compiles,
    /// and the three that waited are counted as waits rather than as hits.
    #[test]
    fn a_burst_on_one_cold_module_compiles_it_once() {
        let t = tenant_with("src/slow.scss", &slow_source());
        let metrics = Arc::new(crate::metrics::Metrics::default());
        let cfg = Config { compile_timeout: Duration::from_secs(1), ..capped(CAP) };
        let engine = Engine::gated(cfg, metrics.clone(), 4, Duration::from_millis(50), MAX_RUNAWAY_COMPILES);

        std::thread::scope(|scope| {
            let leader = scope.spawn(|| engine.build(&t, "src/slow.scss", Kind::Module).err().map(|e| e.message));
            let deadline = Instant::now() + Duration::from_secs(5);
            while engine.compile_stats().running < 1 && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let followers: Vec<_> =
                (0..3).map(|_| scope.spawn(|| engine.build(&t, "src/slow.scss", Kind::Module).err().map(|e| e.message))).collect();
            let first = leader.join().unwrap();
            for f in followers {
                assert_eq!(f.join().unwrap(), first, "a follower answers with what the one compile produced");
            }
        });

        let cache = engine.stats();
        assert_eq!((cache.misses, cache.coalesced), (4, 3), "four lookups missed, three of them waited");
        assert_eq!(metrics.compiles()[0].count(), 1, "one compile, not four");
        assert_eq!(engine.compile_stats().running, 0);
    }

    /// Issue #54: `check` builds every source file of a tenant, and a permit per file meant that on
    /// a saturated daemon the error page waited the queue deadline once per file. The sweep takes
    /// one permit and holds it; the builds inside take none, so a gate emptied under them still lets
    /// every file through.
    #[test]
    fn a_sweep_compiles_every_file_under_one_permit() {
        let files = ["src/a.ts", "src/b.ts", "src/c.ts"];
        let t = tenant(&[(files[0], "export const a = 1;\n"), (files[1], "export const b = 2;\n"), (files[2], "export const c = 3;\n")]);
        let engine = engine_gated(capped(CAP), 1, Duration::from_millis(50));
        let built = engine
            .sweep(|| {
                engine.gate.limit.store(0, Ordering::Relaxed);
                files.map(|p| engine.build(&t, p, Kind::Module).is_ok())
            })
            .unwrap();
        assert_eq!(built, [true, true, true]);
        assert_eq!(engine.compile_stats().refused, 0, "the sweep queued once and nothing inside it queued again");
    }

    /// And when it cannot have that one permit it says so once, after one wait.
    #[test]
    fn a_saturated_sweep_is_refused_once_not_once_per_file() {
        let engine = engine_bounded(capped(CAP), 0, Duration::from_millis(100), MAX_RUNAWAY_COMPILES);
        let started = Instant::now();
        let e = engine.sweep(|| 0u8).unwrap_err();
        assert_eq!(e.status, 503);
        assert!(e.message.contains("waited 100 ms"), "{}", e.message);
        assert!(started.elapsed() < Duration::from_secs(1), "the sweep waited {:?}", started.elapsed());
        assert_eq!(engine.compile_stats().refused, 1);
    }

    const POST: &str = "---\ntitle: a post\n---\n# heading\n\nbody\n";

    /// Issue #90: the collection endpoint read and parsed every entry of a collection on every
    /// request, so a page calling `getCollection` paid for the whole collection on every load. The
    /// second request for an unchanged collection must cost nothing — no build, and no permit.
    #[test]
    fn a_second_collection_build_is_a_cache_hit_and_takes_no_permit() {
        let t = tenant(&[("src/content/posts/a.md", POST), ("src/content/posts/b.md", POST)]);
        let engine = engine_gated(capped(CAP), 1, Duration::from_millis(50));
        let first = engine.collection(&t, "posts").unwrap();
        assert!(first.body.contains("heading"), "{}", first.body);
        engine.gate.limit.store(0, Ordering::Relaxed);
        let second = engine.collection(&t, "posts").unwrap();
        assert_eq!(first.body, second.body);
        let cache = engine.stats();
        assert_eq!((cache.misses, cache.hits), (1, 1), "one build, then a hit");
        assert_eq!(engine.compile_stats().refused, 0, "the hit never asked the gate for anything");
    }

    /// And an edit invalidates it: the key carries the tenant's version, which every write bumps.
    #[test]
    fn an_edit_rebuilds_the_collection() {
        let t = tenant(&[("src/content/posts/a.md", POST)]);
        let engine = engine_gated(capped(CAP), 1, Duration::from_millis(50));
        assert!(!engine.collection(&t, "posts").unwrap().body.contains("written later"));
        let later = "---\ntitle: written later\n---\n";
        t.write("src/content/posts/b.md", later.as_bytes().to_vec(), UpdateKind::Module).unwrap();
        assert!(engine.collection(&t, "posts").unwrap().body.contains("written later"));
        assert_eq!(engine.stats().misses, 2, "the write invalidated what the first build cached");
    }

    /// One permit for the whole collection, taken before a single entry is read: a build that
    /// cannot have one waits for it and is refused with the 503 a module build gives.
    #[test]
    fn a_collection_build_that_cannot_have_a_permit_is_refused() {
        let t = tenant(&[("src/content/posts/a.md", POST)]);
        let engine = engine_gated(capped(CAP), 0, Duration::from_millis(100));
        let started = Instant::now();
        let e = build_err(engine.collection(&t, "posts"));
        assert_eq!(e.status, 503);
        assert!(e.message.contains("content/posts"), "{}", e.message);
        assert!(e.message.contains("waited 100 ms"), "{}", e.message);
        assert!(started.elapsed() >= Duration::from_millis(100), "it waited {:?}", started.elapsed());
        assert_eq!((engine.compile_stats().refused, engine.stats().entries), (1, 0));
    }

    /// A collection the deadline runs out on is answered with a diagnostic rather than held open:
    /// nothing caps the size of an entry file, so a 60 MiB `.md` under the tenant's quota is parsed
    /// on a thread the request must be able to give up on.
    #[test]
    fn a_collection_past_the_deadline_answers_with_a_diagnostic() {
        let long = format!("---\ntitle: long\n---\n{}", "# heading\n\n".repeat(20_000));
        let t = tenant(&[("src/content/posts/long.md", long.as_str())]);
        let engine = engine_gated(Config { compile_timeout: Duration::from_millis(1), ..capped(CAP) }, 1, Duration::from_millis(50));
        let e = build_err(engine.collection(&t, "posts"));
        assert!(e.message.contains("content/posts: compile did not finish within 1 ms"), "{}", e.message);
        assert_eq!(e.diagnostics.len(), 1);
        assert_eq!(engine.compile_stats().timeouts, 1);
    }

    const CARD: &str = "---\nconst n = 1;\n---\n<p>{n}</p>\n<style>p{color:red}</style>\n<script>console.log(1)</script>\n";
    const CARD_PATH: &str = "src/components/Card.astro";

    /// Issue #13: what the client is told a write changed. Only an `.astro` file that compiles to
    /// the JS the page is already running may swap its styles; everything else reloads.
    #[test]
    fn an_astro_write_is_a_style_swap_only_when_its_js_is_unchanged() {
        let t = tenant(&[(CARD_PATH, CARD)]);
        let e = engine();
        let restyled = CARD.replace("color:red", "color:blue");
        assert_eq!(
            e.update_kind(&t, CARD_PATH, restyled.as_bytes()),
            UpdateKind::Module,
            "nothing has built this file, so nothing can be proved about it"
        );
        e.build(&t, CARD_PATH, Kind::Module).unwrap();
        assert_eq!(e.update_kind(&t, CARD_PATH, restyled.as_bytes()), UpdateKind::Style);
        for edited in [
            CARD.replace("console.log(1)", "console.log(2)"),
            CARD.replace("const n = 1;", "const n = 2;"),
            CARD.replace("<p>{n}</p>", "<p>{n}!</p>"),
            CARD.replace("<style>p{color:red}</style>", ""),
            "---\nconst n = ;\n---\n".to_string(),
        ] {
            assert_eq!(e.update_kind(&t, CARD_PATH, edited.as_bytes()), UpdateKind::Module, "{edited}");
        }
    }

    /// A refused write must leave no fingerprint behind, or the write after it would be compared
    /// against JS no browser ever ran.
    #[test]
    fn a_classified_write_that_never_lands_is_not_remembered() {
        let t = tenant(&[(CARD_PATH, CARD)]);
        let e = engine();
        e.build(&t, CARD_PATH, Kind::Module).unwrap();
        let rewritten = CARD.replace("const n = 1;", "const n = 2;");
        assert_eq!(e.update_kind(&t, CARD_PATH, rewritten.as_bytes()), UpdateKind::Module);
        let restyled = rewritten.replace("color:red", "color:blue");
        assert_eq!(e.update_kind(&t, CARD_PATH, restyled.as_bytes()), UpdateKind::Module);
    }

    #[test]
    fn a_stylesheet_needs_no_compile_to_be_classified() {
        let t = tenant(&[]);
        let e = engine();
        assert_eq!(e.update_kind(&t, "src/styles/tokens.css", b"a{}"), UpdateKind::Css);
        assert_eq!(e.update_kind(&t, "src/styles/app.scss", b"a{}"), UpdateKind::Css);
        assert_eq!(e.update_kind(&t, "src/lib/x.ts", b"export {};"), UpdateKind::Module);
        assert_eq!(e.stats().misses, 0);
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
