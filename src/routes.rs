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

pub fn build(tenant: &Tenant) -> Vec<Route> {
    let mut routes = Vec::new();
    for e in tenant.list() {
        let Some(rest) = e.path.strip_prefix("src/pages/") else { continue };
        let Some((stem, ext)) = rest.rsplit_once('.') else { continue };
        let kind = match ext {
            "astro" => "astro",
            "md" => "md",
            "mdx" => "mdx",
            "ts" | "js" | "mjs" | "mts" => "endpoint",
            _ => continue,
        };
        let mut segs: Vec<&str> = stem.split('/').collect();
        if segs.iter().any(|s| s.starts_with('_')) {
            continue;
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
        routes.push(Route { route, component: e.path.clone(), kind, params, pattern, key });
    }
    routes.sort_by(|a, b| a.key.cmp(&b.key).then(b.key.len().cmp(&a.key.len())).then(a.route.cmp(&b.route)));
    routes
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
    use super::*;

    #[test]
    fn segment_patterns() {
        assert_eq!(segment_regex("about"), ("about".into(), 0, vec![]));
        assert_eq!(segment_regex("[slug]"), ("([^/]+?)".into(), 1, vec!["slug".into()]));
        assert_eq!(segment_regex("[...rest]"), ("(.*?)".into(), 2, vec!["rest".into()]));
        assert_eq!(segment_regex("v1.2-[id]"), ("v1\\.2-([^/]+?)".into(), 1, vec!["id".into()]));
    }
}
