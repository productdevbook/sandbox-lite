use std::borrow::Cow;
use std::collections::BTreeMap;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use oxc_allocator::Allocator;
use oxc_ast::ast::{BindingPattern, CallExpression, Declaration, Expression, ObjectExpression, ObjectPropertyKind, Statement};
use oxc_parser::Parser as JsParser;
use oxc_span::SourceType;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::resolve::{join, normalize};
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

pub fn collection_json(tenant: &Tenant, name: &str, max_config_bytes: usize) -> Value {
    let def = config(tenant, max_config_bytes).remove(name).unwrap_or_default();
    let entries = match &def.loader {
        Some(Loader::Glob { patterns, base }) => glob_entries(tenant, name, patterns, base),
        Some(Loader::File { path }) => file_entries(tenant, name, path),
        None => dir_entries(tenant, name),
    };
    json!({ "entries": entries, "dates": def.dates })
}

pub fn empty_collection() -> Value {
    json!({ "entries": [], "dates": Value::Null })
}

fn dir_entries(tenant: &Tenant, name: &str) -> Vec<Value> {
    let prefix = format!("src/content/{name}/");
    let mut entries = Vec::new();
    for e in tenant.list() {
        let Some(id) = e.path.strip_prefix(&prefix).and_then(entry_id) else { continue };
        push_file(tenant, name, &e.path, &id, &mut entries);
    }
    entries
}

fn glob_entries(tenant: &Tenant, name: &str, patterns: &[String], base: &str) -> Vec<Value> {
    let files: Vec<String> = tenant.list().into_iter().map(|e| e.path).collect();
    let mut entries = Vec::new();
    for (path, id) in glob_matches(&files, patterns, base) {
        push_file(tenant, name, &path, &id, &mut entries);
    }
    entries
}

/// The files a glob loader picks up, each with the id Astro derives from its path relative to `base`.
fn glob_matches(files: &[String], patterns: &[String], base: &str) -> Vec<(String, String)> {
    let (mut pos, mut neg) = (GlobSetBuilder::new(), GlobSetBuilder::new());
    for p in patterns {
        let (negated, p) = match p.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, p.as_str()),
        };
        let Ok(g) = GlobBuilder::new(&join(base, p)).literal_separator(true).build() else { continue };
        if negated { &mut neg } else { &mut pos }.add(g);
    }
    let (Ok(pos), Ok(neg)): (Result<GlobSet, _>, Result<GlobSet, _>) = (pos.build(), neg.build()) else { return Vec::new() };
    let prefix = if base.is_empty() { String::new() } else { format!("{base}/") };
    files
        .iter()
        .filter(|p| pos.is_match(p.as_str()) && !neg.is_match(p.as_str()))
        .filter_map(|p| Some((p.clone(), entry_id(p.strip_prefix(prefix.as_str())?)?)))
        .collect()
}

fn entry_id(relative: &str) -> Option<String> {
    let (stem, _) = relative.rsplit_once('.')?;
    Some(stem.to_ascii_lowercase())
}

fn file_entries(tenant: &Tenant, name: &str, path: &str) -> Vec<Value> {
    let Some(text) = tenant.read_text(path) else { return Vec::new() };
    let parsed = match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("yaml" | "yml") => serde_yaml::from_str::<Value>(&text).ok(),
        _ => serde_json::from_str::<Value>(&text).ok(),
    };
    match parsed {
        Some(Value::Array(items)) => {
            items.into_iter().enumerate().map(|(i, item)| entry(&item_id(&item, i), name, item, None, None, path)).collect()
        }
        Some(Value::Object(items)) => items.into_iter().map(|(id, item)| entry(&id, name, item, None, None, path)).collect(),
        _ => Vec::new(),
    }
}

fn item_id(item: &Value, index: usize) -> String {
    let field = item.get("id").or_else(|| item.get("slug"));
    match field {
        Some(Value::String(s)) => s.clone(),
        Some(v @ Value::Number(_)) => v.to_string(),
        _ => index.to_string(),
    }
}

fn push_file(tenant: &Tenant, name: &str, path: &str, id: &str, entries: &mut Vec<Value>) {
    let Some((_, ext)) = path.rsplit_once('.') else { return };
    let Some(text) = tenant.read_text(path) else { return };
    match ext {
        "md" | "markdown" => {
            let md = parse_markdown(&text);
            entries.push(entry(id, name, md.frontmatter, Some(&md.body), Some((md.html, md.headings)), path));
        }
        "mdx" => {
            let (fm, body) = split_frontmatter(&text);
            let data = fm.map(yaml_to_json).unwrap_or_else(|| Value::Object(Map::new()));
            entries.push(entry(id, name, data, Some(body.trim()), None, path));
        }
        "json" => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Array(items)) => {
                for (i, item) in items.into_iter().enumerate() {
                    entries.push(entry(&item_id(&item, i), name, item, None, None, path));
                }
            }
            Ok(v) => entries.push(entry(id, name, v, None, None, path)),
            Err(_) => {}
        },
        "yaml" | "yml" => entries.push(entry(id, name, yaml_to_json(&text), None, None, path)),
        _ => {}
    }
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

/// Where a collection's entries come from, as far as `content.config.ts` could be read without running it.
#[derive(Debug, Clone, PartialEq)]
pub enum Loader {
    Glob { patterns: Vec<String>, base: String },
    File { path: String },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Definition {
    /// `None` when the loader is missing or dynamic; the caller then reads `src/content/<name>/`.
    pub loader: Option<Loader>,
    /// Fields declared `z.date()` or `z.coerce.date()`, dotted for nested objects. `None` when no schema was understood.
    pub dates: Option<Vec<String>>,
}

const CONFIG_PATHS: [&str; 6] = [
    "src/content.config.ts",
    "src/content.config.js",
    "src/content.config.mjs",
    "src/content/config.ts",
    "src/content/config.js",
    "src/content/config.mjs",
];

/// The config goes to oxc, which recurses over it. This is the one parsed source the content route
/// reaches without going through `Engine::build`, so the size and nesting caps are applied here;
/// past either, it is left unparsed and the collection falls back to the directory layout, as it
/// does when there is no config at all.
pub fn config(tenant: &Tenant, max_bytes: usize) -> BTreeMap<String, Definition> {
    CONFIG_PATHS
        .iter()
        .find_map(|p| tenant.read_text(p))
        .filter(|src| src.len() <= max_bytes && super::nesting_depth(src.as_bytes()) <= super::MAX_NESTING_DEPTH)
        .map(|src| parse_config(&src))
        .unwrap_or_default()
}

pub fn parse_config(source: &str) -> BTreeMap<String, Definition> {
    let allocator = Allocator::default();
    let ret = JsParser::new(&allocator, source, SourceType::ts()).parse();
    if !ret.errors.is_empty() {
        return BTreeMap::new();
    }
    let mut bindings: BTreeMap<&str, &Expression> = BTreeMap::new();
    for stmt in &ret.program.body {
        let decl = match stmt {
            Statement::VariableDeclaration(d) => &**d,
            Statement::ExportNamedDeclaration(e) => match e.declaration.as_ref() {
                Some(Declaration::VariableDeclaration(d)) => &**d,
                _ => continue,
            },
            _ => continue,
        };
        for d in &decl.declarations {
            if let (BindingPattern::BindingIdentifier(id), Some(init)) = (&d.id, d.init.as_ref()) {
                bindings.insert(id.name.as_str(), init);
            }
        }
    }
    let Some(Expression::ObjectExpression(collections)) = bindings.get("collections").copied() else { return BTreeMap::new() };
    let mut out = BTreeMap::new();
    for (name, value) in props(collections) {
        let value = match value {
            Expression::Identifier(id) => match bindings.get(id.name.as_str()) {
                Some(e) => *e,
                None => continue,
            },
            other => other,
        };
        out.insert(name.into_owned(), definition(value));
    }
    out
}

fn definition(expr: &Expression) -> Definition {
    let Some(Expression::ObjectExpression(obj)) = call(expr, "defineCollection").and_then(|c| arg(c, 0)) else {
        return Definition::default();
    };
    let mut def = Definition::default();
    for (key, value) in props(obj) {
        match key.as_ref() {
            "loader" => def.loader = loader(value),
            "schema" => def.dates = date_fields(value),
            _ => {}
        }
    }
    def
}

fn loader(expr: &Expression) -> Option<Loader> {
    if let Some(c) = call(expr, "glob") {
        let Expression::ObjectExpression(obj) = arg(c, 0)? else { return None };
        let (mut patterns, mut base) = (None, String::new());
        for (key, value) in props(obj) {
            match key.as_ref() {
                "pattern" => patterns = Some(strings(value)?),
                "base" => base = normalize(&string(value)?),
                _ => {}
            }
        }
        return Some(Loader::Glob { patterns: patterns?, base });
    }
    let c = call(expr, "file")?;
    Some(Loader::File { path: normalize(&string(arg(c, 0)?)?) })
}

fn date_fields(expr: &Expression) -> Option<Vec<String>> {
    let expr = match expr {
        Expression::ArrowFunctionExpression(f) => f.get_expression()?,
        other => other,
    };
    let mut out = Vec::new();
    collect_dates(zod_object(expr)?, "", &mut out);
    Some(out)
}

fn collect_dates(obj: &ObjectExpression, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in props(obj) {
        let path = if prefix.is_empty() { key.into_owned() } else { format!("{prefix}.{key}") };
        if is_date(value) {
            out.push(path);
        } else if let Some(nested) = zod_object(value) {
            collect_dates(nested, &path, out);
        }
    }
}

/// The object literal of the nearest `.object({…})` call in a chain like `z.object({…}).partial()`.
fn zod_object<'a>(expr: &'a Expression<'a>) -> Option<&'a ObjectExpression<'a>> {
    let mut cur = expr;
    loop {
        let Expression::CallExpression(c) = cur else { return None };
        let Expression::StaticMemberExpression(m) = &c.callee else { return None };
        if m.property.name == "object" {
            return match arg(c, 0) {
                Some(Expression::ObjectExpression(o)) => Some(o),
                _ => None,
            };
        }
        cur = &m.object;
    }
}

fn is_date(expr: &Expression) -> bool {
    let mut chain: Vec<&str> = Vec::new();
    let mut cur = expr;
    loop {
        match cur {
            Expression::CallExpression(c) => cur = &c.callee,
            Expression::StaticMemberExpression(m) => {
                chain.push(m.property.name.as_str());
                cur = &m.object;
            }
            Expression::Identifier(_) => break,
            _ => return false,
        }
    }
    chain.reverse();
    matches!(chain.as_slice(), ["date", ..] | ["coerce", "date", ..])
}

fn props<'a>(obj: &'a ObjectExpression<'a>) -> impl Iterator<Item = (Cow<'a, str>, &'a Expression<'a>)> {
    obj.properties.iter().filter_map(|p| {
        let ObjectPropertyKind::ObjectProperty(p) = p else { return None };
        Some((p.key.static_name()?, &p.value))
    })
}

fn call<'a>(expr: &'a Expression<'a>, name: &str) -> Option<&'a CallExpression<'a>> {
    match expr {
        Expression::CallExpression(c) => match &c.callee {
            Expression::Identifier(id) if id.name == name => Some(c),
            _ => None,
        },
        _ => None,
    }
}

fn arg<'a>(c: &'a CallExpression<'a>, index: usize) -> Option<&'a Expression<'a>> {
    c.arguments.get(index)?.as_expression()
}

fn string(expr: &Expression) -> Option<String> {
    match expr {
        Expression::StringLiteral(s) => Some(s.value.to_string()),
        Expression::TemplateLiteral(t) => t.single_quasi().map(|q| q.to_string()),
        _ => None,
    }
}

fn strings(expr: &Expression) -> Option<Vec<String>> {
    match expr {
        Expression::ArrayExpression(a) => a.elements.iter().map(|e| string(e.as_expression()?)).collect(),
        other => string(other).map(|s| vec![s]),
    }
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

    fn glob(patterns: &[&str], base: &str) -> Loader {
        Loader::Glob { patterns: patterns.iter().map(|p| p.to_string()).collect(), base: base.to_string() }
    }

    #[test]
    fn reads_glob_loader_and_date_fields() {
        let cfg = parse_config(
            r#"import { defineCollection, z } from "astro:content";
import { glob } from "astro/loaders";

const posts = defineCollection({
  loader: glob({ pattern: "**/*.{md,mdx}", base: "./src/content/posts" }),
  schema: z.object({
    title: z.string(),
    date: z.coerce.date(),
    updated: z.date().optional(),
    version: z.string(),
    meta: z.object({ published: z.coerce.date() }),
  }),
});

export const collections = { posts };
"#,
        );
        assert_eq!(cfg["posts"].loader, Some(glob(&["**/*.{md,mdx}"], "src/content/posts")));
        assert_eq!(cfg["posts"].dates.as_deref(), Some(["date", "updated", "meta.published"].map(String::from).as_slice()));
    }

    #[test]
    fn reads_file_loader_and_array_patterns() {
        let cfg = parse_config(
            r#"import { defineCollection } from "astro:content";
import { file, glob } from "astro/loaders";

const team = defineCollection({ loader: file("src/content/team.json") });
const docs = defineCollection({ loader: glob({ base: `./src/docs`, pattern: ["**/*.md", "!**/_*.md"] }) });

export const collections = { team, docs: docs };
"#,
        );
        assert_eq!(cfg["team"].loader, Some(Loader::File { path: "src/content/team.json".into() }));
        assert_eq!(cfg["team"].dates, None);
        assert_eq!(cfg["docs"].loader, Some(glob(&["**/*.md", "!**/_*.md"], "src/docs")));
    }

    #[test]
    fn schema_may_be_a_function_of_the_image_helper() {
        let cfg = parse_config(
            r#"import { defineCollection, z } from "astro:content";
const authors = defineCollection({
  schema: ({ image }) => z.object({ avatar: image(), joined: z.coerce.date() }),
});
export const collections = { authors };
"#,
        );
        assert_eq!(cfg["authors"].loader, None);
        assert_eq!(cfg["authors"].dates.as_deref(), Some(["joined".to_string()].as_slice()));
    }

    #[test]
    fn dynamic_config_falls_back() {
        let dynamic = parse_config(
            r#"import { defineCollection } from "astro:content";
import { glob } from "astro/loaders";
const base = process.env.CONTENT_DIR;
const posts = defineCollection({ loader: glob({ pattern: patternsFor("posts"), base }), schema: buildSchema() });
export const collections = { posts };
"#,
        );
        assert_eq!(dynamic["posts"], Definition::default());

        assert!(parse_config(r#"export const collections = await loadCollections();"#).is_empty());
        assert!(parse_config("export const collections = {").is_empty());
    }

    #[test]
    fn glob_ids_are_relative_to_the_base() {
        let files: Vec<String> = ["src/content/posts/Hello World.md", "src/content/posts/nested/Deep.md", "src/content/posts/_wip.md"]
            .iter()
            .chain(["src/content/posts/notes.txt", "src/content/other/skip.md", "src/pages/index.astro"].iter())
            .map(|p| p.to_string())
            .collect();
        let patterns = ["**/*.{md,mdx}".to_string(), "!**/_*.md".to_string()];
        assert_eq!(
            glob_matches(&files, &patterns, "src/content/posts"),
            vec![
                ("src/content/posts/Hello World.md".to_string(), "hello world".to_string()),
                ("src/content/posts/nested/Deep.md".to_string(), "nested/deep".to_string()),
            ]
        );
    }

    #[test]
    fn an_empty_base_matches_from_the_project_root() {
        let files = ["src/blog/a.md".to_string(), "src/blog/b.mdx".to_string()];
        assert_eq!(glob_matches(&files, &["src/blog/*.md".to_string()], ""), vec![("src/blog/a.md".to_string(), "src/blog/a".to_string())]);
    }
}
