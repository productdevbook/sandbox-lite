use std::convert::Infallible;
use std::io;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State as AxState};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::anthropic::{Emit, Frames, Reply};
use super::api::{check_tenant, err, preview_url_path};
use super::chats::{Conversation, ToolCall, Turn};
use super::{AppState, State};
use crate::store::{Tenant, clean_path};

const MAX_ITERATIONS: usize = 16;
const MAX_READ: usize = 200 * 1024;
const MAX_SUMMARY: usize = 200;
const SHOT_TIMEOUT: Duration = Duration::from_secs(20);
const VIRTUAL_TIME: Duration = Duration::from_secs(8);
const SHOT_SIZE: &str = "1280,900";
const MAX_SHOT: usize = 4 << 20;

#[derive(Deserialize)]
pub struct ChatMsg {
    role: String,
    content: String,
}

#[derive(Deserialize)]
pub struct ChatReq {
    messages: Vec<ChatMsg>,
    /// Continue this stored conversation: its turns come before `messages`.
    #[serde(default)]
    chat: Option<String>,
}

fn tools(st: &AppState) -> Value {
    let mut tools = json!([
        { "name": "list_files", "description": "List every file in the site project with its size in bytes.", "input_schema": { "type": "object", "properties": {} } },
        { "name": "read_file", "description": "Read a text file from the project.", "input_schema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] } },
        { "name": "write_file", "description": "Create or overwrite a file with the full new content. The preview updates immediately.", "input_schema": { "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" } }, "required": ["path", "content"] } },
        { "name": "delete_file", "description": "Delete a file from the project.", "input_schema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] } },
        { "name": "check_site", "description": "Compile every source file and return diagnostics. Call it after editing.", "input_schema": { "type": "object", "properties": {} } }
    ]);
    if st.chrome.is_some() {
        let shot = json!({ "name": "screenshot", "description": "Render one page of the live preview in headless Chrome and return it as an image. Use it to see the layout you just changed.", "input_schema": { "type": "object", "properties": { "path": { "type": "string", "description": "site path, e.g. / or /blog" } }, "required": ["path"] } });
        if let Some(list) = tools.as_array_mut() {
            list.push(shot);
        }
    }
    tools
}

fn system_prompt(t: &Tenant) -> String {
    format!(
        "You edit an Astro 7 website for a customer inside sandbox-lite, a live preview sandbox. \
Project '{}' is based on the '{}' template. Pages live in src/pages, layouts in src/layouts, components in src/components, \
content collections in src/content, styles in src/styles, static files in public. \
Work in small, complete steps: read the files you will change first, write whole files back, then call check_site and fix every error it reports. \
Do not invent npm packages; only plain Astro components, TypeScript and CSS are available. \
Answer in the customer's language, briefly, and describe what you changed.",
        t.id, t.base.name
    )
}

struct ToolOut {
    content: Value,
    summary: String,
}

impl ToolOut {
    fn text(s: impl Into<String>) -> ToolOut {
        let s = s.into();
        ToolOut { summary: summarize(&s), content: Value::String(s) }
    }
}

fn summarize(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(MAX_SUMMARY) {
        Some((cut, _)) => format!("{}…", &one_line[..cut]),
        None => one_line,
    }
}

fn file_tool(st: &AppState, t: &Tenant, name: &str, input: &Value, changes: &mut Vec<String>) -> String {
    let path = || input.get("path").and_then(|p| p.as_str()).and_then(clean_path);
    match name {
        "list_files" => t.list().iter().map(|e| format!("{} ({} bytes)", e.path, e.size)).collect::<Vec<_>>().join("\n"),
        "read_file" => match path().and_then(|p| t.read(&p)) {
            Some(bytes) if bytes.len() <= MAX_READ => String::from_utf8_lossy(&bytes).into_owned(),
            Some(_) => "error: file too large to read".into(),
            None => "error: no such file".into(),
        },
        "write_file" => {
            let Some(p) = path() else { return "error: bad path".into() };
            let content = input.get("content").and_then(|c| c.as_str()).unwrap_or("");
            match t.write(&p, content.as_bytes().to_vec()) {
                Ok(_) => {
                    changes.push(p.clone());
                    format!("wrote {p} ({} bytes)", content.len())
                }
                Err(e) => format!("error: {e}"),
            }
        }
        "delete_file" => {
            let Some(p) = path() else { return "error: bad path".into() };
            match t.delete(&p) {
                Ok(_) => {
                    changes.push(p.clone());
                    format!("deleted {p}")
                }
                Err(e) => format!("error: {e}"),
            }
        }
        "check_site" => check_tenant(st, t).to_string(),
        _ => format!("error: unknown tool {name}"),
    }
}

async fn run_tool(st: &State, t: &Arc<Tenant>, name: &str, input: &Value, changes: &mut Vec<String>) -> ToolOut {
    if name == "screenshot" {
        return screenshot(st, t, input).await;
    }
    let (st, t, name, input) = (st.clone(), t.clone(), name.to_string(), input.clone());
    let (text, written) = tokio::task::spawn_blocking(move || {
        let mut written = Vec::new();
        let text = file_tool(&st, &t, &name, &input, &mut written);
        (text, written)
    })
    .await
    .unwrap_or_else(|e| (format!("error: {e}"), Vec::new()));
    changes.extend(written);
    ToolOut::text(text)
}

/// A path the model asked for, as a site path. Chrome takes the URL as one argument, and the URL
/// always begins with the tenant's own origin, so this only has to keep out whitespace and
/// anything that would read as a second URL.
fn shot_path(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.len() > 512 || raw.starts_with("//") || raw.contains("://") || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    Some(if raw.starts_with('/') { raw.to_string() } else { format!("/{raw}") })
}

async fn screenshot(st: &AppState, t: &Tenant, input: &Value) -> ToolOut {
    let Some(chrome) = st.chrome.clone() else {
        return ToolOut::text("error: the screenshot tool is not configured; the daemon was started without --chrome");
    };
    let Some(path) = shot_path(input.get("path").and_then(|p| p.as_str()).unwrap_or("/")) else {
        return ToolOut::text("error: bad path; pass a site path such as /blog");
    };
    let scratch = match Scratch::new() {
        Ok(s) => s,
        Err(e) => return ToolOut::text(format!("error: cannot create a temporary directory: {e}")),
    };
    let png = scratch.dir.join("shot.png");
    let url = preview_url_path(st, &t.id, &path);
    let child = Command::new(&chrome)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--no-sandbox")
        .arg("--hide-scrollbars")
        .arg(format!("--window-size={SHOT_SIZE}"))
        // a preview renders itself after load, so the shot has to wait for the page's own clock
        .arg(format!("--virtual-time-budget={}", VIRTUAL_TIME.as_millis()))
        // its own profile: chrome refuses to start a second instance on a profile already in use
        .arg(format!("--user-data-dir={}", scratch.dir.display()))
        .arg(format!("--screenshot={}", png.display()))
        .arg(format!("{url}{}sl_shot=1", if url.contains('?') { '&' } else { '?' }))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => return ToolOut::text(format!("error: cannot run {}: {e}", chrome.display())),
    };
    let finished = match tokio::time::timeout(SHOT_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return ToolOut::text(format!("error: chrome failed: {e}")),
        Err(_) => return ToolOut::text(format!("error: chrome did not finish within {}s", SHOT_TIMEOUT.as_secs())),
    };
    let Ok(bytes) = std::fs::read(&png) else {
        let stderr = String::from_utf8_lossy(&finished.stderr);
        return ToolOut::text(format!("error: chrome wrote no screenshot ({}): {}", finished.status, summarize(&stderr)));
    };
    if bytes.len() > MAX_SHOT {
        return ToolOut::text(format!("error: the screenshot is {} bytes, more than the {MAX_SHOT} byte limit", bytes.len()));
    }
    let note = format!("screenshot of {path} at {SHOT_SIZE}, {} kB", bytes.len() / 1024);
    ToolOut {
        content: json!([
            { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": base64(&bytes) } },
            { "type": "text", "text": note.clone() },
        ]),
        summary: note,
    }
}

/// A temporary directory that goes away with the value, however the tool returned.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new() -> io::Result<Scratch> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("sandbox-lite-shot-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir)?;
        Ok(Scratch { dir })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= chunk.len() { ALPHABET[(n >> shift) as usize & 63] as char } else { '=' });
        }
    }
    out
}

/// Where the answer goes while it is being produced: an SSE channel, or nowhere for the JSON path.
struct Emitter {
    tx: Option<mpsc::Sender<Result<Event, Infallible>>>,
}

impl Emitter {
    async fn send(&self, event: &str, data: Value) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Ok(Event::default().event(event).data(data.to_string()))).await;
        }
    }

    fn gone(&self) -> bool {
        self.tx.as_ref().is_some_and(|tx| tx.is_closed())
    }
}

struct Fail {
    status: StatusCode,
    error: Value,
}

impl Fail {
    fn gateway(message: impl Into<String>) -> Fail {
        Fail { status: StatusCode::BAD_GATEWAY, error: Value::String(message.into()) }
    }
}

struct Answer {
    text: String,
    changes: Vec<String>,
    iterations: usize,
    tools: Vec<ToolCall>,
}

/// The tool loop. Every turn is streamed from the API, forwarded to `out` as it arrives, and put
/// back together as content blocks so the next request can echo it as the assistant's message.
async fn converse(st: &State, t: &Arc<Tenant>, key: &str, mut messages: Vec<Value>, out: &Emitter) -> Result<Answer, Fail> {
    let client = reqwest::Client::new();
    let mut changes: Vec<String> = Vec::new();
    let mut tools_used: Vec<ToolCall> = Vec::new();
    let mut text = String::new();
    let mut iterations = 0;
    while iterations < MAX_ITERATIONS && !out.gone() {
        iterations += 1;
        let body = json!({
            "model": st.model,
            "max_tokens": 8192,
            "system": system_prompt(t),
            "tools": tools(st),
            "messages": messages,
            "stream": true,
        });
        let resp = client
            .post(format!("{}/v1/messages", st.api_base))
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => return Err(Fail::gateway(e.to_string())),
        };
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Fail { status: StatusCode::BAD_GATEWAY, error: serde_json::from_str(&body).unwrap_or(Value::String(body)) });
        }
        let mut frames = Frames::default();
        let mut reply = Reply::default();
        let mut stream = resp.bytes_stream();
        'turn: while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => return Err(Fail::gateway(e.to_string())),
            };
            for data in frames.push(&chunk) {
                let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
                match reply.event(&v) {
                    Some(Emit::Text(delta)) => out.send("text", json!({ "text": delta })).await,
                    Some(Emit::Tool { name, input }) => {
                        let summary =
                            input.get("path").and_then(|p| p.as_str()).map(str::to_string).unwrap_or_else(|| summarize(&input.to_string()));
                        out.send("tool", json!({ "name": name, "input": summary })).await;
                    }
                    Some(Emit::Error(_)) => break 'turn,
                    None => {}
                }
            }
        }
        if let Some(message) = reply.error() {
            return Err(Fail::gateway(message.to_string()));
        }
        if !reply.done() {
            return Err(Fail::gateway("the upstream stream ended before the message did"));
        }
        let turn = reply.text();
        if !turn.is_empty() {
            text = turn;
        }
        if reply.stop_reason() != Some("tool_use") {
            break;
        }
        messages.push(json!({ "role": "assistant", "content": reply.content() }));
        let mut results = Vec::new();
        for (id, name, input) in reply.tool_calls() {
            let done = run_tool(st, t, &name, &input, &mut changes).await;
            tools_used.push(ToolCall { name: name.clone(), path: input.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string() });
            out.send("tool_result", json!({ "name": name, "result": done.summary })).await;
            results.push(json!({ "type": "tool_result", "tool_use_id": id, "content": done.content }));
        }
        messages.push(json!({ "role": "user", "content": results }));
    }
    changes.sort();
    changes.dedup();
    Ok(Answer { text, changes, iterations, tools: tools_used })
}

/// Runs one request end to end and records it. The value is the `done` payload, which is also the
/// JSON body of the non-streaming reply.
async fn run(st: State, t: Arc<Tenant>, key: String, req: ChatReq, mut conv: Conversation, out: Emitter) -> Result<Value, Fail> {
    let mut messages = conv.for_model();
    messages.extend(req.messages.iter().map(|m| json!({ "role": m.role, "content": m.content })));
    let answer = converse(&st, &t, &key, messages, &out).await?;
    for m in &req.messages {
        conv.push(Turn { role: m.role.clone(), text: m.content.clone(), tools: Vec::new() });
    }
    conv.push(Turn { role: "assistant".into(), text: answer.text.clone(), tools: answer.tools });
    if let Err(e) = st.chats.save(&t, &conv) {
        eprintln!("tenant {}: cannot save chat {}: {e}", t.id, conv.id);
    }
    Ok(json!({
        "text": answer.text,
        "changes": answer.changes,
        "iterations": answer.iterations,
        "version": t.version(),
        "chat": conv.id,
    }))
}

pub async fn chat(AxState(st): AxState<State>, Path(id): Path<String>, headers: HeaderMap, Json(req): Json<ChatReq>) -> Response {
    let Some(key) = st.api_key.clone() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "ANTHROPIC_API_KEY is not set on the daemon");
    };
    let Some(t) = st.store.tenant(&id) else { return err(StatusCode::NOT_FOUND, "unknown tenant") };
    let conv = match &req.chat {
        Some(chat) => match st.chats.load(&t, chat) {
            Some(c) => c,
            None => return err(StatusCode::NOT_FOUND, "unknown chat"),
        },
        None => Conversation::new(super::chats::new_id()),
    };
    if conv.messages.is_empty() && req.messages.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no messages");
    }
    let accept = headers.get(header::ACCEPT).and_then(|a| a.to_str().ok()).unwrap_or("");
    if !accept.contains("text/event-stream") {
        return match run(st, t, key, req, conv, Emitter { tx: None }).await {
            Ok(done) => Json(done).into_response(),
            Err(f) => (f.status, Json(json!({ "error": f.error }))).into_response(),
        };
    }
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let out = Emitter { tx: Some(tx.clone()) };
        let (event, data) = match run(st, t, key, req, conv, out).await {
            Ok(done) => ("done", done),
            Err(f) => ("error", json!({ "error": f.error })),
        };
        let _ = tx.send(Ok(Event::default().event(event).data(data.to_string()))).await;
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Instant;

    use axum::Router;
    use axum::body::to_bytes;
    use axum::routing::post;

    use super::*;
    use crate::store::{Base, Store};
    use crate::transform::{Config, Engine};

    const TEST_QUOTA: u64 = 1 << 20;

    fn events(list: &[Value]) -> String {
        list.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or(""))).collect()
    }

    /// A turn that says one sentence and asks for a `write_file`, streamed the way the API does it.
    fn turn_with_tool() -> String {
        events(&[
            json!({"type":"message_start","message":{"id":"msg_1","role":"assistant","content":[]}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Adding "}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"the page."}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"write_file","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/pages/new.astro\","}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"content\":\"<h1>hi</h1>\"}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
            json!({"type":"message_stop"}),
        ])
    }

    fn turn_with_text(text: &str) -> String {
        events(&[
            json!({"type":"message_start","message":{"id":"msg_2","role":"assistant","content":[]}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            json!({"type":"message_stop"}),
        ])
    }

    /// Replays `turns` in order and records every request body it was sent.
    async fn upstream(turns: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move |Json(body): Json<Value>| {
                let (recorder, turns) = (recorder.clone(), turns.clone());
                async move {
                    let n = {
                        let mut seen = recorder.lock().unwrap();
                        seen.push(body);
                        seen.len()
                    };
                    let body = turns.get(n - 1).cloned().unwrap_or_else(|| turn_with_text("(unexpected extra turn)"));
                    ([(header::CONTENT_TYPE, "text/event-stream")], body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    struct Fixture {
        st: State,
        seen: Arc<Mutex<Vec<Value>>>,
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    async fn fixture(turns: Vec<String>, persist: bool) -> Fixture {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!("sandbox-lite-chat-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let (base_dir, data_dir) = (root.join("base"), root.join("data"));
        std::fs::create_dir_all(base_dir.join("src/pages")).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(base_dir.join("src/pages/index.astro"), "<h1>home</h1>").unwrap();
        let store = Store::new(persist.then(|| data_dir.clone()), TEST_QUOTA);
        store.add_base(Base::load("test", &base_dir).unwrap());
        store.create_tenant("acme", "test").unwrap();
        let (api_base, seen) = upstream(turns).await;
        let metrics = Arc::new(crate::metrics::Metrics::default());
        let st = Arc::new(AppState {
            store,
            engine: Engine::new(Config { cache_bytes: 1 << 20, ..Config::default() }, metrics.clone()),
            metrics,
            chats: super::super::chats::Chats::default(),
            domain: "localhost".into(),
            port: 4321,
            model: "test-model".into(),
            api_key: Some("test-key".into()),
            api_base,
            api_token: None,
            preview_secret: None,
            cookie_samesite: super::super::SameSite::Lax,
            chrome: None,
            started: Instant::now(),
        });
        Fixture { st, seen, root }
    }

    async fn post_chat(st: &State, accept: Option<&str>, req: ChatReq) -> (StatusCode, String) {
        let mut headers = HeaderMap::new();
        if let Some(accept) = accept {
            headers.insert(header::ACCEPT, accept.parse().unwrap());
        }
        let resp = chat(AxState(st.clone()), Path("acme".into()), headers, Json(req)).await;
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    fn ask(text: &str, chat: Option<&str>) -> ChatReq {
        ChatReq { messages: vec![ChatMsg { role: "user".into(), content: text.into() }], chat: chat.map(str::to_string) }
    }

    /// `(event name, payload)` for each frame of an SSE body.
    fn parse_sse(body: &str) -> Vec<(String, Value)> {
        body.split("\n\n")
            .filter(|f| !f.trim().is_empty())
            .map(|frame| {
                let field = |name: &str| {
                    frame
                        .lines()
                        .filter_map(|l| l.strip_prefix(name))
                        .map(|v| v.strip_prefix(' ').unwrap_or(v).to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                (field("event:"), serde_json::from_str(&field("data:")).unwrap_or(Value::Null))
            })
            .filter(|(name, _)| !name.is_empty())
            .collect()
    }

    #[tokio::test]
    async fn streaming_forwards_text_tool_calls_and_a_done_event() {
        let f = fixture(vec![turn_with_tool(), turn_with_text("Done: the page is there.")], true).await;
        let (status, body) = post_chat(&f.st, Some("text/event-stream"), ask("add a page", None)).await;
        assert_eq!(status, StatusCode::OK);
        let frames = parse_sse(&body);
        let named = |name: &str| frames.iter().filter(|(n, _)| n == name).map(|(_, v)| v.clone()).collect::<Vec<_>>();

        let text: String = named("text").iter().filter_map(|v| v["text"].as_str().map(str::to_string)).collect();
        assert_eq!(text, "Adding the page.Done: the page is there.");
        assert_eq!(named("tool"), vec![json!({"name":"write_file","input":"src/pages/new.astro"})]);
        assert_eq!(named("tool_result").len(), 1);
        assert_eq!(named("tool_result")[0]["name"], "write_file");
        assert!(named("tool_result")[0]["result"].as_str().unwrap().starts_with("wrote src/pages/new.astro"));

        let done = named("done");
        assert_eq!(done.len(), 1, "exactly one done event: {frames:?}");
        assert_eq!(done[0]["text"], "Done: the page is there.");
        assert_eq!(done[0]["changes"], json!(["src/pages/new.astro"]));
        assert_eq!(done[0]["iterations"], 2);
        assert!(done[0]["version"].as_u64().unwrap() > 0);

        let t = f.st.store.tenant("acme").unwrap();
        assert_eq!(t.read_text("src/pages/new.astro").as_deref(), Some("<h1>hi</h1>"));

        // the second request has to carry the first turn's blocks back, with the tool result after them
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["stream"], true);
        let messages = seen[1]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0], json!({"role":"user","content":"add a page"}));
        assert_eq!(
            messages[1],
            json!({"role":"assistant","content":[
                {"type":"text","text":"Adding the page."},
                {"type":"tool_use","id":"toolu_1","name":"write_file","input":{"path":"src/pages/new.astro","content":"<h1>hi</h1>"}},
            ]})
        );
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");

        let chat_id = done[0]["chat"].as_str().unwrap();
        let stored = f.st.chats.load(&t, chat_id).expect("the conversation was saved");
        assert_eq!(stored.title, "add a page");
        assert_eq!(stored.messages[0], Turn { role: "user".into(), text: "add a page".into(), tools: vec![] });
        assert_eq!(
            stored.messages[1],
            Turn {
                role: "assistant".into(),
                text: "Done: the page is there.".into(),
                tools: vec![ToolCall { name: "write_file".into(), path: "src/pages/new.astro".into() }],
            }
        );
        assert_eq!(f.st.chats.list(&t).len(), 1);
    }

    #[tokio::test]
    async fn without_the_sse_accept_header_the_reply_is_the_old_json_shape() {
        let f = fixture(vec![turn_with_tool(), turn_with_text("All done.")], true).await;
        let (status, body) = post_chat(&f.st, None, ask("add a page", None)).await;
        assert_eq!(status, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["text"], "All done.");
        assert_eq!(v["changes"], json!(["src/pages/new.astro"]));
        assert_eq!(v["iterations"], 2);
        assert!(v["version"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn a_stored_conversation_is_replayed_when_it_is_continued() {
        let f = fixture(vec![turn_with_text("First."), turn_with_text("Second.")], false).await;
        let (_, first) = post_chat(&f.st, None, ask("one", None)).await;
        let chat_id = serde_json::from_str::<Value>(&first).unwrap()["chat"].as_str().unwrap().to_string();
        let (status, second) = post_chat(&f.st, None, ask("two", Some(&chat_id))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(serde_json::from_str::<Value>(&second).unwrap()["chat"], chat_id);

        let seen = f.seen.lock().unwrap();
        assert_eq!(
            seen[1]["messages"],
            json!([
                {"role":"user","content":"one"},
                {"role":"assistant","content":"First."},
                {"role":"user","content":"two"},
            ])
        );
        let t = f.st.store.tenant("acme").unwrap();
        assert_eq!(f.st.chats.load(&t, &chat_id).unwrap().messages.len(), 4, "in memory, without a data dir");
        assert_eq!(f.st.chats.list(&t).len(), 1);
    }

    #[tokio::test]
    async fn an_unknown_chat_is_a_404_and_an_unreachable_api_a_502() {
        let f = fixture(Vec::new(), true).await;
        let (status, body) = post_chat(&f.st, None, ask("hi", Some("nope"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["error"], "unknown chat");

        let metrics = Arc::new(crate::metrics::Metrics::default());
        let broken = Arc::new(AppState {
            store: Store::new(None, TEST_QUOTA),
            engine: Engine::new(Config { cache_bytes: 1 << 20, ..Config::default() }, metrics.clone()),
            metrics,
            chats: super::super::chats::Chats::default(),
            domain: "localhost".into(),
            port: 4321,
            model: "m".into(),
            api_key: Some("k".into()),
            api_base: "http://127.0.0.1:1".into(),
            api_token: None,
            preview_secret: None,
            cookie_samesite: super::super::SameSite::Lax,
            chrome: None,
            started: Instant::now(),
        });
        broken.store.add_base(Base::load("test", &f.root.join("base")).unwrap());
        broken.store.create_tenant("acme", "test").unwrap();
        let (status, body) = post_chat(&broken, None, ask("hi", None)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(serde_json::from_str::<Value>(&body).unwrap()["error"].is_string());
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (input, want) in
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(base64(input.as_bytes()), want);
        }
        assert_eq!(base64(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(base64(&[0, 0, 0]), "AAAA");
    }

    #[test]
    fn screenshot_paths_are_site_paths() {
        assert_eq!(shot_path("/blog").as_deref(), Some("/blog"));
        assert_eq!(shot_path("blog").as_deref(), Some("/blog"));
        assert_eq!(shot_path(" /a?b=1 ").as_deref(), Some("/a?b=1"));
        assert_eq!(shot_path("--headless").as_deref(), Some("/--headless"));
        assert_eq!(shot_path("//evil.example"), None);
        assert_eq!(shot_path("http://evil.example"), None);
        assert_eq!(shot_path("/a b"), None);
        assert_eq!(shot_path(&"/x".repeat(400)), None);
    }
}
