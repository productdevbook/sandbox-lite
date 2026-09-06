use std::array;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::transform::Kind;

pub const KINDS: [&str; 5] = ["module", "style", "script", "raw", "url"];
pub const STATUSES: [&str; 4] = ["200", "400", "404", "500"];
pub const BUCKETS: [f64; 8] = [0.001, 0.005, 0.02, 0.05, 0.1, 0.5, 1.0, 5.0];
const SLOTS: usize = BUCKETS.len() + 1;

fn kind_index(kind: Kind) -> usize {
    match kind {
        Kind::Module => 0,
        Kind::Style(_) => 1,
        Kind::Script(_) => 2,
        Kind::Raw => 3,
        Kind::Url => 4,
    }
}

fn status_index(status: u16) -> usize {
    match status {
        200 => 0,
        400 => 1,
        404 => 2,
        _ => 3,
    }
}

/// Prometheus buckets are upper-inclusive; anything past the last bound, NaN included, lands in `+Inf`.
pub fn bucket_index(seconds: f64) -> usize {
    BUCKETS.iter().position(|le| seconds <= *le).unwrap_or(BUCKETS.len())
}

#[derive(Default)]
struct Histogram {
    slots: [AtomicU64; SLOTS],
    micros: AtomicU64,
}

impl Histogram {
    fn observe(&self, elapsed: Duration) {
        self.slots[bucket_index(elapsed.as_secs_f64())].fetch_add(1, Ordering::Relaxed);
        self.micros.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
    }

    fn snapshot(&self) -> HistogramSnapshot {
        let mut running = 0;
        let cumulative = array::from_fn(|i| {
            running += self.slots[i].load(Ordering::Relaxed);
            running
        });
        HistogramSnapshot { cumulative, micros: self.micros.load(Ordering::Relaxed) }
    }
}

#[derive(Clone, Copy, Default)]
pub struct HistogramSnapshot {
    cumulative: [u64; SLOTS],
    micros: u64,
}

impl HistogramSnapshot {
    /// Derived from the last bucket rather than counted separately, so `_count` can never disagree with `+Inf`.
    pub fn count(&self) -> u64 {
        self.cumulative[SLOTS - 1]
    }

    pub fn seconds(&self) -> f64 {
        self.micros as f64 / 1e6
    }
}

#[derive(Default)]
pub struct Metrics {
    requests: [[AtomicU64; STATUSES.len()]; KINDS.len()],
    compile: [Histogram; KINDS.len()],
}

impl Metrics {
    pub fn module_request(&self, kind: Kind, status: u16) {
        self.requests[kind_index(kind)][status_index(status)].fetch_add(1, Ordering::Relaxed);
    }

    pub fn compiled(&self, kind: Kind, elapsed: Duration) {
        self.compile[kind_index(kind)].observe(elapsed);
    }

    pub fn requests(&self) -> [[u64; STATUSES.len()]; KINDS.len()] {
        array::from_fn(|k| array::from_fn(|s| self.requests[k][s].load(Ordering::Relaxed)))
    }

    pub fn compiles(&self) -> [HistogramSnapshot; KINDS.len()] {
        array::from_fn(|k| self.compile[k].snapshot())
    }
}

pub struct BaseSize {
    pub name: String,
    pub files: u64,
    pub bytes: u64,
}

pub struct Snapshot {
    pub uptime_seconds: u64,
    pub rss_bytes: Option<u64>,
    pub tenants: u64,
    pub overlay_bytes: u64,
    pub bases: Vec<BaseSize>,
    pub cache_entries: u64,
    pub cache_bytes: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub sse_subscribers: u64,
    pub sass_running: u64,
    pub sass_runaway: u64,
    pub sass_timeouts: u64,
    pub sass_refused: u64,
    pub requests: [[u64; STATUSES.len()]; KINDS.len()],
    pub compile: [HistogramSnapshot; KINDS.len()],
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn scalar(out: &mut String, name: &str, kind: &str, help: &str, value: u64) {
    family(out, name, kind, help);
    let _ = writeln!(out, "{name} {value}");
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

pub fn render(s: &Snapshot) -> String {
    let mut out = String::with_capacity(4096);
    scalar(&mut out, "sandbox_lite_uptime_seconds", "gauge", "Seconds since the daemon started.", s.uptime_seconds);
    if let Some(rss) = s.rss_bytes {
        scalar(&mut out, "sandbox_lite_rss_bytes", "gauge", "Resident set size of the daemon process.", rss);
    }
    scalar(&mut out, "sandbox_lite_tenants", "gauge", "Tenants currently loaded.", s.tenants);
    scalar(&mut out, "sandbox_lite_overlay_bytes", "gauge", "Bytes of tenant-edited files held across every tenant.", s.overlay_bytes);

    family(&mut out, "sandbox_lite_bases", "gauge", "Base projects loaded, one series per base.");
    for b in &s.bases {
        let _ = writeln!(out, "sandbox_lite_bases{{base=\"{}\"}} 1", escape(&b.name));
    }
    family(&mut out, "sandbox_lite_base_files", "gauge", "Files in each loaded base project.");
    for b in &s.bases {
        let _ = writeln!(out, "sandbox_lite_base_files{{base=\"{}\"}} {}", escape(&b.name), b.files);
    }
    family(&mut out, "sandbox_lite_base_bytes", "gauge", "Bytes in each loaded base project.");
    for b in &s.bases {
        let _ = writeln!(out, "sandbox_lite_base_bytes{{base=\"{}\"}} {}", escape(&b.name), b.bytes);
    }

    scalar(&mut out, "sandbox_lite_cache_entries", "gauge", "Entries in the transform cache.", s.cache_entries);
    scalar(&mut out, "sandbox_lite_cache_bytes", "gauge", "Bytes retained by the transform cache.", s.cache_bytes);
    scalar(&mut out, "sandbox_lite_cache_hits_total", "counter", "Transform cache lookups that found an entry.", s.cache_hits);
    scalar(&mut out, "sandbox_lite_cache_misses_total", "counter", "Transform cache lookups that had to compile.", s.cache_misses);
    scalar(&mut out, "sandbox_lite_sse_subscribers", "gauge", "Open live-reload event streams across every tenant.", s.sse_subscribers);

    scalar(&mut out, "sandbox_lite_sass_running", "gauge", "Sass compilations in flight.", s.sass_running);
    scalar(
        &mut out,
        "sandbox_lite_sass_runaway",
        "gauge",
        "Sass threads still running past their deadline; grass cannot be interrupted, so these are abandoned rather than killed.",
        s.sass_runaway,
    );
    scalar(&mut out, "sandbox_lite_sass_timeouts_total", "counter", "Sass compilations that overran their deadline.", s.sass_timeouts);
    scalar(
        &mut out,
        "sandbox_lite_sass_refused_total",
        "counter",
        "Sass compilations refused by a size cap, the thread cap, or a runaway thread holding the same source.",
        s.sass_refused,
    );

    family(&mut out, "sandbox_lite_module_requests_total", "counter", "Requests answered by the preview module endpoint.");
    for (k, kind) in KINDS.iter().enumerate() {
        for (i, status) in STATUSES.iter().enumerate() {
            let _ = writeln!(out, "sandbox_lite_module_requests_total{{kind=\"{kind}\",status=\"{status}\"}} {}", s.requests[k][i]);
        }
    }

    family(
        &mut out,
        "sandbox_lite_compile_seconds",
        "histogram",
        "Seconds spent compiling one file after a transform-cache miss; a style or script compile includes any module compile it triggers.",
    );
    for (k, kind) in KINDS.iter().enumerate() {
        let h = &s.compile[k];
        for (i, le) in BUCKETS.iter().enumerate() {
            let _ = writeln!(out, "sandbox_lite_compile_seconds_bucket{{kind=\"{kind}\",le=\"{le}\"}} {}", h.cumulative[i]);
        }
        let _ = writeln!(out, "sandbox_lite_compile_seconds_bucket{{kind=\"{kind}\",le=\"+Inf\"}} {}", h.count());
        let _ = writeln!(out, "sandbox_lite_compile_seconds_sum{{kind=\"{kind}\"}} {}.{:06}", h.micros / 1_000_000, h.micros % 1_000_000);
        let _ = writeln!(out, "sandbox_lite_compile_seconds_count{{kind=\"{kind}\"}} {}", h.count());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_upper_inclusive() {
        assert_eq!(bucket_index(0.0), 0);
        assert_eq!(bucket_index(0.001), 0);
        assert_eq!(bucket_index(0.0010001), 1);
        assert_eq!(bucket_index(0.005), 1);
        assert_eq!(bucket_index(0.02), 2);
        assert_eq!(bucket_index(0.05), 3);
        assert_eq!(bucket_index(0.1), 4);
        assert_eq!(bucket_index(0.5), 5);
        assert_eq!(bucket_index(1.0), 6);
        assert_eq!(bucket_index(5.0), 7);
        assert_eq!(bucket_index(5.000001), 8);
        assert_eq!(bucket_index(f64::INFINITY), 8);
        assert_eq!(bucket_index(f64::NAN), 8);
    }

    #[test]
    fn observations_accumulate_into_every_bucket_above_them() {
        let m = Metrics::default();
        m.compiled(Kind::Style(0), Duration::from_micros(1000));
        m.compiled(Kind::Style(3), Duration::from_micros(1001));
        let h = m.compiles()[1];
        assert_eq!(h.cumulative, [1, 2, 2, 2, 2, 2, 2, 2, 2]);
        assert_eq!(h.count(), 2);
        assert_eq!(h.micros, 2001);
        assert_eq!(m.compiles()[0].count(), 0);
    }

    #[test]
    fn renders_the_prometheus_text_format() {
        let m = Metrics::default();
        m.module_request(Kind::Module, 200);
        m.module_request(Kind::Module, 200);
        m.module_request(Kind::Module, 200);
        m.module_request(Kind::Module, 404);
        m.module_request(Kind::Url, 503);
        m.module_request(Kind::Url, 503);
        m.compiled(Kind::Module, Duration::from_micros(500));
        m.compiled(Kind::Module, Duration::from_micros(30_000));
        let snapshot = Snapshot {
            uptime_seconds: 42,
            rss_bytes: Some(101_384_192),
            tenants: 2,
            overlay_bytes: 4096,
            bases: vec![
                BaseSize { name: "starter".into(), files: 12, bytes: 34567 },
                BaseSize { name: "he\"llo".into(), files: 1, bytes: 2 },
            ],
            cache_entries: 7,
            cache_bytes: 8192,
            cache_hits: 10,
            cache_misses: 3,
            sse_subscribers: 1,
            sass_running: 1,
            sass_runaway: 2,
            sass_timeouts: 3,
            sass_refused: 4,
            requests: m.requests(),
            compile: m.compiles(),
        };
        let expected = r#"# HELP sandbox_lite_uptime_seconds Seconds since the daemon started.
# TYPE sandbox_lite_uptime_seconds gauge
sandbox_lite_uptime_seconds 42
# HELP sandbox_lite_rss_bytes Resident set size of the daemon process.
# TYPE sandbox_lite_rss_bytes gauge
sandbox_lite_rss_bytes 101384192
# HELP sandbox_lite_tenants Tenants currently loaded.
# TYPE sandbox_lite_tenants gauge
sandbox_lite_tenants 2
# HELP sandbox_lite_overlay_bytes Bytes of tenant-edited files held across every tenant.
# TYPE sandbox_lite_overlay_bytes gauge
sandbox_lite_overlay_bytes 4096
# HELP sandbox_lite_bases Base projects loaded, one series per base.
# TYPE sandbox_lite_bases gauge
sandbox_lite_bases{base="starter"} 1
sandbox_lite_bases{base="he\"llo"} 1
# HELP sandbox_lite_base_files Files in each loaded base project.
# TYPE sandbox_lite_base_files gauge
sandbox_lite_base_files{base="starter"} 12
sandbox_lite_base_files{base="he\"llo"} 1
# HELP sandbox_lite_base_bytes Bytes in each loaded base project.
# TYPE sandbox_lite_base_bytes gauge
sandbox_lite_base_bytes{base="starter"} 34567
sandbox_lite_base_bytes{base="he\"llo"} 2
# HELP sandbox_lite_cache_entries Entries in the transform cache.
# TYPE sandbox_lite_cache_entries gauge
sandbox_lite_cache_entries 7
# HELP sandbox_lite_cache_bytes Bytes retained by the transform cache.
# TYPE sandbox_lite_cache_bytes gauge
sandbox_lite_cache_bytes 8192
# HELP sandbox_lite_cache_hits_total Transform cache lookups that found an entry.
# TYPE sandbox_lite_cache_hits_total counter
sandbox_lite_cache_hits_total 10
# HELP sandbox_lite_cache_misses_total Transform cache lookups that had to compile.
# TYPE sandbox_lite_cache_misses_total counter
sandbox_lite_cache_misses_total 3
# HELP sandbox_lite_sse_subscribers Open live-reload event streams across every tenant.
# TYPE sandbox_lite_sse_subscribers gauge
sandbox_lite_sse_subscribers 1
# HELP sandbox_lite_sass_running Sass compilations in flight.
# TYPE sandbox_lite_sass_running gauge
sandbox_lite_sass_running 1
# HELP sandbox_lite_sass_runaway Sass threads still running past their deadline; grass cannot be interrupted, so these are abandoned rather than killed.
# TYPE sandbox_lite_sass_runaway gauge
sandbox_lite_sass_runaway 2
# HELP sandbox_lite_sass_timeouts_total Sass compilations that overran their deadline.
# TYPE sandbox_lite_sass_timeouts_total counter
sandbox_lite_sass_timeouts_total 3
# HELP sandbox_lite_sass_refused_total Sass compilations refused by a size cap, the thread cap, or a runaway thread holding the same source.
# TYPE sandbox_lite_sass_refused_total counter
sandbox_lite_sass_refused_total 4
# HELP sandbox_lite_module_requests_total Requests answered by the preview module endpoint.
# TYPE sandbox_lite_module_requests_total counter
sandbox_lite_module_requests_total{kind="module",status="200"} 3
sandbox_lite_module_requests_total{kind="module",status="400"} 0
sandbox_lite_module_requests_total{kind="module",status="404"} 1
sandbox_lite_module_requests_total{kind="module",status="500"} 0
sandbox_lite_module_requests_total{kind="style",status="200"} 0
sandbox_lite_module_requests_total{kind="style",status="400"} 0
sandbox_lite_module_requests_total{kind="style",status="404"} 0
sandbox_lite_module_requests_total{kind="style",status="500"} 0
sandbox_lite_module_requests_total{kind="script",status="200"} 0
sandbox_lite_module_requests_total{kind="script",status="400"} 0
sandbox_lite_module_requests_total{kind="script",status="404"} 0
sandbox_lite_module_requests_total{kind="script",status="500"} 0
sandbox_lite_module_requests_total{kind="raw",status="200"} 0
sandbox_lite_module_requests_total{kind="raw",status="400"} 0
sandbox_lite_module_requests_total{kind="raw",status="404"} 0
sandbox_lite_module_requests_total{kind="raw",status="500"} 0
sandbox_lite_module_requests_total{kind="url",status="200"} 0
sandbox_lite_module_requests_total{kind="url",status="400"} 0
sandbox_lite_module_requests_total{kind="url",status="404"} 0
sandbox_lite_module_requests_total{kind="url",status="500"} 2
# HELP sandbox_lite_compile_seconds Seconds spent compiling one file after a transform-cache miss; a style or script compile includes any module compile it triggers.
# TYPE sandbox_lite_compile_seconds histogram
sandbox_lite_compile_seconds_bucket{kind="module",le="0.001"} 1
sandbox_lite_compile_seconds_bucket{kind="module",le="0.005"} 1
sandbox_lite_compile_seconds_bucket{kind="module",le="0.02"} 1
sandbox_lite_compile_seconds_bucket{kind="module",le="0.05"} 2
sandbox_lite_compile_seconds_bucket{kind="module",le="0.1"} 2
sandbox_lite_compile_seconds_bucket{kind="module",le="0.5"} 2
sandbox_lite_compile_seconds_bucket{kind="module",le="1"} 2
sandbox_lite_compile_seconds_bucket{kind="module",le="5"} 2
sandbox_lite_compile_seconds_bucket{kind="module",le="+Inf"} 2
sandbox_lite_compile_seconds_sum{kind="module"} 0.030500
sandbox_lite_compile_seconds_count{kind="module"} 2
sandbox_lite_compile_seconds_bucket{kind="style",le="0.001"} 0
sandbox_lite_compile_seconds_bucket{kind="style",le="0.005"} 0
sandbox_lite_compile_seconds_bucket{kind="style",le="0.02"} 0
sandbox_lite_compile_seconds_bucket{kind="style",le="0.05"} 0
sandbox_lite_compile_seconds_bucket{kind="style",le="0.1"} 0
sandbox_lite_compile_seconds_bucket{kind="style",le="0.5"} 0
sandbox_lite_compile_seconds_bucket{kind="style",le="1"} 0
sandbox_lite_compile_seconds_bucket{kind="style",le="5"} 0
sandbox_lite_compile_seconds_bucket{kind="style",le="+Inf"} 0
sandbox_lite_compile_seconds_sum{kind="style"} 0.000000
sandbox_lite_compile_seconds_count{kind="style"} 0
sandbox_lite_compile_seconds_bucket{kind="script",le="0.001"} 0
sandbox_lite_compile_seconds_bucket{kind="script",le="0.005"} 0
sandbox_lite_compile_seconds_bucket{kind="script",le="0.02"} 0
sandbox_lite_compile_seconds_bucket{kind="script",le="0.05"} 0
sandbox_lite_compile_seconds_bucket{kind="script",le="0.1"} 0
sandbox_lite_compile_seconds_bucket{kind="script",le="0.5"} 0
sandbox_lite_compile_seconds_bucket{kind="script",le="1"} 0
sandbox_lite_compile_seconds_bucket{kind="script",le="5"} 0
sandbox_lite_compile_seconds_bucket{kind="script",le="+Inf"} 0
sandbox_lite_compile_seconds_sum{kind="script"} 0.000000
sandbox_lite_compile_seconds_count{kind="script"} 0
sandbox_lite_compile_seconds_bucket{kind="raw",le="0.001"} 0
sandbox_lite_compile_seconds_bucket{kind="raw",le="0.005"} 0
sandbox_lite_compile_seconds_bucket{kind="raw",le="0.02"} 0
sandbox_lite_compile_seconds_bucket{kind="raw",le="0.05"} 0
sandbox_lite_compile_seconds_bucket{kind="raw",le="0.1"} 0
sandbox_lite_compile_seconds_bucket{kind="raw",le="0.5"} 0
sandbox_lite_compile_seconds_bucket{kind="raw",le="1"} 0
sandbox_lite_compile_seconds_bucket{kind="raw",le="5"} 0
sandbox_lite_compile_seconds_bucket{kind="raw",le="+Inf"} 0
sandbox_lite_compile_seconds_sum{kind="raw"} 0.000000
sandbox_lite_compile_seconds_count{kind="raw"} 0
sandbox_lite_compile_seconds_bucket{kind="url",le="0.001"} 0
sandbox_lite_compile_seconds_bucket{kind="url",le="0.005"} 0
sandbox_lite_compile_seconds_bucket{kind="url",le="0.02"} 0
sandbox_lite_compile_seconds_bucket{kind="url",le="0.05"} 0
sandbox_lite_compile_seconds_bucket{kind="url",le="0.1"} 0
sandbox_lite_compile_seconds_bucket{kind="url",le="0.5"} 0
sandbox_lite_compile_seconds_bucket{kind="url",le="1"} 0
sandbox_lite_compile_seconds_bucket{kind="url",le="5"} 0
sandbox_lite_compile_seconds_bucket{kind="url",le="+Inf"} 0
sandbox_lite_compile_seconds_sum{kind="url"} 0.000000
sandbox_lite_compile_seconds_count{kind="url"} 0
"#;
        assert_eq!(render(&snapshot), expected);
    }

    #[test]
    fn rss_family_is_absent_when_proc_cannot_be_read() {
        let m = Metrics::default();
        let snapshot = Snapshot {
            uptime_seconds: 0,
            rss_bytes: None,
            tenants: 0,
            overlay_bytes: 0,
            bases: vec![],
            cache_entries: 0,
            cache_bytes: 0,
            cache_hits: 0,
            cache_misses: 0,
            sse_subscribers: 0,
            sass_running: 0,
            sass_runaway: 0,
            sass_timeouts: 0,
            sass_refused: 0,
            requests: m.requests(),
            compile: m.compiles(),
        };
        assert!(!render(&snapshot).contains("sandbox_lite_rss_bytes"));
    }
}
