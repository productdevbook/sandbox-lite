pub mod ai;
pub mod api;
pub mod archive;
pub mod preview;

use std::io::ErrorKind;
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State as AxState};
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, LOCATION, SET_COOKIE};
use axum::http::{StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tower::ServiceExt;

use crate::store::{Store, valid_id};
use crate::transform::Engine;

pub struct AppState {
    pub store: Store,
    pub engine: Engine,
    pub domain: String,
    pub port: u16,
    pub model: String,
    pub api_key: Option<String>,
    pub api_token: Option<String>,
    pub preview_secret: Option<String>,
    pub cookie_samesite: SameSite,
    pub started: Instant,
}

pub type State = Arc<AppState>;

#[derive(Clone, Copy)]
pub enum SameSite {
    Lax,
    None,
}

impl std::str::FromStr for SameSite {
    type Err = ();

    fn from_str(s: &str) -> Result<SameSite, ()> {
        match s.to_ascii_lowercase().as_str() {
            "lax" => Ok(SameSite::Lax),
            "none" => Ok(SameSite::None),
            _ => Err(()),
        }
    }
}

const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub async fn serve(listener: TcpListener, app: Router, shutdown: impl Future<Output = ()>) {
    let mut http = http1::Builder::new();
    // hyper arms this timer whenever it waits for a request head, so it also closes idle keep-alive connections
    http.timer(TokioTimer::new()).header_read_timeout(HEADER_READ_TIMEOUT);
    let graceful = GracefulShutdown::new();
    let mut shutdown = pin!(shutdown);
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(e) => {
                    if !matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset) {
                        eprintln!("accept: {e}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };
        let conn = http.serve_connection(TokioIo::new(stream), TowerToHyperService::new(app.clone()));
        tokio::spawn(graceful.watch(conn));
    }
    drop(listener);
    if tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown()).await.is_err() {
        eprintln!("shutdown: closing connections still open after {}s", SHUTDOWN_GRACE.as_secs());
    }
}

#[derive(Clone)]
pub struct TenantId(pub String);

pub fn app(state: State) -> Router {
    let root = Router::new()
        .route("/", get(api::editor))
        .route("/health", get(api::health))
        .route("/api/stats", get(api::stats))
        .route("/api/bases", get(api::bases))
        .route("/api/tenants", get(api::tenants).post(api::create_tenant))
        .route("/api/tenants/{id}", delete(api::delete_tenant))
        .route("/api/tenants/{id}/import", post(archive::import))
        .route("/api/t/{id}/files", get(api::files))
        .route("/api/t/{id}/export", get(archive::export))
        .route("/api/t/{id}/file/{*path}", get(api::read_file).put(api::write_file).delete(api::delete_file))
        .route("/api/t/{id}/events", get(api::events))
        .route("/api/t/{id}/check", get(api::check))
        .route("/api/t/{id}/chat", post(ai::chat))
        .layer(DefaultBodyLimit::max(64 << 20))
        .layer(middleware::from_fn_with_state(state.clone(), require_api_token))
        .with_state(state.clone());
    let tenant = Router::new()
        .route("/__sl/m/{*path}", get(preview::module))
        .route("/__sl/raw/{*path}", get(preview::raw))
        .route("/__sl/routes.json", get(preview::routes_json))
        .route("/__sl/renderers.json", get(preview::renderers_json))
        .route("/__sl/events", get(preview::events))
        .route("/__sl/check", get(preview::check))
        .route("/__sl/content/{name}", get(preview::content))
        .route("/__sl/shim/{name}", get(preview::shim))
        .route("/__sl/astro.js", get(preview::astro_js))
        .route("/__sl/shell.js", get(preview::shell_js))
        .route("/__sl/live.js", get(preview::live_js))
        .route("/__sl/missing.js", get(preview::missing))
        .fallback(preview::page)
        .layer(middleware::from_fn_with_state(state.clone(), require_preview_token))
        .with_state(state.clone());
    let domain = state.domain.clone();
    Router::new().fallback(move |req: Request| {
        let root = root.clone();
        let tenant = tenant.clone();
        let domain = domain.clone();
        async move { dispatch(root, tenant, &domain, req).await }
    })
}

async fn dispatch(root: Router, tenant: Router, domain: &str, mut req: Request) -> Response {
    let host = req.headers().get(HOST).and_then(|h| h.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let host = host.split(':').next().unwrap_or("").to_string();
    match tenant_from_host(&host, domain) {
        Some(id) => {
            req.extensions_mut().insert(TenantId(id));
            tenant.oneshot(req).await.into_response()
        }
        None => root.oneshot(req).await.into_response(),
    }
}

fn query_param<'a>(uri: &'a Uri, key: &str) -> Option<&'a str> {
    uri.query()?.split('&').find_map(|p| p.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

async fn require_api_token(AxState(st): AxState<State>, req: Request, next: Next) -> Response {
    let Some(expected) = &st.api_token else { return next.run(req).await };
    let path = req.uri().path();
    if path == "/" || path == "/health" {
        return next.run(req).await;
    }
    let header = req.headers().get(AUTHORIZATION).and_then(|h| h.to_str().ok()).and_then(|h| h.strip_prefix("Bearer "));
    let query = query_param(req.uri(), "token");
    let valid = |given: Option<&str>| given.is_some_and(|g| constant_time_eq(g.as_bytes(), expected.as_bytes()));
    if valid(header) || valid(query) {
        return next.run(req).await;
    }
    (StatusCode::UNAUTHORIZED, "missing or wrong api token\n").into_response()
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    // black_box keeps the optimizer from turning the fold into an early exit
    std::hint::black_box(diff) == 0
}

pub fn preview_token(secret: &str, tenant: &str) -> String {
    let key = blake3::derive_key("sandbox-lite preview token", secret.as_bytes());
    blake3::keyed_hash(&key, tenant.as_bytes()).to_hex()[..32].to_string()
}

const PREVIEW_COOKIE: &str = "sl_t";

async fn require_preview_token(AxState(st): AxState<State>, req: Request, next: Next) -> Response {
    let Some(secret) = &st.preview_secret else { return next.run(req).await };
    let Some(TenantId(id)) = req.extensions().get::<TenantId>().cloned() else { return next.run(req).await };
    let expected = preview_token(secret, &id);
    if let Some(token) = query_param(req.uri(), "sl_token") {
        if !constant_time_eq(token.as_bytes(), expected.as_bytes()) {
            return (StatusCode::FORBIDDEN, "wrong preview token\n").into_response();
        }
        let rest: Vec<&str> = req.uri().query().unwrap_or("").split('&').filter(|p| !p.starts_with("sl_token=")).collect();
        let location = if rest.is_empty() { req.uri().path().to_string() } else { format!("{}?{}", req.uri().path(), rest.join("&")) };
        let forwarded_https = req.headers().get("x-forwarded-proto").and_then(|h| h.to_str().ok()) == Some("https");
        let attributes = match st.cookie_samesite {
            SameSite::Lax if forwarded_https => "SameSite=Lax; Secure",
            SameSite::Lax => "SameSite=Lax",
            SameSite::None => "SameSite=None; Secure",
        };
        let cookie = format!("{PREVIEW_COOKIE}={expected}; Path=/; HttpOnly; {attributes}");
        return (StatusCode::SEE_OTHER, [(LOCATION, location), (SET_COOKIE, cookie)]).into_response();
    }
    let has_cookie = req
        .headers()
        .get_all(COOKIE)
        .iter()
        .filter_map(|c| c.to_str().ok())
        .flat_map(|c| c.split(';'))
        .filter_map(|c| c.trim().strip_prefix(PREVIEW_COOKIE).and_then(|r| r.strip_prefix('=')))
        .any(|given| constant_time_eq(given.as_bytes(), expected.as_bytes()));
    if has_cookie {
        return next.run(req).await;
    }
    (StatusCode::FORBIDDEN, "this preview needs a token: open it from the editor\n").into_response()
}

pub fn tenant_from_host(host: &str, domain: &str) -> Option<String> {
    let label = host.strip_suffix(domain)?.strip_suffix('.')?;
    if label.contains('.') || !valid_id(label) {
        return None;
    }
    Some(label.to_string())
}

pub fn mime(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, e)| e).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" | "md" | "mdx" | "astro" | "ts" | "tsx" | "jsx" | "mts" | "env" | "yaml" | "yml" | "toml" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, preview_token, tenant_from_host};

    #[test]
    fn constant_time_eq_compares_whole_slices() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"token", b"token"));
        assert!(!constant_time_eq(b"token", b"Token"));
        assert!(!constant_time_eq(b"token", b"tokem"));
        assert!(!constant_time_eq(b"token", b"toke"));
        assert!(!constant_time_eq(b"token", b"tokens"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn preview_tokens_are_stable_and_tenant_specific() {
        assert_eq!(preview_token("s", "acme"), preview_token("s", "acme"));
        assert_ne!(preview_token("s", "acme"), preview_token("s", "bakery"));
        assert_ne!(preview_token("s", "acme"), preview_token("t", "acme"));
        assert_eq!(preview_token("s", "acme").len(), 32);
    }

    #[test]
    fn host_to_tenant() {
        assert_eq!(tenant_from_host("acme.localhost", "localhost"), Some("acme".into()));
        assert_eq!(tenant_from_host("localhost", "localhost"), None);
        assert_eq!(tenant_from_host("a.b.localhost", "localhost"), None);
        assert_eq!(tenant_from_host("acme.preview.example.com", "preview.example.com"), Some("acme".into()));
        assert_eq!(tenant_from_host("evil-localhost", "localhost"), None);
    }
}
