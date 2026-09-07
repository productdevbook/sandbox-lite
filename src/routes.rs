use serde::Serialize;

use crate::store::Tenant;

#[derive(Serialize)]
pub struct Route {
    pub route: String,
    pub component: String,
    pub kind: &'static str,
    pub params: Vec<String>,
    pub pattern: String,
    #[serde(skip)]
    key: Vec<u8>,
}

/// Where Astro looks for the middleware, in Vite's resolution order — which is what decides the
/// answer for a project that has more than one of them.
const MIDDLEWARE_FILES: [&str; 8] = [
    "src/middleware.mjs",
    "src/middleware.js",
    "src/middleware.mts",
    "src/middleware.ts",
    "src/middleware/index.mjs",
    "src/middleware/index.js",
    "src/middleware/index.mts",
    "src/middleware/index.ts",
];

/// The tenant's middleware, whose `onRequest` runs before every page and endpoint. `None` is a
/// project that has none, which is most of them.
pub fn middleware(tenant: &Tenant) -> Option<&'static str> {
    MIDDLEWARE_FILES.into_iter().find(|path| tenant.exists(path))
}

pub fn build(tenant: &Tenant) -> Vec<Route> {
    let mut routes: Vec<Route> = tenant.list().iter().filter_map(|e| route(&e.path)).collect();
    routes.sort_by(|a, b| a.key.cmp(&b.key).then(b.key.len().cmp(&a.key.len())).then(a.route.cmp(&b.route)));
    routes
}

fn route(path: &str) -> Option<Route> {
    let rest = path.strip_prefix("src/pages/")?;
    // Only the last dot is the extension: rss.xml.ts is the route /rss.xml.
    let (stem, ext) = rest.rsplit_once('.')?;
    let kind = match ext {
        "astro" => "astro",
        "md" => "md",
        "mdx" => "mdx",
        "ts" | "js" | "mjs" | "mts" => "endpoint",
        _ => return None,
    };
    let mut segs: Vec<&str> = stem.split('/').collect();
    if segs.iter().any(|s| s.starts_with('_')) {
        return None;
    }
    if segs.last() == Some(&"index") {
        segs.pop();
    }
    let mut params = Vec::new();
    let mut pattern = String::from("^");
    let mut key = Vec::new();
    let mut route = String::new();
    for seg in &segs {
        route.push('/');
        route.push_str(seg);
        let (re, kind_max, ps) = segment_regex(seg);
        if seg.starts_with("[...") && seg.ends_with(']') && ps.len() == 1 && kind_max == 2 {
            pattern.push_str("(?:/(.*?))?");
        } else {
            pattern.push('/');
            pattern.push_str(&re);
        }
        key.push(kind_max);
        params.extend(ps);
    }
    if segs.is_empty() {
        route.push('/');
    }
    pattern.push_str("/?$");
    Some(Route { route, component: path.to_string(), kind, params, pattern, key })
}

fn segment_regex(seg: &str) -> (String, u8, Vec<String>) {
    let mut re = String::new();
    let mut params = Vec::new();
    let mut kind_max = 0u8;
    let mut rest = seg;
    while !rest.is_empty() {
        match rest.find('[') {
            Some(i) => {
                re.push_str(&escape(&rest[..i]));
                let after = &rest[i + 1..];
                let Some(j) = after.find(']') else {
                    re.push_str(&escape(after));
                    break;
                };
                let inner = &after[..j];
                if let Some(name) = inner.strip_prefix("...") {
                    re.push_str("(.*?)");
                    params.push(name.to_string());
                    kind_max = kind_max.max(2);
                } else {
                    re.push_str("([^/]+?)");
                    params.push(inner.to_string());
                    kind_max = kind_max.max(1);
                }
                rest = &after[j + 1..];
            }
            None => {
                re.push_str(&escape(rest));
                break;
            }
        }
    }
    (re, kind_max, params)
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::store::{Base, Store, UpdateKind};

    /// Issue #87: `shell.js` loads exactly this file and hands its `onRequest` to the container, so
    /// which file it is has to be the one Astro would have loaded.
    #[test]
    fn the_middleware_is_the_file_astro_would_load() {
        let store = Store::new(None, u64::MAX);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/starter");
        store.add_base(Base::load("starter", &root).unwrap());
        let t = store.create_tenant("acme", "starter").unwrap();
        assert_eq!(middleware(&t), Some("src/middleware.ts"));

        let body = b"export const onRequest = (context, next) => next();\n".to_vec();
        t.write("src/middleware.js", body, UpdateKind::from_path("src/middleware.js")).unwrap();
        assert_eq!(middleware(&t), Some("src/middleware.js"), "Vite resolves .js before .ts");

        let bare = store.create_tenant("none", "starter").unwrap();
        bare.delete("src/middleware.ts").unwrap();
        assert_eq!(middleware(&bare), None, "a project with no middleware renders without one");
    }

    fn of(path: &str) -> (String, &'static str, String) {
        let r = route(path).unwrap_or_else(|| panic!("{path} is not a route"));
        (r.route, r.kind, r.pattern)
    }

    #[test]
    fn endpoints_keep_the_extension_the_file_name_carries() {
        assert_eq!(of("src/pages/rss.xml.ts"), ("/rss.xml".into(), "endpoint", "^/rss\\.xml/?$".into()));
        assert_eq!(of("src/pages/api/products.json.ts"), ("/api/products.json".into(), "endpoint", "^/api/products\\.json/?$".into()));
        assert_eq!(of("src/pages/sitemap.js").0, "/sitemap");
        assert_eq!(of("src/pages/feed.mjs"), ("/feed".into(), "endpoint", "^/feed/?$".into()));
        assert_eq!(of("src/pages/atom.mts"), ("/atom".into(), "endpoint", "^/atom/?$".into()));
        assert_eq!(of("src/pages/api/index.ts").0, "/api");
        assert_eq!(of("src/pages/about.astro"), ("/about".into(), "astro", "^/about/?$".into()));
        assert!(route("src/pages/_helpers.ts").is_none());
        assert!(route("src/pages/styles.css").is_none());
        assert!(route("src/data/products.ts").is_none());
    }

    #[test]
    fn segment_patterns() {
        assert_eq!(segment_regex("about"), ("about".into(), 0, vec![]));
        assert_eq!(segment_regex("[slug]"), ("([^/]+?)".into(), 1, vec!["slug".into()]));
        assert_eq!(segment_regex("[...rest]"), ("(.*?)".into(), 2, vec!["rest".into()]));
        assert_eq!(segment_regex("v1.2-[id]"), ("v1\\.2-([^/]+?)".into(), 1, vec!["id".into()]));
    }
}
