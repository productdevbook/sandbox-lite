use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;

use crate::metrics::Metrics;
use crate::resolve::package_deps;
use crate::store::{Base, Store};
use crate::transform::{Config, Diag, Engine, Kind, is_source, scss};

pub struct Report {
    pub name: String,
    pub files: usize,
    pub diagnostics: Vec<Diag>,
    pub islands: usize,
    pub globs: usize,
    pub sass: usize,
    pub mdx: usize,
    pub endpoints: usize,
    pub integrations: Vec<String>,
    pub bare_imports: BTreeSet<String>,
}

impl Report {
    pub fn errors(&self) -> usize {
        self.diagnostics.iter().filter(|d| d.severity == "error").count()
    }

    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "project": self.name,
            "files": self.files,
            "errors": self.errors(),
            "diagnostics": self.diagnostics,
            "islands": self.islands,
            "import_meta_glob": self.globs,
            "sass": self.sass,
            "mdx": self.mdx,
            "endpoints": self.endpoints,
            "integrations": self.integrations,
            "bare_imports": self.bare_imports,
        })
    }
}

/// Anything the resolver would send to the CDN: not relative, not a project path, not an `astro:` or `astro/` module.
fn is_npm_import(spec: &str) -> bool {
    let (first, _) = spec.split_once('/').unwrap_or((spec, ""));
    !(spec.starts_with('.')
        || spec.starts_with('/')
        || spec.starts_with("astro:")
        || spec == "astro"
        || spec.starts_with("astro/")
        || spec.starts_with("http:")
        || spec.starts_with("https:")
        || first == "@"
        || spec.starts_with("@/")
        || spec.starts_with("~/"))
}

pub fn inspect(dir: &Path) -> Result<Report, String> {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| dir.display().to_string());
    let base = Base::load("check", dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let store = Store::new(None, u64::MAX);
    store.add_base(base);
    let tenant = store.create_tenant("check", "check")?;
    let engine = Engine::new(Config::default(), Arc::new(Metrics::default()));
    let mut report = Report {
        name,
        files: 0,
        diagnostics: vec![],
        islands: 0,
        globs: 0,
        sass: 0,
        mdx: 0,
        endpoints: 0,
        integrations: package_deps(&tenant)?.into_iter().map(|(n, _)| n).filter(|n| n.starts_with("@astrojs/")).collect(),
        bare_imports: BTreeSet::new(),
    };
    // Counted over the route table rather than re-derived from the path, so the census and the
    // preview cannot disagree about what an endpoint is: `routes::route` also takes `.mjs`/`.mts`
    // and skips any `_`-prefixed segment (issue #76).
    report.endpoints = crate::routes::build(&tenant).iter().filter(|r| r.kind == "endpoint").count();
    for entry in tenant.list() {
        let path = entry.path;
        // Every `.mdx` file, not only the routed ones: a collection entry is MDX the preview has to
        // support too, which is why this one is deliberately wider than the route table.
        if path.ends_with(".mdx") {
            report.mdx += 1;
        }
        if scss::is_sass_path(&path) {
            report.sass += 1;
        }
        if !is_source(&path) {
            continue;
        }
        report.files += 1;
        if path.ends_with(".astro") && tenant.read_text(&path).ok().flatten().is_some_and(|t| scss::uses_sass(&t)) {
            report.sass += 1;
        }
        match engine.build(&tenant, &path, Kind::Module) {
            Ok(built) => {
                report.diagnostics.extend(built.warnings.iter().cloned());
                report.islands += built.islands;
                report.globs += built.globs.len();
                for spec in &built.specs {
                    if is_npm_import(&spec.spec) {
                        report.bare_imports.insert(spec.spec.clone());
                    }
                }
            }
            Err(e) => {
                if e.diagnostics.is_empty() {
                    report.diagnostics.push(Diag {
                        severity: "error".into(),
                        text: e.message,
                        hint: String::new(),
                        file: path.clone(),
                        line: 0,
                        column: 0,
                    });
                } else {
                    report.diagnostics.extend(e.diagnostics);
                }
            }
        }
    }
    Ok(report)
}

pub fn run(dirs: &[std::path::PathBuf], json: bool) -> i32 {
    let mut status = 0;
    let mut reports = Vec::new();
    for dir in dirs {
        match inspect(dir) {
            Ok(r) => {
                if r.errors() > 0 {
                    status = 1;
                }
                reports.push(r);
            }
            Err(e) => {
                eprintln!("{e}");
                status = 2;
            }
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&reports.iter().map(|r| r.to_json()).collect::<Vec<_>>()).unwrap_or_default());
        return status;
    }
    for r in &reports {
        println!("{}: {} source files, {} errors", r.name, r.files, r.errors());
        for d in &r.diagnostics {
            let loc = if d.line > 0 { format!("{}:{}:{}", d.file, d.line, d.column) } else { d.file.clone() };
            println!("  {:<7} {loc} — {}{}", d.severity, d.text, if d.hint.is_empty() { String::new() } else { format!(" ({})", d.hint) });
        }
        println!(
            "  islands {}  import.meta.glob {}  sass {}  mdx {}  endpoints {}  integrations [{}]  npm imports [{}]",
            r.islands,
            r.globs,
            r.sass,
            r.mdx,
            r.endpoints,
            r.integrations.join(", "),
            r.bare_imports.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    status
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{inspect, is_npm_import};
    use crate::store::{Base, Store};

    fn seed(root: &Path, files: &[(&str, &str)]) {
        let _ = std::fs::remove_dir_all(root);
        for (path, body) in files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
    }

    /// Issue #76: the census counted `src/pages/**.ts|.js` while `routes::route` routes `.ts`, `.js`,
    /// `.mjs` and `.mts` and skips any `_`-prefixed segment. A project written in `.mjs` reported
    /// "endpoints 0", and `_helpers.ts` counted as a route that does not exist.
    #[test]
    fn the_endpoint_census_is_the_route_table() {
        let handler = "export function GET() { return new Response(\"ok\"); }\n";
        let root = std::env::temp_dir().join(format!("sandbox-lite-census-{}", std::process::id()));
        seed(
            &root,
            &[
                ("src/pages/index.astro", "<h1>home</h1>\n"),
                ("src/pages/about.md", "# about\n"),
                ("src/pages/note.mdx", "# note\n"),
                ("src/pages/rss.xml.ts", handler),
                ("src/pages/sitemap.js", handler),
                ("src/pages/feed.mjs", handler),
                ("src/pages/atom.mts", handler),
                ("src/pages/api/products.json.ts", handler),
                // not routes: routes.rs skips any path with a `_`-prefixed segment
                ("src/pages/_helpers.ts", "export const n = 1;\n"),
                ("src/pages/_drafts/hidden.mjs", handler),
                // not a route either: outside src/pages
                ("src/lib/data.ts", "export const n = 1;\n"),
                ("src/content/posts/one.mdx", "# one\n"),
            ],
        );

        let report = inspect(&root).unwrap();

        // the same tenant the census walked, so the two answers cannot come from different rules
        let store = Store::new(None, u64::MAX);
        store.add_base(Base::load("check", &root).unwrap());
        let tenant = store.create_tenant("check", "check").unwrap();
        let mut routed: Vec<String> =
            crate::routes::build(&tenant).iter().filter(|r| r.kind == "endpoint").map(|r| r.component.clone()).collect();
        routed.sort();
        assert_eq!(
            routed,
            ["src/pages/api/products.json.ts", "src/pages/atom.mts", "src/pages/feed.mjs", "src/pages/rss.xml.ts", "src/pages/sitemap.js"],
            "the router takes every one of these extensions and skips the `_` ones"
        );
        assert_eq!(report.endpoints, routed.len(), "the census counts exactly what the preview routes");
        assert_eq!(report.endpoints, 5);
        // deliberately wider than the route table: a collection entry is MDX the preview must support
        assert_eq!(report.mdx, 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn npm_import_filter() {
        assert!(is_npm_import("react"));
        assert!(is_npm_import("react/jsx-runtime"));
        assert!(is_npm_import("@scope/pkg"));
        assert!(!is_npm_import("./x"));
        assert!(!is_npm_import("/src/x"));
        assert!(!is_npm_import("@/data/products"));
        assert!(!is_npm_import("astro:content"));
        assert!(!is_npm_import("astro/loaders"));
    }
}
