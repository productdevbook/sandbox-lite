use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;
use xxhash_rust::xxh3::xxh3_64;

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

    fn from_file(path: &Path, len: u64) -> io::Result<FileData> {
        if len <= INLINE_LIMIT { Ok(FileData::Mem(std::fs::read(path)?.into())) } else { Ok(FileData::Disk(path.to_path_buf(), len)) }
    }
}

pub struct Base {
    pub name: String,
    pub root: PathBuf,
    /// What `stamp(root)` returned for the tree this base was read from; a poll compares it.
    pub stamp: u64,
    files: BTreeMap<String, FileData>,
}

impl Base {
    pub fn load(name: &str, root: &Path) -> io::Result<Base> {
        let root = root.canonicalize()?;
        let mut files = BTreeMap::new();
        let mut stamps = BTreeMap::new();
        walk(&root, &root, true, &mut |rel: String, path: &Path, md: &std::fs::Metadata| {
            stamps.insert(rel.clone(), file_stamp(md));
            files.insert(rel, FileData::from_file(path, md.len())?);
            Ok(())
        })?;
        Ok(Base { name: name.to_string(), root, stamp: hash_stamps(&stamps), files })
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

type Visit<'a> = &'a mut dyn FnMut(String, &Path, &std::fs::Metadata) -> io::Result<()>;

/// Base projects skip build output and dependencies; a tenant's own overlay must come back whole.
/// `DirEntry::metadata` does not follow symbolic links, so a link is neither descended nor taken.
fn walk(root: &Path, dir: &Path, skip_build_dirs: bool, visit: Visit<'_>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let md = entry.metadata()?;
        if md.is_dir() {
            if skip_build_dirs && SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, skip_build_dirs, &mut *visit)?;
        } else if md.is_file() {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            visit(rel, &path, &md)?;
        }
    }
    Ok(())
}

fn file_stamp(md: &std::fs::Metadata) -> (u128, u64) {
    let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    (mtime, md.len())
}

fn hash_stamps(stamps: &BTreeMap<String, (u128, u64)>) -> u64 {
    let mut buf = Vec::with_capacity(stamps.len() * 32);
    for (path, (mtime, len)) in stamps {
        buf.extend_from_slice(path.as_bytes());
        buf.push(0);
        buf.extend_from_slice(&mtime.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
    }
    xxh3_64(&buf)
}

/// The name, mtime and size of every file `Base::load` would take, hashed in path order. Two walks
/// of an unchanged tree agree; a rewrite that keeps both the size and the mtime does not show up.
pub fn stamp(root: &Path) -> io::Result<u64> {
    let mut stamps = BTreeMap::new();
    walk(root, root, true, &mut |rel: String, _: &Path, md: &std::fs::Metadata| {
        stamps.insert(rel, file_stamp(md));
        Ok(())
    })?;
    Ok(hash_stamps(&stamps))
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
    base: RwLock<Arc<Base>>,
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
        Tenant {
            id,
            base: RwLock::new(base),
            overlay: RwLock::new(BTreeMap::new()),
            version: AtomicU64::new(now_millis()),
            events,
            dir,
            quota,
        }
    }

    pub fn base(&self) -> Arc<Base> {
        self.base.read().unwrap().clone()
    }

    /// Points the tenant at a freshly loaded copy of its base and tells its previews to reload: the
    /// overlay is untouched, so an edited file still wins over the new base copy.
    pub fn set_base(&self, base: Arc<Base>) -> u64 {
        *self.base.write().unwrap() = base;
        // Every file in the tenant may have changed underneath, so the preview reloads rather
        // than swapping a stylesheet.
        self.bump("update", "", UpdateKind::Module)
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
            None => self.base.read().unwrap().get(path).cloned(),
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
        let base = self.base.read().unwrap();
        let mut out: BTreeMap<String, (u64, bool)> = base.files.iter().map(|(p, d)| (p.clone(), (d.size(), false))).collect();
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
            match self.base.read().unwrap().get(path) {
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
            if self.base.read().unwrap().get(path).is_some() {
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
            walk(&files, &files, false, &mut |rel: String, path: &Path, md: &std::fs::Metadata| {
                loaded.insert(rel, FileData::from_file(path, md.len())?);
                Ok(())
            })?;
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
    bases_dir: Option<PathBuf>,
    tenant_quota: u64,
}

impl Store {
    pub fn new(data_dir: Option<PathBuf>, tenant_quota: u64) -> Store {
        Store { bases: RwLock::new(HashMap::new()), tenants: RwLock::new(HashMap::new()), data_dir, bases_dir: None, tenant_quota }
    }

    /// `--bases`: the directory a base added through the API must live under.
    pub fn with_bases_dir(mut self, dir: Option<PathBuf>) -> Store {
        self.bases_dir = dir;
        self
    }

    pub fn add_base(&self, base: Base) -> Arc<Base> {
        let base = Arc::new(base);
        self.bases.write().unwrap().insert(base.name.clone(), base.clone());
        base
    }

    /// Where `POST /api/bases` may read a project from. With `--bases` set the path must resolve
    /// inside it — canonicalized first, so a symbolic link out of the directory is refused too.
    /// Without the flag any readable directory on the host is allowed, which is why the route is
    /// operator-only (see SECURITY.md).
    pub fn base_root(&self, path: &Path) -> Result<PathBuf, String> {
        let root = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        if let Some(dir) = &self.bases_dir {
            let dir = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
            if !root.starts_with(&dir) {
                return Err(format!("path must be inside --bases ({})", dir.display()));
            }
        }
        Ok(root)
    }

    /// Re-reads a base from its own root and points every tenant on it at the result. Each of those
    /// tenants gets a version bump and one `update` event, so open previews reload; the transform
    /// cache needs nothing, being keyed by content. `None` means there is no such base.
    pub fn reload_base(&self, name: &str) -> io::Result<Option<(Arc<Base>, Vec<String>)>> {
        let Some(old) = self.base(name) else { return Ok(None) };
        let fresh = self.add_base(Base::load(name, &old.root)?);
        let mut repointed = Vec::new();
        for t in self.tenants() {
            if t.base().name == name {
                t.set_base(fresh.clone());
                repointed.push(t.id.clone());
            }
        }
        Ok(Some((fresh, repointed)))
    }

    /// One walk per base, comparing `stamp` against what the loaded copy was read from; the names
    /// reloaded come back. Reading a root fails loudly and leaves the loaded copy in place — a base
    /// directory being replaced wholesale should not empty every tenant's site.
    pub fn reload_changed_bases(&self) -> Vec<String> {
        let mut reloaded = Vec::new();
        for base in self.bases() {
            match stamp(&base.root) {
                Ok(s) if s == base.stamp => continue,
                Ok(_) => match self.reload_base(&base.name) {
                    Ok(_) => reloaded.push(base.name.clone()),
                    Err(e) => eprintln!("watch: cannot reload base {}: {e}", base.name),
                },
                Err(e) => eprintln!("watch: cannot read base {} at {}: {e}", base.name, base.root.display()),
            }
        }
        reloaded
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
        // `tenant` rather than the map: a tenant another node created under the same --data-dir
        // exists, and creating over its directory would hide the files already in it
        if self.tenant(id).is_some() {
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

    /// A miss falls back to `--data-dir`: with two daemons sharing one, a tenant created on the
    /// other node is not in this node's map until something asks for it. What this does not do is
    /// refresh a tenant already in memory — it never sees the other node's later writes
    /// (`docs/multi-node.md`).
    pub fn tenant(&self, id: &str) -> Option<Arc<Tenant>> {
        if let Some(t) = self.tenants.read().unwrap().get(id).cloned() {
            return Some(t);
        }
        let restored = Arc::new(self.read_tenant(id).ok()?);
        Some(self.tenants.write().unwrap().entry(id.to_string()).or_insert(restored).clone())
    }

    /// Builds a tenant from `<data-dir>/<id>` without registering it. Every reason it cannot —
    /// no `--data-dir`, no such directory, a base this node has not loaded — is an `Err`.
    fn read_tenant(&self, id: &str) -> Result<Tenant, String> {
        let data_dir = self.data_dir.as_ref().ok_or("no --data-dir")?;
        if !valid_id(id) {
            return Err(format!("'{id}' is not a valid tenant id"));
        }
        let dir = data_dir.join(id);
        let meta = std::fs::read(dir.join("tenant.json")).map_err(|e| format!("tenant.json: {e}"))?;
        let meta: serde_json::Value = serde_json::from_slice(&meta).unwrap_or_default();
        let base_name = meta.get("base").and_then(|b| b.as_str()).unwrap_or("");
        let base = self.base(base_name).ok_or_else(|| format!("base '{base_name}' is not loaded"))?;
        let tenant = Tenant::new(id.to_string(), base, Some(dir.clone()), self.tenant_quota);
        tenant.restore_overlay(&dir).map_err(|e| e.to_string())?;
        Ok(tenant)
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
            if !entry.path().join("tenant.json").is_file() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            match self.read_tenant(&id) {
                Ok(tenant) => {
                    self.tenants.write().unwrap().insert(id, Arc::new(tenant));
                    n += 1;
                }
                Err(e) => eprintln!("data dir entry '{id}': {e}, skipping"),
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant(quota: u64, dir: Option<PathBuf>) -> Tenant {
        let base = Base { name: "b".into(), root: PathBuf::from("."), stamp: 0, files: BTreeMap::new() };
        Tenant::new("t".into(), Arc::new(base), dir, quota)
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sandbox-lite-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seed(path: PathBuf, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    const PAGE: &str = "src/pages/index.astro";

    #[test]
    fn reload_re_points_every_tenant_on_the_base() {
        let root = temp("reload");
        seed(root.join("theme").join(PAGE), "<h1>one</h1>\n");
        let store = Store::new(None, 1 << 20);
        store.add_base(Base::load("theme", &root.join("theme")).unwrap());
        store.add_base(Base::load("other", &root.join("theme")).unwrap());
        let t = store.create_tenant("acme", "theme").unwrap();
        let untouched = store.create_tenant("bakery", "other").unwrap();
        t.write("src/pages/mine.astro", b"<h1>mine</h1>".to_vec(), UpdateKind::Module).unwrap();
        let (version, other_version) = (t.version(), untouched.version());
        let mut events = t.events.subscribe();

        seed(root.join("theme").join(PAGE), "<h1>two</h1>\n");
        let (base, repointed) = store.reload_base("theme").unwrap().unwrap();

        assert_eq!(repointed, vec!["acme".to_string()], "only tenants on that base are re-pointed");
        assert_eq!(t.read_text(PAGE).as_deref(), Some("<h1>two</h1>\n"));
        assert_eq!(t.base().name, base.name);
        assert!(Arc::ptr_eq(&t.base(), &base), "the tenant holds the fresh base, not a copy of the old one");
        assert_eq!(t.read_text("src/pages/mine.astro").as_deref(), Some("<h1>mine</h1>"), "the overlay survives a reload");
        assert!(t.version() > version, "an open preview reloads on the new version");
        assert_eq!(untouched.version(), other_version, "a tenant on another base is not disturbed");
        let event = events.try_recv().unwrap();
        assert!(event.contains(r#""type":"update""#), "{event}");
        assert!(events.try_recv().is_err(), "one event per tenant per reload");
        assert!(store.reload_base("nope").unwrap().is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_watcher_notices_a_changed_file_and_ignores_skipped_directories() {
        let root = temp("watch");
        seed(root.join("theme").join(PAGE), "<h1>one</h1>\n");
        let store = Store::new(None, 1 << 20);
        store.add_base(Base::load("theme", &root.join("theme")).unwrap());
        let t = store.create_tenant("acme", "theme").unwrap();
        assert!(store.reload_changed_bases().is_empty(), "an untouched tree is not reloaded");

        seed(root.join("theme/node_modules/pkg/index.js"), "export const x = 1;\n");
        seed(root.join("theme/dist/index.html"), "<html></html>\n");
        assert!(store.reload_changed_bases().is_empty(), "what Base::load skips, the poll skips");

        seed(root.join("theme").join(PAGE), "<h1>two, and longer</h1>\n");
        assert_eq!(store.reload_changed_bases(), vec!["theme".to_string()]);
        assert_eq!(t.read_text(PAGE).as_deref(), Some("<h1>two, and longer</h1>\n"));
        assert!(store.reload_changed_bases().is_empty(), "the reloaded base is the new stamp");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Two `Store`s over one `--data-dir` are the two daemons of `docs/multi-node.md`.
    #[test]
    fn a_tenant_created_on_one_node_is_served_by_the_other() {
        let root = temp("multi-node");
        seed(root.join("theme").join(PAGE), "<h1>base</h1>\n");
        let base = || Base::load("theme", &root.join("theme")).unwrap();
        let (a, b) = (Store::new(Some(root.join("data")), 1 << 20), Store::new(Some(root.join("data")), 1 << 20));
        a.add_base(base());
        b.add_base(base());
        let ta = a.create_tenant("acme", "theme").unwrap();
        ta.write(PAGE, b"<h1>from a</h1>".to_vec(), UpdateKind::Module).unwrap();

        assert!(b.tenants().is_empty(), "node b learned nothing from node a's write");
        let tb = b.tenant("acme").unwrap();
        assert_eq!(tb.read_text(PAGE).as_deref(), Some("<h1>from a</h1>"), "the miss restored the tenant from the data dir");
        assert_eq!(b.tenants().len(), 1);
        assert!(Arc::ptr_eq(&tb, &b.tenant("acme").unwrap()), "a restored tenant is registered, not rebuilt per request");
        assert!(b.tenant("nobody").is_none());
        assert!(b.create_tenant("acme", "theme").is_err(), "creating over another node's tenant would hide its files");

        // the gap docs/multi-node.md names: node b holds the tenant now, and nothing tells it that
        // node a wrote again. Sticky routing per tenant is what keeps this from being reachable.
        ta.write(PAGE, b"<h1>from a, later</h1>".to_vec(), UpdateKind::Module).unwrap();
        assert_eq!(tb.read_text(PAGE).as_deref(), Some("<h1>from a</h1>"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_base_added_at_runtime_stays_inside_bases() {
        let root = temp("base-root");
        std::fs::create_dir_all(root.join("bases/theme")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        seed(root.join("bases/notadir"), "x");
        let store = Store::new(None, 0).with_bases_dir(Some(root.join("bases")));

        assert_eq!(store.base_root(&root.join("bases/theme")).unwrap(), root.join("bases/theme").canonicalize().unwrap());
        assert!(store.base_root(&root.join("elsewhere")).is_err(), "outside --bases");
        assert!(store.base_root(&root.join("bases/../elsewhere")).is_err(), "a traversal out of --bases");
        assert!(store.base_root(&root.join("bases/missing")).is_err(), "no such directory");
        assert!(store.base_root(&root.join("bases/notadir")).is_err(), "a file is not a base");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("elsewhere"), root.join("bases/link")).unwrap();
            assert!(store.base_root(&root.join("bases/link")).is_err(), "a symlink out of --bases is resolved and refused");
        }
        assert!(Store::new(None, 0).base_root(&root.join("elsewhere")).is_ok(), "without --bases any directory is allowed");
        std::fs::remove_dir_all(&root).unwrap();
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
