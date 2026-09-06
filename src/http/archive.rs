//! Tenant snapshots. `GET /api/t/{id}/export` packs a tenant's files into a tar.gz;
//! `POST /api/tenants/{id}/import` reads one back into an existing tenant's overlay.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State as AxState};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde_json::{Value, json};
use tar::{Archive, Builder, EntryType, Header};

use super::State;
use super::api::{err, tenant_or_404};
use crate::store::{Applied, FileData, Tenant, WriteError, clean_path};

/// Where an overlay-only export records the base files the tenant deleted. An import applies the
/// list as tombstones instead of writing the file, so overlay export → import is a round trip.
const DELETED_ENTRY: &str = ".sandbox-lite/deleted.json";

/// `?name`, `?name=1`, `?name=true` — anything but `0` and an empty value.
fn flag(query: Option<&str>, name: &str) -> bool {
    query.unwrap_or_default().split('&').any(|p| match p.split_once('=') {
        Some((k, v)) => k == name && !v.is_empty() && v != "0",
        None => p == name,
    })
}

fn header_for(size: u64) -> Header {
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_size(size);
    header
}

fn append(builder: &mut Builder<impl Write>, path: &str, data: &FileData) -> io::Result<()> {
    match data {
        FileData::Mem(bytes) => builder.append_data(&mut header_for(bytes.len() as u64), path, &bytes[..]),
        // sized from the open handle and streamed straight in, so a file kept on disk because it
        // is large is never held in memory
        FileData::Disk(disk, _) => {
            let file = std::fs::File::open(disk)?;
            let size = file.metadata()?.len();
            builder.append_data(&mut header_for(size), path, file.take(size))
        }
    }
}

fn pack(t: &Tenant, overlay_only: bool) -> io::Result<Vec<u8>> {
    let mut builder = Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    if overlay_only {
        let mut deleted = Vec::new();
        for (path, data) in t.overlay() {
            match data {
                Some(data) => append(&mut builder, &path, &data)?,
                None => deleted.push(path),
            }
        }
        let list = serde_json::to_vec(&deleted)?;
        builder.append_data(&mut header_for(list.len() as u64), DELETED_ENTRY, &list[..])?;
    } else {
        for entry in t.list() {
            if let Some(data) = t.data(&entry.path) {
                append(&mut builder, &entry.path, &data)?;
            }
        }
    }
    builder.into_inner()?.finish()
}

pub async fn export(AxState(st): AxState<State>, Path(id): Path<String>, RawQuery(query): RawQuery) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let overlay_only = flag(query.as_deref(), "overlay");
    match tokio::task::spawn_blocking(move || pack(&t, overlay_only)).await {
        Ok(Ok(body)) => (
            [
                (header::CONTENT_TYPE, "application/gzip".to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{id}.tar.gz\"")),
                (header::CACHE_CONTROL, "no-store".to_string()),
            ],
            body,
        )
            .into_response(),
        Ok(Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

struct Unpacked {
    files: Vec<(String, Vec<u8>)>,
    deleted: Vec<String>,
}

/// An entry name must already be a tenant-relative path: a leading `/`, a `..` segment, a
/// backslash or a NUL byte is refused rather than normalised away.
pub(crate) fn entry_path(raw: &str) -> Option<String> {
    if raw.starts_with('/') { None } else { clean_path(raw) }
}

fn reject(name: &str, why: &str) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, format!("{name}: {why}"))
}

/// Framing an archive may spend above the tenant's quota: a 512-byte header and up to 511 bytes of
/// padding per file, so about sixteen thousand files. An import that needs more than this is refused
/// rather than read, and the message says so.
const FRAMING_SLACK: u64 = 16 << 20;
/// However well it compresses, an archive may not hand `tar` more than this many times its own size:
/// gzip itself tops out near 1030:1, so this is a ceiling on the amplification, not on the content.
const MAX_EXPANSION: u64 = 1000;
/// The floor under that ratio — an archive this small is not worth bounding more tightly.
const MIN_EXPANSION: u64 = 1 << 20;

/// What `tar` may read out of the decompressor, whatever the entries declare.
fn stream_cap(compressed: u64, quota: u64) -> u64 {
    compressed.saturating_mul(MAX_EXPANSION).max(MIN_EXPANSION).min(quota.saturating_add(FRAMING_SLACK))
}

/// Cuts the decompressor off at `limit` bytes and remembers that it did, so a bomb is told apart
/// from a truncated archive.
struct Capped<R> {
    inner: R,
    read: u64,
    limit: u64,
    tripped: Arc<AtomicBool>,
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        if self.read > self.limit {
            self.tripped.store(true, Ordering::Relaxed);
            return Err(io::Error::other("the archive expands past what the tenant may hold"));
        }
        Ok(n)
    }
}

/// Reads the archive into memory, refusing anything that is not a plain file inside the tenant
/// tree. `quota` bounds the total two ways: every entry costs its declared size whether or not it is
/// taken — `tar` reads those bytes to reach the next header either way — and the decompressor itself
/// is cut off at `stream_cap`, which also bounds what `tar` reads without ever showing it as an
/// entry (a GNU long name, a pax payload, padding). So an archive that decompresses past the quota
/// is refused without being decompressed.
fn unpack(body: &[u8], quota: u64) -> Result<Unpacked, (StatusCode, String)> {
    let cap = stream_cap(body.len() as u64, quota);
    let tripped = Arc::new(AtomicBool::new(false));
    let mut archive = Archive::new(Capped { inner: GzDecoder::new(body), read: 0, limit: cap, tripped: tripped.clone() });
    let unreadable = |e: io::Error| {
        if tripped.load(Ordering::Relaxed) {
            let over = format!(
                "the archive expands past {cap} bytes, the most an import may read for a quota of {quota} bytes (--tenant-quota-mb)"
            );
            (StatusCode::PAYLOAD_TOO_LARGE, over)
        } else {
            (StatusCode::BAD_REQUEST, format!("cannot read the archive: {e}"))
        }
    };
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut total: u64 = 0;
    for entry in archive.entries().map_err(unreadable)? {
        let mut entry = entry.map_err(unreadable)?;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        // before the type match: a directory or pax entry declares a size too, and those bytes are
        // read out of the decompressor to reach the next header even though nothing is kept
        total = total.saturating_add(entry.size());
        if total > quota {
            let over =
                format!("the archive declares at least {total} bytes of entries, the tenant quota is {quota} bytes (--tenant-quota-mb)");
            return Err((StatusCode::PAYLOAD_TOO_LARGE, over));
        }
        match entry.header().entry_type() {
            EntryType::Regular | EntryType::Continuous => {}
            EntryType::Directory | EntryType::XHeader | EntryType::XGlobalHeader => continue,
            EntryType::Symlink | EntryType::Link => return Err(reject(&name, "symbolic and hard links are not imported")),
            other => return Err(reject(&name, &format!("unsupported tar entry (type byte {})", other.as_byte()))),
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut bytes).map_err(unreadable)?;
        if name == DELETED_ENTRY {
            deleted = serde_json::from_slice(&bytes).map_err(|e| reject(DELETED_ENTRY, &format!("not a JSON array of paths: {e}")))?;
            for path in &deleted {
                if entry_path(path).as_deref() != Some(path.as_str()) {
                    return Err(reject(DELETED_ENTRY, &format!("path '{path}' escapes the tenant tree")));
                }
            }
            continue;
        }
        let Some(path) = entry_path(&name) else { return Err(reject(&name, "path escapes the tenant tree")) };
        files.insert(path, bytes);
    }
    Ok(Unpacked { files: files.into_iter().collect(), deleted })
}

/// A refused import applies nothing, and says so where a caller looking for the counts will see it.
/// Only an error `write_many` has undone may answer this way — a `Torn` one has not, and says so.
fn refused(message: String) -> Json<Value> {
    Json(json!({ "error": message, "files": 0, "deleted": 0 }))
}

pub async fn import(AxState(st): AxState<State>, Path(id): Path<String>, RawQuery(query): RawQuery, body: Bytes) -> Response {
    let t = match tenant_or_404(&st, &id) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let replace = flag(query.as_deref(), "replace");
    let quota = t.quota();
    let unpacked = match tokio::task::spawn_blocking(move || unpack(&body, quota)).await {
        Ok(Ok(u)) => u,
        Ok(Err((status, message))) => return (status, refused(message)).into_response(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    match t.write_many(unpacked.files, &unpacked.deleted, replace) {
        Ok(Applied { version, written, deleted }) => {
            Json(json!({ "files": written, "deleted": deleted, "version": version })).into_response()
        }
        Err(e @ WriteError::Quota { .. }) => (StatusCode::PAYLOAD_TOO_LARGE, refused(e.to_string())).into_response(),
        // the batch put itself back, so the tenant is what it was and the counts say it
        Err(e @ WriteError::Io(_)) => (StatusCode::INTERNAL_SERVER_ERROR, refused(e.to_string())).into_response(),
        // it could not, so there are no counts to give: the message names what is neither way
        Err(e @ WriteError::Torn { .. }) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::http::AppState;
    use crate::store::{Base, Store};
    use crate::transform::{Config, Engine};

    struct Fixture {
        app: axum::Router,
        state: State,
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn seed(path: PathBuf, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A base of three files, a data dir, and two tenants on it.
    fn fixture(name: &str, quota: u64) -> Fixture {
        let root = std::env::temp_dir().join(format!("sandbox-lite-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        seed(root.join("base/src/pages/index.astro"), "<h1>base</h1>\n");
        seed(root.join("base/src/data.ts"), "export const n = 1;\n");
        seed(root.join("base/readme.md"), "base readme\n");
        let store = Store::new(Some(root.join("data")), quota);
        store.add_base(Base::load("b", &root.join("base")).unwrap());
        store.create_tenant("a", "b").unwrap();
        store.create_tenant("bb", "b").unwrap();
        let metrics = Arc::new(crate::metrics::Metrics::default());
        let state = Arc::new(AppState {
            store,
            engine: Engine::new(Config { cache_bytes: 1 << 20, ..Config::default() }, metrics.clone()),
            metrics,
            chats: crate::http::chats::Chats::default(),
            domain: "localhost".into(),
            port: 4321,
            model: "m".into(),
            api_key: None,
            api_base: "http://127.0.0.1:1".into(),
            api_token: None,
            preview_secret: None,
            cookie_samesite: crate::http::SameSite::Lax,
            chrome: None,
            shots: crate::http::ai::Shots::default(),
            started: Instant::now(),
        });
        Fixture { app: crate::http::app(state.clone()), state, root }
    }

    async fn call(f: &Fixture, method: &str, uri: &str, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
        let req = Request::builder().method(method).uri(uri).body(Body::from(body)).unwrap();
        let res = f.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, body.to_vec())
    }

    fn listing(f: &Fixture, id: &str) -> Vec<(String, u64, bool)> {
        let t = f.state.store.tenant(id).unwrap();
        t.list().into_iter().map(|e| (e.path, e.size, e.modified)).collect()
    }

    fn bytes_of(f: &Fixture, id: &str) -> Vec<(String, Vec<u8>)> {
        let t = f.state.store.tenant(id).unwrap();
        t.list().into_iter().map(|e| (e.path.clone(), t.read(&e.path).unwrap().unwrap().to_vec())).collect()
    }

    fn tar_gz(entries: &[(&str, EntryType, &[u8])]) -> Vec<u8> {
        let mut builder = Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for (name, kind, body) in entries {
            // written into the raw header so a test can send names tar's own writer would refuse
            let mut header = header_for(body.len() as u64);
            header.set_entry_type(*kind);
            header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            builder.append(&header, *body).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[tokio::test]
    async fn overlay_export_imports_into_an_identical_tenant() {
        let f = fixture("roundtrip", 1 << 20);
        call(&f, "PUT", "/api/t/a/file/src/pages/index.astro", b"<h1>edited</h1>\n".to_vec()).await;
        call(&f, "PUT", "/api/t/a/file/src/new.ts", b"export const added = true;\n".to_vec()).await;
        call(&f, "DELETE", "/api/t/a/file/readme.md", Vec::new()).await;

        let (status, archive) = call(&f, "GET", "/api/t/a/export?overlay=1", Vec::new()).await;
        assert_eq!(status, StatusCode::OK);
        let (status, report) = call(&f, "POST", "/api/tenants/bb/import", archive).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&report));
        let report: Value = serde_json::from_slice(&report).unwrap();
        assert_eq!(report["files"], 2);
        assert_eq!(report["deleted"], 1);

        assert_eq!(listing(&f, "a"), listing(&f, "bb"));
        assert_eq!(bytes_of(&f, "a"), bytes_of(&f, "bb"));
        assert!(!listing(&f, "bb").iter().any(|(p, _, _)| p == "readme.md"), "the tombstone did not cross over");
    }

    #[tokio::test]
    async fn merged_export_carries_the_whole_tree() {
        let f = fixture("merged", 1 << 20);
        call(&f, "PUT", "/api/t/a/file/src/pages/index.astro", b"<h1>edited</h1>\n".to_vec()).await;

        let (status, archive) = call(&f, "GET", "/api/t/a/export", Vec::new()).await;
        assert_eq!(status, StatusCode::OK);
        call(&f, "POST", "/api/tenants/bb/import", archive).await;

        let paths = |id| listing(&f, id).into_iter().map(|(p, n, _)| (p, n)).collect::<Vec<_>>();
        assert_eq!(paths("a"), paths("bb"));
        assert_eq!(bytes_of(&f, "a"), bytes_of(&f, "bb"));
        // the merged tree lands as the tenant's own edits, base file or not
        assert!(listing(&f, "bb").iter().all(|(_, _, modified)| *modified));
    }

    #[tokio::test]
    async fn export_headers_name_the_tenant() {
        let f = fixture("headers", 1 << 20);
        let req = Request::builder().method("GET").uri("/api/t/a/export").body(Body::empty()).unwrap();
        let res = f.app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.headers()[header::CONTENT_TYPE], "application/gzip");
        assert_eq!(res.headers()[header::CONTENT_DISPOSITION], "attachment; filename=\"a.tar.gz\"");
    }

    #[tokio::test]
    async fn replace_drops_the_edits_the_archive_does_not_carry() {
        let f = fixture("replace", 1 << 20);
        call(&f, "PUT", "/api/t/a/file/src/pages/index.astro", b"<h1>edited</h1>\n".to_vec()).await;
        let archive = tar_gz(&[("src/data.ts", EntryType::Regular, b"export const n = 2;\n")]);

        call(&f, "POST", "/api/tenants/a/import", archive.clone()).await;
        assert_eq!(f.state.store.tenant("a").unwrap().overlay_stats().0, 2);

        let (status, report) = call(&f, "POST", "/api/tenants/a/import?replace=1", archive).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(serde_json::from_slice::<Value>(&report).unwrap()["deleted"], 1);
        let t = f.state.store.tenant("a").unwrap();
        assert_eq!(t.overlay_stats().0, 1);
        assert_eq!(t.read_text("src/pages/index.astro").unwrap().as_deref(), Some("<h1>base</h1>\n"));
        assert!(!f.root.join("data/a/files/src/pages/index.astro").exists());
    }

    #[tokio::test]
    async fn paths_that_leave_the_tenant_tree_are_refused() {
        let f = fixture("paths", 1 << 20);
        for name in ["../x", "/etc/passwd", "src/../../x", "a\\b"] {
            let archive = tar_gz(&[(name, EntryType::Regular, b"x")]);
            let (status, body) = call(&f, "POST", "/api/tenants/a/import", archive).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{name}: {}", String::from_utf8_lossy(&body));
            let error = serde_json::from_slice::<Value>(&body).unwrap()["error"].as_str().unwrap().to_string();
            assert!(error.starts_with(&format!("{name}: ")), "the error should name the entry: {error}");
        }
        let deleted = serde_json::to_vec(&["../x"]).unwrap();
        let archive = tar_gz(&[(DELETED_ENTRY, EntryType::Regular, &deleted)]);
        let (status, body) = call(&f, "POST", "/api/tenants/a/import", archive).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&body));
        assert_eq!(f.state.store.tenant("a").unwrap().overlay_stats(), (0, 0));
    }

    #[tokio::test]
    async fn links_and_devices_are_refused_and_directories_skipped() {
        let f = fixture("types", 1 << 20);
        for kind in [EntryType::Symlink, EntryType::Link, EntryType::Fifo, EntryType::Char] {
            let archive = tar_gz(&[("evil", kind, b"")]);
            let (status, body) = call(&f, "POST", "/api/tenants/a/import", archive).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{kind:?}: {}", String::from_utf8_lossy(&body));
        }
        let archive = tar_gz(&[("src", EntryType::Directory, b""), ("src/ok.ts", EntryType::Regular, b"1")]);
        let (status, report) = call(&f, "POST", "/api/tenants/a/import", archive).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&report));
        assert_eq!(serde_json::from_slice::<Value>(&report).unwrap()["files"], 1);
    }

    #[tokio::test]
    async fn an_import_past_the_quota_writes_nothing() {
        let f = fixture("quota", 64);
        // under the quota on its own, over it once the tenant's existing edit is counted
        call(&f, "PUT", "/api/t/a/file/src/data.ts", vec![b'x'; 40]).await;
        let archive = tar_gz(&[("src/big.ts", EntryType::Regular, &[b'y'; 40])]);
        let (status, body) = call(&f, "POST", "/api/tenants/a/import", archive).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["files"], 0);
        assert_eq!(body["deleted"], 0);
        assert!(body["error"].as_str().unwrap().contains("quota"), "{body}");
        assert_eq!(f.state.store.tenant("a").unwrap().overlay_stats(), (1, 40));
        assert!(!f.root.join("data/a/files/src/big.ts").exists());

        // an archive over the quota all by itself is refused before its entries are read
        let archive = tar_gz(&[("src/huge.ts", EntryType::Regular, &[b'z'; 200])]);
        let (status, _) = call(&f, "POST", "/api/tenants/a/import", archive).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(f.state.store.tenant("a").unwrap().overlay_stats(), (1, 40));
        assert!(!f.root.join("data/a/files/src/huge.ts").exists());
    }

    #[tokio::test]
    async fn a_bomb_is_refused_before_it_lands() {
        let f = fixture("bomb", 64);
        // a directory entry declares a size like any other, and tar reads those bytes to reach the
        // next header even though the entry itself is skipped
        let archive = tar_gz(&[("dir", EntryType::Directory, &vec![0u8; 8 << 20][..])]);
        assert!(archive.len() < 64 << 10, "the bomb is small on the wire: {} bytes", archive.len());
        let (status, body) = call(&f, "POST", "/api/tenants/a/import", archive).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{}", String::from_utf8_lossy(&body));
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["files"], 0);
        assert!(body["error"].as_str().unwrap().contains("quota"), "{body}");
        assert_eq!(f.state.store.tenant("a").unwrap().overlay_stats(), (0, 0));

        // tar reads a GNU long name itself and never shows it as an entry, so only the cap on the
        // decompressor bounds it — and it reads the whole thing into memory
        let archive = tar_gz(&[("longname", EntryType::GNULongName, &vec![b'a'; 24 << 20][..])]);
        let (status, body) = call(&f, "POST", "/api/tenants/a/import", archive).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{}", String::from_utf8_lossy(&body));
        assert_eq!(f.state.store.tenant("a").unwrap().overlay_stats(), (0, 0));
    }

    #[test]
    fn the_cap_on_the_decompressor_bounds_both_ways() {
        // a small request cannot buy a large decompression...
        assert_eq!(stream_cap(1 << 10, 64 << 20), MIN_EXPANSION);
        assert_eq!(stream_cap(64 << 10, 64 << 20), (64 << 10) * MAX_EXPANSION);
        // ...and no request buys more than the quota and the framing allowed above it
        assert_eq!(stream_cap(64 << 20, 64 << 20), (64 << 20) + FRAMING_SLACK);
        assert_eq!(stream_cap(u64::MAX, u64::MAX), u64::MAX);
    }

    #[tokio::test]
    async fn a_failed_import_leaves_the_tenant_exactly_as_it_was() {
        for (name, query) in [("atomic", ""), ("atomic-replace", "?replace=1")] {
            let f = fixture(name, 1 << 20);
            call(&f, "PUT", "/api/t/a/file/src/data.ts", b"export const n = 9;\n".to_vec()).await;
            call(&f, "DELETE", "/api/t/a/file/readme.md", Vec::new()).await;
            let before = (listing(&f, "a"), bytes_of(&f, "a"));
            let tombstones = std::fs::read(f.root.join("data/a/deleted.json")).ok();

            // "x" < "x/y", so the file lands first and the directory the second entry needs cannot
            // be created over it: the batch fails on its last entry
            let archive = tar_gz(&[("x", EntryType::Regular, b"i am a file"), ("x/y", EntryType::Regular, b"child")]);
            let (status, body) = call(&f, "POST", &format!("/api/tenants/a/import{query}"), archive).await;
            assert!(status.is_server_error(), "{query}: {status} {}", String::from_utf8_lossy(&body));
            let report: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(report["files"], 0, "{query}: {report}");
            assert_eq!(report["deleted"], 0, "{query}: {report}");

            assert_eq!((listing(&f, "a"), bytes_of(&f, "a")), before, "{query}");
            assert!(!f.root.join("data/a/files/x").exists(), "{query}: the failed import left its file on disk");
            assert!(f.root.join("data/a/files/src/data.ts").exists(), "{query}: the failed import took an edit away");
            assert_eq!(std::fs::read(f.root.join("data/a/deleted.json")).ok(), tombstones, "{query}");
            let left: Vec<String> =
                std::fs::read_dir(f.root.join("data/a")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            assert!(!left.iter().any(|n| n.starts_with(".staged")), "{query}: staging left behind: {left:?}");

            // and the daemon does not change its mind about what it holds when it is restarted
            let store = Store::new(Some(f.root.join("data")), 1 << 20);
            store.add_base(Base::load("b", &f.root.join("base")).unwrap());
            store.restore().unwrap();
            let t = store.tenant("a").unwrap();
            let restored: Vec<(String, u64, bool)> = t.list().into_iter().map(|e| (e.path, e.size, e.modified)).collect();
            assert_eq!(restored, before.0, "{query}: a restart reads a different tenant");
            assert_eq!(t.read_text("x").unwrap(), None, "{query}: a restart reads the file the import was told not to write");
        }
    }

    #[tokio::test]
    async fn an_import_is_one_event() {
        let f = fixture("events", 1 << 20);
        let mut events = f.state.store.tenant("a").unwrap().events.subscribe();
        let archive = tar_gz(&[("one.ts", EntryType::Regular, b"1"), ("two.ts", EntryType::Regular, b"2")]);
        call(&f, "POST", "/api/tenants/a/import", archive).await;
        let event = events.try_recv().unwrap();
        assert!(event.contains(r#""type":"update""#), "{event}");
        assert!(events.try_recv().is_err(), "an import must emit one event, not one per file");
    }

    #[tokio::test]
    async fn unknown_tenants_and_broken_archives() {
        let f = fixture("errors", 1 << 20);
        let (status, _) = call(&f, "GET", "/api/t/nope/export", Vec::new()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(&f, "POST", "/api/tenants/nope/import", b"not a gzip".to_vec()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = call(&f, "POST", "/api/tenants/a/import", b"not a gzip".to_vec()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&body));
    }

    #[test]
    fn flags_and_entry_paths() {
        assert!(flag(Some("overlay=1"), "overlay"));
        assert!(flag(Some("a=1&overlay"), "overlay"));
        assert!(!flag(Some("overlay=0"), "overlay"));
        assert!(!flag(Some("overlay="), "overlay"));
        assert!(!flag(Some("overlays=1"), "overlay"));
        assert!(!flag(None, "overlay"));
        assert_eq!(entry_path("./src/x.ts").as_deref(), Some("src/x.ts"));
        assert_eq!(entry_path("src//x.ts").as_deref(), Some("src/x.ts"));
        assert_eq!(entry_path(".env").as_deref(), Some(".env"));
        assert_eq!(entry_path("/src/x.ts"), None);
        assert_eq!(entry_path("../x"), None);
        assert_eq!(entry_path("src/../../x"), None);
        assert_eq!(entry_path("a\\b"), None);
        assert_eq!(entry_path(""), None);
    }
}
