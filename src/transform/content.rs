use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use oxc_allocator::Allocator;
use oxc_ast::ast::{BindingPattern, CallExpression, Declaration, Expression, ObjectExpression, ObjectPropertyKind, Statement};
use oxc_parser::Parser as JsParser;
use oxc_span::SourceType;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{BuildError, Diag};
use crate::resolve::{join, normalize};
use crate::store::Tenant;

/// A collection that could not be built answers 500 with this, rather than an empty collection:
/// a page rendered from no posts looks exactly like a page rendered from a tenant that has none.
fn failed(file: &str, text: impl Into<String>, hint: &str) -> BuildError {
    let diag = Diag { severity: "error".into(), text: text.into(), hint: hint.into(), file: file.to_string(), line: 0, column: 0 };
    BuildError { status: 500, message: format!("{file}: {}", diag.text), diagnostics: vec![diag] }
}

fn read(tenant: &Tenant, path: &str) -> Result<Option<String>, BuildError> {
    tenant.read_text(path).map_err(|e| failed(path, e.to_string(), ""))
}

/// The path came out of `Tenant::list`, so absent here means unreadable, not missing.
fn read_listed(tenant: &Tenant, path: &str) -> Result<String, BuildError> {
    read(tenant, path)?.ok_or_else(|| failed(path, "the tenant lists this file but it cannot be read", ""))
}

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

/// Empty frontmatter is no data; frontmatter that does not parse, or that is not a mapping, is a
/// file whose data could not be read — served as `{}` it renders a page with every field missing.
pub fn yaml_to_json(yaml: &str) -> Result<Value, String> {
    if yaml.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    match serde_yaml::from_str::<Value>(yaml) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Err("frontmatter is not a mapping of keys to values".into()),
        Err(e) => Err(format!("frontmatter is not valid YAML: {e}")),
    }
}

pub fn frontmatter(src: &str) -> Result<(Value, &str), String> {
    let (fm, body) = split_frontmatter(src);
    Ok((fm.map(yaml_to_json).transpose()?.unwrap_or_else(|| Value::Object(Map::new())), body))
}

pub fn parse_markdown(src: &str) -> Result<Md, String> {
    let (frontmatter, body) = frontmatter(src)?;
    let (html, headings) = render(body);
    Ok(Md { frontmatter, body: body.to_string(), html, headings })
}

fn render(body: &str) -> (String, Vec<Heading>) {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_FOOTNOTES | Options::ENABLE_TASKLISTS;
    let mut headings = Vec::new();
    let mut slugs = Slugs::default();
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
                    // Issue #63: `.md` used to leave duplicate slugs alone, so `getHeadings()`
                    // disagreed with the same page written as `.mdx`. One rule for both.
                    headings.push(Heading { depth, slug: slugs.unique(slugify(&text)), text });
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

/// The ids a page's headings get: the first heading keeps its slug, and a later heading whose slug
/// is taken becomes `slug-1`, `slug-2`, and so on — what Astro's processor produces.
///
/// Issue #63: the count of `slug-n` tried so far is kept per base slug, so it never restarts at 1.
/// Rescanning the headings already assigned made one heading cost O(i) probes of an O(i) scan and a
/// page of n identical headings O(n^3): 64 KiB of `# a` is about forty minutes of CPU. Advancing
/// the counter instead makes a probe O(1) and bounds the failed ones by the ids actually taken, so
/// the page is linear in its headings.
#[derive(Default)]
pub struct Slugs {
    taken: HashSet<String>,
    next: HashMap<String, u32>,
}

impl Slugs {
    /// An id the author wrote themselves, which no generated id may then collide with.
    pub fn claim(&mut self, slug: &str) {
        self.taken.insert(slug.to_string());
    }

    pub fn unique(&mut self, slug: String) -> String {
        let mut n = self.next.get(&slug).copied().unwrap_or(0);
        let mut candidate = slug.clone();
        while !self.taken.insert(candidate.clone()) {
            n += 1;
            candidate = format!("{slug}-{n}");
        }
        self.next.insert(slug, n);
        candidate
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

pub fn collection_json(tenant: &Tenant, name: &str, max_config_bytes: usize) -> Result<Value, BuildError> {
    let (config_path, mut defs) = config(tenant, max_config_bytes)?;
    let def = defs.remove(name).unwrap_or_default();
    let config_path = config_path.unwrap_or_else(|| CONFIG_PATHS[0].to_string());
    let entries = match &def.loader {
        Some(Loader::Glob { patterns, base }) => glob_entries(tenant, name, patterns, base, &config_path)?,
        Some(Loader::File { path }) => file_entries(tenant, name, path, &config_path)?,
        None => dir_entries(tenant, name)?,
    };
    Ok(json!({ "entries": entries, "dates": def.dates }))
}

fn dir_entries(tenant: &Tenant, name: &str) -> Result<Vec<Value>, BuildError> {
    let prefix = format!("src/content/{name}/");
    let mut entries = Vec::new();
    for e in tenant.list() {
        let Some(id) = e.path.strip_prefix(&prefix).and_then(entry_id) else { continue };
        push_file(tenant, name, &e.path, &id, &mut entries)?;
    }
    Ok(entries)
}

fn glob_entries(tenant: &Tenant, name: &str, patterns: &[String], base: &str, config_path: &str) -> Result<Vec<Value>, BuildError> {
    let files: Vec<String> = tenant.list().into_iter().map(|e| e.path).collect();
    let matched = glob_matches(&files, patterns, base).map_err(|e| failed(config_path, e, "check the collection's glob pattern"))?;
    let mut entries = Vec::new();
    for (path, id) in matched {
        push_file(tenant, name, &path, &id, &mut entries)?;
    }
    Ok(entries)
}

/// The files a glob loader picks up, each with the id Astro derives from its path relative to `base`.
/// A pattern that does not compile is refused: dropped, it silently narrows the collection.
fn glob_matches(files: &[String], patterns: &[String], base: &str) -> Result<Vec<(String, String)>, String> {
    let (mut pos, mut neg) = (GlobSetBuilder::new(), GlobSetBuilder::new());
    for p in patterns {
        let (negated, p) = match p.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, p.as_str()),
        };
        let g = GlobBuilder::new(&join(base, p)).literal_separator(true).build().map_err(|e| format!("pattern '{p}': {e}"))?;
        if negated { &mut neg } else { &mut pos }.add(g);
    }
    let (pos, neg): (GlobSet, GlobSet) = (pos.build().map_err(|e| e.to_string())?, neg.build().map_err(|e| e.to_string())?);
    let prefix = if base.is_empty() { String::new() } else { format!("{base}/") };
    Ok(files
        .iter()
        .filter(|p| pos.is_match(p.as_str()) && !neg.is_match(p.as_str()))
        .filter_map(|p| Some((p.clone(), entry_id(p.strip_prefix(prefix.as_str())?)?)))
        .collect())
}

fn entry_id(relative: &str) -> Option<String> {
    let (stem, _) = relative.rsplit_once('.')?;
    Some(stem.to_ascii_lowercase())
}

fn file_entries(tenant: &Tenant, name: &str, path: &str, config_path: &str) -> Result<Vec<Value>, BuildError> {
    let Some(text) = read(tenant, path)? else {
        return Err(failed(config_path, format!("the file() loader of '{name}' points at '{path}', which this tenant does not have"), ""));
    };
    let parsed = match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("yaml" | "yml") => serde_yaml::from_str::<Value>(&text).map_err(|e| failed(path, format!("invalid YAML: {e}"), ""))?,
        _ => serde_json::from_str::<Value>(&text).map_err(|e| failed(path, format!("invalid JSON: {e}"), ""))?,
    };
    match parsed {
        Value::Array(items) => {
            Ok(items.into_iter().enumerate().map(|(i, item)| entry(&item_id(&item, i), name, item, None, None, path)).collect())
        }
        Value::Object(items) => Ok(items.into_iter().map(|(id, item)| entry(&id, name, item, None, None, path)).collect()),
        _ => Err(failed(path, "a file() loader needs an array of entries or an object keyed by id", "")),
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

fn push_file(tenant: &Tenant, name: &str, path: &str, id: &str, entries: &mut Vec<Value>) -> Result<(), BuildError> {
    // An extension the reader has no format for is not an entry, the way it is not one to Astro.
    let Some((_, ext)) = path.rsplit_once('.') else { return Ok(()) };
    if !matches!(ext, "md" | "markdown" | "mdx" | "json" | "yaml" | "yml") {
        return Ok(());
    }
    let text = read_listed(tenant, path)?;
    match ext {
        "md" | "markdown" => {
            let md = parse_markdown(&text).map_err(|e| failed(path, e, ""))?;
            entries.push(entry(id, name, md.frontmatter, Some(&md.body), Some((md.html, md.headings)), path));
        }
        "mdx" => {
            let (data, body) = frontmatter(&text).map_err(|e| failed(path, e, ""))?;
            entries.push(entry(id, name, data, Some(body.trim()), None, path));
        }
        "json" => match serde_json::from_str::<Value>(&text).map_err(|e| failed(path, format!("invalid JSON: {e}"), ""))? {
            Value::Array(items) => {
                for (i, item) in items.into_iter().enumerate() {
                    entries.push(entry(&item_id(&item, i), name, item, None, None, path));
                }
            }
            v => entries.push(entry(id, name, v, None, None, path)),
        },
        _ => entries.push(entry(id, name, yaml_to_json(&text).map_err(|e| failed(path, e, ""))?, None, None, path)),
    }
    Ok(())
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
/// reaches without going through `Engine::build`, so the size and nesting caps are applied here.
/// No config at all is the directory layout; a config the caps refuse, or one that does not parse,
/// is a collection whose shape is unknown — falling back to the directory layout there answers with
/// somebody else's entries, or none.
pub fn config(tenant: &Tenant, max_bytes: usize) -> Result<(Option<String>, BTreeMap<String, Definition>), BuildError> {
    let mut found = None;
    for p in CONFIG_PATHS {
        if let Some(src) = read(tenant, p)? {
            found = Some((p, src));
            break;
        }
    }
    let Some((path, src)) = found else { return Ok((None, BTreeMap::new())) };
    if src.len() > max_bytes {
        let text = format!("content config is {} KiB, over the {} KiB limit", src.len() / 1024, max_bytes / 1024);
        return Err(failed(path, text, "raise --max-source-kb, or split the file"));
    }
    let depth = super::nesting_depth(src.as_bytes());
    if depth > super::MAX_NESTING_DEPTH {
        let text = format!("content config nests {depth} deep, over the {} level limit", super::MAX_NESTING_DEPTH);
        return Err(failed(path, text, ""));
    }
    let defs = parse_config(&src).map_err(|e| failed(path, e, ""))?;
    Ok((Some(path.to_string()), defs))
}

pub fn parse_config(source: &str) -> Result<BTreeMap<String, Definition>, String> {
    let allocator = Allocator::default();
    let ret = JsParser::new(&allocator, source, SourceType::ts()).parse();
    if let Some(e) = ret.errors.first() {
        return Err(format!("content config does not parse: {}", e.message));
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
    // A `collections` the reader cannot see through — built by a call, awaited — is the documented
    // fallback to the directory layout, not a failure to read the file.
    let Some(Expression::ObjectExpression(collections)) = bindings.get("collections").copied() else { return Ok(BTreeMap::new()) };
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
    Ok(out)
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
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::store::{Base, Store, UpdateKind};

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
        let md = parse_markdown("---\ntitle: T\n---\n## Hello World\ntext").unwrap();
        assert_eq!(md.frontmatter["title"], "T");
        assert_eq!(md.headings[0].slug, "hello-world");
        assert!(md.html.contains("<h2>Hello World</h2>"));
    }

    /// Issue #63: `.md` handed `getHeadings()` the same slug twice where `.mdx` numbered them.
    #[test]
    fn markdown_headings_are_uniquified_like_mdx() {
        let md = parse_markdown("## Second\n\n## Second\n\n## Second\n").unwrap();
        let slugs: Vec<&str> = md.headings.iter().map(|h| h.slug.as_str()).collect();
        assert_eq!(slugs, ["second", "second-1", "second-2"]);
    }

    /// The per-base-slug counter never restarts, so n identical headings cost n probes, not n^2 —
    /// and it still may not hand out an id something else already holds.
    #[test]
    fn slugs_advance_without_rescanning_and_skip_what_is_taken() {
        let mut slugs = Slugs::default();
        slugs.claim("a-1");
        let out: Vec<String> = (0..4).map(|_| slugs.unique("a".to_string())).collect();
        assert_eq!(out, ["a", "a-2", "a-3", "a-4"]);
        assert_eq!(slugs.unique("b".to_string()), "b");
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
        )
        .unwrap();
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
        )
        .unwrap();
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
        )
        .unwrap();
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
        )
        .unwrap();
        assert_eq!(dynamic["posts"], Definition::default());
        assert!(parse_config(r#"export const collections = await loadCollections();"#).unwrap().is_empty());
    }

    /// Issue #48: a config the reader cannot see through falls back to the directory layout on
    /// purpose; one that does not parse is a config nobody read, and the fallback then answers with
    /// the wrong entries — or none — as if that were the tenant's own content.
    #[test]
    fn a_config_that_does_not_parse_is_an_error() {
        let e = parse_config("export const collections = {").unwrap_err();
        assert!(e.contains("does not parse"), "{e}");
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
            glob_matches(&files, &patterns, "src/content/posts").unwrap(),
            vec![
                ("src/content/posts/Hello World.md".to_string(), "hello world".to_string()),
                ("src/content/posts/nested/Deep.md".to_string(), "nested/deep".to_string()),
            ]
        );
    }

    #[test]
    fn an_empty_base_matches_from_the_project_root() {
        let files = ["src/blog/a.md".to_string(), "src/blog/b.mdx".to_string()];
        assert_eq!(
            glob_matches(&files, &["src/blog/*.md".to_string()], "").unwrap(),
            vec![("src/blog/a.md".to_string(), "src/blog/a".to_string())]
        );
    }

    fn tenant(files: &[(&str, &str)]) -> Arc<Tenant> {
        let store = Store::new(None, u64::MAX);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/starter");
        store.add_base(Base::load("starter", &root).unwrap());
        let t = store.create_tenant("t", "starter").unwrap();
        for (path, body) in files {
            t.write(path, body.as_bytes().to_vec(), UpdateKind::from_path(path)).unwrap();
        }
        t
    }

    const MAX: usize = 64 << 10;

    #[test]
    fn a_collection_that_builds_answers_with_its_entries() {
        let out = collection_json(&tenant(&[]), "posts", MAX).unwrap();
        assert!(!out["entries"].as_array().unwrap().is_empty());
    }

    /// Issue #48: each of these used to answer `{"entries": []}`, which renders as a tenant that
    /// simply has no posts.
    #[test]
    fn a_collection_that_cannot_be_built_is_an_error() {
        let broken_frontmatter = tenant(&[("src/content/posts/bad.md", "---\ntitle: \"unterminated\n---\nbody\n")]);
        let e = collection_json(&broken_frontmatter, "posts", MAX).unwrap_err();
        assert_eq!(e.status, 500);
        assert!(e.message.contains("src/content/posts/bad.md") && e.message.contains("YAML"), "{}", e.message);
        assert_eq!(e.diagnostics.len(), 1);

        let bad_config = tenant(&[("src/content.config.ts", "export const collections = {")]);
        let e = collection_json(&bad_config, "posts", MAX).unwrap_err();
        assert!(e.message.contains("does not parse"), "{}", e.message);

        let huge_config = tenant(&[("src/content.config.ts", &"// x\n".repeat(200))]);
        let e = collection_json(&huge_config, "posts", 64).unwrap_err();
        assert!(e.message.contains("over the") && e.message.contains("KiB limit"), "{}", e.message);

        let missing_source = tenant(&[(
            "src/content.config.ts",
            "import { file } from \"astro/loaders\";\nexport const collections = { team: defineCollection({ loader: file(\"src/data/team.json\") }) };\n",
        )]);
        let e = collection_json(&missing_source, "team", MAX).unwrap_err();
        assert!(e.message.contains("src/data/team.json"), "{}", e.message);
    }

    #[test]
    fn an_entry_file_that_is_not_json_is_an_error() {
        let t = tenant(&[("src/content/data/broken.json", "{ nope ")]);
        let e = collection_json(&t, "data", MAX).unwrap_err();
        assert!(e.message.contains("invalid JSON"), "{}", e.message);
    }
}
