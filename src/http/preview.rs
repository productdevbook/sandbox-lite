use std::sync::{Arc, OnceLock};

use axum::Json;
use axum::extract::{Extension, Path, RawQuery, State as AxState};
use axum::http::{StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use serde_json::{Map, Value, json};
use xxhash_rust::xxh3::xxh3_64;

use super::api::{check_tenant, err, sse};
use super::{AppState, State, TenantId, mime};
use crate::resolve::{Resolver, renderers};
use crate::store::{Tenant, clean_path, is_private_path};
use crate::transform::{BuildError, JS, Kind, content, css, js, json_str};

const ASTRO_JS: &str = include_str!("../../assets/astro.js");
const SHELL_HTML: &str = include_str!("../../assets/shell.html");
const SHELL_JS: &str = include_str!("../../assets/shell.js");
const LIVE_JS: &str = include_str!("../../assets/live.js");
const VIEW_TRANSITIONS_CSS: &str = include_str!("../../assets/viewtransitions.css");

pub fn asset_version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| format!("{:x}", xxh3_64(ASTRO_JS.as_bytes()) ^ xxh3_64(SHELL_JS.as_bytes()) ^ xxh3_64(LIVE_JS.as_bytes())))
}

#[allow(clippy::result_large_err)]
fn tenant(st: &AppState, id: &TenantId) -> Result<Arc<Tenant>, Response> {
    st.store.tenant(&id.0).ok_or_else(|| err(StatusCode::NOT_FOUND, format!("unknown tenant '{}'", id.0)))
}

fn text(body: impl Into<String>, content_type: &'static str, cache: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, cache)], body.into()).into_response()
}

fn versioned_asset(body: &'static str, content_type: &'static str) -> Response {
    let etag = format!("\"{}\"", asset_version());
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache"), (header::ETAG, etag.as_str())], body).into_response()
}

fn build_error(e: BuildError) -> Response {
    (
        StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({ "error": e.message, "diagnostics": e.diagnostics })),
    )
        .into_response()
}

pub async fn module(
    AxState(st): AxState<State>,
    Extension(id): Extension<TenantId>,
    Path(path): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let query = query.unwrap_or_default();
    let kind = Kind::from_query(&query);
    let response = module_response(&st, &id, path, &query, kind).await;
    st.metrics.module_request(kind, response.status().as_u16());
    response
}

async fn module_response(st: &State, id: &TenantId, path: String, query: &str, kind: Kind) -> Response {
    let t = match tenant(st, id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let Some(path) = clean_path(&path) else { return err(StatusCode::BAD_REQUEST, "bad path") };
    if is_private_path(&path) {
        return err(StatusCode::NOT_FOUND, format!("{path}: not found"));
    }
    let versioned = query.split('&').any(|p| p.starts_with("v="));
    let st2 = st.clone();
    let result = tokio::task::spawn_blocking(move || {
        let resolver = Resolver::new(&t, &st2.engine.cfg.cdn, t.version());
        st2.engine.serve(&t, &path, kind, &resolver)
    })
    .await;
    match result {
        Ok(Ok((body, content_type))) => {
            text(body, content_type, if versioned { "public, max-age=31536000, immutable" } else { "no-cache" })
        }
        Ok(Err(e)) => build_error(e),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn raw(AxState(st): AxState<State>, Extension(id): Extension<TenantId>, Path(path): Path<String>) -> Response {
    let t = match tenant(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let Some(path) = clean_path(&path) else { return err(StatusCode::BAD_REQUEST, "bad path") };
    match t.read(&path).filter(|_| !is_private_path(&path)) {
        Some(bytes) => ([(header::CONTENT_TYPE, mime(&path)), (header::CACHE_CONTROL, "no-cache")], bytes.to_vec()).into_response(),
        None => err(StatusCode::NOT_FOUND, format!("{path}: not found")),
    }
}

pub async fn routes_json(AxState(st): AxState<State>, Extension(id): Extension<TenantId>) -> Response {
    match tenant(&st, &id) {
        Ok(t) => ([(header::CACHE_CONTROL, "no-cache")], Json(crate::routes::build(&t))).into_response(),
        Err(r) => r,
    }
}

pub async fn renderers_json(AxState(st): AxState<State>, Extension(id): Extension<TenantId>) -> Response {
    match tenant(&st, &id) {
        Ok(t) => ([(header::CACHE_CONTROL, "no-cache")], Json(renderers(&t, &st.engine.cfg.cdn))).into_response(),
        Err(r) => r,
    }
}

pub async fn events(AxState(st): AxState<State>, Extension(id): Extension<TenantId>) -> Response {
    match tenant(&st, &id) {
        Ok(t) => sse(&t).into_response(),
        Err(r) => r,
    }
}

pub async fn check(AxState(st): AxState<State>, Extension(id): Extension<TenantId>) -> Response {
    let t = match tenant(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let st2 = st.clone();
    let out = tokio::task::spawn_blocking(move || check_tenant(&st2, &t)).await.unwrap_or_else(|e| json!({ "error": e.to_string() }));
    ([(header::CACHE_CONTROL, "no-store")], Json(out)).into_response()
}

pub async fn content(AxState(st): AxState<State>, Extension(id): Extension<TenantId>, Path(name): Path<String>) -> Response {
    let t = match tenant(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return err(StatusCode::BAD_REQUEST, "bad collection name");
    }
    let st2 = st.clone();
    let out = tokio::task::spawn_blocking(move || {
        let max = st2.engine.cfg.max_source_bytes;
        st2.engine.on_parser_stack(move || content::collection_json(&t, &name, max))
    })
    .await;
    let out = out.ok().and_then(Result::ok).unwrap_or_else(content::empty_collection);
    ([(header::CACHE_CONTROL, "no-cache")], Json(out)).into_response()
}

fn env_map(t: &Tenant) -> Map<String, Value> {
    let mut env = Map::new();
    env.insert("DEV".into(), json!(true));
    env.insert("PROD".into(), json!(false));
    env.insert("MODE".into(), json!("development"));
    env.insert("SSR".into(), json!(true));
    env.insert("BASE_URL".into(), json!("/"));
    env.insert("SITE".into(), crate::resolve::tenant_site(t).map(Value::String).unwrap_or(Value::Null));
    env.insert("ASSETS_PREFIX".into(), Value::Null);
    if let Some(dotenv) = t.read_text(".env") {
        for line in dotenv.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let k = k.trim();
            if !k.starts_with("PUBLIC_") {
                continue;
            }
            let v = v.trim().trim_matches('"').trim_matches('\'');
            env.insert(k.to_string(), Value::String(v.to_string()));
        }
    }
    env
}

pub async fn shim(AxState(st): AxState<State>, Extension(id): Extension<TenantId>, Path(name): Path<String>) -> Response {
    let t = match tenant(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let Some(name) = name.strip_suffix(".js") else { return err(StatusCode::NOT_FOUND, "no such shim") };
    let resolver = Resolver::new(&t, &st.engine.cfg.cdn, t.version());
    let body: String = match name {
        "astro-content" => include_str!("../../assets/shims/astro-content.js").into(),
        "astro-assets" => include_str!("../../assets/shims/astro-assets.js").into(),
        "astro-transitions" => include_str!("../../assets/shims/astro-transitions.js").into(),
        "astro-transitions-client" => include_str!("../../assets/shims/astro-transitions-client.js").into(),
        "astro-i18n" => include_str!("../../assets/shims/astro-i18n.js").into(),
        "astro-actions" => include_str!("../../assets/shims/astro-actions.js").into(),
        "astro-schema" | "zod" => include_str!("../../assets/shims/zod.js").into(),
        "astro-middleware" => include_str!("../../assets/shims/astro-middleware.js").into(),
        "astro-components" => include_str!("../../assets/shims/astro-components.js").into(),
        "renderer-react" => include_str!("../../assets/shims/renderer-react.js").into(),
        "renderer-preact" => include_str!("../../assets/shims/renderer-preact.js").into(),
        "renderer-vue" => include_str!("../../assets/shims/renderer-vue.js").into(),
        "renderer-vue-client" => include_str!("../../assets/shims/renderer-vue-client.js").into(),
        "vue-static-html" => include_str!("../../assets/shims/vue-static-html.js").into(),
        "renderer-svelte" => include_str!("../../assets/shims/renderer-svelte.js").into(),
        "renderer-svelte-client" => include_str!("../../assets/shims/renderer-svelte-client.js").into(),
        // The loaders build blob modules, whose bare specifiers no resolver sees, so their URLs are baked in.
        "vue-loader" => include_str!("../../assets/shims/vue-loader.js")
            .replace("%VUE_COMPILER%", &crate::resolve::vue_compiler_url(&t, &st.engine.cfg.cdn))
            .replace("%VUE%", &resolver.resolve("", "vue")),
        "svelte-loader" => include_str!("../../assets/shims/svelte-loader.js").replace("%SVELTE%", &resolver.resolve("", "svelte")),
        "astro-config" => include_str!("../../assets/shims/astro-config.js").into(),
        "astro-loaders" => include_str!("../../assets/shims/astro-loaders.js").into(),
        "astro-jsx-runtime" => include_str!("../../assets/astro-jsx.js").into(),
        "astro-types" | "astro-prefetch" | "astro-scripts-before-hydration" | "astro-scripts-page" => "export {};\n".into(),
        "viewtransitions-css" => css::to_module("astro:viewtransitions", VIEW_TRANSITIONS_CSS, ""),
        "astro-env-client" | "astro-env-server" => {
            let mut out = String::new();
            for (k, v) in env_map(&t) {
                if k.starts_with("PUBLIC_") {
                    out.push_str(&format!("export const {k} = {v};\n"));
                }
            }
            out.push_str("export function getSecret() { return undefined; }\n");
            out
        }
        _ => return err(StatusCode::NOT_FOUND, format!("no shim for astro:{name}")),
    };
    let mut out = String::with_capacity(body.len() + 128);
    let mut last = 0;
    for spec in js::scan(&body) {
        out.push_str(&body[last..spec.start]);
        out.push_str(&resolver.resolve("", &spec.spec));
        last = spec.end;
    }
    out.push_str(&body[last..]);
    text(out, JS, "no-cache")
}

pub async fn astro_js() -> Response {
    versioned_asset(ASTRO_JS, JS)
}

pub async fn shell_js() -> Response {
    versioned_asset(SHELL_JS, JS)
}

pub async fn live_js() -> Response {
    versioned_asset(LIVE_JS, JS)
}

pub async fn missing(RawQuery(query): RawQuery) -> Response {
    let mut spec = String::new();
    let mut from = String::new();
    for part in query.unwrap_or_default().split('&') {
        if let Some(v) = part.strip_prefix("spec=") {
            spec = percent_decode(v);
        } else if let Some(v) = part.strip_prefix("from=") {
            from = percent_decode(v);
        }
    }
    let msg = format!("Cannot resolve import '{spec}' from '{from}'");
    text(format!("throw new Error({});\n", json_str(&msg)), JS, "no-cache")
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let digit = |at: usize| bytes.get(at).and_then(|b| (*b as char).to_digit(16));
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (digit(i + 1), digit(i + 2))
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub async fn page(AxState(st): AxState<State>, Extension(id): Extension<TenantId>, uri: Uri) -> Response {
    let t = match tenant(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let decoded = percent_decode(uri.path());
    if let Some(rel) = clean_path(&decoded).filter(|rel| !is_private_path(rel)) {
        let public = format!("public/{rel}");
        if let Some(bytes) = t.read(&public) {
            return ([(header::CONTENT_TYPE, mime(&public)), (header::CACHE_CONTROL, "no-cache")], bytes.to_vec()).into_response();
        }
    }
    let env = Value::Object(env_map(&t)).to_string().replace('<', "\\u003c");
    let token = st.preview_secret.as_deref().map(|s| super::preview_token(s, &t.id)).unwrap_or_default();
    let token_query = if token.is_empty() { String::new() } else { format!("?sl_token={token}") };
    let html = SHELL_HTML
        .replace("%TENANT%", &t.id)
        .replace("%VERSION%", &t.version().to_string())
        .replace("%ASSETS%", asset_version())
        .replace("%TOKENQ%", &token_query)
        .replace("%TOKEN%", &token)
        .replace("%ENV%", &env);
    ([(header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

#[cfg(test)]
mod tests {
    use super::percent_decode;

    #[test]
    fn percent_decode_takes_any_string() {
        assert_eq!(percent_decode("%aé"), "%aé");
        assert_eq!(percent_decode("%ࠀ"), "%ࠀ");
        assert_eq!(percent_decode("%C3%A9%"), "é%");
        assert_eq!(percent_decode("%+1%-1%2"), "%+1%-1%2");
        assert_eq!(percent_decode("%FF"), "\u{fffd}");
    }
}
