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
    Quota {
        quota: u64,
        after: u64,
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
        let was_tombstone = matches!(overlay.get(path), Some(None));
        let mut staged = Staged::new(self.dir.as_deref());
        let data = match stage_write(&mut staged, &overlay, path, bytes, was_tombstone) {
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
        let (written, removed) = (files.len(), dropped.len());
        let mut staged = Staged::new(self.dir.as_deref());
        if let Err(cause) = stage_batch(&mut staged, &mut next, &dropped, files) {
            return Err(staged.undo(cause));
        }
        *overlay = next;
        drop(overlay);
        staged.commit();
        Ok(Applied { version: self.bump("update", "", UpdateKind::Module), written, deleted: removed })
    }

    pub fn delete(&self, path: &str) -> io::Result<u64> {
        // held across the disk work, as a write is: the file leaves the directory and the tombstone
        // list is rewritten before the overlay hears about it, and a failure puts both back
        let mut overlay = self.overlay.write().unwrap();
        let tombstone = self.base.read().unwrap().get(path).is_some();
        let mut staged = Staged::new(self.dir.as_deref());
        if let Err(cause) = stage_delete(&mut staged, &overlay, path, tombstone) {
            staged.rollback();
            return Err(cause);
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
            // A tombstone list that is dropped brings deleted files back, which is not the tenant.
            let list: Vec<String> = serde_json::from_slice(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
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
        // Read as null, a tenant.json that does not parse is reported as an unloaded base '' rather
        // than as the unreadable file it is.
        let meta: serde_json::Value = serde_json::from_slice(&meta).map_err(|e| format!("tenant.json does not parse: {e}"))?;
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

    /// `Ok(false)` is a tenant that was not there. A data directory that survives the delete is an
    /// error: it answered 204 and the tenant comes back at the next restore.
    pub fn remove_tenant(&self, id: &str) -> io::Result<bool> {
        let Some(t) = self.tenants.write().unwrap().remove(id) else { return Ok(false) };
        if let Some(dir) = &t.dir
            && let Err(e) = std::fs::remove_dir_all(dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(e);
        }
        Ok(true)
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
