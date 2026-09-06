use std::collections::{BTreeMap, HashMap};
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

pub struct Tenant {
    pub id: String,
    pub base: Arc<Base>,
    overlay: RwLock<BTreeMap<String, Option<FileData>>>,
    version: AtomicU64,
    pub events: broadcast::Sender<String>,
    dir: Option<PathBuf>,
}

pub struct Entry {
    pub path: String,
    pub size: u64,
    pub modified: bool,
}

impl Tenant {
    fn new(id: String, base: Arc<Base>, dir: Option<PathBuf>) -> Tenant {
        let (events, _) = broadcast::channel(64);
        Tenant { id, base, overlay: RwLock::new(BTreeMap::new()), version: AtomicU64::new(now_millis()), events, dir }
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
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

    pub fn write(&self, path: &str, bytes: Vec<u8>) -> io::Result<u64> {
        if let Some(dir) = &self.dir {
            let target = dir.join("files").join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, &bytes)?;
        }
        let data = match &self.dir {
            Some(dir) if bytes.len() as u64 > INLINE_LIMIT => FileData::Disk(dir.join("files").join(path), bytes.len() as u64),
            _ => FileData::Mem(bytes.into()),
        };
        let was_tombstone = {
            let mut overlay = self.overlay.write().unwrap();
            let prev = overlay.insert(path.to_string(), Some(data));
            matches!(prev, Some(None))
        };
        if was_tombstone {
            self.persist_tombstones()?;
        }
        Ok(self.bump("update", path))
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
        if let Some(dir) = &self.dir {
            let target = dir.join("files").join(path);
            if target.exists() {
                std::fs::remove_file(target)?;
            }
        }
        self.persist_tombstones()?;
        Ok(self.bump("delete", path))
    }

    fn persist_tombstones(&self) -> io::Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let overlay = self.overlay.read().unwrap();
        let list: Vec<&str> = overlay.iter().filter(|(_, d)| d.is_none()).map(|(p, _)| p.as_str()).collect();
        std::fs::write(dir.join("deleted.json"), serde_json::to_vec(&list)?)
    }

    fn bump(&self, kind: &str, path: &str) -> u64 {
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
        let _ =
            self.events.send(format!(r#"{{"type":"{kind}","path":{},"version":{v}}}"#, serde_json::to_string(path).unwrap_or_default()));
        v
    }

    pub fn overlay_stats(&self) -> (usize, u64) {
        let overlay = self.overlay.read().unwrap();
        let bytes = overlay.values().filter_map(|d| d.as_ref()).map(|d| d.size()).sum();
        (overlay.len(), bytes)
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

pub struct Store {
    bases: RwLock<HashMap<String, Arc<Base>>>,
    tenants: RwLock<HashMap<String, Arc<Tenant>>>,
    data_dir: Option<PathBuf>,
}

impl Store {
    pub fn new(data_dir: Option<PathBuf>) -> Store {
        Store { bases: RwLock::new(HashMap::new()), tenants: RwLock::new(HashMap::new()), data_dir }
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
        let tenant = Arc::new(Tenant::new(id.to_string(), base, dir));
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
            let tenant = Tenant::new(id.clone(), base, Some(dir.clone()));
            tenant.restore_overlay(&dir)?;
            self.tenants.write().unwrap().insert(id, Arc::new(tenant));
            n += 1;
        }
        Ok(n)
    }
}
