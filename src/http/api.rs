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
use crate::store::{Tenant, WriteError, clean_path};
use crate::transform::{Kind, is_source};

pub async fn editor() -> Html<&'static str> {
    Html(include_str!("../../assets/editor.html"))
}

pub async fn health() -> &'static str {
    "ok\n"
}

pub fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

#[allow(clippy::result_large_err)]
pub fn tenant_or_404(st: &AppState, id: &str) -> Result<Arc<Tenant>, Response> {
    st.store.tenant(id).ok_or_else(|| err(StatusCode::NOT_FOUND, format!("unknown tenant '{id}'")))
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
/// so `overlay_bytes` does not see them and this is the only place the chats directory is counted.
fn chat_stats(st: &AppState) -> Value {
    let (conversations, bytes) = st.store.tenants().iter().map(|t| st.chats.usage(t)).fold((0, 0), |(n, b), (cn, cb)| (n + cn, b + cb));
    json!({
        "conversations": conversations,
        "bytes": bytes,
        "max_per_tenant": st.chats.cap(),
        "window_turns": st.chats.window(),
    })
}

pub async fn stats(AxState(st): AxState<State>) -> Json<Value> {
    let tenants = st.store.tenants();
    let overlay_bytes: u64 = tenants.iter().map(|t| t.overlay_stats().1).sum();
    let subscribers: usize = tenants.iter().map(|t| t.events.receiver_count()).sum();
    Json(json!({
        "rss_kb": rss_kb(),
        "uptime_s": st.started.elapsed().as_secs(),
        "tenants": tenants.len(),
        "overlay_bytes": overlay_bytes,
        "sse_subscribers": subscribers,
        "bases": st.store.bases().iter().map(|b| json!({ "name": b.name, "files": b.file_count(), "bytes": b.bytes() })).collect::<Vec<_>>(),
        "cache": st.engine.stats(),
        "sass": st.engine.sass_stats(),
        "compiles": st.engine.compile_stats(),
        "chats": chat_stats(&st),
        "screenshots": st.shots.stats(),
        "modules": module_stats(&st),
        "ai": st.api_key.is_some(),
        "preview_auth": st.preview_secret.is_some(),
        "model": st.model,
        "domain": st.domain,
        "port": st.port,
        "astro": include_str!("../../assets/astro.version").trim(),
    }))
}

fn snapshot(st: &AppState) -> Snapshot {
    let tenants = st.store.tenants();
    let cache = st.engine.stats();
    let sass = st.engine.sass_stats();
    let compiles = st.engine.compile_stats();
    let shots = st.shots.stats();
    Snapshot {
        uptime_seconds: st.started.elapsed().as_secs(),
        rss_bytes: rss_kb().map(|kb| kb * 1024),
        tenants: tenants.len() as u64,
        overlay_bytes: tenants.iter().map(|t| t.overlay_stats().1).sum(),
        bases: st.store.bases().iter().map(|b| BaseSize { name: b.name.clone(), files: b.file_count() as u64, bytes: b.bytes() }).collect(),
        cache_entries: cache.entries as u64,
        cache_bytes: cache.bytes as u64,
        cache_hits: cache.hits,
        cache_misses: cache.misses,
        sse_subscribers: tenants.iter().map(|t| t.events.receiver_count() as u64).sum(),
        sass_running: sass.running as u64,
        sass_runaway: sass.runaway as u64,
        sass_timeouts: sass.timeouts,
        sass_refused: sass.refused,
        compiles_running: compiles.running as u64,
        compiles_queued: compiles.queued as u64,
        compiles_limit: compiles.limit as u64,
        compiles_refused: compiles.refused,
        shots_running: shots.running as u64,
        shots_busy: shots.busy,
        shots_timeouts: shots.timeouts,
        requests: st.metrics.requests(),
        compile: st.metrics.compiles(),
    }
}

pub async fn metrics(AxState(st): AxState<State>) -> Response {
    let body = render(&snapshot(&st));
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

pub async fn bases(AxState(st): AxState<State>) -> Json<Value> {
    Json(Value::Array(
        st.store.bases().iter().map(|b| json!({ "name": b.name, "files": b.file_count(), "bytes": b.bytes(), "root": b.root })).collect(),
    ))
}

fn tenant_json(st: &AppState, t: &Tenant) -> Value {
    let (files, bytes) = t.overlay_stats();
    json!({
        "id": t.id,
        "base": t.base.name,
        "version": t.version(),
        "overlay_files": files,
        "overlay_bytes": bytes,
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
    if st.store.remove_tenant(&id) { StatusCode::NO_CONTENT.into_response() } else { err(StatusCode::NOT_FOUND, "unknown tenant") }
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
        Some(bytes) => ([(header::CONTENT_TYPE, mime(&path)), (header::CACHE_CONTROL, "no-store")], bytes.to_vec()).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such file"),
    }
}

pub async fn write_file(AxState(st): AxState<State>, Path((id, path)): Path<(String, String)>, body: Bytes) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let Some(path) = clean_path(&path) else { return err(StatusCode::BAD_REQUEST, "bad path") };
    match t.write(&path, body.to_vec()) {
        Ok(version) => Json(json!({ "path": path, "version": version })).into_response(),
        Err(e @ WriteError::Quota { .. }) => err(StatusCode::PAYLOAD_TOO_LARGE, e.to_string()),
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
    match t.delete(&path) {
        Ok(version) => Json(json!({ "path": path, "version": version })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub fn sse(t: &Tenant) -> Sse<impl Stream<Item = Result<Event, Infallible>> + use<>> {
    let hello = tokio_stream::once(Ok(Event::default().event("hello").data(t.version().to_string())));
    let updates = BroadcastStream::new(t.events.subscribe()).filter_map(|r| r.ok()).map(|s| Ok(Event::default().data(s)));
    Sse::new(hello.chain(updates)).keep_alive(KeepAlive::default())
}

pub async fn events(AxState(st): AxState<State>, Path(id): Path<String>) -> Response {
    match tenant_or_404(&st, &id) {
        Ok(t) => sse(&t).into_response(),
        Err(r) => r,
    }
}

pub fn check_tenant(st: &AppState, t: &Tenant) -> Value {
    let mut diagnostics = Vec::new();
    let mut files = 0;
    for e in t.list() {
        if !is_source(&e.path) {
            continue;
        }
        files += 1;
        match st.engine.build(t, &e.path, Kind::Module) {
            Ok(b) => diagnostics.extend(b.warnings.iter().cloned()),
            Err(be) => {
                if be.diagnostics.is_empty() {
                    diagnostics.push(crate::transform::Diag {
                        severity: "error".into(),
                        text: be.message,
                        hint: String::new(),
                        file: e.path.clone(),
                        line: 0,
                        column: 0,
                    });
                } else {
                    diagnostics.extend(be.diagnostics);
                }
            }
        }
    }
    let errors = diagnostics.iter().filter(|d| d.severity == "error").count();
    json!({ "files": files, "errors": errors, "diagnostics": diagnostics })
}

pub async fn check(AxState(st): AxState<State>, Path(id): Path<String>) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let st2 = st.clone();
    let out = tokio::task::spawn_blocking(move || check_tenant(&st2, &t)).await.unwrap_or_else(|e| json!({ "error": e.to_string() }));
    Json(out).into_response()
}
