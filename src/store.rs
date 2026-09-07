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
/// Edited files one tenant may hold, what `--tenant-max-files` sets. The byte quota bounds none of
/// them: 32,001 empty files fit a 1 KiB quota, and each is an inode of its own (issue #88).
pub const DEFAULT_MAX_FILES: usize = 10_000;
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
    Quota {
        quota: u64,
        after: u64,
    },
    /// The other half of the quota. An empty file weighs nothing and still costs an inode, a
    /// directory entry and a line of tombstone bookkeeping, so bytes alone bound none of them.
    Files {
        limit: usize,
        after: usize,
    },
    Io(io::Error),
    /// A write that failed and could not be put back. The named paths are neither what they were nor
    /// what the write asked for, so the caller is told that rather than that nothing landed.
    Torn {
        cause: io::Error,
        paths: Vec<String>,
    },
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Quota { quota, after } => {
                write!(f, "tenant quota exceeded: edited files would total {after} bytes, the quota is {quota} bytes (--tenant-quota-mb)")
            }
            WriteError::Files { limit, after } => {
                write!(
                    f,
                    "tenant file limit exceeded: the tenant would hold {after} edited files, the limit is {limit} (--tenant-max-files)"
                )
            }
            WriteError::Io(e) => fmt::Display::fmt(e, f),
            WriteError::Torn { cause, paths } => write!(
                f,
                "{cause}; undoing the write failed too, so {} on disk are now neither what they were nor what the write asked for: {}",
                paths.len(),
                paths.join(", ")
            ),
        }
    }
}

impl std::error::Error for WriteError {}

impl From<io::Error> for WriteError {
    fn from(e: io::Error) -> WriteError {
        WriteError::Io(e)
    }
}

/// A tenant's edits and what they weigh. Both totals move with every insert, replace and tombstone
/// rather than being summed when asked: the quota is checked under the write lock, and a walk of the
/// map there makes one write O(files) and blocks every read of the tenant while it runs (issue #88).
#[derive(Default)]
struct Overlay {
    files: BTreeMap<String, Option<FileData>>,
    /// What the written files hold. A tombstone is not a file and weighs nothing.
    bytes: u64,
    /// How many entries are written files rather than tombstones.
    written: usize,
}

/// What one entry contributes to the two totals.
fn weight(data: &Option<FileData>) -> (usize, u64) {
    match data {
        Some(d) => (1, d.size()),
        None => (0, 0),
    }
}

impl Overlay {
    fn get(&self, path: &str) -> Option<&Option<FileData>> {
        self.files.get(path)
    }

    /// What a write to `path` would replace, which the quota check gives back before charging it.
    fn size_of(&self, path: &str) -> u64 {
        self.files.get(path).and_then(|d| d.as_ref()).map_or(0, |d| d.size())
    }

    fn insert(&mut self, path: String, data: Option<FileData>) {
        let (files, bytes) = weight(&data);
        let (was_files, was_bytes) = self.files.insert(path, data).as_ref().map_or((0, 0), weight);
        self.written = self.written - was_files + files;
        self.bytes = self.bytes - was_bytes + bytes;
    }

    fn remove(&mut self, path: &str) {
        if let Some(gone) = self.files.remove(path) {
            let (files, bytes) = weight(&gone);
            self.written -= files;
            self.bytes -= bytes;
        }
    }

    /// A whole new map, counted once. `write_many` builds one, so an import pays a single walk of
    /// what it wrote rather than one per file.
    fn replace(&mut self, files: BTreeMap<String, Option<FileData>>) {
        let (written, bytes) = files.values().map(weight).fold((0, 0), |(n, b), (dn, db)| (n + dn, b + db));
        self.files = files;
        self.written = written;
        self.bytes = bytes;
    }
}

pub struct Tenant {
    pub id: String,
    base: RwLock<Arc<Base>>,
    overlay: RwLock<Overlay>,
    version: AtomicU64,
    pub events: broadcast::Sender<String>,
    dir: Option<PathBuf>,
    quota: u64,
    max_files: usize,
}

pub struct Entry {
    pub path: String,
    pub size: u64,
    pub modified: bool,
}

/// What one `write_many` changed: files written, and overlay entries it dropped or hid.
#[derive(Debug)]
pub struct Applied {
    pub version: u64,
    pub written: usize,
    pub deleted: usize,
}

impl Tenant {
    fn new(id: String, base: Arc<Base>, dir: Option<PathBuf>, quota: u64, max_files: usize) -> Tenant {
        let (events, _) = broadcast::channel(64);
        Tenant {
            id,
            base: RwLock::new(base),
            overlay: RwLock::new(Overlay::default()),
            version: AtomicU64::new(now_millis()),
            events,
            dir,
            quota,
            max_files,
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

    /// `Ok(None)` is a file the tenant does not have; `Err` is one it has and could not read. A
    /// `FileData::Disk` entry is re-read on every access, so the two are different answers.
    pub fn read(&self, path: &str) -> io::Result<Option<Arc<[u8]>>> {
        self.data(path).map(|d| d.read()).transpose()
    }

    pub fn read_text(&self, path: &str) -> io::Result<Option<String>> {
        Ok(self.read(path)?.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    pub fn list(&self) -> Vec<Entry> {
        let overlay = self.overlay.read().unwrap();
        let base = self.base.read().unwrap();
        let mut out: BTreeMap<String, (u64, bool)> = base.files.iter().map(|(p, d)| (p.clone(), (d.size(), false))).collect();
        for (p, d) in overlay.files.iter() {
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
        let after = overlay.bytes - overlay.size_of(path) + bytes.len() as u64;
        if after > self.quota {
            return Err(WriteError::Quota { quota: self.quota, after });
        }
        let files = overlay.written + usize::from(!matches!(overlay.get(path), Some(Some(_))));
        if files > self.max_files {
            return Err(WriteError::Files { limit: self.max_files, after: files });
        }
        let was_tombstone = matches!(overlay.get(path), Some(None));
        let mut staged = Staged::new(self.dir.as_deref());
        let data = match stage_write(&mut staged, &overlay.files, path, bytes, was_tombstone) {
            Ok(data) => data,
            Err(cause) => return Err(staged.undo(cause)),
        };
        overlay.insert(path.to_string(), Some(data));
        drop(overlay);
        staged.commit();
        Ok(self.bump("update", path, kind))
    }

    /// Applies a whole import: every removal and every file land under one lock, one version bump
    /// and one `update` event. `deleted` names paths to hide, `replace` drops every edit the import
    /// does not carry — dropping an edit over a base file brings the base copy back, which is what
    /// makes an overlay export reproduce the source overlay exactly. The quota is checked against
    /// the resulting overlay before anything is written, so a refusal leaves the tenant untouched.
    ///
    /// It is all or nothing: the disk work is staged with an undo — a replaced or removed file is
    /// moved aside rather than deleted — and the overlay is swapped only once every step has
    /// succeeded. A failure puts the directory back, so what the caller is told and what a restart
    /// reads are the same tenant.
    pub fn write_many(&self, files: Vec<(String, Vec<u8>)>, deleted: &[String], replace: bool) -> Result<Applied, WriteError> {
        let mut overlay = self.overlay.write().unwrap();
        let incoming: BTreeSet<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        let keep = |path: &str, data: &Option<FileData>| !replace || data.is_none() || incoming.contains(path);
        let mut next: BTreeMap<String, Option<FileData>> =
            overlay.files.iter().filter(|(p, d)| keep(p, d)).map(|(p, d)| (p.clone(), d.clone())).collect();
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
        let held = next.iter().filter(|(p, d)| !incoming.contains(p.as_str()) && d.is_some()).count() + files.len();
        if held > self.max_files {
            return Err(WriteError::Files { limit: self.max_files, after: held });
        }
        // `None` is untouched, `Some(true)` an edit, `Some(false)` a tombstone: a dropped edit and
        // a new tombstone both read as a change, and only writes are left out
        let overlaid = |o: &BTreeMap<String, Option<FileData>>, p: &str| o.get(p).map(Option::is_some);
        let touched: BTreeSet<&str> = overlay.files.keys().chain(deleted.iter()).map(String::as_str).collect();
        let dropped: Vec<&str> =
            touched.into_iter().filter(|p| !incoming.contains(p) && overlaid(&overlay.files, p) != overlaid(&next, p)).collect();
        let (written, removed) = (files.len(), dropped.len());
        let mut staged = Staged::new(self.dir.as_deref());
        if let Err(cause) = stage_batch(&mut staged, &mut next, &dropped, files) {
            return Err(staged.undo(cause));
        }
        overlay.replace(next);
        drop(overlay);
        staged.commit();
        Ok(Applied { version: self.bump("update", "", UpdateKind::Module), written, deleted: removed })
    }

    pub fn delete(&self, path: &str) -> Result<u64, WriteError> {
        // held across the disk work, as a write is: the file leaves the directory and the tombstone
        // list is rewritten before the overlay hears about it, and a failure puts both back
        let mut overlay = self.overlay.write().unwrap();
        let tombstone = self.base.read().unwrap().get(path).is_some();
        let mut staged = Staged::new(self.dir.as_deref());
        if let Err(cause) = stage_delete(&mut staged, &overlay.files, path, tombstone) {
            return Err(staged.undo(cause));
        }
        if tombstone {
            overlay.insert(path.to_string(), None);
        } else {
            overlay.remove(path);
        }
        drop(overlay);
        staged.commit();
        Ok(self.bump("delete", path, UpdateKind::Module))
    }

    /// The event carries `prev`, the version it follows, as well as the one it makes. A client that
    /// holds some other version has missed a message, and `live.js` reloads rather than swapping
    /// (issue #62) — a lost `module` event would otherwise leave it swapping CSS over stale JS.
    fn bump(&self, event: &str, path: &str, kind: UpdateKind) -> u64 {
        let mut prev = self.version.load(Ordering::Relaxed);
        let v = loop {
            let next = now_millis().max(prev + 1);
            match self.version.compare_exchange(prev, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break next,
                Err(cur) => prev = cur,
            }
        };
        let _ = self.events.send(format!(
            r#"{{"type":"{event}","path":{},"kind":"{}","version":{v},"prev":{prev}}}"#,
            serde_json::to_string(path).unwrap_or_default(),
            kind.as_str()
        ));
        v
    }

    /// The tenant's own edits: `Some` is a written file, `None` a tombstone over a base file.
    pub fn overlay(&self) -> Vec<(String, Option<FileData>)> {
        self.overlay.read().unwrap().files.iter().map(|(p, d)| (p.clone(), d.clone())).collect()
    }

    pub fn quota(&self) -> u64 {
        self.quota
    }

    /// Edited files the tenant may hold, the count the byte quota does not bound.
    pub fn max_files(&self) -> usize {
        self.max_files
    }

    /// Entries and bytes, read off the running totals rather than summed: `/api/stats` and
    /// `/metrics` add this up over every loaded tenant, and the editor polls the first every 5 s.
    pub fn overlay_stats(&self) -> (usize, u64) {
        let overlay = self.overlay.read().unwrap();
        (overlay.files.len(), overlay.bytes)
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
            // A tombstone list that is dropped brings deleted files back, which is not the tenant.
            let list: Vec<String> = serde_json::from_slice(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            for p in list {
                if !overlay.files.contains_key(&p) {
                    overlay.insert(p, None);
                }
            }
        }
        Ok(())
    }
}

/// The disk half of one write, and what it takes to put `<data-dir>/<id>` back. Nothing there is
/// changed that is not recorded first, and a file that is replaced or removed is moved aside rather
/// than deleted — a `FileData::Disk` entry is the only copy of its contents there is. So a failure
/// anywhere can leave the directory exactly as it was, which is what lets the overlay be swapped
/// only after every file has landed.
struct Staged {
    dir: Option<PathBuf>,
    aside: Option<PathBuf>,
    /// (the copy waiting in `aside`, where it came from)
    moved: Vec<(PathBuf, PathBuf)>,
    created: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
    /// `Some` once `deleted.json` has been rewritten: what it held before, or `None` if there was no
    /// such file.
    tombstones: Option<Option<Vec<u8>>>,
}

impl Staged {
    fn new(dir: Option<&Path>) -> Staged {
        Staged { dir: dir.map(Path::to_path_buf), aside: None, moved: Vec::new(), created: Vec::new(), dirs: Vec::new(), tombstones: None }
    }

    /// `<data-dir>/<id>/.staged-<pid>-<n>`, a sibling of `files/` rather than a directory inside it,
    /// so a copy left behind by a process that dies mid-write is not read back as a tenant file.
    fn aside_dir(&mut self, dir: &Path) -> io::Result<PathBuf> {
        if let Some(p) = &self.aside {
            return Ok(p.clone());
        }
        static N: AtomicU64 = AtomicU64::new(0);
        let p = dir.join(format!(".staged-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&p)?;
        self.aside = Some(p.clone());
        Ok(p)
    }

    fn move_aside(&mut self, dir: &Path, target: &Path) -> io::Result<()> {
        if !target.exists() {
            return Ok(());
        }
        let kept = self.aside_dir(dir)?.join(self.moved.len().to_string());
        std::fs::rename(target, &kept)?;
        self.moved.push((kept, target.to_path_buf()));
        Ok(())
    }

    /// Creates the directories `target` needs, recording the ones that did not exist so the undo can
    /// take them away again.
    fn create_dirs(&mut self, target: &Path) -> io::Result<()> {
        let mut missing = Vec::new();
        let mut parent = target.parent();
        while let Some(d) = parent.filter(|d| !d.exists()) {
            missing.push(d.to_path_buf());
            parent = d.parent();
        }
        for d in missing.into_iter().rev() {
            std::fs::create_dir(&d)?;
            self.dirs.push(d);
        }
        Ok(())
    }

    /// Writes the file under `--data-dir` when there is one, and says how the overlay should hold it:
    /// anything past the inline limit lives on disk only.
    fn write(&mut self, path: &str, bytes: Vec<u8>) -> io::Result<FileData> {
        let Some(dir) = self.dir.clone() else { return Ok(FileData::Mem(bytes.into())) };
        let target = dir.join("files").join(path);
        self.create_dirs(&target)?;
        self.move_aside(&dir, &target)?;
        // recorded before the write, because a write that fails partway still leaves a file
        self.created.push(target.clone());
        std::fs::write(&target, &bytes)?;
        if bytes.len() as u64 > INLINE_LIMIT { Ok(FileData::Disk(target, bytes.len() as u64)) } else { Ok(FileData::Mem(bytes.into())) }
    }

    fn remove(&mut self, path: &str) -> io::Result<()> {
        let Some(dir) = self.dir.clone() else { return Ok(()) };
        let target = dir.join("files").join(path);
        self.move_aside(&dir, &target)
    }

    fn tombstones(&mut self, list: &[&str]) -> io::Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let file = dir.join("deleted.json");
        let before = match std::fs::read(&file) {
            Ok(raw) => Some(raw),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            // a list that is there and unreadable must not be undone as if there were none
            Err(e) => return Err(e),
        };
        self.tombstones = Some(before);
        std::fs::write(file, serde_json::to_vec(list)?)
    }

    fn commit(self) {
        if let Some(aside) = &self.aside {
            sweep(aside);
        }
    }

    /// Puts the directory back, newest step first. What it could not put back comes back as paths,
    /// and those are the only ones the caller may not describe as untouched.
    fn rollback(self) -> Vec<String> {
        let mut torn = Vec::new();
        for target in self.created.iter().rev() {
            if let Err(e) = remove_if_present(target) {
                torn.push(format!("{} ({e})", target.display()));
            }
        }
        for (kept, target) in self.moved.iter().rev() {
            if let Err(e) = std::fs::rename(kept, target) {
                torn.push(format!("{} ({e})", target.display()));
            }
        }
        if let (Some(dir), Some(before)) = (&self.dir, &self.tombstones) {
            let file = dir.join("deleted.json");
            let back = match before {
                Some(raw) => std::fs::write(&file, raw),
                None => remove_if_present(&file),
            };
            if let Err(e) = back {
                torn.push(format!("{} ({e})", file.display()));
            }
        }
        for d in self.dirs.iter().rev() {
            // a directory something else has since put a file in is not empty, and stays
            if let Err(e) = std::fs::remove_dir(d)
                && !matches!(e.kind(), io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound)
            {
                eprintln!("write: cannot remove the directory {} the write made: {e}", d.display());
            }
        }
        if let Some(aside) = &self.aside {
            sweep(aside);
        }
        if !torn.is_empty() {
            eprintln!("write: undoing a failed write left {} path(s) neither way: {}", torn.len(), torn.join(", "));
        }
        torn
    }

    fn undo(self, cause: io::Error) -> WriteError {
        match self.rollback() {
            paths if paths.is_empty() => WriteError::Io(cause),
            paths => WriteError::Torn { cause, paths },
        }
    }
}

/// `remove_file` on a path whose parent turned out not to be a directory answers `ENOTDIR`, not
/// `NotFound`, so whether the file is there is asked rather than read out of the error.
fn remove_if_present(target: &Path) -> io::Result<()> {
    if target.exists() { std::fs::remove_file(target) } else { Ok(()) }
}

/// The copies waiting in the staging directory are dead once a write has landed or been undone, but
/// failing to sweep them cannot fail the write: it is reported instead.
fn sweep(aside: &Path) {
    if let Err(e) = std::fs::remove_dir_all(aside) {
        eprintln!("write: cannot remove the staging directory {}: {e}", aside.display());
    }
}

/// One file's disk work. The tombstone list is rewritten here rather than after the overlay changes,
/// so a failure to record it is a failure of the whole write.
fn stage_write(
    staged: &mut Staged,
    overlay: &BTreeMap<String, Option<FileData>>,
    path: &str,
    bytes: Vec<u8>,
    was_tombstone: bool,
) -> io::Result<FileData> {
    let data = staged.write(path, bytes)?;
    if was_tombstone {
        let list: Vec<&str> = overlay.iter().filter(|(p, d)| d.is_none() && p.as_str() != path).map(|(p, _)| p.as_str()).collect();
        staged.tombstones(&list)?;
    }
    Ok(data)
}

/// Every disk change one batch makes. Until it answers `Ok` nothing has touched the overlay, and the
/// `next` it fills in is only worth swapping in if it did.
fn stage_batch(
    staged: &mut Staged,
    next: &mut BTreeMap<String, Option<FileData>>,
    dropped: &[&str],
    files: Vec<(String, Vec<u8>)>,
) -> io::Result<()> {
    for path in dropped {
        staged.remove(path)?;
    }
    for (path, bytes) in files {
        let data = staged.write(&path, bytes)?;
        next.insert(path, Some(data));
    }
    let list: Vec<&str> = next.iter().filter(|(_, d)| d.is_none()).map(|(p, _)| p.as_str()).collect();
    staged.tombstones(&list)
}

fn stage_delete(staged: &mut Staged, overlay: &BTreeMap<String, Option<FileData>>, path: &str, tombstone: bool) -> io::Result<()> {
    staged.remove(path)?;
    let mut list: Vec<&str> = overlay.iter().filter(|(p, d)| d.is_none() && p.as_str() != path).map(|(p, _)| p.as_str()).collect();
    if tombstone {
        list.push(path);
        list.sort_unstable();
    }
    staged.tombstones(&list)
}

/// Why a tenant is not here. A caller that cannot tell the two apart reports a customer's site that
/// this node could not load as one that never existed (issue #61).
#[derive(Debug)]
pub enum NoTenant {
    /// Neither the map nor `--data-dir` holds it.
    Unknown,
    /// `<data-dir>/<id>` is there and a tenant could not be built from it; the reason is the string.
    Failed(String),
}

pub struct Store {
    bases: RwLock<HashMap<String, Arc<Base>>>,
    tenants: RwLock<HashMap<String, Arc<Tenant>>>,
    /// Tenant ids whose data directory is on disk and would not load, and why: `restore` fills it at
    /// startup and `resolve` on a later miss. Reported by `/api/stats`, `/metrics` and the editor,
    /// because a tenant that answers 404 while the daemon reports itself healthy is invisible.
    failed: RwLock<BTreeMap<String, String>>,
    data_dir: Option<PathBuf>,
    bases_dir: Option<PathBuf>,
    tenant_quota: u64,
    tenant_max_files: usize,
}

impl Store {
    pub fn new(data_dir: Option<PathBuf>, tenant_quota: u64) -> Store {
        Store {
            bases: RwLock::new(HashMap::new()),
            tenants: RwLock::new(HashMap::new()),
            failed: RwLock::new(BTreeMap::new()),
            data_dir,
            bases_dir: None,
            tenant_quota,
            tenant_max_files: DEFAULT_MAX_FILES,
        }
    }

    /// `--tenant-max-files`: edited files one tenant may hold, whatever they weigh.
    pub fn with_max_files(mut self, max_files: usize) -> Store {
        self.tenant_max_files = max_files.max(1);
        self
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

    /// Where a base project may be read from — `POST /api/bases` and the startup scan alike. With
    /// `--bases` set the path must resolve inside it — canonicalized first, so a symbolic link out
    /// of the directory is refused too. Without the flag any readable directory on the host is
    /// allowed, which is why the route is operator-only (see SECURITY.md).
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

    /// The base projects `--bases` holds: one per sub-directory, named after it, in name order.
    /// Every root goes through `base_root`, so the startup scan and `POST /api/bases` refuse the
    /// same paths. `Ok(Vec::new())` without the flag.
    pub fn bases_in_dir(&self) -> io::Result<Vec<(String, PathBuf)>> {
        let Some(dir) = &self.bases_dir else { return Ok(Vec::new()) };
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // the entry's own type, not the resolved one: `Path::is_dir` follows the link and takes
            // whatever it points at as a base
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                eprintln!("bases dir: skipping '{name}': a symbolic link is not followed (SECURITY.md)");
                continue;
            }
            if !file_type.is_dir() {
                continue;
            }
            match self.base_root(&entry.path()) {
                Ok(root) => found.push((name, root)),
                Err(e) => eprintln!("bases dir: skipping '{name}': {e}"),
            }
        }
        found.sort();
        Ok(found)
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
        // One acquisition for the whole operation — the refusal, the directory and the insert. Taken
        // and released around the check, two callers of one id both read "not there" and both went
        // on to write `tenant.json` and insert, so all of them were told 201 and all but the last
        // held a tenant this node does not serve (issue #92).
        let mut tenants = self.tenants.write().unwrap();
        // the data directory as well as the map: a tenant another node created exists, and creating
        // over its directory would hide the files already in it. The map alone would miss one whose
        // base this node has not loaded, which is exactly when the map is empty of it.
        if tenants.contains_key(id) || self.tenant_dir(id).is_some() {
            return Err(format!("tenant '{id}' already exists"));
        }
        let dir = self.data_dir.as_ref().map(|d| d.join(id));
        if let Some(dir) = &dir {
            std::fs::create_dir_all(dir.join("files")).map_err(|e| e.to_string())?;
            std::fs::write(dir.join("tenant.json"), format!(r#"{{"base":{}}}"#, serde_json::to_string(base_name).unwrap()))
                .map_err(|e| e.to_string())?;
        }
        let tenant = Arc::new(Tenant::new(id.to_string(), base, dir, self.tenant_quota, self.tenant_max_files));
        tenants.insert(id.to_string(), tenant.clone());
        Ok(tenant)
    }

    /// A miss falls back to `--data-dir`: with two daemons sharing one, a tenant created on the
    /// other node is not in this node's map until something asks for it. What this does not do is
    /// refresh a tenant already in memory — it never sees the other node's later writes
    /// (`docs/multi-node.md`).
    ///
    /// A directory that is there and will not load is `NoTenant::Failed`, not `Unknown`, and is
    /// remembered so `/api/stats`, `/metrics` and the editor can report it.
    pub fn resolve(&self, id: &str) -> Result<Arc<Tenant>, NoTenant> {
        if let Some(t) = self.tenants.read().unwrap().get(id).cloned() {
            return Ok(t);
        }
        // A miss reads the directory under the write lock, not around it: `remove_tenant` holds the
        // same lock from the drop to the last file, so what this reads back is a tenant that is
        // still there rather than one being deleted behind it (issue #92). The lock is re-checked
        // because a create or another miss may have won it first.
        let mut tenants = self.tenants.write().unwrap();
        if let Some(t) = tenants.get(id).cloned() {
            return Ok(t);
        }
        if self.tenant_dir(id).is_none() {
            return Err(NoTenant::Unknown);
        }
        match self.read_tenant(id) {
            Ok(t) => {
                self.failed.write().unwrap().remove(id);
                let restored = Arc::new(t);
                tenants.insert(id.to_string(), restored.clone());
                Ok(restored)
            }
            Err(e) => {
                self.failed.write().unwrap().insert(id.to_string(), e.clone());
                Err(NoTenant::Failed(e))
            }
        }
    }

    /// `resolve` for a test that only asks whether the tenant is here. Nothing in the daemon calls
    /// it: every caller there reports *why* a tenant is missing, and `create_tenant` — the last one
    /// that did not — now asks the map it already holds the lock on.
    #[cfg(test)]
    pub fn tenant(&self, id: &str) -> Option<Arc<Tenant>> {
        self.resolve(id).ok()
    }

    /// The tenants whose data directory this node could not build a tenant from, id and reason, in
    /// id order. Empty is the healthy answer; anything in it is a site answering nothing.
    pub fn failed_tenants(&self) -> Vec<(String, String)> {
        self.failed.read().unwrap().iter().map(|(id, e)| (id.clone(), e.clone())).collect()
    }

    /// `<data-dir>/<id>` when that directory is there: the bytes a tenant is, whether or not this
    /// node has it in memory and whether or not it could build one from them. `None` with
    /// `--no-persist`, and for an id that is not a valid directory name.
    fn tenant_dir(&self, id: &str) -> Option<PathBuf> {
        if !valid_id(id) {
            return None;
        }
        let dir = self.data_dir.as_ref()?.join(id);
        dir.is_dir().then_some(dir)
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
        // Read as null, a tenant.json that does not parse is reported as an unloaded base '' rather
        // than as the unreadable file it is.
        let meta: serde_json::Value = serde_json::from_slice(&meta).map_err(|e| format!("tenant.json does not parse: {e}"))?;
        let base_name = meta.get("base").and_then(|b| b.as_str()).unwrap_or("");
        let base = self.base(base_name).ok_or_else(|| format!("base '{base_name}' is not loaded"))?;
        let tenant = Tenant::new(id.to_string(), base, Some(dir.clone()), self.tenant_quota, self.tenant_max_files);
        tenant.restore_overlay(&dir).map_err(|e| e.to_string())?;
        Ok(tenant)
    }

    /// The tenants this node holds in memory. Not every tenant under `--data-dir`: one no request
    /// has asked for since startup is not here (`Store::tenant`), so on a second node this is a
    /// report of what is loaded rather than a census. Anything that acts on a tenant resolves it
    /// through `tenant` or `tenant_dir` instead.
    pub fn tenants(&self) -> Vec<Arc<Tenant>> {
        let mut v: Vec<_> = self.tenants.read().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Drops the tenant from memory and removes `<data-dir>/<id>`, resolving it the way
    /// `Store::tenant` does: a tenant that is on disk and not loaded is deleted, not answered
    /// `404`, or the next request would restore it and serve it again. `Ok(false)` is a tenant
    /// neither memory nor the data directory holds. A data directory that survives the delete is an
    /// error: it answered 204 and the tenant comes back at the next restore.
    ///
    /// The map's write lock is held across the directory removal. Released after the drop, the
    /// directory was still there for `resolve` to rebuild the tenant from and put back in the map,
    /// so a 204 left a tenant serving traffic from a directory that was about to go (issue #92).
    pub fn remove_tenant(&self, id: &str) -> io::Result<bool> {
        let mut tenants = self.tenants.write().unwrap();
        let dropped = tenants.remove(id);
        self.failed.write().unwrap().remove(id);
        let dir = dropped.as_ref().and_then(|t| t.dir.clone()).or_else(|| self.tenant_dir(id));
        let mut held = dropped.is_some();
        if let Some(dir) = dir {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => held = true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(held)
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
                    self.failed.write().unwrap().remove(&id);
                    self.tenants.write().unwrap().insert(id, Arc::new(tenant));
                    n += 1;
                }
                Err(e) => {
                    eprintln!("data dir entry '{id}': {e}, skipping");
                    self.failed.write().unwrap().insert(id, e);
                }
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant(quota: u64, dir: Option<PathBuf>) -> Tenant {
        tenant_holding(quota, DEFAULT_MAX_FILES, dir)
    }

    fn tenant_holding(quota: u64, max_files: usize, dir: Option<PathBuf>) -> Tenant {
        let base = Base { name: "b".into(), root: PathBuf::from("."), stamp: 0, files: BTreeMap::new() };
        Tenant::new("t".into(), Arc::new(base), dir, quota, max_files)
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
        assert_eq!(t.read_text(PAGE).unwrap().as_deref(), Some("<h1>two</h1>\n"));
        assert_eq!(t.base().name, base.name);
        assert!(Arc::ptr_eq(&t.base(), &base), "the tenant holds the fresh base, not a copy of the old one");
        assert_eq!(t.read_text("src/pages/mine.astro").unwrap().as_deref(), Some("<h1>mine</h1>"), "the overlay survives a reload");
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
        assert_eq!(t.read_text(PAGE).unwrap().as_deref(), Some("<h1>two, and longer</h1>\n"));
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
        assert_eq!(tb.read_text(PAGE).unwrap().as_deref(), Some("<h1>from a</h1>"), "the miss restored the tenant from the data dir");
        assert_eq!(b.tenants().len(), 1);
        assert!(Arc::ptr_eq(&tb, &b.tenant("acme").unwrap()), "a restored tenant is registered, not rebuilt per request");
        assert!(b.tenant("nobody").is_none());
        assert!(b.create_tenant("acme", "theme").is_err(), "creating over another node's tenant would hide its files");

        // the gap docs/multi-node.md names: node b holds the tenant now, and nothing tells it that
        // node a wrote again. Sticky routing per tenant is what keeps this from being reachable.
        ta.write(PAGE, b"<h1>from a, later</h1>".to_vec(), UpdateKind::Module).unwrap();
        assert_eq!(tb.read_text(PAGE).unwrap().as_deref(), Some("<h1>from a</h1>"));
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

    /// Issue #73: `Path::is_dir` resolved a link in `--bases`, so a directory the API refused a
    /// second later was already a base — and every file under it was served through `/__sl/raw/`.
    #[test]
    fn the_startup_scan_takes_only_what_the_api_would_take() {
        let root = temp("bases-scan");
        std::fs::create_dir_all(root.join("bases/theme")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        seed(root.join("outside/secret.txt"), "SECRET\n");
        seed(root.join("bases/README.md"), "not a base\n");
        let store = Store::new(None, 0).with_bases_dir(Some(root.join("bases")));
        let theme = || vec![("theme".to_string(), root.join("bases/theme").canonicalize().unwrap())];

        assert_eq!(store.bases_in_dir().unwrap(), theme(), "a file in the bases dir is not a base");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("outside"), root.join("bases/sneaky")).unwrap();
            std::os::unix::fs::symlink(root.join("bases/theme"), root.join("bases/alias")).unwrap();
            assert_eq!(store.bases_in_dir().unwrap(), theme(), "a symbolic link in --bases is skipped, wherever it points");
            assert!(store.base_root(&root.join("bases/sneaky")).is_err(), "which is what the API answers for the same path");
        }
        assert!(Store::new(None, 0).bases_in_dir().unwrap().is_empty(), "no --bases, nothing to scan");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Issue #74: the delete acted on the map, and `Store::tenant` restores from the data
    /// directory on a miss — so a tenant nothing had asked for yet answered 404, kept its files,
    /// and was served again by the next request.
    #[test]
    fn deleting_a_tenant_that_is_not_loaded_removes_its_files() {
        let root = temp("delete-unloaded");
        seed(root.join("theme").join(PAGE), "<h1>base</h1>\n");
        let data = root.join("data");
        let base = || Base::load("theme", &root.join("theme")).unwrap();
        let store = Store::new(Some(data.clone()), 1 << 20);
        store.add_base(base());
        let t = store.create_tenant("acme", "theme").unwrap();
        t.write(PAGE, b"<h1>mine</h1>".to_vec(), UpdateKind::Module).unwrap();
        // what another node's daemon holds: the tenant is on disk and in nobody's map
        store.tenants.write().unwrap().remove("acme");
        assert!(store.tenant("acme").is_some(), "the restore-on-miss path serves it");
        store.tenants.write().unwrap().remove("acme");

        assert!(store.remove_tenant("acme").unwrap(), "a tenant the daemon can serve is deleted, not answered 404");
        assert!(!data.join("acme").exists(), "the tenant's bytes are gone");
        assert!(store.tenant("acme").is_none(), "and nothing brings it back");
        let fresh = Store::new(Some(data.clone()), 1 << 20);
        fresh.add_base(base());
        assert_eq!(fresh.restore().unwrap(), 0, "a fresh store over the same data dir restores nothing");
        assert!(fresh.tenant("acme").is_none());

        assert!(!store.remove_tenant("acme").unwrap(), "neither memory nor disk holds it now");
        assert!(!store.remove_tenant("../escape").unwrap(), "an id that is not a directory name never reaches the data dir");
        assert!(store.create_tenant("acme", "theme").is_ok(), "and the id is free again");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The other half of the same question: a directory under `--data-dir` is a tenant even when
    /// this node cannot build one from it, and creating over it would adopt the files in it.
    #[test]
    fn a_tenant_directory_is_taken_as_existing_even_when_its_base_is_not_loaded() {
        let root = temp("create-over");
        seed(root.join("theme").join(PAGE), "<h1>base</h1>\n");
        let data = root.join("data");
        let a = Store::new(Some(data.clone()), 1 << 20);
        a.add_base(Base::load("theme", &root.join("theme")).unwrap());
        a.create_tenant("acme", "theme").unwrap().write("secret.txt", b"SECRET".to_vec(), UpdateKind::Module).unwrap();

        let b = Store::new(Some(data.clone()), 1 << 20);
        b.add_base(Base::load("other", &root.join("theme")).unwrap());
        assert!(b.tenant("acme").is_none(), "b cannot restore it: the base it names is not loaded here");
        assert!(b.create_tenant("acme", "other").is_err(), "so the directory is what says the tenant exists");
        assert_eq!(std::fs::read_to_string(data.join("acme/files/secret.txt")).unwrap(), "SECRET", "and its files are untouched");
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

    /// Issue #88: `--tenant-quota-mb` counts bytes, and an empty file has none. A 1 KiB tenant took
    /// 32,001 of them without one write being refused, each a real inode and a real directory entry.
    #[test]
    fn a_flood_of_empty_files_is_refused_at_the_file_limit() {
        let t = tenant_holding(1024, 4, None);
        for n in 0..4 {
            t.write(&format!("src/e{n}.ts"), Vec::new(), UpdateKind::Module).unwrap();
        }
        let err = t.write("src/e4.ts", Vec::new(), UpdateKind::Module).unwrap_err();
        assert!(matches!(err, WriteError::Files { limit: 4, after: 5 }), "{err}");
        assert!(err.to_string().contains("--tenant-max-files"), "{err}");
        assert_eq!(t.overlay_stats(), (4, 0), "the tenant is what it was: nothing weighed anything");
        // the byte quota is untouched by any of it, which is the hole
        assert!(t.write("src/e0.ts", vec![b'x'; 8], UpdateKind::Module).is_ok(), "replacing a file it already holds still fits");

        // and the import cannot get round it either
        let flood: Vec<(String, Vec<u8>)> = (0..5).map(|n| (format!("src/i{n}.ts"), Vec::new())).collect();
        let err = tenant_holding(1024, 4, None).write_many(flood, &[], true).unwrap_err();
        assert!(matches!(err, WriteError::Files { limit: 4, after: 5 }), "{err}");
    }

    /// Issue #88: `Tenant::write` re-summed the whole overlay under the write lock, so one write
    /// cost O(files) — 23 s to reach 32k, with every read of that tenant blocked behind it. A
    /// timing assertion would be flaky, so what is asserted is the property that lets the check be
    /// O(1): the totals the overlay keeps equal a fresh sum after every kind of change, at the size
    /// where re-summing was the cost.
    #[test]
    fn the_overlay_keeps_its_own_totals_at_thirty_two_thousand_files() {
        let summed = |t: &Tenant| {
            let held = t.overlay();
            (held.len(), held.iter().filter_map(|(_, d)| d.as_ref()).map(|d| d.size()).sum::<u64>())
        };
        let t = tenant_holding(1 << 30, 40_000, None);
        for n in 0..32_000 {
            t.write(&format!("src/e{n}.ts"), Vec::new(), UpdateKind::Module).unwrap();
        }
        assert_eq!(t.overlay_stats(), (32_000, 0));
        assert_eq!(t.overlay_stats(), summed(&t));

        t.write("src/e0.ts", vec![b'x'; 100], UpdateKind::Module).unwrap();
        assert_eq!(t.overlay_stats(), (32_000, 100), "a replacement charges the difference, not the file");
        assert_eq!(t.overlay_stats(), summed(&t));

        t.write("src/e0.ts", vec![b'x'; 10], UpdateKind::Module).unwrap();
        assert_eq!(t.overlay_stats(), (32_000, 10));

        t.delete("src/e0.ts").unwrap();
        assert_eq!(t.overlay_stats(), (31_999, 0), "no base copy, so the entry goes rather than turning into a tombstone");
        assert_eq!(t.overlay_stats(), summed(&t));

        t.write_many(vec![("a.ts".into(), vec![b'x'; 7]), ("b.ts".into(), vec![b'y'; 3])], &[], true).unwrap();
        assert_eq!(t.overlay_stats(), (2, 10), "`replace` dropped every edit the import did not carry");
        assert_eq!(t.overlay_stats(), summed(&t));
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

    /// Issue #61: a tenant whose directory is there and will not load used to leave nothing behind
    /// but a line on stderr — the site answered 404 and the daemon called itself healthy.
    #[test]
    fn a_tenant_that_cannot_be_restored_is_counted_and_named() {
        let root = temp("restore-failure");
        seed(root.join("theme").join(PAGE), "<h1>base</h1>\n");
        let data = root.join("data");
        let store = Store::new(Some(data.clone()), 1 << 20);
        store.add_base(Base::load("theme", &root.join("theme")).unwrap());
        store.create_tenant("acme", "theme").unwrap();
        // a tenant another node created on a base this one has not loaded
        seed(data.join("bakery/tenant.json"), r#"{"base":"missing"}"#);
        seed(data.join("bakery/files").join(PAGE), "<h1>bakery</h1>\n");

        let fresh = Store::new(Some(data.clone()), 1 << 20);
        fresh.add_base(Base::load("theme", &root.join("theme")).unwrap());
        assert_eq!(fresh.restore().unwrap(), 1, "the good tenant is restored");

        let failed = fresh.failed_tenants();
        assert_eq!(failed.len(), 1, "the one that would not load is counted, not dropped: {failed:?}");
        assert_eq!(failed[0].0, "bakery");
        assert!(failed[0].1.contains("missing"), "the reason names the base: {}", failed[0].1);

        // never existed and could not be loaded stay apart
        assert!(matches!(fresh.resolve("bakery"), Err(NoTenant::Failed(_))));
        assert!(matches!(fresh.resolve("nobody"), Err(NoTenant::Unknown)));
        assert!(fresh.resolve("acme").is_ok());

        // and a miss on a later request records the same way
        let cold = Store::new(Some(data.clone()), 1 << 20);
        assert!(cold.failed_tenants().is_empty());
        assert!(matches!(cold.resolve("bakery"), Err(NoTenant::Failed(_))));
        assert_eq!(cold.failed_tenants().len(), 1);

        // deleting it closes the gap rather than leaving a phantom failure behind
        assert!(fresh.remove_tenant("bakery").unwrap());
        assert!(fresh.failed_tenants().is_empty());
        assert!(matches!(fresh.resolve("bakery"), Err(NoTenant::Unknown)));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Issue #62: the client can only refuse a swap it should not apply if every event says which
    /// version it follows.
    #[test]
    fn every_event_names_the_version_it_follows() {
        let t = tenant(u64::MAX, None);
        let mut events = t.events.subscribe();
        let start = t.version();
        let a = t.write("src/styles/a.css", b"a{}".to_vec(), UpdateKind::Css).unwrap();
        let b = t.write("src/x.astro", b"<p/>".to_vec(), UpdateKind::Style).unwrap();
        let c = t.delete("src/styles/a.css").unwrap();
        let seen: Vec<String> = (0..3).map(|_| events.try_recv().unwrap()).collect();
        assert!(seen[0].contains(&format!(r#""version":{a},"prev":{start}"#)), "{}", seen[0]);
        assert!(seen[1].contains(&format!(r#""version":{b},"prev":{a}"#)), "{}", seen[1]);
        assert!(seen[2].contains(&format!(r#""version":{c},"prev":{b}"#)), "{}", seen[2]);
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

    /// Issue #92: the check and the mutation used to sit under two acquisitions of the `tenants`
    /// lock, so eight threads creating one id were all told it was theirs. Seven of them held a
    /// tenant — its own overlay, its own version, its own event channel — that the store did not
    /// serve.
    #[test]
    fn one_create_of_an_id_wins_and_every_other_caller_is_refused() {
        let root = temp("create-race");
        seed(root.join("theme").join(PAGE), "<h1>base</h1>\n");
        let data = root.join("data");
        let store = Arc::new(Store::new(Some(data.clone()), 1 << 20));
        store.add_base(Base::load("theme", &root.join("theme")).unwrap());

        let gun = Arc::new(std::sync::Barrier::new(8));
        let racing: Vec<_> = (0..8)
            .map(|_| {
                let (store, gun) = (store.clone(), gun.clone());
                std::thread::spawn(move || {
                    gun.wait();
                    store.create_tenant("acme", "theme")
                })
            })
            .collect();
        let answers: Vec<Result<Arc<Tenant>, String>> = racing.into_iter().map(|t| t.join().unwrap()).collect();

        let created: Vec<&Arc<Tenant>> = answers.iter().filter_map(|a| a.as_ref().ok()).collect();
        assert_eq!(created.len(), 1, "exactly one create may be told it made the tenant");
        for refused in answers.iter().filter_map(|a| a.as_ref().err()) {
            assert!(refused.contains("already exists"), "the losers are refused, not served: {refused}");
        }
        assert!(Arc::ptr_eq(&store.tenant("acme").unwrap(), created[0]), "and the winner is the tenant the store serves");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Issue #92: `remove_tenant` dropped the map entry, released the lock and only then removed the
    /// directory. A request in that gap read the tenant back off disk and put it in the map, so a
    /// 204 left it answering from a directory that was about to go — for ever, since a later write
    /// recreates `files/` without `tenant.json` and `restore` skips such a directory.
    #[test]
    fn a_tenant_deleted_while_a_request_resolves_it_stays_deleted() {
        let root = temp("remove-race");
        seed(root.join("theme").join(PAGE), "<h1>base</h1>\n");
        let data = root.join("data");
        let store = Arc::new(Store::new(Some(data.clone()), 1 << 20));
        store.add_base(Base::load("theme", &root.join("theme")).unwrap());

        for round in 0..200 {
            store.create_tenant("acme", "theme").unwrap();
            let gun = Arc::new(std::sync::Barrier::new(2));
            let (remover, requester) = (store.clone(), store.clone());
            let (start, also) = (gun.clone(), gun);
            let removing = std::thread::spawn(move || {
                start.wait();
                remover.remove_tenant("acme").unwrap()
            });
            let resolving = std::thread::spawn(move || {
                also.wait();
                requester.tenant("acme")
            });
            assert!(removing.join().unwrap(), "the delete answered 204, round {round}");
            resolving.join().unwrap();
            assert!(store.tenant("acme").is_none(), "so nothing may serve it afterwards, round {round}");
            assert!(!data.join("acme").exists(), "and its data directory is gone, round {round}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Issue #96: `delete` is the third writer, and its undo can fail like the other two. It used to
    /// answer `io::Result`, which cannot say so — the paths its rollback could not put back went on
    /// the floor and the caller read the bare cause as "your file is still there".
    #[test]
    fn a_delete_whose_undo_fails_names_the_paths_that_are_neither_way() {
        let dir = temp("torn-delete");
        let t = tenant(1 << 20, Some(dir.clone()));
        t.write("a.txt", b"one".to_vec(), UpdateKind::Module).unwrap();
        // `stage_delete` moves the file aside and then rewrites the tombstone list; the undo rewrites
        // it back. A mode that refuses both is the failure and the failed undo in one.
        let list = dir.join("deleted.json");
        std::fs::write(&list, b"[]").unwrap();
        let mut mode = std::fs::metadata(&list).unwrap().permissions();
        mode.set_readonly(true);
        std::fs::set_permissions(&list, mode).unwrap();
        if std::fs::OpenOptions::new().write(true).open(&list).is_ok() {
            // running as root, where the mode is not enforced and the write this needs cannot fail
            eprintln!("skipped: this process can write a read-only file, so a failed undo cannot be staged");
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let err = t.delete("a.txt").unwrap_err();
        let WriteError::Torn { cause, paths } = &err else { panic!("a delete whose undo failed must be Torn, not {err:?}") };
        assert!(paths.iter().any(|p| p.contains("deleted.json")), "naming what it could not put back: {paths:?}");
        assert!(err.to_string().contains(&cause.to_string()), "and the cause survives: {err}");
        assert!(err.to_string().contains("neither what they were"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
