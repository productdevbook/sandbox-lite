//! Conversations, one JSON file per chat under `<data-dir>/<tenant>/chats/`, or in memory with
//! `--no-persist`. They are not site files: they never go through the tenant overlay, so they are
//! invisible to the preview, to `/api/t/{id}/files` and to the tenant quota. What bounds them
//! instead is here: a window of turns per conversation, with everything older folded into a
//! stored summary, and a cap on how many conversations one tenant keeps.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
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

/// Turns of a conversation replayed to the model in full. Everything older is folded into
/// `Conversation::summary`, so the request stops growing with the conversation.
pub const DEFAULT_WINDOW_TURNS: usize = 24;
/// Conversations one tenant may keep. They do not count against the tenant quota, so this is the
/// only thing bounding what the chats directory holds.
pub const DEFAULT_MAX_CHATS: usize = 50;
/// The summary is prompt text on every later turn, so it is capped like one.
const MAX_SUMMARY_CHARS: usize = 2000;
/// How the summary reaches the model: as the opening user turn, which is also the shape
/// `for_model` needs, since the API wants a user message first.
const SUMMARY_PREFIX: &str = "Summary of the earlier part of this conversation: ";

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
    /// What the turns dropped by `compact` said. Absent from a conversation that never grew
    /// past the window, and from every conversation stored before there was a window.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    pub messages: Vec<Turn>,
}

impl Conversation {
    pub fn new(id: String) -> Conversation {
        let now = now_millis();
        Conversation { id, created: now, updated: now, title: String::new(), summary: String::new(), messages: Vec::new() }
    }

    pub fn push(&mut self, turn: Turn) {
        if self.title.is_empty() && turn.role == "user" {
            self.title = turn.text.chars().take(TITLE_CHARS).collect::<String>().replace('\n', " ").trim().to_string();
        }
        self.messages.push(turn);
        self.updated = now_millis();
    }

    /// The stored turns as Messages API input: the summary of what came before, then the last
    /// `keep` turns and no more. Tool calls stay behind as a record for the editor; what they did
    /// is already in the tenant's files. Empty turns are dropped and same-role neighbours joined,
    /// because the API refuses empty content and two messages in a row from the same role.
    ///
    /// `compact` normally keeps the conversation inside the window, so nothing is cut here. The
    /// cut is what bounds the request when it could not: a summary the API refused to write, or a
    /// conversation stored before the window existed.
    pub fn for_model(&self, keep: usize) -> Vec<Value> {
        let start = self.messages.len().saturating_sub(keep.max(1));
        let head = (!self.summary.is_empty()).then(|| Turn {
            role: "user".into(),
            text: format!("{SUMMARY_PREFIX}{}", self.summary),
            tools: Vec::new(),
        });
        let mut out: Vec<(String, String)> = Vec::new();
        for turn in head.iter().chain(&self.messages[start..]).filter(|t| !t.text.trim().is_empty()) {
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

    /// How many of the oldest turns the summary should swallow, and zero while the conversation
    /// is inside the window. Past it the window is emptied to half, so the summary is written
    /// once every `keep / 2` turns rather than once per turn.
    pub fn overflow(&self, keep: usize) -> usize {
        let keep = keep.max(2);
        if self.messages.len() <= keep { 0 } else { self.messages.len() - keep / 2 }
    }

    /// Replaces the oldest `drop` turns with `summary`, which the caller wrote from them and from
    /// the summary before it.
    pub fn compact(&mut self, drop: usize, summary: String) {
        let drop = drop.min(self.messages.len());
        if drop == 0 {
            return;
        }
        self.messages.drain(..drop);
        self.summary = summary.chars().take(MAX_SUMMARY_CHARS).collect::<String>().trim().to_string();
    }

    fn brief(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "created": self.created,
            "updated": self.updated,
            "turns": self.messages.len(),
            "summarized": !self.summary.is_empty(),
        })
    }
}

/// The ids to remove so that at most `cap` conversations remain, least recently updated first.
/// `keep` is the conversation the save was for and is never evicted, however old it looks.
pub fn evictable(newest_first: &[Conversation], cap: usize, keep: &str) -> Vec<String> {
    let over = newest_first.len().saturating_sub(cap.max(1));
    newest_first.iter().rev().filter(|c| c.id != keep).take(over).map(|c| c.id.clone()).collect()
}

/// Disk is the store when the tenant has a data directory; the map holds everything otherwise.
pub struct Chats {
    mem: RwLock<HashMap<String, BTreeMap<String, Conversation>>>,
    /// What each tenant's conversations weigh, `<tenant> → <chat id> → bytes`. Seeded from the
    /// store the first time a tenant is touched and moved by every save and delete after that, so
    /// `usage` answers from memory: `/api/stats` is polled every five seconds by every open editor,
    /// and a directory walk per tenant per poll is O(tenants) of disk on the endpoint whose numbers
    /// say this daemon's cost does not grow with tenant count (#60).
    sizes: RwLock<HashMap<String, BTreeMap<String, u64>>>,
    cap: usize,
    window: usize,
}

impl Default for Chats {
    fn default() -> Chats {
        Chats::new(DEFAULT_MAX_CHATS, DEFAULT_WINDOW_TURNS)
    }
}

impl Chats {
    pub fn new(cap: usize, window: usize) -> Chats {
        Chats { mem: RwLock::new(HashMap::new()), sizes: RwLock::new(HashMap::new()), cap: cap.max(1), window: window.max(2) }
    }

    /// Turns `for_model` replays in full; see `DEFAULT_WINDOW_TURNS`.
    pub fn window(&self) -> usize {
        self.window
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    /// `Ok(None)` is a chat that is not there; `Err` is one that is and could not be read back.
    /// Answering "unknown chat" for the second loses a conversation the caller still has.
    pub fn load(&self, t: &Tenant, id: &str) -> io::Result<Option<Conversation>> {
        if !valid_id(id) {
            return Ok(None);
        }
        let Some(dir) = dir(t) else { return Ok(self.mem.read().unwrap().get(&t.id).and_then(|m| m.get(id)).cloned()) };
        match std::fs::read(dir.join(format!("{id}.json"))) {
            Ok(raw) => serde_json::from_slice(&raw).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, t: &Tenant, chat: &Conversation) -> io::Result<()> {
        self.seed(t)?;
        let raw = serde_json::to_vec(chat)?;
        let bytes = raw.len() as u64;
        match dir(t) {
            Some(dir) => {
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join(format!("{}.json", chat.id)), &raw)?;
            }
            None => {
                self.mem.write().unwrap().entry(t.id.clone()).or_default().insert(chat.id.clone(), chat.clone());
            }
        }
        self.sizes.write().unwrap().entry(t.id.clone()).or_default().insert(chat.id.clone(), bytes);
        self.evict(t, &chat.id)
    }

    /// The bytes each of a tenant's conversations holds, read once and then maintained. A tenant
    /// already seeded is left alone, so this is one directory walk per tenant per process.
    fn seed(&self, t: &Tenant) -> io::Result<()> {
        if self.sizes.read().unwrap().contains_key(&t.id) {
            return Ok(());
        }
        let seeded: BTreeMap<String, u64> = match dir(t) {
            Some(dir) => {
                let mut out = BTreeMap::new();
                for entry in chat_files(&dir)? {
                    out.insert(chat_id(&entry), entry.metadata()?.len());
                }
                out
            }
            None => match self.mem.read().unwrap().get(&t.id) {
                Some(held) => held.iter().map(|(id, c)| (id.clone(), serialized_bytes(c))).collect(),
                None => BTreeMap::new(),
            },
        };
        self.sizes.write().unwrap().entry(t.id.clone()).or_insert(seeded);
        Ok(())
    }

    /// Brings the tenant back under the cap after a save. The count is taken first because it
    /// costs one `read_dir`, where choosing what to drop costs a parse of every conversation.
    fn evict(&self, t: &Tenant, saved: &str) -> io::Result<()> {
        if self.count(t)? <= self.cap {
            return Ok(());
        }
        for id in evictable(&self.list(t)?, self.cap, saved) {
            self.delete(t, &id)?;
        }
        Ok(())
    }

    fn count(&self, t: &Tenant) -> io::Result<usize> {
        match dir(t) {
            Some(dir) => Ok(chat_files(&dir)?.len()),
            None => Ok(self.mem.read().unwrap().get(&t.id).map_or(0, BTreeMap::len)),
        }
    }

    /// What the tenant's conversations hold, from the counters rather than the disk: the tenant is
    /// walked once, on the first call, and every save and delete after that moves the numbers.
    /// `/api/stats` and `/metrics` are the callers, and the editor polls the first every 5 s.
    pub fn usage(&self, t: &Tenant) -> io::Result<(usize, u64)> {
        self.seed(t)?;
        let sizes = self.sizes.read().unwrap();
        Ok(sizes.get(&t.id).map_or((0, 0), |held| (held.len(), held.values().sum())))
    }

    /// Newest first. A conversation file that cannot be read fails the listing rather than
    /// disappearing from it: a short list looks exactly like a tenant that had fewer chats.
    pub fn list(&self, t: &Tenant) -> io::Result<Vec<Conversation>> {
        let mut all: Vec<Conversation> = match dir(t) {
            Some(dir) => {
                let mut out = Vec::new();
                for entry in chat_files(&dir)? {
                    let raw = std::fs::read(entry.path())?;
                    out.push(serde_json::from_slice(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?);
                }
                out
            }
            None => self.mem.read().unwrap().get(&t.id).map(|m| m.values().cloned().collect()).unwrap_or_default(),
        };
        all.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
        Ok(all)
    }

    /// `Ok(false)` is a chat that was not there. An `Err` is a chat still there after a delete that
    /// answered 204.
    pub fn delete(&self, t: &Tenant, id: &str) -> io::Result<bool> {
        if !valid_id(id) {
            return Ok(false);
        }
        self.seed(t)?;
        let removed = match dir(t) {
            Some(dir) => match std::fs::remove_file(dir.join(format!("{id}.json"))) {
                Ok(()) => true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                Err(e) => return Err(e),
            },
            None => self.mem.write().unwrap().get_mut(&t.id).is_some_and(|m| m.remove(id).is_some()),
        };
        if removed && let Some(held) = self.sizes.write().unwrap().get_mut(&t.id) {
            held.remove(id);
        }
        Ok(removed)
    }

    /// A deleted tenant takes its data directory with it; the in-memory map has to be told, and so
    /// do the counters, or a tenant re-created with the same id would start from the old numbers.
    pub fn forget(&self, tenant: &str) {
        self.mem.write().unwrap().remove(tenant);
        self.sizes.write().unwrap().remove(tenant);
    }
}

/// What a conversation weighs in the store: exactly the bytes `save` writes for it.
fn serialized_bytes(c: &Conversation) -> u64 {
    serde_json::to_vec(c).map_or(0, |v| v.len() as u64)
}

fn chat_id(entry: &std::fs::DirEntry) -> String {
    entry.file_name().to_string_lossy().trim_end_matches(".json").to_string()
}

fn dir(t: &Tenant) -> Option<PathBuf> {
    t.dir().map(|d| d.join("chats"))
}

fn is_chat_file(e: &std::fs::DirEntry) -> bool {
    e.file_name().to_string_lossy().ends_with(".json")
}

/// A tenant that has never been chatted with has no `chats/` directory, which is an empty listing.
/// Anything else that stops the directory being read is not one, and must not be counted as one:
/// a zero here silently stops the cap being enforced and understates what the tenant is holding.
fn chat_files(dir: &Path) -> io::Result<Vec<std::fs::DirEntry>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if is_chat_file(&entry) {
            out.push(entry);
        }
    }
    Ok(out)
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
    match st.chats.list(&t) {
        Ok(all) => Json(json!({ "chats": all.iter().map(Conversation::brief).collect::<Vec<_>>() })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn get(AxState(st): AxState<State>, AxPath((id, chat)): AxPath<(String, String)>) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    match st.chats.load(&t, &chat) {
        Ok(Some(c)) => Json(c).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "unknown chat"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn remove(AxState(st): AxState<State>, AxPath((id, chat)): AxPath<(String, String)>) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    match st.chats.delete(&t, &chat) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "unknown chat"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Chats, Conversation, DEFAULT_WINDOW_TURNS, SUMMARY_PREFIX, Turn, evictable, new_id};
    use crate::store::{Base, Store, Tenant};
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn turn(role: &str, text: &str) -> Turn {
        Turn { role: role.into(), text: text.into(), tools: Vec::new() }
    }

    /// `count` turns, alternating, so `messages[i]` is identifiable by its text.
    fn conversation(id: &str, count: usize) -> Conversation {
        let mut c = Conversation::new(id.into());
        for i in 0..count {
            c.push(turn(if i % 2 == 0 { "user" } else { "assistant" }, &format!("turn {i}")));
        }
        c
    }

    #[test]
    fn model_input_alternates_and_starts_with_the_user() {
        let mut c = Conversation::new("c".into());
        for t in [turn("assistant", "orphan"), turn("user", "one"), turn("assistant", ""), turn("assistant", "two"), turn("user", "three")]
        {
            c.push(t);
        }
        assert_eq!(
            c.for_model(DEFAULT_WINDOW_TURNS),
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

    /// Issue #45: the request has to stop growing with the conversation.
    #[test]
    fn the_window_opens_only_once_the_conversation_passes_it() {
        for turns in 0..=8 {
            assert_eq!(conversation("c", turns).overflow(8), 0, "{turns} turns are inside a window of 8");
        }
        assert_eq!(conversation("c", 9).overflow(8), 5, "9 turns drop to half the window");
        assert_eq!(conversation("c", 10).overflow(8), 6);
        assert_eq!(conversation("c", 10_000).overflow(8), 9996, "a conversation from before the window is cut in one go");
        // half a window of headroom, so the summary is written once every four turns, not every turn
        let mut c = conversation("c", 9);
        c.compact(c.overflow(8), "earlier".into());
        assert_eq!(c.messages.len(), 4);
        assert_eq!(c.overflow(8), 0);
    }

    #[test]
    fn compacting_replaces_the_oldest_turns_with_the_summary() {
        let mut c = conversation("c", 6);
        c.compact(c.overflow(4), "they asked for a green headline".into());
        assert_eq!(c.summary, "they asked for a green headline");
        assert_eq!(c.messages.iter().map(|t| t.text.clone()).collect::<Vec<_>>(), ["turn 4", "turn 5"]);
        assert_eq!(c.title, "turn 0", "the title survives the turn it was taken from");
        assert_eq!(
            c.for_model(4),
            vec![
                json!({"role":"user","content":format!("{SUMMARY_PREFIX}they asked for a green headline\n\nturn 4")}),
                json!({"role":"assistant","content":"turn 5"}),
            ],
            "the summary opens the request as a user turn, joined to the user turn that follows it"
        );
    }

    /// The summary is written by the API, and the API can be down. `for_model` still has to bound
    /// the request, so it cuts what `compact` could not.
    #[test]
    fn for_model_never_sends_more_than_the_window() {
        let mut c = conversation("c", 100);
        let sent = c.for_model(6);
        assert_eq!(sent.len(), 6);
        assert_eq!(sent[0], json!({"role":"user","content":"turn 94"}));
        assert_eq!(sent[5], json!({"role":"assistant","content":"turn 99"}));
        c.summary = "earlier".into();
        assert_eq!(c.for_model(0).len(), 2, "a window of zero is the summary and one turn, not a slice out of bounds");
    }

    #[test]
    fn a_compacted_conversation_round_trips_through_json() {
        let mut c = conversation("c", 6);
        c.compact(c.overflow(4), "x".repeat(super::MAX_SUMMARY_CHARS + 100));
        assert_eq!(c.summary.chars().count(), super::MAX_SUMMARY_CHARS);
        let back: Conversation = serde_json::from_slice(&serde_json::to_vec(&c).unwrap()).unwrap();
        assert_eq!(back.summary, c.summary);
        assert_eq!(back.messages.len(), 2);

        let old = json!({"id":"c","created":1,"updated":2,"title":"t","messages":[]});
        let parsed: Conversation = serde_json::from_value(old).unwrap();
        assert_eq!(parsed.summary, "", "a conversation stored before the window still loads");
    }

    #[test]
    fn eviction_takes_the_least_recently_updated_and_never_the_one_just_saved() {
        let mut list: Vec<Conversation> = (0..5).map(|i| conversation(&format!("c{i}"), 1)).collect();
        for (i, c) in list.iter_mut().enumerate() {
            c.updated = 100 - i as u64;
        }
        // `list` is newest first, the order `Chats::list` returns
        assert_eq!(evictable(&list, 3, "c0"), vec!["c4".to_string(), "c3".to_string()]);
        assert_eq!(evictable(&list, 5, "c0"), Vec::<String>::new());
        assert_eq!(evictable(&list, 9, "c0"), Vec::<String>::new());
        assert_eq!(evictable(&list, 3, "c4"), vec!["c3".to_string(), "c2".to_string()], "the saved one is skipped, two others go");
        assert_eq!(evictable(&list, 0, "c0"), vec!["c4".to_string(), "c3".to_string(), "c2".to_string(), "c1".to_string()]);
    }

    fn tenant(persist: bool) -> (Arc<Tenant>, PathBuf) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!("sandbox-lite-chats-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::new(persist.then(|| root.clone()), u64::MAX);
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/starter");
        store.add_base(Base::load("starter", &base).unwrap());
        (store.create_tenant("acme", "starter").unwrap(), root)
    }

    /// Issue #45: conversations do not count against the tenant quota, so the cap is what keeps a
    /// tenant from accumulating chat files without end.
    #[test]
    fn saving_past_the_cap_drops_the_oldest_conversation() {
        for persist in [true, false] {
            let (t, root) = tenant(persist);
            let chats = Chats::new(3, 8);
            for i in 0..6 {
                let mut c = conversation(&format!("c{i}"), 2);
                c.updated = 1000 + i as u64;
                chats.save(&t, &c).unwrap();
                assert!(chats.list(&t).unwrap().len() <= 3, "persist={persist}, after {i}");
            }
            let left: Vec<String> = chats.list(&t).unwrap().into_iter().map(|c| c.id).collect();
            assert_eq!(left, ["c5", "c4", "c3"], "persist={persist}");
            assert!(chats.load(&t, "c0").unwrap().is_none(), "persist={persist}");
            let (count, bytes) = chats.usage(&t).unwrap();
            assert_eq!(count, 3, "persist={persist}");
            assert!(bytes > 0, "persist={persist}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn usage_is_zero_for_a_tenant_that_has_never_chatted() {
        let (t, root) = tenant(true);
        assert_eq!(Chats::default().usage(&t).unwrap(), (0, 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Issue #60: `/api/stats` walked every tenant's chats directory, and the editor polls it every
    /// five seconds. The counters answer instead — proved here by taking the directory away — and
    /// they still agree with the files after a save, an evict and a restore onto a fresh daemon.
    #[test]
    fn usage_is_counted_in_memory_and_still_matches_the_files() {
        let (t, root) = tenant(true);
        let chats = Chats::new(2, 8);
        for i in 0..3 {
            let mut c = conversation(&format!("c{i}"), 2);
            c.updated = 1000 + i as u64;
            chats.save(&t, &c).unwrap();
        }
        let counted = chats.usage(&t).unwrap();
        assert_eq!(counted.0, 2, "the third save evicted the oldest");

        let stored = super::dir(&t).unwrap();
        let on_disk: (usize, u64) = std::fs::read_dir(&stored)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .fold((0, 0), |(n, bytes), len| (n + 1, bytes + len));
        assert_eq!(counted, on_disk, "the counters and the files agree");

        // A daemon that has never seen this tenant seeds itself from the same directory.
        assert_eq!(Chats::new(2, 8).usage(&t).unwrap(), counted);

        // And once they are warm the directory is not read again: it is not even there.
        std::fs::remove_dir_all(&stored).unwrap();
        assert_eq!(chats.usage(&t).unwrap(), counted);

        chats.forget(&t.id);
        assert_eq!(chats.usage(&t).unwrap(), (0, 0), "a deleted tenant starts from nothing");
        let _ = std::fs::remove_dir_all(&root);
    }
}
