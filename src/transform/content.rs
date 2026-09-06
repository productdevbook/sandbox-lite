use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::store::Tenant;

#[derive(Serialize, Clone)]
pub struct Heading {
    pub depth: u8,
    pub slug: String,
    pub text: String,
}

pub struct Md {
    pub frontmatter: Value,
    pub body: String,
    pub html: String,
    pub headings: Vec<Heading>,
}

pub fn split_frontmatter(src: &str) -> (Option<&str>, &str) {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let Some(rest) = src.strip_prefix("---") else { return (None, src) };
    let Some(nl) = rest.find('\n') else { return (None, src) };
    if !rest[..nl].trim().is_empty() {
        return (None, src);
    }
    let after = &rest[nl + 1..];
    let mut offset = 0;
    for line in after.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (Some(&after[..offset]), &after[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, src)
}

pub fn yaml_to_json(yaml: &str) -> Value {
    if yaml.trim().is_empty() {
        return Value::Object(Map::new());
    }
    match serde_yaml::from_str::<Value>(yaml) {
        Ok(v @ Value::Object(_)) => v,
        _ => Value::Object(Map::new()),
    }
}

pub fn parse_markdown(src: &str) -> Md {
    let (fm, body) = split_frontmatter(src);
    let frontmatter = fm.map(yaml_to_json).unwrap_or_else(|| Value::Object(Map::new()));
    let (html, headings) = render(body);
    Md { frontmatter, body: body.to_string(), html, headings }
}

fn render(body: &str) -> (String, Vec<Heading>) {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_FOOTNOTES | Options::ENABLE_TASKLISTS;
    let mut headings = Vec::new();
    let mut current: Option<(u8, String)> = None;
    let events: Vec<Event> = Parser::new_ext(body, options)
        .inspect(|ev| match ev {
            Event::Start(Tag::Heading { level, .. }) => current = Some((level_depth(*level), String::new())),
            Event::Text(t) | Event::Code(t) => {
                if let Some((_, s)) = current.as_mut() {
                    s.push_str(t);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((depth, text)) = current.take() {
                    headings.push(Heading { depth, slug: slugify(&text), text });
                }
            }
            _ => {}
        })
        .collect();
    let mut out = String::with_capacity(body.len() * 2);
    html::push_html(&mut out, events.into_iter());
    (out, headings)
}

fn level_depth(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

pub fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut dash = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

pub fn collection_json(tenant: &Tenant, name: &str) -> Value {
    let prefix = format!("src/content/{name}/");
    let mut entries = Vec::new();
    for e in tenant.list() {
        let Some(rest) = e.path.strip_prefix(&prefix) else { continue };
        let Some((stem, ext)) = rest.rsplit_once('.') else { continue };
        let Some(text) = tenant.read_text(&e.path) else { continue };
        let id = stem.to_ascii_lowercase();
        match ext {
            "md" | "mdx" | "markdown" => {
                let md = parse_markdown(&text);
                entries.push(entry(&id, name, md.frontmatter, Some(&md.body), Some((md.html, md.headings)), &e.path));
            }
            "json" => match serde_json::from_str::<Value>(&text) {
                Ok(Value::Array(items)) => {
                    for (i, item) in items.into_iter().enumerate() {
                        let item_id = item.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(|| i.to_string());
                        entries.push(entry(&item_id, name, item, None, None, &e.path));
                    }
                }
                Ok(v) => entries.push(entry(&id, name, v, None, None, &e.path)),
                Err(_) => {}
            },
            "yaml" | "yml" => entries.push(entry(&id, name, yaml_to_json(&text), None, None, &e.path)),
            _ => {}
        }
    }
    Value::Array(entries)
}

fn entry(id: &str, collection: &str, data: Value, body: Option<&str>, rendered: Option<(String, Vec<Heading>)>, path: &str) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), Value::String(id.to_string()));
    m.insert("slug".into(), Value::String(id.to_string()));
    m.insert("collection".into(), Value::String(collection.to_string()));
    m.insert("data".into(), data);
    m.insert("filePath".into(), Value::String(path.to_string()));
    if let Some(b) = body {
        m.insert("body".into(), Value::String(b.to_string()));
    }
    if let Some((html, headings)) = rendered {
        m.insert("rendered".into(), serde_json::json!({ "html": html, "metadata": { "headings": headings } }));
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_frontmatter() {
        let (fm, body) = split_frontmatter("---\ntitle: Hi\n---\n# Hello\n");
        assert_eq!(fm, Some("title: Hi\n"));
        assert_eq!(body, "# Hello\n");
        let (fm, body) = split_frontmatter("# no fm");
        assert!(fm.is_none());
        assert_eq!(body, "# no fm");
    }

    #[test]
    fn renders_headings() {
        let md = parse_markdown("---\ntitle: T\n---\n## Hello World\ntext");
        assert_eq!(md.frontmatter["title"], "T");
        assert_eq!(md.headings[0].slug, "hello-world");
        assert!(md.html.contains("<h2>Hello World</h2>"));
    }
}
