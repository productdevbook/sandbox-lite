use std::fmt;
use std::io;
use std::path::Path;

use xxhash_rust::xxh3::xxh3_64;

use crate::resolve::normalize;
use crate::store::Tenant;

struct TenantFs<'a>(&'a Tenant);

impl fmt::Debug for TenantFs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TenantFs")
    }
}

fn norm(path: &Path) -> String {
    normalize(&path.to_string_lossy())
}

impl grass::Fs for TenantFs<'_> {
    fn is_dir(&self, path: &Path) -> bool {
        let prefix = format!("{}/", norm(path));
        self.0.list().iter().any(|e| e.path.starts_with(&prefix))
    }

    fn is_file(&self, path: &Path) -> bool {
        self.0.exists(&norm(path))
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.0.read(&norm(path)).map(|b| b.to_vec()).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, norm(path)))
    }
}

pub fn compile(tenant: &Tenant, dir: &str, source: &str, indented: bool) -> Result<String, String> {
    let fs = TenantFs(tenant);
    let mut options = grass::Options::default().fs(&fs).load_path(if dir.is_empty() { "." } else { dir });
    if indented {
        options = options.input_syntax(grass::InputSyntax::Sass);
    }
    grass::from_string(source.to_string(), &options).map_err(|e| e.to_string())
}

pub fn is_sass_path(path: &str) -> bool {
    path.ends_with(".scss") || path.ends_with(".sass")
}

pub fn uses_sass(source: &str) -> bool {
    source.match_indices("lang=").any(|(i, _)| {
        let value = source[i + 5..].trim_start_matches(['"', '\'']);
        value.starts_with("scss") || value.starts_with("sass")
    })
}

/// Changes whenever any Sass file in the tenant changes, so cached output that `@use`d one is invalidated.
pub fn fingerprint(tenant: &Tenant) -> u64 {
    let mut buf = Vec::new();
    for e in tenant.list() {
        if is_sass_path(&e.path)
            && let Some(bytes) = tenant.read(&e.path)
        {
            buf.extend_from_slice(e.path.as_bytes());
            buf.push(0);
            buf.extend_from_slice(&xxh3_64(&bytes).to_le_bytes());
        }
    }
    xxh3_64(&buf)
}

#[cfg(test)]
mod tests {
    use super::uses_sass;

    #[test]
    fn detects_sass_style_blocks_with_or_without_quotes() {
        assert!(uses_sass("<style lang=\"scss\">"));
        assert!(uses_sass("<style lang='sass'>"));
        assert!(uses_sass("<style lang=scss>"));
        assert!(!uses_sass("<style lang=\"less\">"));
        assert!(!uses_sass("<style>"));
    }
}
