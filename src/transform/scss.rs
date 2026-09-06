use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use xxhash_rust::xxh3::{xxh3_64, xxh3_128};

use crate::resolve::normalize;
use crate::store::{FileData, Tenant};

/// One Sass file, source or `@use`d, that the compiler may read.
const MAX_FILE: usize = 1 << 20;
/// Everything one compilation may read through `@use`/`@import` together.
const MAX_READ: usize = 4 << 20;
/// `@for $i from 1 through 300000 { ... }` is 82 bytes of source and 16 MB of CSS.
const MAX_OUTPUT: usize = 4 << 20;

/// A whole second is already 100x a normal compile of the example projects; five leaves room for a
/// slow machine under load while still bounding what one request can hold.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Threads that may be compiling at once. Only ever reached by compiles that are already
/// pathological, and it is what keeps a tenant from turning distinct runaway sources into threads.
const MAX_THREADS: usize = 8;

/// The compile runs here rather than on the stack `Engine::build` reserved, so this thread needs
/// its own. grass costs about 8 KiB per nesting level and the loader refuses anything past
/// `MAX_NESTING_DEPTH`, which puts the deepest stylesheet it will accept well inside this.
const STACK: usize = 64 << 20;

fn norm(path: &Path) -> String {
    normalize(&path.to_string_lossy())
}

/// The tenant's files as they were when the compile started. Holding `FileData` copies no file
/// content, and it makes the file set `'static`, so the compile thread may outlive the request.
struct Snapshot {
    files: BTreeMap<String, FileData>,
    read: AtomicUsize,
    denied: Mutex<Option<String>>,
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Snapshot")
    }
}

impl Snapshot {
    fn of(tenant: &Tenant, source_len: usize) -> Snapshot {
        let files = tenant.list().into_iter().filter_map(|e| tenant.data(&e.path).map(|d| (e.path, d))).collect();
        Snapshot { files, read: AtomicUsize::new(source_len), denied: Mutex::new(None) }
    }

    /// An `io::Error` out of `Fs::read` reaches `SassError::raw`, which panics; a file the limits
    /// refuse is hidden from the compiler instead, and the reason kept for the error it then makes.
    fn deny(&self, message: String) -> bool {
        let mut slot = self.denied.lock().unwrap();
        if slot.is_none() {
            *slot = Some(message);
        }
        false
    }

    fn refusal(&self) -> Option<String> {
        self.denied.lock().unwrap().take()
    }
}

impl grass::Fs for Snapshot {
    fn is_dir(&self, path: &Path) -> bool {
        let prefix = format!("{}/", norm(path));
        self.files.range(prefix.clone()..).next().is_some_and(|(p, _)| p.starts_with(&prefix))
    }

    fn is_file(&self, path: &Path) -> bool {
        let name = norm(path);
        let Some(data) = self.files.get(&name) else { return false };
        let size = data.size() as usize;
        if size > MAX_FILE {
            return self.deny(format!("{name} is {size} bytes, over the {MAX_FILE} byte Sass file limit"));
        }
        if self.read.load(Ordering::Relaxed) + size > MAX_READ {
            return self.deny(format!("{name} takes this compilation past the {MAX_READ} byte Sass import limit"));
        }
        true
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let name = norm(path);
        match self.files.get(&name).map(FileData::read) {
            Some(Ok(bytes)) => {
                let depth = super::nesting_depth(&bytes);
                if depth > super::MAX_NESTING_DEPTH {
                    let limit = super::MAX_NESTING_DEPTH;
                    self.deny(format!("{name} nests {depth} deep, over the {limit} level Sass nesting limit"));
                    return Ok(Vec::new());
                }
                self.read.fetch_add(bytes.len(), Ordering::Relaxed);
                Ok(bytes.to_vec())
            }
            _ => {
                self.deny(format!("{name} could not be read; it changed while the stylesheet was compiling"));
                Ok(Vec::new())
            }
        }
    }
}

struct Flight {
    deadline: Instant,
    done: Mutex<Option<Arc<Result<String, String>>>>,
    ready: Condvar,
    runaway: AtomicBool,
}

#[derive(Serialize, Clone, Copy)]
pub struct SassStats {
    pub running: usize,
    pub runaway: usize,
    pub timeouts: u64,
    pub refused: u64,
}

struct Flights {
    timeout: Duration,
    threads: usize,
    live: Mutex<HashMap<u128, Arc<Flight>>>,
    runaway: AtomicUsize,
    timeouts: AtomicU64,
    refused: AtomicU64,
}

impl Flights {
    fn refuse(&self, message: String) -> String {
        self.refused.fetch_add(1, Ordering::Relaxed);
        message
    }

    /// A compile that failed after the snapshot hid a file reports why it was hidden, not grass's
    /// "Can't find stylesheet to import."
    fn failed(&self, fs: &Snapshot, message: String) -> Result<String, String> {
        Err(match fs.refusal() {
            Some(reason) => self.refuse(reason),
            None => message,
        })
    }

    fn run(&self, fs: &Snapshot, dir: &str, source: String, indented: bool) -> Result<String, String> {
        let mut options = grass::Options::default().fs(fs).load_path(if dir.is_empty() { "." } else { dir });
        if indented {
            options = options.input_syntax(grass::InputSyntax::Sass);
        }
        let css = match grass::from_string(source, &options) {
            // A file the limits hid reads as empty, which can still compile. The CSS that comes
            // back is then not the tenant's stylesheet, so say why rather than serve it.
            Ok(css) => match fs.refusal() {
                Some(reason) => return Err(self.refuse(reason)),
                None => css,
            },
            Err(e) => return self.failed(fs, e.to_string()),
        };
        if css.len() > MAX_OUTPUT {
            return Err(self.refuse(format!("compiled CSS is {} bytes, over the {MAX_OUTPUT} byte limit", css.len())));
        }
        Ok(css)
    }

    fn finish(&self, key: u128, flight: &Arc<Flight>, out: Result<String, String>) {
        {
            let mut live = self.live.lock().unwrap();
            if live.get(&key).is_some_and(|f| Arc::ptr_eq(f, flight)) {
                live.remove(&key);
            }
            if flight.runaway.swap(false, Ordering::Relaxed) {
                self.runaway.fetch_sub(1, Ordering::Relaxed);
            }
        }
        *flight.done.lock().unwrap() = Some(Arc::new(out));
        flight.ready.notify_all();
    }

    /// The deadline passed and grass cannot be interrupted: the thread stays, and every later
    /// request for the same source is refused rather than starting a second one beside it.
    fn overrun(&self, key: u128, flight: &Arc<Flight>) {
        let live = self.live.lock().unwrap();
        if live.get(&key).is_some_and(|f| Arc::ptr_eq(f, flight)) && !flight.runaway.swap(true, Ordering::Relaxed) {
            self.runaway.fetch_add(1, Ordering::Relaxed);
            self.timeouts.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Sass compilation, bounded. grass offers neither cancellation nor a resource budget, so each
/// compile runs on its own thread with a deadline: everything it may read is capped before it
/// starts, and a thread that overruns is abandoned rather than waited on.
pub struct Sass(Arc<Flights>);

impl Sass {
    pub fn new(timeout: Duration) -> Sass {
        Sass::with_threads(timeout, MAX_THREADS)
    }

    fn with_threads(timeout: Duration, threads: usize) -> Sass {
        Sass(Arc::new(Flights {
            timeout,
            threads,
            live: Mutex::new(HashMap::new()),
            runaway: AtomicUsize::new(0),
            timeouts: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        }))
    }

    pub fn stats(&self) -> SassStats {
        SassStats {
            running: self.0.live.lock().unwrap().len(),
            runaway: self.0.runaway.load(Ordering::Relaxed),
            timeouts: self.0.timeouts.load(Ordering::Relaxed),
            refused: self.0.refused.load(Ordering::Relaxed),
        }
    }

    pub fn compile(&self, tenant: &Tenant, dir: &str, source: &str, indented: bool) -> Result<String, String> {
        if source.len() > MAX_FILE {
            return Err(self.0.refuse(format!("Sass source is {} bytes, over the {MAX_FILE} byte limit", source.len())));
        }
        let depth = super::nesting_depth(source.as_bytes());
        if depth > super::MAX_NESTING_DEPTH {
            let limit = super::MAX_NESTING_DEPTH;
            return Err(self.0.refuse(format!("Sass source nests {depth} deep, over the {limit} level limit")));
        }
        let mut h = Vec::with_capacity(source.len() + dir.len() + 2);
        h.push(u8::from(indented));
        h.extend_from_slice(dir.as_bytes());
        h.push(0);
        h.extend_from_slice(source.as_bytes());
        let key = xxh3_128(&h);

        let (flight, start) = {
            let mut live = self.0.live.lock().unwrap();
            match live.get(&key).cloned() {
                Some(f) if f.runaway.load(Ordering::Relaxed) => {
                    let ms = self.0.timeout.as_millis();
                    return Err(self.0.refuse(format!(
                        "an identical Sass compilation has been running for more than {ms} ms and cannot be interrupted; not starting another"
                    )));
                }
                Some(f) => (f, false),
                None if live.len() >= self.0.threads => {
                    let n = self.0.threads;
                    return Err(self.0.refuse(format!("{n} Sass compilations are already running; not starting another")));
                }
                None => {
                    let f = Arc::new(Flight {
                        deadline: Instant::now() + self.0.timeout,
                        done: Mutex::new(None),
                        ready: Condvar::new(),
                        runaway: AtomicBool::new(false),
                    });
                    live.insert(key, f.clone());
                    (f, true)
                }
            }
        };

        if start {
            let fs = Snapshot::of(tenant, source.len());
            let (inner, f, dir, source) = (self.0.clone(), flight.clone(), dir.to_string(), source.to_string());
            let spawned = std::thread::Builder::new().name("sass".into()).stack_size(STACK).spawn(move || {
                let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.run(&fs, &dir, source, indented)))
                    .unwrap_or_else(|_| inner.failed(&fs, "the Sass compiler panicked".to_string()));
                inner.finish(key, &f, out);
            });
            if let Err(e) = spawned {
                self.0.finish(key, &flight, Err(format!("cannot start a Sass compile thread: {e}")));
            }
        }

        let mut done = flight.done.lock().unwrap();
        loop {
            if let Some(out) = done.as_ref() {
                return (**out).clone();
            }
            let Some(left) = flight.deadline.checked_duration_since(Instant::now()) else { break };
            done = flight.ready.wait_timeout(done, left).unwrap().0;
        }
        drop(done);
        self.0.overrun(key, &flight);
        Err(format!(
            "Sass compilation did not finish within {} ms; it cannot be interrupted, so the thread was left to run out",
            self.0.timeout.as_millis()
        ))
    }
}

pub fn is_sass_path(path: &str) -> bool {
    path.ends_with(".scss") || path.ends_with(".sass")
}

pub fn uses_sass(source: &str) -> bool {
    source.match_indices("lang=").any(|(i, _)| {
        let value = source[i + 5..].trim_start_matches(['"', '\'']);
        value.starts_with("scss") || value.starts_with("sass")
    })
}

/// Changes whenever any Sass file in the tenant changes, so cached output that `@use`d one is invalidated.
pub fn fingerprint(tenant: &Tenant) -> u64 {
    let mut buf = Vec::new();
    for e in tenant.list() {
        if is_sass_path(&e.path)
            && let Ok(Some(bytes)) = tenant.read(&e.path)
        {
            buf.extend_from_slice(e.path.as_bytes());
            buf.push(0);
            buf.extend_from_slice(&xxh3_64(&bytes).to_le_bytes());
        }
    }
    xxh3_64(&buf)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{MAX_FILE, MAX_OUTPUT, MAX_READ, Sass, uses_sass};
    use crate::store::{Base, Store, Tenant, UpdateKind};

    fn tenant(files: &[(&str, String)]) -> Arc<Tenant> {
        let store = Store::new(None, u64::MAX);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/starter");
        store.add_base(Base::load("starter", &root).unwrap());
        let t = store.create_tenant("t", "starter").unwrap();
        for (path, body) in files {
            t.write(path, body.clone().into_bytes(), UpdateKind::from_path(path)).unwrap();
        }
        t
    }

    #[test]
    fn detects_sass_style_blocks_with_or_without_quotes() {
        assert!(uses_sass("<style lang=\"scss\">"));
        assert!(uses_sass("<style lang='sass'>"));
        assert!(uses_sass("<style lang=scss>"));
        assert!(!uses_sass("<style lang=\"less\">"));
        assert!(!uses_sass("<style>"));
    }

    #[test]
    fn compiles_a_normal_stylesheet() {
        let t = tenant(&[("src/styles/_vars.scss", "$brand: #0af;\n".into())]);
        let sass = Sass::new(Duration::from_millis(super::DEFAULT_TIMEOUT_MS));
        let css = sass.compile(&t, "src/styles", "@use 'vars';\n.a { color: vars.$brand; b { color: red } }", false).unwrap();
        assert!(css.contains("#0af"), "{css}");
        assert!(css.contains(".a b"), "{css}");
        assert_eq!(sass.stats().running, 0);
    }

    #[test]
    fn refuses_an_oversized_source() {
        let t = tenant(&[]);
        let sass = Sass::new(Duration::from_millis(super::DEFAULT_TIMEOUT_MS));
        let source = format!("/*{}*/\n.a {{ color: red }}", "x".repeat(MAX_FILE));
        let e = sass.compile(&t, "src", &source, false).unwrap_err();
        assert!(e.contains(&format!("over the {MAX_FILE} byte limit")), "{e}");
        assert_eq!(sass.stats().refused, 1);
    }

    #[test]
    fn refuses_an_oversized_import() {
        let t = tenant(&[("src/styles/_big.scss", format!("/*{}*/\n$a: 1;\n", "x".repeat(MAX_FILE)))]);
        let sass = Sass::new(Duration::from_millis(super::DEFAULT_TIMEOUT_MS));
        let e = sass.compile(&t, "src/styles", "@use 'big';\n.a { color: red }", false).unwrap_err();
        assert!(e.contains("over the") && e.contains("byte Sass file limit"), "{e}");
    }

    /// Issue #25: `Engine::build` bounds the stylesheet it was asked for, but not the partials
    /// grass then goes and reads, and grass runs on its own thread rather than the one `build`
    /// reserved. Deep nesting has to be refused where the file is loaded.
    #[test]
    fn refuses_a_deeply_nested_import() {
        let t = tenant(&[("src/styles/_deep.scss", "a{".repeat(20_000))]);
        let sass = Sass::new(Duration::from_millis(super::DEFAULT_TIMEOUT_MS));
        let e = sass.compile(&t, "src/styles", "@use 'deep';\n.a { color: red }", false).unwrap_err();
        assert!(e.contains("level Sass nesting limit"), "{e}");
    }

    #[test]
    fn refuses_a_deeply_nested_source() {
        let t = tenant(&[]);
        let sass = Sass::new(Duration::from_millis(super::DEFAULT_TIMEOUT_MS));
        let e = sass.compile(&t, "src", &"a{".repeat(20_000), false).unwrap_err();
        assert!(e.contains("over the") && e.contains("level limit"), "{e}");
        assert_eq!(sass.stats().refused, 1);
    }

    #[test]
    fn refuses_imports_past_the_total_read_budget() {
        let body = format!("/*{}*/\n", "x".repeat(MAX_FILE - 16));
        let names = ["src/_a.scss", "src/_b.scss", "src/_c.scss", "src/_d.scss", "src/_e.scss"];
        let files: Vec<(&str, String)> = names.into_iter().map(|n| (n, body.clone())).collect();
        let t = tenant(&files);
        let sass = Sass::new(Duration::from_millis(super::DEFAULT_TIMEOUT_MS));
        let src = "@use 'a';\n@use 'b';\n@use 'c';\n@use 'd';\n@use 'e';\n.a { color: red }";
        let e = sass.compile(&t, "src", src, false).unwrap_err();
        assert!(e.contains(&format!("past the {MAX_READ} byte Sass import limit")), "{e}");
    }

    #[test]
    fn a_loop_bomb_hits_the_deadline_and_is_not_started_twice() {
        let t = tenant(&[]);
        let sass = Sass::new(Duration::from_millis(200));
        let bomb = "@for $i from 1 through 100000 { .a-#{$i} { color: red; background: url(x.png) } }";
        let started = Instant::now();
        let e = sass.compile(&t, "src/styles", bomb, false).unwrap_err();
        assert!(e.contains("did not finish within 200 ms"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        assert_eq!(sass.stats().timeouts, 1);
        assert_eq!(sass.stats().runaway, 1);

        let again = Instant::now();
        let e = sass.compile(&t, "src/styles", bomb, false).unwrap_err();
        assert!(e.contains("not starting another"), "{e}");
        assert!(again.elapsed() < Duration::from_millis(100), "{:?}", again.elapsed());
        assert_eq!(sass.stats().runaway, 1);
    }

    #[test]
    fn caps_the_number_of_compile_threads() {
        let t = tenant(&[]);
        let sass = Sass::with_threads(Duration::from_millis(150), 2);
        let bomb = |n: u32| format!("@for $i from 1 through {n} {{ .a-#{{$i}} {{ color: red }} }}");
        std::thread::scope(|scope| {
            for n in [100_000, 100_001] {
                let (sass, t) = (&sass, &t);
                scope.spawn(move || assert!(sass.compile(t, "src", &bomb(n), false).is_err()));
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while sass.stats().running < 2 && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let e = sass.compile(&t, "src", &bomb(100_002), false).unwrap_err();
            assert!(e.contains("2 Sass compilations are already running"), "{e}");
        });
    }

    #[test]
    fn refuses_output_over_the_cap() {
        let t = tenant(&[]);
        let sass = Sass::new(Duration::from_secs(120));
        let bomb = format!("@for $i from 1 through 8 {{ .a-#{{$i}} {{ background: url({}) }} }}", "x".repeat(600_000));
        let e = sass.compile(&t, "src/styles", &bomb, false).unwrap_err();
        assert!(e.contains(&format!("over the {MAX_OUTPUT} byte limit")), "{e}");
    }
}
