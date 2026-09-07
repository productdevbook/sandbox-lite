use std::convert::Infallible;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State as AxState};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use super::{AppState, State, mime};
use crate::metrics::{BaseSize, KINDS, STATUSES, Snapshot, render};
use crate::store::{Base, NoTenant, Tenant, WriteError, clean_path, valid_id};
use crate::transform::{Engine, Kind, is_source};

pub async fn editor() -> Html<&'static str> {
    Html(include_str!("../../assets/editor.html"))
}

pub async fn health() -> &'static str {
    "ok\n"
}

pub fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// A tenant whose data directory is there and would not load answers 500 with the reason, not 404:
/// *never existed* and *could not be loaded* are different facts (issue #61).
#[allow(clippy::result_large_err)]
pub fn tenant_or_404(st: &AppState, id: &str) -> Result<Arc<Tenant>, Response> {
    st.store.resolve(id).map_err(|e| no_tenant(id, e))
}

pub fn no_tenant(id: &str, e: NoTenant) -> Response {
    match e {
        NoTenant::Unknown => err(StatusCode::NOT_FOUND, format!("unknown tenant '{id}'")),
        NoTenant::Failed(why) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("tenant '{id}' exists but could not be loaded: {why}")),
    }
}

pub fn rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

pub fn preview_url(st: &AppState, id: &str) -> String {
    preview_url_path(st, id, "/")
}

pub fn preview_url_path(st: &AppState, id: &str, path: &str) -> String {
    let slash = if path.starts_with('/') { "" } else { "/" };
    let url = format!("http://{id}.{}:{}{slash}{path}", st.domain, st.port);
    match &st.preview_secret {
        Some(secret) => {
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}sl_token={}", super::preview_token(secret, id))
        }
        None => url,
    }
}

/// One row per `Kind`: the requests the module endpoint answered and the compiles they cost.
fn module_stats(st: &AppState) -> Vec<Value> {
    let requests = st.metrics.requests();
    let compile = st.metrics.compiles();
    KINDS
        .iter()
        .enumerate()
        .map(|(k, kind)| {
            let by_status: Map<String, Value> =
                STATUSES.iter().enumerate().map(|(i, status)| ((*status).to_string(), json!(requests[k][i]))).collect();
            json!({ "kind": kind, "requests": by_status, "compiles": compile[k].count(), "compile_seconds": compile[k].seconds() })
        })
        .collect()
}

/// Conversations and the bytes they hold across every tenant. They are outside the tenant quota,
/// so `overlay_bytes` does not see them and this is the only place they are counted. `Chats` keeps
/// the numbers as it writes and evicts, so this is memory rather than a directory walk per tenant:
/// the editor polls `/api/stats` every five seconds, and #60 is what that cost at 1000 tenants.
fn chat_totals(st: &AppState) -> std::io::Result<(u64, u64)> {
    let (mut conversations, mut bytes) = (0u64, 0u64);
    for t in st.store.tenants() {
        let (n, b) = st.chats.usage(&t)?;
        conversations += n as u64;
        bytes += b;
    }
    Ok((conversations, bytes))
}

/// The first poll after a restart walks every tenant's `chats/` directory to seed the counters, so
/// even the endpoint that only reports numbers goes to the blocking pool (#94).
async fn chat_totals_off_the_workers(st: &State) -> std::io::Result<(u64, u64)> {
    let st = st.clone();
    match tokio::task::spawn_blocking(move || chat_totals(&st)).await {
        Ok(totals) => totals,
        Err(e) => Err(std::io::Error::other(e.to_string())),
    }
}

pub async fn stats(AxState(st): AxState<State>) -> Response {
    let tenants = st.store.tenants();
    let overlay_bytes: u64 = tenants.iter().map(|t| t.overlay_stats().1).sum();
    let subscribers: usize = tenants.iter().map(|t| t.events.receiver_count()).sum();
    // A gauge that reads zero because a directory could not be listed is a wrong number, not a
    // missing one, and nothing downstream can tell the two apart.
    let (conversations, chat_bytes) = match chat_totals_off_the_workers(&st).await {
        Ok(totals) => totals,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("chats: {e}")),
    };
    let chats = json!({
        "conversations": conversations,
        "bytes": chat_bytes,
        "max_per_tenant": st.chats.cap(),
        // what the count was always taken to mean: conversations are outside the tenant quota, so
        // a number of files of no particular size said nothing about what the tenant holds (#89)
        "max_bytes_per_tenant": st.chats.tenant_quota(),
        "window_turns": st.chats.window(),
    });
    let failed = st.store.failed_tenants();
    Json(json!({
        "rss_kb": rss_kb(),
        "uptime_s": st.started.elapsed().as_secs(),
        "tenants": tenants.len(),
        // A tenant that would not load is a site answering nothing while the daemon reports itself
        // healthy, so the count and the ids are part of the answer rather than a line in the log.
        "tenants_failed": failed.len(),
        "failed_tenants": failed.iter().map(|(id, e)| json!({ "id": id, "error": e })).collect::<Vec<_>>(),
        "overlay_bytes": overlay_bytes,
        "sse_subscribers": subscribers,
        "bases": st.store.bases().iter().map(|b| json!({ "name": b.name, "files": b.file_count(), "bytes": b.bytes() })).collect::<Vec<_>>(),
        "cache": st.engine.stats(),
        "sass": st.engine.sass_stats(),
        "compiles": st.engine.compile_stats(),
        "chats": chats,
        "screenshots": st.shots.stats(),
        "modules": module_stats(&st),
        "ai": st.api_key.is_some(),
        "preview_auth": st.preview_secret.is_some(),
        "model": st.model,
        "domain": st.domain,
        "port": st.port,
        "astro": include_str!("../../assets/astro.version").trim(),
    }))
    .into_response()
}

fn snapshot(st: &AppState, chats: (u64, u64)) -> Snapshot {
    let tenants = st.store.tenants();
    let cache = st.engine.stats();
    let sass = st.engine.sass_stats();
    let compiles = st.engine.compile_stats();
    let shots = st.shots.stats();
    Snapshot {
        uptime_seconds: st.started.elapsed().as_secs(),
        rss_bytes: rss_kb().map(|kb| kb * 1024),
        tenants: tenants.len() as u64,
        tenants_failed: st.store.failed_tenants().len() as u64,
        overlay_bytes: tenants.iter().map(|t| t.overlay_stats().1).sum(),
        bases: st.store.bases().iter().map(|b| BaseSize { name: b.name.clone(), files: b.file_count() as u64, bytes: b.bytes() }).collect(),
        cache_entries: cache.entries as u64,
        cache_bytes: cache.bytes as u64,
        cache_hits: cache.hits,
        cache_misses: cache.misses,
        cache_coalesced: cache.coalesced,
        chat_conversations: chats.0,
        chat_bytes: chats.1,
        sse_subscribers: tenants.iter().map(|t| t.events.receiver_count() as u64).sum(),
        sass_running: sass.running as u64,
        sass_runaway: sass.runaway as u64,
        sass_timeouts: sass.timeouts,
        sass_refused: sass.refused,
        compiles_running: compiles.running as u64,
        compiles_queued: compiles.queued as u64,
        compiles_limit: compiles.limit as u64,
        compiles_refused: compiles.refused,
        compiles_runaway: compiles.runaway as u64,
        compiles_timeouts: compiles.timeouts,
        shots_running: shots.running as u64,
        shots_busy: shots.busy,
        shots_timeouts: shots.timeouts,
        requests: st.metrics.requests(),
        compile: st.metrics.compiles(),
    }
}

pub async fn metrics(AxState(st): AxState<State>) -> Response {
    // As in `stats`: a chats gauge that reads zero because the store could not be read is a wrong
    // number wearing the shape of a right one, so the scrape fails instead.
    let chats = match chat_totals_off_the_workers(&st).await {
        Ok(totals) => totals,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("chats: {e}")),
    };
    let body = render(&snapshot(&st, chats));
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

fn base_json(b: &Base) -> Value {
    json!({ "name": b.name, "files": b.file_count(), "bytes": b.bytes(), "root": b.root })
}

pub async fn bases(AxState(st): AxState<State>) -> Json<Value> {
    Json(Value::Array(st.store.bases().iter().map(|b| base_json(b)).collect()))
}

#[derive(Deserialize)]
pub struct AddBaseReq {
    name: String,
    path: String,
}

/// Adds a base project from a directory on the host. Operator-only: with `--bases` set the path
/// must resolve inside it, and without it any readable directory will do — see SECURITY.md.
pub async fn add_base(AxState(st): AxState<State>, Json(req): Json<AddBaseReq>) -> Response {
    if !valid_id(&req.name) {
        return err(StatusCode::BAD_REQUEST, "base name must be lowercase letters, digits and dashes");
    }
    if st.store.base(&req.name).is_some() {
        return err(StatusCode::CONFLICT, format!("base '{}' already exists; reload it instead", req.name));
    }
    let root = match st.store.base_root(std::path::Path::new(&req.path)) {
        Ok(root) => root,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let name = req.name.clone();
    let loaded = tokio::task::spawn_blocking(move || Base::load(&name, &root)).await;
    match loaded {
        Ok(Ok(base)) => (StatusCode::CREATED, Json(base_json(&st.store.add_base(base)))).into_response(),
        Ok(Err(e)) => err(StatusCode::BAD_REQUEST, format!("cannot load base '{}': {e}", req.name)),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Re-reads one base and re-points every tenant on it, each of which gets an `update` event.
pub async fn reload_base(AxState(st): AxState<State>, Path(name): Path<String>) -> Response {
    let (st2, n) = (st.clone(), name.clone());
    match tokio::task::spawn_blocking(move || st2.store.reload_base(&n)).await {
        Ok(Ok(Some((base, tenants)))) => {
            let mut body = base_json(&base);
            body["tenants"] = json!(tenants);
            Json(body).into_response()
        }
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, format!("unknown base '{name}'")),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot reload base '{name}': {e}")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn tenant_json(st: &AppState, t: &Tenant) -> Value {
    let (files, bytes) = t.overlay_stats();
    json!({
        "id": t.id,
        "base": t.base().name,
        "version": t.version(),
        "overlay_files": files,
        "overlay_bytes": bytes,
        "max_files": t.max_files(),
        "quota_bytes": t.quota(),
        "preview": preview_url(st, &t.id),
        "preview_token": st.preview_secret.as_ref().map(|s| super::preview_token(s, &t.id)),
    })
}

pub async fn tenants(AxState(st): AxState<State>) -> Json<Value> {
    Json(Value::Array(st.store.tenants().iter().map(|t| tenant_json(&st, t)).collect()))
}

#[derive(Deserialize)]
pub struct CreateReq {
    id: String,
    base: String,
}

pub async fn create_tenant(AxState(st): AxState<State>, Json(req): Json<CreateReq>) -> Response {
    match st.store.create_tenant(&req.id, &req.base) {
        Ok(t) => (StatusCode::CREATED, Json(tenant_json(&st, &t))).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

pub async fn delete_tenant(AxState(st): AxState<State>, Path(id): Path<String>) -> Response {
    st.chats.forget(&id);
    match st.store.remove_tenant(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "unknown tenant"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("tenant '{id}' was dropped but its data directory was not: {e}")),
    }
}

pub async fn files(AxState(st): AxState<State>, Path(id): Path<String>) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let list: Vec<Value> = t.list().into_iter().map(|e| json!({ "path": e.path, "size": e.size, "modified": e.modified })).collect();
    Json(json!({ "version": t.version(), "files": list })).into_response()
}

pub async fn read_file(AxState(st): AxState<State>, Path((id, path)): Path<(String, String)>) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let Some(path) = clean_path(&path) else { return err(StatusCode::BAD_REQUEST, "bad path") };
    match t.read(&path) {
        Ok(Some(bytes)) => ([(header::CONTENT_TYPE, mime(&path)), (header::CACHE_CONTROL, "no-store")], bytes.to_vec()).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "no such file"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{path}: {e}")),
    }
}

pub async fn write_file(AxState(st): AxState<State>, Path((id, path)): Path<(String, String)>, body: Bytes) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let Some(path) = clean_path(&path) else { return err(StatusCode::BAD_REQUEST, "bad path") };
    // Classifying an `.astro` write compiles it, so the whole write goes to a blocking thread.
    let p2 = path.clone();
    let written = tokio::task::spawn_blocking(move || {
        let kind = st.engine.update_kind(&t, &p2, &body);
        t.write(&p2, body.to_vec(), kind)
    })
    .await;
    match written {
        Ok(Ok(version)) => Json(json!({ "path": path, "version": version })).into_response(),
        Ok(Err(e @ (WriteError::Quota { .. } | WriteError::Files { .. }))) => err(StatusCode::PAYLOAD_TOO_LARGE, e.to_string()),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn delete_file(AxState(st): AxState<State>, Path((id, path)): Path<(String, String)>) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let Some(path) = clean_path(&path) else { return err(StatusCode::BAD_REQUEST, "bad path") };
    if !t.exists(&path) {
        return err(StatusCode::NOT_FOUND, "no such file");
    }
    // A rename and a `deleted.json` rewrite under the overlay's write lock — small, but disk, and
    // `write_file` two functions up is already careful about exactly this.
    let p2 = path.clone();
    match tokio::task::spawn_blocking(move || t.delete(&p2)).await {
        Ok(Ok(version)) => Json(json!({ "path": path, "version": version })).into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub fn sse(t: &Tenant) -> Sse<impl Stream<Item = Result<Event, Infallible>> + use<>> {
    let hello = tokio_stream::once(Ok(Event::default().event("hello").data(t.version().to_string())));
    // A subscriber that fell behind the 64-slot channel has missed edits. Dropped, the last one it
    // missed is a reload that never happens and a preview that quietly stops following the tenant.
    // An update with no path is what a whole-tree change already looks like to both clients, and it
    // reloads rather than swapping CSS, which is the only safe thing to do about writes nobody saw.
    let missed = r#"{"type":"update","path":"","kind":"module","version":0}"#;
    let updates =
        BroadcastStream::new(t.events.subscribe()).map(|r| r.unwrap_or_else(|_| missed.to_string())).map(|s| Ok(Event::default().data(s)));
    Sse::new(hello.chain(updates)).keep_alive(KeepAlive::default())
}

pub async fn events(AxState(st): AxState<State>, Path(id): Path<String>) -> Response {
    match tenant_or_404(&st, &id) {
        Ok(t) => sse(&t).into_response(),
        Err(r) => r,
    }
}

fn diag(file: &str, text: String) -> crate::transform::Diag {
    crate::transform::Diag { severity: "error".into(), text, hint: String::new(), file: file.to_string(), line: 0, column: 0 }
}

/// Every source file of the tenant, compiled. Issue #54: the sweep takes one compile permit and
/// holds it for the whole run, so a saturated daemon costs this one wait rather than one per file —
/// the error page fetches `/__sl/check`, and it is the page that has to explain a failure.
pub fn check_tenant(engine: &Engine, t: &Arc<Tenant>) -> Value {
    let sources: Vec<String> = t.list().into_iter().map(|e| e.path).filter(|p| is_source(p)).collect();
    let files = sources.len();
    let swept = engine.sweep(|| {
        let mut diagnostics = Vec::new();
        for path in &sources {
            match engine.build(t, path, Kind::Module) {
                Ok(b) => diagnostics.extend(b.warnings.iter().cloned()),
                Err(be) if be.diagnostics.is_empty() => diagnostics.push(diag(path, be.message)),
                Err(be) => diagnostics.extend(be.diagnostics),
            }
        }
        diagnostics
    });
    // A sweep that never got a permit is a saturated daemon, not a project with nothing wrong: the
    // one refusal is the answer, and it is what the error overlay shows.
    let diagnostics = swept.unwrap_or_else(|be| vec![diag("", be.message)]);
    let errors = diagnostics.iter().filter(|d| d.severity == "error").count();
    json!({ "files": files, "errors": errors, "diagnostics": diagnostics })
}

pub async fn check(AxState(st): AxState<State>, Path(id): Path<String>) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let st2 = st.clone();
    match tokio::task::spawn_blocking(move || check_tenant(&st2.engine, &t)).await {
        Ok(out) => Json(out).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::check_tenant;
    use crate::metrics::Metrics;
    use crate::store::{Base, Store, Tenant, UpdateKind};
    use crate::transform::{Config, Engine};

    fn tenant(files: &[(&str, &str)]) -> Arc<Tenant> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let store = Store::new(None, u64::MAX);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/starter");
        store.add_base(Base::load("starter", &root).unwrap());
        let id = format!("acme{}", SEQ.fetch_add(1, Ordering::Relaxed));
        let t = store.create_tenant(&id, "starter").unwrap();
        for (path, body) in files {
            t.write(path, body.as_bytes().to_vec(), UpdateKind::from_path(path)).unwrap();
        }
        t
    }

    fn engine(limit: usize, wait: Duration) -> Engine {
        Engine::gated(Config { cache_bytes: 1 << 20, ..Config::default() }, Arc::new(Metrics::default()), limit, wait, 8)
    }

    #[test]
    fn check_names_the_file_that_does_not_compile() {
        let t = tenant(&[("src/pages/bad.astro", "---\nconst n = ;\n---\n")]);
        let engine = engine(2, Duration::from_millis(50));
        let out = check_tenant(&engine, &t);
        assert!(out["files"].as_u64().unwrap() > 1, "{out}");
        assert_eq!(out["errors"], 1, "{out}");
        let named = out["diagnostics"].as_array().unwrap().iter().any(|d| d["file"] == "src/pages/bad.astro" && d["severity"] == "error");
        assert!(named, "{out}");
        assert_eq!(engine.compile_stats().running, 0, "the sweep gave its permit back");
    }

    /// Issue #54: `check` took a permit per source file, so with every permit busy the page that
    /// exists to explain a failure waited the queue deadline once per file — the slowest thing on
    /// the site, exactly when the site is already broken. One wait now answers for the whole sweep.
    #[test]
    fn check_on_a_saturated_daemon_waits_once_not_once_per_file() {
        let t = tenant(&[]);
        let wait = Duration::from_millis(100);
        // a gate no compile can pass: every permit is busy, for as long as the test needs it
        let engine = engine(0, wait);
        let started = Instant::now();
        let out = check_tenant(&engine, &t);
        let files = out["files"].as_u64().unwrap();
        assert!(files >= 5, "the starter example has source files to sweep: {files}");
        assert!(started.elapsed() < wait * 3, "{files} files took {:?}", started.elapsed());
        assert_eq!(out["errors"], 1, "one refusal, not one per file: {out}");
        assert!(out["diagnostics"][0]["text"].as_str().unwrap().contains("waited 100 ms"), "{out}");
        assert_eq!(engine.compile_stats().refused, 1);
    }
}
