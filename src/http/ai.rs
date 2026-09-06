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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::{Semaphore, mpsc};
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

/// Screenshots that may run at once. One headless Chrome costs more memory than every tenant on
/// the daemon put together, so the default is the smallest number that still works.
pub const DEFAULT_CHROME_JOBS: usize = 1;
/// How long a screenshot waits for a free slot before telling the model the tool is busy. Short
/// enough that the model gets an answer rather than a stalled tool call.
const SHOT_WAIT: Duration = Duration::from_secs(10);

/// Tokens the summary of a compacted conversation may use, and what one of its turns contributes
/// to the transcript the summary is written from. Both bound a request whose whole point is that
/// the conversation was already too long to send.
const SUMMARY_TOKENS: u32 = 512;
const SUMMARY_TURN_CHARS: usize = 1000;
const SUMMARY_TRANSCRIPT: usize = 24 << 10;
const SUMMARY_PROMPT: &str = "You compress the earlier part of a conversation between a customer and an assistant that edits their website. \
Reply with a short summary — a few sentences, no preamble — of what the customer asked for, what was changed and what is still open, \
so the assistant can carry on without the full transcript.";

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
        t.id,
        t.base().name
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
        "read_file" => match path().map(|p| t.read(&p)).transpose() {
            Ok(Some(Some(bytes))) if bytes.len() <= MAX_READ => String::from_utf8_lossy(&bytes).into_owned(),
            Ok(Some(Some(_))) => "error: file too large to read".into(),
            Ok(_) => "error: no such file".into(),
            Err(e) => format!("error: {e}"),
        },
        "write_file" => {
            let Some(p) = path() else { return "error: bad path".into() };
            let content = input.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let kind = st.engine.update_kind(t, &p, content.as_bytes());
            match t.write(&p, content.as_bytes().to_vec(), kind) {
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

#[derive(Serialize, Clone, Copy)]
pub struct ShotStats {
    pub running: usize,
    pub limit: usize,
    pub busy: u64,
    pub timeouts: u64,
}

/// The screenshot tool, bounded. Each call spawns a browser with its own profile directory, so
/// only `jobs` of them run at once and a call that cannot get a slot within `wait` tells the model
/// the tool is busy rather than queueing behind an unbounded number of browsers.
pub struct Shots {
    permits: Semaphore,
    jobs: usize,
    wait: Duration,
    deadline: Duration,
    busy: AtomicU64,
    timeouts: AtomicU64,
}

impl Default for Shots {
    fn default() -> Shots {
        Shots::new(DEFAULT_CHROME_JOBS)
    }
}

impl Shots {
    pub fn new(jobs: usize) -> Shots {
        Shots::with(jobs, SHOT_WAIT, SHOT_TIMEOUT)
    }

    fn with(jobs: usize, wait: Duration, deadline: Duration) -> Shots {
        let jobs = jobs.max(1);
        Shots { permits: Semaphore::new(jobs), jobs, wait, deadline, busy: AtomicU64::new(0), timeouts: AtomicU64::new(0) }
    }

    pub fn stats(&self) -> ShotStats {
        ShotStats {
            running: self.jobs.saturating_sub(self.permits.available_permits()),
            limit: self.jobs,
            busy: self.busy.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
        }
    }
}

async fn screenshot(st: &AppState, t: &Tenant, input: &Value) -> ToolOut {
    let Some(chrome) = st.chrome.clone() else {
        return ToolOut::text("error: the screenshot tool is not configured; the daemon was started without --chrome");
    };
    let Some(path) = shot_path(input.get("path").and_then(|p| p.as_str()).unwrap_or("/")) else {
        return ToolOut::text("error: bad path; pass a site path such as /blog");
    };
    let permit = match tokio::time::timeout(st.shots.wait, st.shots.permits.acquire()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(e)) => return ToolOut::text(format!("error: the screenshot tool is closed: {e}")),
        Err(_) => {
            st.shots.busy.fetch_add(1, Ordering::Relaxed);
            let (jobs, waited) = (st.shots.jobs, st.shots.wait.as_secs());
            return ToolOut::text(format!(
                "error: the screenshot tool is busy; {jobs} may run at once and none finished within {waited}s. Try again in a moment."
            ));
        }
    };
    let out = shoot(st, &chrome, t, &path).await;
    drop(permit);
    out
}

async fn shoot(st: &AppState, chrome: &std::path::Path, t: &Tenant, path: &str) -> ToolOut {
    let scratch = match Scratch::new() {
        Ok(s) => s,
        Err(e) => return ToolOut::text(format!("error: cannot create a temporary directory: {e}")),
    };
    let png = scratch.dir.join("shot.png");
    // chrome's stderr goes to a file in the profile rather than a pipe: nothing has to drain it
    // while the browser runs, and it is removed with everything else
    let log = scratch.dir.join("chrome.log");
    let errors = match std::fs::File::create(&log) {
        Ok(f) => f,
        Err(e) => return ToolOut::text(format!("error: cannot create a temporary file: {e}")),
    };
    let url = preview_url_path(st, &t.id, path);
    let child = Command::new(chrome)
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
        .stderr(Stdio::from(errors))
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return ToolOut::text(format!("error: cannot run {}: {e}", chrome.display())),
    };
    let status = match tokio::time::timeout(st.shots.deadline, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return ToolOut::text(format!("error: chrome failed: {e}")),
        Err(_) => {
            st.shots.timeouts.fetch_add(1, Ordering::Relaxed);
            // killed and reaped here rather than left to `kill_on_drop`, so that nothing is still
            // writing into the profile directory when `Scratch` removes it
            let _ = child.kill().await;
            return ToolOut::text(format!("error: chrome did not finish within {}s", st.shots.deadline.as_secs()));
        }
    };
    let Ok(bytes) = std::fs::read(&png) else {
        let stderr = std::fs::read_to_string(&log).unwrap_or_default();
        return ToolOut::text(format!("error: chrome wrote no screenshot ({status}): {}", summarize(&stderr)));
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
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            eprintln!("cannot remove the screenshot directory {}: {e}", self.dir.display());
        }
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

/// The turns past the window, as one transcript for the summarizer. Newest first while the budget
/// is spent, so what survives the cut is the part closest to the conversation still going on.
fn transcript(previous: &str, turns: &[Turn]) -> String {
    let mut budget = SUMMARY_TRANSCRIPT;
    let mut lines: Vec<String> = Vec::new();
    for turn in turns.iter().rev().filter(|t| !t.text.trim().is_empty()) {
        let line = format!("{}: {}", turn.role, turn.text.chars().take(SUMMARY_TURN_CHARS).collect::<String>());
        let Some(left) = budget.checked_sub(line.len()) else { break };
        budget = left;
        lines.push(line);
    }
    lines.reverse();
    match previous.trim() {
        "" => lines.join("\n\n"),
        earlier => format!("Summary of what came before this transcript:\n{earlier}\n\nTranscript:\n{}", lines.join("\n\n")),
    }
}

/// Folds everything past the window into the conversation's stored summary, with one call to the
/// same API. It runs when a conversation crosses the window and not on the turns between, so the
/// summary is written roughly once every half window rather than once per turn. A summary the API
/// will not write is not fatal: the turns stay stored, and `for_model` cuts them out of the
/// request instead.
async fn compact(st: &State, key: &str, conv: &mut Conversation) {
    let drop = conv.overflow(st.chats.window());
    if drop == 0 {
        return;
    }
    let prompt = transcript(&conv.summary, &conv.messages[..drop]);
    match summarize_conversation(st, key, prompt).await {
        Some(summary) => conv.compact(drop, summary),
        None => eprintln!("chat {}: cannot summarize the {drop} oldest turns; they are left out of the request instead", conv.id),
    }
}

async fn summarize_conversation(st: &State, key: &str, prompt: String) -> Option<String> {
    let body = json!({
        "model": st.model,
        "max_tokens": SUMMARY_TOKENS,
        "system": SUMMARY_PROMPT,
        "messages": [{ "role": "user", "content": prompt }],
    });
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/messages", st.api_base))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let reply: Value = resp.json().await.ok()?;
    let text = reply["content"]
        .as_array()?
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Runs one request end to end and records it. The value is the `done` payload, which is also the
/// JSON body of the non-streaming reply.
async fn run(st: State, t: Arc<Tenant>, key: String, req: ChatReq, mut conv: Conversation, out: Emitter) -> Result<Value, Fail> {
    compact(&st, &key, &mut conv).await;
    let mut messages = conv.for_model(st.chats.window());
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
            Ok(Some(c)) => c,
            Ok(None) => return err(StatusCode::NOT_FOUND, "unknown chat"),
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
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

    /// What the stub answers a summarize request with; the tool loop never asks for one.
    const STUB_SUMMARY: &str = "The customer asked for a green headline and got one.";
    /// A turn carrying this makes the stub refuse to summarize it.
    const REFUSE: &str = "(unsummarizable)";

    /// Replays `turns` in order and records every request body it was sent. A request that did not
    /// ask for a stream is the summarizer's: it is answered as one plain message and does not
    /// consume a turn, unless what it was asked to summarize says to refuse.
    async fn upstream(turns: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move |Json(body): Json<Value>| {
                let (recorder, turns) = (recorder.clone(), turns.clone());
                async move {
                    let streaming = body["stream"] == true;
                    let refuse = !streaming && body["messages"][0]["content"].as_str().is_some_and(|p| p.contains(REFUSE));
                    let n = {
                        let mut seen = recorder.lock().unwrap();
                        seen.push(body);
                        seen.iter().filter(|b| b["stream"] == true).count()
                    };
                    if refuse {
                        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "no" }))).into_response();
                    }
                    if !streaming {
                        let reply = json!({ "content": [{ "type": "text", "text": STUB_SUMMARY }] });
                        return ([(header::CONTENT_TYPE, "application/json")], reply.to_string()).into_response();
                    }
                    let body = turns.get(n - 1).cloned().unwrap_or_else(|| turn_with_text("(unexpected extra turn)"));
                    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
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
        fixture_with(turns, persist, None, Shots::default()).await
    }

    async fn fixture_with(turns: Vec<String>, persist: bool, chrome: Option<PathBuf>, shots: Shots) -> Fixture {
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
            chrome,
            shots,
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
        assert_eq!(t.read_text("src/pages/new.astro").unwrap().as_deref(), Some("<h1>hi</h1>"));

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
        let stored = f.st.chats.load(&t, chat_id).unwrap().expect("the conversation was saved");
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
        assert_eq!(f.st.chats.list(&t).unwrap().len(), 1);
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
        assert_eq!(f.st.chats.load(&t, &chat_id).unwrap().unwrap().messages.len(), 4, "in memory, without a data dir");
        assert_eq!(f.st.chats.list(&t).unwrap().len(), 1);
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
            shots: super::super::ai::Shots::default(),
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

    /// Issue #45: replaying every stored turn grows the request until the model's context is the
    /// limit. Past the window the older turns become one summary, written by the same API.
    #[tokio::test]
    async fn a_conversation_past_the_window_is_summarized_once_and_the_summary_is_stored() {
        let f = fixture(vec![turn_with_text("First."), turn_with_text("Second.")], true).await;
        let t = f.st.store.tenant("acme").unwrap();
        let window = f.st.chats.window();
        let mut old = Conversation::new("old".into());
        for i in 0..window * 2 {
            old.push(Turn { role: if i % 2 == 0 { "user" } else { "assistant" }.into(), text: format!("turn {i}"), tools: Vec::new() });
        }
        f.st.chats.save(&t, &old).unwrap();
        let dropped = old.overflow(window);

        let (status, _) = post_chat(&f.st, None, ask("carry on", Some("old"))).await;
        assert_eq!(status, StatusCode::OK);

        let stored = f.st.chats.load(&t, "old").unwrap().unwrap();
        assert_eq!(stored.summary, STUB_SUMMARY);
        assert_eq!(stored.messages.len(), window * 2 - dropped + 2, "the window, plus this request's two turns");
        assert_eq!(stored.messages[0].text, format!("turn {dropped}"));

        let seen = f.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "one summarize call and one turn of the tool loop");
        assert!(seen[0]["stream"].is_null(), "the summarizer does not stream: {}", seen[0]);
        assert_eq!(seen[0]["messages"].as_array().unwrap().len(), 1);
        let prompt = seen[0]["messages"][0]["content"].as_str().unwrap();
        assert!(prompt.contains("turn 0") && prompt.contains(&format!("turn {}", dropped - 1)), "{prompt}");
        assert!(!prompt.contains(&format!("turn {dropped}")), "the turns that stay are not summarized: {prompt}");

        let sent = seen[1]["messages"].as_array().unwrap();
        // the summary joins the user turn that follows it, so this is the turns left plus the new message
        assert_eq!(sent.len(), window * 2 - dropped + 1);
        assert!(sent[0]["content"].as_str().unwrap().contains(STUB_SUMMARY), "{}", sent[0]);
        assert_eq!(sent[sent.len() - 1], json!({"role":"user","content":"carry on"}));
    }

    /// The summary is written once, when the conversation crosses the window — not on the turns
    /// after it, when replaying it again would cost a call per message.
    #[tokio::test]
    async fn the_summary_is_not_rewritten_on_every_turn() {
        let f = fixture(vec![turn_with_text("First."), turn_with_text("Second."), turn_with_text("Third.")], false).await;
        let t = f.st.store.tenant("acme").unwrap();
        let mut old = Conversation::new("old".into());
        for i in 0..f.st.chats.window() + 1 {
            old.push(Turn { role: if i % 2 == 0 { "user" } else { "assistant" }.into(), text: format!("turn {i}"), tools: Vec::new() });
        }
        f.st.chats.save(&t, &old).unwrap();
        for _ in 0..3 {
            assert_eq!(post_chat(&f.st, None, ask("more", Some("old"))).await.0, StatusCode::OK);
        }
        let summarize_calls = f.seen.lock().unwrap().iter().filter(|b| b["stream"].is_null()).count();
        assert_eq!(summarize_calls, 1, "three requests, one summary");
    }

    /// A summary the API will not write must not lose the turns it was meant to replace — and the
    /// request has to be bounded anyway, so `for_model` cuts them out instead.
    #[tokio::test]
    async fn a_summary_the_api_refuses_leaves_the_turns_stored_and_out_of_the_request() {
        let f = fixture(vec![turn_with_text("Fine.")], false).await;
        let t = f.st.store.tenant("acme").unwrap();
        let window = f.st.chats.window();
        let mut old = Conversation::new("old".into());
        for i in 0..window * 4 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            old.push(Turn { role: role.into(), text: format!("{REFUSE} turn {i}"), tools: Vec::new() });
        }
        f.st.chats.save(&t, &old).unwrap();

        let (status, _) = post_chat(&f.st, None, ask("carry on", Some("old"))).await;
        assert_eq!(status, StatusCode::OK, "a summary that cannot be written is not a failed request");

        let stored = f.st.chats.load(&t, "old").unwrap().unwrap();
        assert_eq!(stored.summary, "");
        assert_eq!(stored.messages.len(), window * 4 + 2, "no turn is dropped without a summary to stand for it");
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen[1]["messages"].as_array().unwrap().len(), window + 1, "the request is bounded by the window regardless");
    }

    /// A stand-in for chrome: it records the profile directory it was handed, sleeps, and writes
    /// the file it was told to screenshot. Chrome itself is not on the machine that runs these,
    /// and what the limiter has to bound is exactly this — a process that takes time and leaves a
    /// profile directory behind.
    #[cfg(unix)]
    struct Stub {
        dir: PathBuf,
        bin: PathBuf,
        report: PathBuf,
    }

    #[cfg(unix)]
    impl Stub {
        fn new(sleep: &str, write_png: bool) -> Stub {
            use std::os::unix::fs::PermissionsExt;
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir =
                std::env::temp_dir().join(format!("sandbox-lite-stub-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&dir).unwrap();
            let (bin, report) = (dir.join("chrome"), dir.join("profiles"));
            let png = if write_png { "echo PNGSTUB > \"$out\"" } else { ":" };
            let script = format!(
                "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --user-data-dir=*) echo \"${{arg#--user-data-dir=}}\" >> \"{}\" ;;\n    --screenshot=*) out=\"${{arg#--screenshot=}}\" ;;\n  esac\ndone\nsleep {sleep}\n{png}\n",
                report.display()
            );
            std::fs::write(&bin, script).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            Stub { dir, bin, report }
        }

        /// Every profile directory the stub was started with, in order.
        fn profiles(&self) -> Vec<PathBuf> {
            std::fs::read_to_string(&self.report).unwrap_or_default().lines().map(PathBuf::from).collect()
        }
    }

    #[cfg(unix)]
    impl Drop for Stub {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Issue #46: N concurrent chats used to spawn N browsers.
    #[cfg(unix)]
    #[tokio::test]
    async fn only_so_many_screenshots_run_at_once_and_the_rest_are_told_to_try_again() {
        let stub = Stub::new("0.6", true);
        let shots = Shots::with(1, Duration::from_millis(150), Duration::from_secs(20));
        let f = fixture_with(Vec::new(), false, Some(stub.bin.clone()), shots).await;
        let t = f.st.store.tenant("acme").unwrap();
        let input = json!({ "path": "/" });
        let (first, second) = tokio::join!(screenshot(&f.st, &t, &input), screenshot(&f.st, &t, &input));

        let answers = [first.summary, second.summary];
        assert!(answers.iter().any(|a| a.starts_with("screenshot of /")), "{answers:?}");
        assert!(answers.iter().any(|a| a.contains("the screenshot tool is busy")), "{answers:?}");
        let stats = f.st.shots.stats();
        assert_eq!((stats.busy, stats.running, stats.limit), (1, 0, 1));
        assert_eq!(stub.profiles().len(), 1, "the refused call never started a browser");
        for dir in stub.profiles() {
            assert!(!dir.exists(), "{} was left behind", dir.display());
        }
    }

    /// The profile directory and the PNG go with the value, and the browser is killed and reaped
    /// before they do, so nothing is still writing into the directory when it is removed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_screenshot_that_overruns_its_deadline_is_killed_and_leaves_nothing_behind() {
        let stub = Stub::new("5", false);
        let shots = Shots::with(1, Duration::from_secs(1), Duration::from_millis(250));
        let f = fixture_with(Vec::new(), false, Some(stub.bin.clone()), shots).await;
        let t = f.st.store.tenant("acme").unwrap();
        let started = Instant::now();
        let out = screenshot(&f.st, &t, &json!({ "path": "/blog" })).await;

        assert!(out.summary.contains("did not finish within"), "{}", out.summary);
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
        let stats = f.st.shots.stats();
        assert_eq!((stats.timeouts, stats.running), (1, 0));
        let profiles = stub.profiles();
        assert_eq!(profiles.len(), 1);
        assert!(!profiles[0].exists(), "{} outlived the browser", profiles[0].display());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_browser_that_writes_no_png_reports_what_it_said_and_still_cleans_up() {
        let stub = Stub::new("0", false);
        let f = fixture_with(Vec::new(), false, Some(stub.bin.clone()), Shots::default()).await;
        let t = f.st.store.tenant("acme").unwrap();
        let out = screenshot(&f.st, &t, &json!({ "path": "/" })).await;
        assert!(out.summary.starts_with("error: chrome wrote no screenshot"), "{}", out.summary);
        assert!(!stub.profiles()[0].exists());
    }

    #[test]
    fn the_summarizer_transcript_keeps_the_newest_turns_it_can_afford() {
        let turns: Vec<Turn> =
            (0..500).map(|i| Turn { role: "user".into(), text: format!("turn {i} ").repeat(200), tools: Vec::new() }).collect();
        let out = transcript("what came before", &turns);
        assert!(out.len() <= SUMMARY_TRANSCRIPT + "what came before".len() + 256, "{}", out.len());
        assert!(out.starts_with("Summary of what came before this transcript:\nwhat came before"), "{out}");
        assert!(out.contains("turn 499"), "the newest turns survive the cut");
        assert!(!out.contains("turn 0 "), "the oldest do not");
        assert_eq!(transcript("", &turns[..1]).lines().count(), 1);
        assert_eq!(transcript("", &[]), "");
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
