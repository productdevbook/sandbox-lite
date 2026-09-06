use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

const INLINE_LIMIT: u64 = 256 * 1024;
const SKIP_DIRS: &[&str] = &["node_modules", ".git", "dist", ".astro", ".vercel", ".netlify", ".output"];

#[derive(Clone)]
pub enum FileData {
    Mem(Arc<[u8]>),
    Disk(PathBuf, u64),
}

impl FileData {
    pub fn size(&self) -> u64 {
        match self {
            FileData::Mem(b) => b.len() as u64,
            FileData::Disk(_, n) => *n,
        }
    }

    pub fn read(&self) -> io::Result<Arc<[u8]>> {
        match self {
            FileData::Mem(b) => Ok(b.clone()),
            FileData::Disk(p, _) => Ok(std::fs::read(p)?.into()),
        }
    }

    fn from_disk(path: &Path) -> io::Result<FileData> {
        let len = std::fs::metadata(path)?.len();
        if len <= INLINE_LIMIT { Ok(FileData::Mem(std::fs::read(path)?.into())) } else { Ok(FileData::Disk(path.to_path_buf(), len)) }
    }
}

pub struct Base {
    pub name: String,
    pub root: PathBuf,
    files: BTreeMap<String, FileData>,
}

impl Base {
    pub fn load(name: &str, root: &Path) -> io::Result<Base> {
        let root = root.canonicalize()?;
        let mut files = BTreeMap::new();
        walk(&root, &root, &mut files, true)?;
        Ok(Base { name: name.to_string(), root, files })
    }

    pub fn get(&self, path: &str) -> Option<&FileData> {
        self.files.get(path)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn bytes(&self) -> u64 {
        self.files.values().map(|f| f.size()).sum()
    }
}

/// Base projects skip build output and dependencies; a tenant's own overlay must come back whole.
fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, FileData>, skip_build_dirs: bool) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            if skip_build_dirs && SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, out, skip_build_dirs)?;
        } else if ft.is_file() {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            out.insert(rel, FileData::from_disk(&path)?);
        }
    }
    Ok(())
}

pub fn clean_path(raw: &str) -> Option<String> {
    let raw = raw.trim_start_matches('/');
    if raw.is_empty() || raw.contains('\\') || raw.contains('\0') {
        return None;
    }
    let mut parts = Vec::new();
    for seg in raw.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return None,
            s => parts.push(s),
        }
    }
    if parts.is_empty() { None } else { Some(parts.join("/")) }
}

/// Dotfiles (`.env`, `.git`, `.astro`) are never served to a preview visitor; `.well-known` is the one
/// hidden directory sites publish on purpose.
pub fn is_private_path(path: &str) -> bool {
    path.split('/').any(|seg| seg.starts_with('.') && seg != ".well-known")
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-')
}

fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(1)
}

/// How the live-reload client should apply one change: swap a stylesheet, swap a component's
/// `<style>` blocks, or reload. A client that renders stale JS is worse than one that reloads too
/// often, so anything unproven is `Module`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UpdateKind {
    Css,
    Style,
    Module,
}

impl UpdateKind {
    /// What the path alone proves. `.astro` needs its compiled JS compared against the last build
    /// before it can claim `Style`, which is `Engine::update_kind`'s job.
    pub fn from_path(path: &str) -> UpdateKind {
        match path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
            Some("css" | "scss" | "sass") => UpdateKind::Css,
            _ => UpdateKind::Module,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UpdateKind::Css => "css",
            UpdateKind::Style => "style",
            UpdateKind::Module => "module",
        }
    }
}

#[derive(Debug)]
pub enum WriteError {
    Quota { quota: u64, after: u64 },
    Io(io::Error),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Quota { quota, after } => {
                write!(f, "tenant quota exceeded: edited files would total {after} bytes, the quota is {quota} bytes (--tenant-quota-mb)")
            }
            WriteError::Io(e) => fmt::Display::fmt(e, f),
        }
    }
}

impl std::error::Error for WriteError {}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> WriteError {
        WriteError::Io(e)
    }
}

pub struct Tenant {
    pub id: String,
    pub base: Arc<Base>,
    overlay: RwLock<BTreeMap<String, Option<FileData>>>,
    version: AtomicU64,
    pub events: broadcast::Sender<String>,
    dir: Option<PathBuf>,
    quota: u64,
}

pub struct Entry {
    pub path: String,
    pub size: u64,
    pub modified: bool,
}

/// What one `write_many` changed: files written, and overlay entries it dropped or hid.
pub struct Applied {
    pub version: u64,
    pub written: usize,
    pub deleted: usize,
}

impl Tenant {
    fn new(id: String, base: Arc<Base>, dir: Option<PathBuf>, quota: u64) -> Tenant {
        let (events, _) = broadcast::channel(64);
        Tenant { id, base, overlay: RwLock::new(BTreeMap::new()), version: AtomicU64::new(now_millis()), events, dir, quota }
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// `<data-dir>/<id>`, or None with `--no-persist`. Site files live under `files/`; chats do not.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn data(&self, path: &str) -> Option<FileData> {
        let overlay = self.overlay.read().unwrap();
        match overlay.get(path) {
            Some(Some(d)) => Some(d.clone()),
            Some(None) => None,
            None => self.base.get(path).cloned(),
        }
    }

    pub fn exists(&self, path: &str) -> bool {
        self.data(path).is_some()
    }

    pub fn read(&self, path: &str) -> Option<Arc<[u8]>> {
        self.data(path).and_then(|d| d.read().ok())
    }

    pub fn read_text(&self, path: &str) -> Option<String> {
        self.read(path).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    pub fn list(&self) -> Vec<Entry> {
        let overlay = self.overlay.read().unwrap();
        let mut out: BTreeMap<String, (u64, bool)> = self.base.files.iter().map(|(p, d)| (p.clone(), (d.size(), false))).collect();
        for (p, d) in overlay.iter() {
            match d {
                Some(d) => {
                    out.insert(p.clone(), (d.size(), true));
                }
                None => {
                    out.remove(p);
                }
            }
        }
        out.into_iter().map(|(path, (size, modified))| Entry { path, size, modified }).collect()
    }

    /// The caller says how the preview should apply the write: `UpdateKind::from_path` is the whole
    /// answer for a stylesheet, and proving `Style` takes a compile — `Engine::update_kind` — which
    /// is why it does not happen here.
    pub fn write(&self, path: &str, bytes: Vec<u8>, kind: UpdateKind) -> Result<u64, WriteError> {
        // held across the disk write so a concurrent write cannot slip past the quota check
        let mut overlay = self.overlay.write().unwrap();
        let replaced = overlay.get(path).and_then(|d| d.as_ref()).map_or(0, |d| d.size());
        let after = overlay_bytes(&overlay) - replaced + bytes.len() as u64;
        if after > self.quota {
            return Err(WriteError::Quota { quota: self.quota, after });
        }
        let data = self.store_on_disk(path, bytes)?;
        let was_tombstone = matches!(overlay.insert(path.to_string(), Some(data)), Some(None));
        drop(overlay);
        if was_tombstone {
            self.persist_tombstones()?;
        }
        Ok(self.bump("update", path, kind))
    }

    /// Applies a whole import: every removal and every file land under one lock, one version bump
    /// and one `update` event. `deleted` names paths to hide, `replace` drops every edit the import
    /// does not carry — dropping an edit over a base file brings the base copy back, which is what
    /// makes an overlay export reproduce the source overlay exactly. The quota is checked against
    /// the resulting overlay before anything is written, so a refusal leaves the tenant untouched.
    pub fn write_many(&self, files: Vec<(String, Vec<u8>)>, deleted: &[String], replace: bool) -> Result<Applied, WriteError> {
        let mut overlay = self.overlay.write().unwrap();
        let incoming: BTreeSet<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        let keep = |path: &str, data: &Option<FileData>| !replace || data.is_none() || incoming.contains(path);
        let mut next: BTreeMap<String, Option<FileData>> =
            overlay.iter().filter(|(p, d)| keep(p, d)).map(|(p, d)| (p.clone(), d.clone())).collect();
        for path in deleted.iter().filter(|p| !incoming.contains(p.as_str())) {
            match self.base.get(path) {
                Some(_) => next.insert(path.clone(), None),
                None => next.remove(path),
            };
        }
        let kept: u64 = next.iter().filter(|(p, _)| !incoming.contains(p.as_str())).filter_map(|(_, d)| d.as_ref()).map(|d| d.size()).sum();
        let after = kept + files.iter().map(|(_, b)| b.len() as u64).sum::<u64>();
        if after > self.quota {
            return Err(WriteError::Quota { quota: self.quota, after });
        }
        // `None` is untouched, `Some(true)` an edit, `Some(false)` a tombstone: a dropped edit and
        // a new tombstone both read as a change, and only writes are left out
        let overlaid = |o: &BTreeMap<String, Option<FileData>>, p: &str| o.get(p).map(Option::is_some);
        let touched: BTreeSet<&str> = overlay.keys().chain(deleted.iter()).map(String::as_str).collect();
        let dropped: Vec<&str> =
            touched.into_iter().filter(|p| !incoming.contains(p) && overlaid(&overlay, p) != overlaid(&next, p)).collect();
        for path in &dropped {
            self.forget_on_disk(path)?;
        }
        let (written, removed) = (files.len(), dropped.len());
        for (path, bytes) in files {
            next.insert(path.clone(), Some(self.store_on_disk(&path, bytes)?));
        }
        *overlay = next;
        drop(overlay);
        self.persist_tombstones()?;
        Ok(Applied { version: self.bump("update", "", UpdateKind::Module), written, deleted: removed })
    }

    /// Writes the file under `--data-dir` when there is one, and says how the overlay should hold it:
    /// anything past the inline limit lives on disk only.
    fn store_on_disk(&self, path: &str, bytes: Vec<u8>) -> io::Result<FileData> {
        let Some(dir) = &self.dir else { return Ok(FileData::Mem(bytes.into())) };
        let target = dir.join("files").join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &bytes)?;
        if bytes.len() as u64 > INLINE_LIMIT { Ok(FileData::Disk(target, bytes.len() as u64)) } else { Ok(FileData::Mem(bytes.into())) }
    }

    fn forget_on_disk(&self, path: &str) -> io::Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let target = dir.join("files").join(path);
        if target.exists() {
            std::fs::remove_file(target)?;
        }
        Ok(())
    }

    pub fn delete(&self, path: &str) -> io::Result<u64> {
        {
            let mut overlay = self.overlay.write().unwrap();
            if self.base.get(path).is_some() {
                overlay.insert(path.to_string(), None);
            } else {
                overlay.remove(path);
            }
        }
        self.forget_on_disk(path)?;
        self.persist_tombstones()?;
        Ok(self.bump("delete", path, UpdateKind::Module))
    }

    fn persist_tombstones(&self) -> io::Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let overlay = self.overlay.read().unwrap();
        let list: Vec<&str> = overlay.iter().filter(|(_, d)| d.is_none()).map(|(p, _)| p.as_str()).collect();
        std::fs::write(dir.join("deleted.json"), serde_json::to_vec(&list)?)
    }

    fn bump(&self, event: &str, path: &str, kind: UpdateKind) -> u64 {
        let mut v = self.version.load(Ordering::Relaxed);
        loop {
            let next = now_millis().max(v + 1);
            match self.version.compare_exchange(v, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => {
                    v = next;
                    break;
                }
                Err(cur) => v = cur,
            }
        }
        let _ = self.events.send(format!(
            r#"{{"type":"{event}","path":{},"kind":"{}","version":{v}}}"#,
            serde_json::to_string(path).unwrap_or_default(),
            kind.as_str()
        ));
        v
    }

    /// The tenant's own edits: `Some` is a written file, `None` a tombstone over a base file.
    pub fn overlay(&self) -> Vec<(String, Option<FileData>)> {
        self.overlay.read().unwrap().iter().map(|(p, d)| (p.clone(), d.clone())).collect()
    }

    pub fn quota(&self) -> u64 {
        self.quota
    }

    pub fn overlay_stats(&self) -> (usize, u64) {
        let overlay = self.overlay.read().unwrap();
        (overlay.len(), overlay_bytes(&overlay))
    }

    fn restore_overlay(&self, dir: &Path) -> io::Result<()> {
        let files = dir.join("files");
        let mut loaded = BTreeMap::new();
        if files.is_dir() {
            walk(&files, &files, &mut loaded, false)?;
        }
        let mut overlay = self.overlay.write().unwrap();
        for (p, d) in loaded {
            overlay.insert(p, Some(d));
        }
        if let Ok(raw) = std::fs::read(dir.join("deleted.json")) {
            let list: Vec<String> = serde_json::from_slice(&raw).unwrap_or_default();
            for p in list {
                overlay.entry(p).or_insert(None);
            }
        }
        Ok(())
    }
}

fn overlay_bytes(overlay: &BTreeMap<String, Option<FileData>>) -> u64 {
    overlay.values().flatten().map(|d| d.size()).sum()
}

pub struct Store {
    bases: RwLock<HashMap<String, Arc<Base>>>,
    tenants: RwLock<HashMap<String, Arc<Tenant>>>,
    data_dir: Option<PathBuf>,
    tenant_quota: u64,
}

impl Store {
    pub fn new(data_dir: Option<PathBuf>, tenant_quota: u64) -> Store {
        Store { bases: RwLock::new(HashMap::new()), tenants: RwLock::new(HashMap::new()), data_dir, tenant_quota }
    }

    pub fn add_base(&self, base: Base) {
        self.bases.write().unwrap().insert(base.name.clone(), Arc::new(base));
    }

    pub fn base(&self, name: &str) -> Option<Arc<Base>> {
        self.bases.read().unwrap().get(name).cloned()
    }

    pub fn bases(&self) -> Vec<Arc<Base>> {
        let mut v: Vec<_> = self.bases.read().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn create_tenant(&self, id: &str, base_name: &str) -> Result<Arc<Tenant>, String> {
        if !valid_id(id) {
            return Err("tenant id must be lowercase letters, digits and dashes".into());
        }
        let base = self.base(base_name).ok_or_else(|| format!("unknown base '{base_name}'"))?;
        if self.tenants.read().unwrap().contains_key(id) {
            return Err(format!("tenant '{id}' already exists"));
        }
        let dir = self.data_dir.as_ref().map(|d| d.join(id));
        if let Some(dir) = &dir {
            std::fs::create_dir_all(dir.join("files")).map_err(|e| e.to_string())?;
            std::fs::write(dir.join("tenant.json"), format!(r#"{{"base":{}}}"#, serde_json::to_string(base_name).unwrap()))
                .map_err(|e| e.to_string())?;
        }
        let tenant = Arc::new(Tenant::new(id.to_string(), base, dir, self.tenant_quota));
        self.tenants.write().unwrap().insert(id.to_string(), tenant.clone());
        Ok(tenant)
    }

    pub fn tenant(&self, id: &str) -> Option<Arc<Tenant>> {
        self.tenants.read().unwrap().get(id).cloned()
    }

    pub fn tenants(&self) -> Vec<Arc<Tenant>> {
        let mut v: Vec<_> = self.tenants.read().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    pub fn remove_tenant(&self, id: &str) -> bool {
        let removed = self.tenants.write().unwrap().remove(id);
        if let Some(t) = removed {
            if let Some(dir) = &t.dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            true
        } else {
            false
        }
    }

    pub fn restore(&self) -> io::Result<usize> {
        let Some(data_dir) = &self.data_dir else { return Ok(0) };
        if !data_dir.is_dir() {
            std::fs::create_dir_all(data_dir)?;
            return Ok(0);
        }
        let mut n = 0;
        for entry in std::fs::read_dir(data_dir)? {
            let entry = entry?;
            let dir = entry.path();
            let Ok(meta) = std::fs::read(dir.join("tenant.json")) else { continue };
            let meta: serde_json::Value = serde_json::from_slice(&meta).unwrap_or_default();
            let id = entry.file_name().to_string_lossy().into_owned();
            if !valid_id(&id) {
                eprintln!("data dir entry '{id}' is not a valid tenant id, skipping");
                continue;
            }
            let base_name = meta.get("base").and_then(|b| b.as_str()).unwrap_or("");
            let Some(base) = self.base(base_name) else {
                eprintln!("tenant {id}: base '{base_name}' is not loaded, skipping");
                continue;
            };
            let tenant = Tenant::new(id.clone(), base, Some(dir.clone()), self.tenant_quota);
            tenant.restore_overlay(&dir)?;
            self.tenants.write().unwrap().insert(id, Arc::new(tenant));
            n += 1;
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant(quota: u64, dir: Option<PathBuf>) -> Tenant {
        let base = Base { name: "b".into(), root: PathBuf::from("."), files: BTreeMap::new() };
        Tenant::new("t".into(), Arc::new(base), dir, quota)
    }

    #[test]
    fn quota_bounds_the_overlay_after_the_write() {
        let t = tenant(10, None);
        t.write("a", vec![0; 6], UpdateKind::Module).unwrap();
        let err = t.write("b", vec![0; 5], UpdateKind::Module).unwrap_err();
        assert!(matches!(err, WriteError::Quota { quota: 10, after: 11 }), "{err}");
        assert!(err.to_string().contains("quota"));
        assert_eq!(t.overlay_stats(), (1, 6));
        t.write("a", vec![0; 10], UpdateKind::Module).unwrap();
        assert!(t.write("a", vec![0; 11], UpdateKind::Module).is_err());
        t.delete("a").unwrap();
        t.write("b", vec![0; 10], UpdateKind::Module).unwrap();
        assert_eq!(t.overlay_stats(), (1, 10));
    }

    #[test]
    fn io_errors_keep_their_message() {
        let err = WriteError::from(io::Error::new(io::ErrorKind::NotFound, "boom"));
        assert_eq!(err.to_string(), "boom");
    }

    #[test]
    fn quota_counts_disk_entries_and_refuses_before_touching_disk() {
        let dir = std::env::temp_dir().join(format!("sandbox-lite-quota-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let big = INLINE_LIMIT as usize + 1;
        let t = tenant(2 * big as u64 - 1, Some(dir.clone()));
        t.write("big", vec![0; big], UpdateKind::Module).unwrap();
        assert!(matches!(t.data("big"), Some(FileData::Disk(_, n)) if n == big as u64));
        assert!(matches!(t.write("big2", vec![0; big], UpdateKind::Module), Err(WriteError::Quota { .. })));
        assert!(!dir.join("files").join("big2").exists());
        assert_eq!(t.overlay_stats(), (1, big as u64));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_stylesheet_extension_swaps_without_the_compiler() {
        for path in ["src/styles/tokens.css", "a.scss", "a.sass", "SRC/A.CSS"] {
            assert_eq!(UpdateKind::from_path(path), UpdateKind::Css, "{path}");
        }
        for path in ["src/pages/index.astro", "src/lib/x.ts", "package.json", "src/styles", "a.css/b"] {
            assert_eq!(UpdateKind::from_path(path), UpdateKind::Module, "{path}");
        }
    }

    #[test]
    fn an_update_event_carries_the_kind() {
        let t = tenant(u64::MAX, None);
        let mut events = t.events.subscribe();
        t.write("src/styles/a.css", b"a{}".to_vec(), UpdateKind::from_path("src/styles/a.css")).unwrap();
        t.write("src/x.astro", b"<p/>".to_vec(), UpdateKind::Style).unwrap();
        t.delete("src/styles/a.css").unwrap();
        let seen: Vec<String> = (0..3).map(|_| events.try_recv().unwrap()).collect();
        assert!(seen[0].contains(r#""type":"update","path":"src/styles/a.css","kind":"css""#), "{}", seen[0]);
        assert!(seen[1].contains(r#""kind":"style""#), "{}", seen[1]);
        assert!(seen[2].contains(r#""type":"delete","path":"src/styles/a.css","kind":"module""#), "{}", seen[2]);
    }

    #[test]
    fn private_paths() {
        assert!(is_private_path(".env"));
        assert!(is_private_path(".env.production"));
        assert!(is_private_path("src/.secret/x.ts"));
        assert!(is_private_path(".git/config"));
        assert!(!is_private_path(".well-known/security.txt"));
        assert!(!is_private_path("src/pages/index.astro"));
        assert_eq!(clean_path("/.env").as_deref(), Some(".env"));
    }
}
