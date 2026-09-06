use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State as AxState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::api::{check_tenant, err};
use super::{AppState, State};
use crate::store::{Tenant, clean_path};

const MAX_ITERATIONS: usize = 16;
const MAX_READ: usize = 200 * 1024;

#[derive(Deserialize)]
pub struct ChatMsg {
    role: String,
    content: String,
}

#[derive(Deserialize)]
pub struct ChatReq {
    messages: Vec<ChatMsg>,
}

fn tools() -> Value {
    json!([
        { "name": "list_files", "description": "List every file in the site project with its size in bytes.", "input_schema": { "type": "object", "properties": {} } },
        { "name": "read_file", "description": "Read a text file from the project.", "input_schema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] } },
        { "name": "write_file", "description": "Create or overwrite a file with the full new content. The preview updates immediately.", "input_schema": { "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" } }, "required": ["path", "content"] } },
        { "name": "delete_file", "description": "Delete a file from the project.", "input_schema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] } },
        { "name": "check_site", "description": "Compile every source file and return diagnostics. Call it after editing.", "input_schema": { "type": "object", "properties": {} } }
    ])
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

fn run_tool(st: &AppState, t: &Tenant, name: &str, input: &Value, changes: &mut Vec<String>) -> String {
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

pub async fn chat(AxState(st): AxState<State>, Path(id): Path<String>, Json(req): Json<ChatReq>) -> Response {
    let Some(key) = st.api_key.clone() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "ANTHROPIC_API_KEY is not set on the daemon");
    };
    let Some(t) = st.store.tenant(&id) else { return err(StatusCode::NOT_FOUND, "unknown tenant") };
    let client = reqwest::Client::new();
    let mut messages: Vec<Value> = req.messages.iter().map(|m| json!({ "role": m.role, "content": m.content })).collect();
    let mut changes = Vec::new();
    let mut text = String::new();
    let mut iterations = 0;
    while iterations < MAX_ITERATIONS {
        iterations += 1;
        let body = json!({
            "model": st.model,
            "max_tokens": 8192,
            "system": system_prompt(&t),
            "tools": tools(),
            "messages": messages,
        });
        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => return err(StatusCode::BAD_GATEWAY, e.to_string()),
        };
        let status = resp.status();
        let v: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_GATEWAY, e.to_string()),
        };
        if !status.is_success() {
            return (StatusCode::BAD_GATEWAY, Json(json!({ "error": v }))).into_response();
        }
        let content = v.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();
        let turn_text: Vec<&str> = content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
        if !turn_text.is_empty() {
            text = turn_text.join("\n");
        }
        if v.get("stop_reason").and_then(|s| s.as_str()) != Some("tool_use") {
            break;
        }
        messages.push(json!({ "role": "assistant", "content": content }));
        let mut results = Vec::new();
        for block in content.iter().filter(|b| b["type"] == "tool_use") {
            let name = block["name"].as_str().unwrap_or("");
            let output = run_tool_blocking(st.clone(), t.clone(), name.to_string(), block["input"].clone(), &mut changes).await;
            results.push(json!({ "type": "tool_result", "tool_use_id": block["id"], "content": output }));
        }
        messages.push(json!({ "role": "user", "content": results }));
    }
    changes.sort();
    changes.dedup();
    Json(json!({ "text": text, "changes": changes, "iterations": iterations, "version": t.version() })).into_response()
}

async fn run_tool_blocking(st: State, t: Arc<Tenant>, name: String, input: Value, changes: &mut Vec<String>) -> String {
    let mut local = Vec::new();
    let (out, local) = tokio::task::spawn_blocking(move || {
        let out = run_tool(&st, &t, &name, &input, &mut local);
        (out, local)
    })
    .await
    .unwrap_or_else(|e| (format!("error: {e}"), Vec::new()));
    changes.extend(local);
    out
}
