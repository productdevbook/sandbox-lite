pub mod ai;
pub mod anthropic;
pub mod api;
pub mod archive;
pub mod chats;
pub mod preview;

use std::io::ErrorKind;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State as AxState};
use axum::http::header::{AUTHORIZATION, COOKIE, HOST, LOCATION, SET_COOKIE};
use axum::http::{HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tower::ServiceExt;

use crate::metrics::Metrics;
use crate::store::{Store, valid_id};
use crate::transform::Engine;

pub struct AppState {
    pub store: Store,
    pub engine: Engine,
    pub metrics: Arc<Metrics>,
    pub chats: chats::Chats,
    pub domain: String,
    pub port: u16,
    pub model: String,
    pub api_key: Option<String>,
    pub api_base: String,
    pub api_token: Option<String>,
    pub preview_secret: Option<String>,
    pub cookie_samesite: SameSite,
    pub chrome: Option<PathBuf>,
    pub shots: ai::Shots,
    pub started: Instant,
}

pub type State = Arc<AppState>;

#[derive(Clone, Copy)]
pub enum SameSite {
    Lax,
    None,
}

impl SameSite {
    /// A framed preview is cross-site, so a browser never stores or sends a Lax cookie there.
    /// With a preview secret the cookie is only worth anything as `SameSite=None; Secure`,
    /// which browsers accept on HTTPS and on loopback.
    pub fn default_for(preview_secret: Option<&str>) -> SameSite {
        if preview_secret.is_some() { SameSite::None } else { SameSite::Lax }
    }
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
        .route("/metrics", get(api::metrics))
        .route("/api/stats", get(api::stats))
        .route("/api/bases", get(api::bases).post(api::add_base))
        .route("/api/bases/{name}/reload", post(api::reload_base))
        .route("/api/tenants", get(api::tenants).post(api::create_tenant))
        .route("/api/tenants/{id}", delete(api::delete_tenant))
        .route("/api/tenants/{id}/import", post(archive::import))
        .route("/api/t/{id}/files", get(api::files))
        .route("/api/t/{id}/export", get(archive::export))
        .route("/api/t/{id}/file/{*path}", get(api::read_file).put(api::write_file).delete(api::delete_file))
        .route("/api/t/{id}/events", get(api::events))
        .route("/api/t/{id}/check", get(api::check))
        .route("/api/t/{id}/chat", post(ai::chat))
        .route("/api/t/{id}/chats", get(chats::list))
        .route("/api/t/{id}/chats/{chat}", get(chats::get).delete(chats::remove))
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
        .layer(middleware::from_fn(no_referrer))
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

fn header_is(req: &Request, name: &str, value: &str) -> bool {
    req.headers().get(name).is_some_and(|h| h.as_bytes() == value.as_bytes())
}

async fn require_preview_token(AxState(st): AxState<State>, req: Request, next: Next) -> Response {
    let Some(secret) = &st.preview_secret else { return next.run(req).await };
    let Some(TenantId(id)) = req.extensions().get::<TenantId>().cloned() else { return next.run(req).await };
    let expected = preview_token(secret, &id);
    if let Some(token) = query_param(req.uri(), "sl_token") {
        if !constant_time_eq(token.as_bytes(), expected.as_bytes()) {
            return (StatusCode::FORBIDDEN, "wrong preview token\n").into_response();
        }
        let forwarded_https = req.headers().get("x-forwarded-proto").and_then(|h| h.to_str().ok()) == Some("https");
        let attributes = match st.cookie_samesite {
            SameSite::Lax if forwarded_https => "SameSite=Lax; Secure",
            SameSite::Lax => "SameSite=Lax",
            SameSite::None => "SameSite=None; Secure",
        };
        let cookie = format!("{PREVIEW_COOKIE}={expected}; Path=/; HttpOnly; {attributes}");
        // A frame reports `iframe` here and a top-level navigation `document`. Only the top-level
        // one is sent to the clean URL — a frame's redirect would lose the token from the URL
        // before anything on the page had it — so the rest are served where they are.
        if !header_is(&req, "sec-fetch-dest", "document") {
            let mut res = next.run(req).await;
            if let Ok(value) = cookie.parse() {
                res.headers_mut().append(SET_COOKIE, value);
            }
            return res;
        }
        let rest: Vec<&str> = req.uri().query().unwrap_or("").split('&').filter(|p| !p.starts_with("sl_token=")).collect();
        let location = if rest.is_empty() { req.uri().path().to_string() } else { format!("{}?{}", req.uri().path(), rest.join("&")) };
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

/// The token rides in query strings, so a preview host must never name one to a third party —
/// `esm.sh` would otherwise read it out of the `Referer` on every bare import.
async fn no_referrer(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut().insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    res
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

/// The state every fixture starts from, so that a field added to `AppState` is one edit here rather
/// than one in each test that builds one. A fixture that needs a different value says only that
/// value: `AppState { api_key: Some("k".into()), ..AppState::for_tests() }`.
#[cfg(test)]
impl AppState {
    pub fn for_tests() -> AppState {
        let engine = Engine::for_tests();
        AppState {
            store: Store::new(None, u64::MAX),
            metrics: engine.metrics(),
            engine,
            chats: chats::Chats::default(),
            domain: "localhost".into(),
            port: 4321,
            model: "m".into(),
            api_key: None,
            api_base: "http://127.0.0.1:1".into(),
            api_token: None,
            preview_secret: None,
            cookie_samesite: SameSite::Lax,
            chrome: None,
            shots: ai::Shots::default(),
            started: Instant::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::header::{LOCATION, SET_COOKIE};
    use axum::http::{HeaderName, Request, StatusCode};
    use tower::ServiceExt;

    use super::{AppState, SameSite, State, app, constant_time_eq, preview_token, tenant_from_host};
    use crate::store::Store;

    fn state_for(store: Store, api_token: Option<&str>, preview_secret: Option<&str>) -> State {
        Arc::new(AppState {
            store,
            api_token: api_token.map(str::to_string),
            preview_secret: preview_secret.map(str::to_string),
            cookie_samesite: SameSite::default_for(preview_secret),
            ..AppState::for_tests()
        })
    }

    fn baseless_app(api_token: Option<&str>, preview_secret: Option<&str>) -> axum::Router {
        app(state_for(Store::new(None, u64::MAX), api_token, preview_secret))
    }

    async fn get(app: &axum::Router, uri: &str, bearer: Option<&str>) -> (StatusCode, String) {
        let mut req = Request::builder().method("GET").uri(uri);
        if let Some(t) = bearer {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let res = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn post(app: &axum::Router, uri: &str, body: &str) -> (StatusCode, String) {
        let req = Request::builder().method("POST").uri(uri).header("content-type", "application/json");
        let res = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn delete(app: &axum::Router, uri: &str) -> (StatusCode, String) {
        let req = Request::builder().method("DELETE").uri(uri);
        let res = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// Issue #61: the daemon skipped a tenant it could not restore and said nothing, so a customer's
    /// site answered "unknown tenant" while `/api/stats` and `/metrics` looked healthy.
    #[tokio::test]
    async fn a_tenant_that_could_not_be_restored_is_reported_rather_than_absent() {
        let root = std::env::temp_dir().join(format!("sandbox-lite-api-failed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let data = root.join("data");
        std::fs::create_dir_all(data.join("bakery/files")).unwrap();
        std::fs::write(data.join("bakery/tenant.json"), r#"{"base":"never-loaded-here"}"#).unwrap();
        let store = Store::new(Some(data), u64::MAX);
        assert_eq!(store.restore().unwrap(), 0, "it does not load, and it does not stop the daemon either");
        let app = app(state_for(store, None, None));

        let (status, body) = get(&app, "/api/stats", None).await;
        assert_eq!(status, StatusCode::OK);
        let stats: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(stats["tenants"], 0);
        assert_eq!(stats["tenants_failed"], 1, "{body}");
        assert_eq!(stats["failed_tenants"][0]["id"], "bakery");
        assert!(stats["failed_tenants"][0]["error"].as_str().unwrap().contains("never-loaded-here"), "{body}");

        let (status, body) = get(&app, "/metrics", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\nsandbox_lite_tenants_failed 1\n"), "{body}");

        // could not be loaded is a 500 that says why; never existed stays a 404
        let (status, body) = get(&app, "/api/t/bakery/files", None).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(body.contains("could not be loaded"), "{body}");
        let (status, body) = get(&app, "/api/t/nobody/files", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("unknown tenant"), "{body}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn bases_are_added_and_reloaded_over_the_api() {
        let root = std::env::temp_dir().join(format!("sandbox-lite-api-bases-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let page = root.join("bases/theme/src/pages/index.astro");
        std::fs::create_dir_all(page.parent().unwrap()).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(&page, "<h1>one</h1>\n").unwrap();
        let store = Store::new(None, 1 << 20).with_bases_dir(Some(root.join("bases")));
        let state = state_for(store, None, None);
        let app = app(state.clone());
        let add = |path: PathBuf| format!(r#"{{"name":"theme","path":{}}}"#, serde_json::to_string(&path).unwrap());

        assert_eq!(post(&app, "/api/bases", &add(root.join("outside"))).await.0, StatusCode::BAD_REQUEST);
        let (status, body) = post(&app, "/api/bases", &add(root.join("bases/theme"))).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(body.contains(r#""files":1"#), "{body}");
        assert_eq!(post(&app, "/api/bases", &add(root.join("bases/theme"))).await.0, StatusCode::CONFLICT);

        let t = state.store.create_tenant("acme", "theme").unwrap();
        let version = t.version();
        std::fs::write(&page, "<h1>two</h1>\n").unwrap();
        let (status, body) = post(&app, "/api/bases/theme/reload", "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains(r#""tenants":["acme"]"#), "{body}");
        assert_eq!(t.read_text("src/pages/index.astro").unwrap().as_deref(), Some("<h1>two</h1>\n"));
        assert!(t.version() > version);
        assert_eq!(post(&app, "/api/bases/nope/reload", "").await.0, StatusCode::NOT_FOUND);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Issue #73: the two ways a `--bases` sub-directory becomes a base, on one path that is a
    /// symbolic link out of the directory. Neither takes it.
    #[tokio::test]
    async fn a_symlink_in_the_bases_directory_is_refused_at_startup_and_over_the_api() {
        let root = std::env::temp_dir().join(format!("sandbox-lite-api-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("bases/theme")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(root.join("outside/secret.txt"), "SECRET\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("outside"), root.join("bases/sneaky")).unwrap();
        let store = Store::new(None, 1 << 20).with_bases_dir(Some(root.join("bases")));

        // startup: what main.rs loads from --bases
        let scanned: Vec<String> = store.bases_in_dir().unwrap().into_iter().map(|(name, _)| name).collect();
        assert_eq!(scanned, vec!["theme".to_string()], "the link is not a base at startup");

        // and the same path over POST /api/bases
        let app = app(state_for(store, None, None));
        let body = format!(r#"{{"name":"sneaky","path":{}}}"#, serde_json::to_string(&root.join("bases/sneaky")).unwrap());
        let (status, body) = post(&app, "/api/bases", &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(get(&app, "/api/bases", None).await.1, "[]", "no base was loaded either way");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Issue #74: this node never loaded the tenant — the other one created it — so the delete used
    /// to answer 404 and leave every byte where it was.
    #[tokio::test]
    async fn deleting_a_tenant_this_node_never_loaded_removes_its_files() {
        let root = std::env::temp_dir().join(format!("sandbox-lite-api-delete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let page = root.join("theme/src/pages/index.astro");
        std::fs::create_dir_all(page.parent().unwrap()).unwrap();
        std::fs::write(&page, "<h1>one</h1>\n").unwrap();
        let data = root.join("data");
        let base = || crate::store::Base::load("theme", &root.join("theme")).unwrap();
        let other = Store::new(Some(data.clone()), 1 << 20);
        other.add_base(base());
        other.create_tenant("acme", "theme").unwrap().write("secret.txt", b"SECRET".to_vec(), crate::store::UpdateKind::Module).unwrap();

        let store = Store::new(Some(data.clone()), 1 << 20);
        store.add_base(base());
        let app = app(state_for(store, None, None));
        assert_eq!(get(&app, "/api/tenants", None).await.1, "[]", "this node holds no tenant in memory");

        assert_eq!(delete(&app, "/api/tenants/acme").await.0, StatusCode::NO_CONTENT);
        assert!(!data.join("acme").exists(), "the tenant's directory is gone");
        assert_eq!(get(&app, "/api/t/acme/files", None).await.0, StatusCode::NOT_FOUND, "and nothing restores it");
        let fresh = Store::new(Some(data.clone()), 1 << 20);
        fresh.add_base(base());
        assert_eq!(fresh.restore().unwrap(), 0, "a fresh daemon over the same data dir does not bring it back");
        assert_eq!(delete(&app, "/api/tenants/acme").await.0, StatusCode::NOT_FOUND, "a tenant nothing holds is still a 404");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn metrics_are_open_until_an_api_token_is_set() {
        let (status, body) = get(&baseless_app(None, None), "/metrics", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("# TYPE sandbox_lite_compile_seconds histogram"), "{body}");
    }

    #[tokio::test]
    async fn metrics_need_the_api_token_when_one_is_set() {
        let app = baseless_app(Some("s3cret"), None);
        assert_eq!(get(&app, "/metrics", None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(get(&app, "/metrics", Some("wrong")).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(get(&app, "/metrics?token=s3cret", None).await.0, StatusCode::OK);
        assert_eq!(get(&app, "/metrics", Some("s3cret")).await.0, StatusCode::OK);
        assert_eq!(get(&app, "/health", None).await.0, StatusCode::OK);
    }

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

    /// No base and no tenant, so a request the middleware lets through reaches a handler that
    /// answers `404` — which is what tells it apart from the `403` the middleware writes itself.
    async fn preview_get(uri: &str, headers: &[(&str, &str)]) -> (StatusCode, Option<String>, Option<String>) {
        let mut req = Request::builder().method("GET").uri(uri).header("host", "acme.localhost");
        for (name, value) in headers {
            req = req.header(*name, *value);
        }
        let res = baseless_app(None, Some("s")).oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let header = |name: HeaderName| res.headers().get(name).and_then(|h| h.to_str().ok()).map(|s| s.to_string());
        (res.status(), header(LOCATION), header(SET_COOKIE))
    }

    #[tokio::test]
    async fn a_valid_token_is_served_and_only_a_document_navigation_is_redirected() {
        let token = preview_token("s", "acme");
        let framed = format!("/?sl_token={token}");

        let (status, _, cookie) = preview_get(&framed, &[("sec-fetch-dest", "iframe"), ("sec-fetch-site", "cross-site")]).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(cookie.as_deref(), Some(format!("sl_t={token}; Path=/; HttpOnly; SameSite=None; Secure").as_str()));
        assert_eq!(
            preview_get(&format!("/__sl/routes.json?sl_token={token}"), &[("sec-fetch-dest", "empty")]).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(preview_get(&framed, &[]).await.0, StatusCode::NOT_FOUND);

        let (status, location, cookie) = preview_get(&format!("/about?sl_token={token}&x=1"), &[("sec-fetch-dest", "document")]).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(location.as_deref(), Some("/about?x=1"));
        assert!(cookie.is_some_and(|c| c.contains(&token)), "the redirect sets the cookie");
    }

    /// `Sec-Fetch-*` is chosen by the client, so it decides presentation and never access:
    /// `curl -H 'Sec-Fetch-Site: same-origin'` must not reach a tenant's files.
    #[tokio::test]
    async fn a_request_without_a_token_or_the_cookie_is_refused_whatever_it_claims() {
        let token = preview_token("s", "acme");

        assert_eq!(preview_get("/", &[]).await.0, StatusCode::FORBIDDEN);
        assert_eq!(preview_get("/?sl_token=nope", &[]).await.0, StatusCode::FORBIDDEN);
        assert_eq!(preview_get("/favicon.svg", &[("sec-fetch-site", "same-origin")]).await.0, StatusCode::FORBIDDEN);
        assert_eq!(
            preview_get("/__sl/raw/.env", &[("sec-fetch-site", "same-origin"), ("sec-fetch-dest", "empty")]).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(preview_get("/favicon.svg", &[("sec-fetch-site", "cross-site")]).await.0, StatusCode::FORBIDDEN);

        assert_eq!(preview_get("/", &[("cookie", &format!("sl_t={token}"))]).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn every_preview_response_forbids_a_referrer() {
        let app = baseless_app(None, Some("s"));
        let req = Request::builder().method("GET").uri("/").header("host", "acme.localhost");
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(res.headers().get("referrer-policy").unwrap(), "no-referrer");
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
