//! Conversations, one JSON file per chat under `<data-dir>/<tenant>/chats/`, or in memory with
//! `--no-persist`. They are not site files: they never go through the tenant overlay, so they are
//! invisible to the preview, to `/api/t/{id}/files` and to the tenant quota.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Path as AxPath, State as AxState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::State;
use super::api::{err, tenant_or_404};
use crate::store::{Tenant, valid_id};

const TITLE_CHARS: usize = 80;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Turn {
    pub role: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolCall>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Conversation {
    pub id: String,
    pub created: u64,
    pub updated: u64,
    pub title: String,
    pub messages: Vec<Turn>,
}

impl Conversation {
    pub fn new(id: String) -> Conversation {
        let now = now_millis();
        Conversation { id, created: now, updated: now, title: String::new(), messages: Vec::new() }
    }

    pub fn push(&mut self, turn: Turn) {
        if self.title.is_empty() && turn.role == "user" {
            self.title = turn.text.chars().take(TITLE_CHARS).collect::<String>().replace('\n', " ").trim().to_string();
        }
        self.messages.push(turn);
        self.updated = now_millis();
    }

    /// The stored turns as Messages API input. Tool calls stay behind as a record for the editor;
    /// what they did is already in the tenant's files. Empty turns are dropped and same-role
    /// neighbours joined, because the API refuses empty content and two messages in a row from
    /// the same role.
    pub fn for_model(&self) -> Vec<Value> {
        let mut out: Vec<(String, String)> = Vec::new();
        for turn in self.messages.iter().filter(|t| !t.text.trim().is_empty()) {
            if out.last().is_some_and(|(role, _)| *role == turn.role) {
                if let Some((_, text)) = out.last_mut() {
                    text.push_str("\n\n");
                    text.push_str(&turn.text);
                }
            } else if !out.is_empty() || turn.role == "user" {
                out.push((turn.role.clone(), turn.text.clone()));
            }
        }
        out.into_iter().map(|(role, content)| json!({ "role": role, "content": content })).collect()
    }

    fn summary(&self) -> Value {
        json!({ "id": self.id, "title": self.title, "created": self.created, "updated": self.updated, "turns": self.messages.len() })
    }
}

/// Disk is the store when the tenant has a data directory; the map holds everything otherwise.
#[derive(Default)]
pub struct Chats {
    mem: RwLock<HashMap<String, BTreeMap<String, Conversation>>>,
}

impl Chats {
    pub fn load(&self, t: &Tenant, id: &str) -> Option<Conversation> {
        if !valid_id(id) {
            return None;
        }
        match dir(t) {
            Some(dir) => serde_json::from_slice(&std::fs::read(dir.join(format!("{id}.json"))).ok()?).ok(),
            None => self.mem.read().unwrap().get(&t.id)?.get(id).cloned(),
        }
    }

    pub fn save(&self, t: &Tenant, chat: &Conversation) -> io::Result<()> {
        match dir(t) {
            Some(dir) => {
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join(format!("{}.json", chat.id)), serde_json::to_vec(chat)?)
            }
            None => {
                self.mem.write().unwrap().entry(t.id.clone()).or_default().insert(chat.id.clone(), chat.clone());
                Ok(())
            }
        }
    }

    /// Newest first.
    pub fn list(&self, t: &Tenant) -> Vec<Conversation> {
        let mut all: Vec<Conversation> = match dir(t) {
            Some(dir) => std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
                .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
                .collect(),
            None => self.mem.read().unwrap().get(&t.id).map(|m| m.values().cloned().collect()).unwrap_or_default(),
        };
        all.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
        all
    }

    pub fn delete(&self, t: &Tenant, id: &str) -> bool {
        if !valid_id(id) {
            return false;
        }
        match dir(t) {
            Some(dir) => std::fs::remove_file(dir.join(format!("{id}.json"))).is_ok(),
            None => self.mem.write().unwrap().get_mut(&t.id).is_some_and(|m| m.remove(id).is_some()),
        }
    }

    /// A deleted tenant takes its data directory with it; the in-memory map has to be told.
    pub fn forget(&self, tenant: &str) {
        self.mem.write().unwrap().remove(tenant);
    }
}

fn dir(t: &Tenant) -> Option<PathBuf> {
    t.dir().map(|d| d.join("chats"))
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(1)
}

/// Unique per process and short enough to be a file name; not a secret — whoever can reach
/// `/api/*` can list every conversation anyway.
pub fn new_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seed = format!(
        "{}-{}-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    blake3::hash(seed.as_bytes()).to_hex()[..16].to_string()
}

pub async fn list(AxState(st): AxState<State>, AxPath(id): AxPath<String>) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    Json(json!({ "chats": st.chats.list(&t).iter().map(Conversation::summary).collect::<Vec<_>>() })).into_response()
}

pub async fn get(AxState(st): AxState<State>, AxPath((id, chat)): AxPath<(String, String)>) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    match st.chats.load(&t, &chat) {
        Some(c) => Json(c).into_response(),
        None => err(StatusCode::NOT_FOUND, "unknown chat"),
    }
}

pub async fn remove(AxState(st): AxState<State>, AxPath((id, chat)): AxPath<(String, String)>) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    if st.chats.delete(&t, &chat) { StatusCode::NO_CONTENT.into_response() } else { err(StatusCode::NOT_FOUND, "unknown chat") }
}

#[cfg(test)]
mod tests {
    use super::{Conversation, Turn, new_id};
    use serde_json::json;

    fn turn(role: &str, text: &str) -> Turn {
        Turn { role: role.into(), text: text.into(), tools: Vec::new() }
    }

    #[test]
    fn model_input_alternates_and_starts_with_the_user() {
        let mut c = Conversation::new("c".into());
        for t in [turn("assistant", "orphan"), turn("user", "one"), turn("assistant", ""), turn("assistant", "two"), turn("user", "three")]
        {
            c.push(t);
        }
        assert_eq!(
            c.for_model(),
            vec![
                json!({"role":"user","content":"one"}),
                json!({"role":"assistant","content":"two"}),
                json!({"role":"user","content":"three"})
            ]
        );
    }

    #[test]
    fn the_title_is_the_first_user_turn() {
        let mut c = Conversation::new("c".into());
        c.push(turn("user", "make the\nheadline green"));
        c.push(turn("user", "and the buttons"));
        assert_eq!(c.title, "make the headline green");
        assert!(c.updated >= c.created);
    }

    #[test]
    fn ids_are_unique_and_usable_as_file_names() {
        let ids: Vec<String> = (0..64).map(|_| new_id()).collect();
        assert!(ids.iter().all(|id| crate::store::valid_id(id)));
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len());
    }
}
