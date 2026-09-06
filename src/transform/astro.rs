use astro_codegen::{Diagnostic, DiagnosticSeverity, HoistedScriptType, TransformOptions, extract_styles, transform};
use oxc_allocator::Allocator;
use oxc_parser::{ParseOptions, Parser};
use oxc_span::SourceType;

use super::js::SpecRef;
use super::{BuildError, Diag, json_str};

pub const INTERNAL_URL: &str = "/__sl/astro.js";
pub const TRANSITIONS_URL: &str = "/__sl/shim/viewtransitions-css.js";

const PATH_ATTRS: [&str; 2] = ["client:component-path", "server:component-path"];

pub enum Script {
    Inline(String),
    External(String),
}

pub struct AstroOut {
    pub code: String,
    pub css: Vec<String>,
    pub scripts: Vec<Script>,
    pub warnings: Vec<Diag>,
    pub islands: usize,
    pub component_paths: Vec<SpecRef>,
}

pub type Preprocess<'a> = &'a dyn Fn(&str, &str) -> Result<String, String>;

pub fn compile(path: &str, source: &str, site: Option<&str>, preprocess: Preprocess) -> Result<AstroOut, BuildError> {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, source, SourceType::astro()).with_options(ParseOptions::default()).parse_astro();
    if !ret.errors.is_empty() {
        let diags = convert(&Diagnostic::from_oxc_list(source, &ret.errors), path);
        return Err(BuildError::compile(format!("{path}: {}", first_text(&diags)), diags));
    }
    let mut preprocessed = Vec::new();
    let mut any_preprocessed = false;
    for block in extract_styles(&ret.root) {
        let lang = block.attrs.iter().find(|(k, _)| k.as_str() == "lang").map(|(_, v)| v.as_str()).unwrap_or("");
        match lang {
            "scss" | "sass" => match preprocess(lang, &block.content) {
                Ok(css) => {
                    any_preprocessed = true;
                    preprocessed.push(Some(css));
                }
                Err(e) => {
                    let diag =
                        Diag { severity: "error".into(), text: e.clone(), hint: String::new(), file: path.to_string(), line: 0, column: 0 };
                    return Err(BuildError::compile(format!("{path}: {e}"), vec![diag]));
                }
            },
            _ => preprocessed.push(None),
        }
    }
    let filename = format!("/{path}");
    let opts = TransformOptions {
        filename: Some(filename.clone()),
        normalized_filename: Some(filename),
        internal_url: Some(INTERNAL_URL.to_string()),
        result_scoped_slot: true,
        resolve_path_provided: true,
        astro_global_args: Some(site.map(json_str).unwrap_or_else(|| "undefined".to_string())),
        transitions_animation_url: Some(TRANSITIONS_URL.to_string()),
        preprocessed_styles: if any_preprocessed { Some(preprocessed) } else { None },
        ..Default::default()
    };
    let r = transform(&allocator, source, opts, &ret.root);
    let diags = convert(&r.diagnostics, path);
    if diags.iter().any(|d| d.severity == "error") {
        return Err(BuildError::compile(format!("{path}: {}", first_text(&diags)), diags));
    }
    let scripts = r
        .scripts
        .into_iter()
        .map(|s| match s.script_type {
            HoistedScriptType::Inline => Script::Inline(s.code.unwrap_or_default()),
            HoistedScriptType::External => Script::External(s.src.unwrap_or_default()),
        })
        .collect();
    let islands = r.hydrated_components.len() + r.client_only_components.len();
    let mut code = r.code;
    let dir = crate::resolve::dirname(path);
    for c in r.hydrated_components.iter().chain(&r.client_only_components).chain(&r.server_components) {
        if c.specifier.starts_with('.') {
            let resolved = format!("/{}", crate::resolve::join(dir, &c.specifier));
            for attr in PATH_ATTRS {
                code = code.replace(&format!("\"{attr}\": \"{}\"", c.specifier), &format!("\"{attr}\": \"{resolved}\""));
            }
        }
    }
    // A package's URL depends on the tenant's package.json pins, which the cache key does not hash: `serve` resolves these.
    let mut component_paths = Vec::new();
    for c in r.hydrated_components.iter().chain(&r.client_only_components).chain(&r.server_components) {
        if c.specifier.starts_with('.') || c.specifier.starts_with('/') {
            continue;
        }
        for attr in PATH_ATTRS {
            let prefix = format!("\"{attr}\": \"");
            for (at, _) in code.match_indices(&format!("{prefix}{}\"", c.specifier)) {
                let start = at + prefix.len();
                component_paths.push(SpecRef { start, end: start + c.specifier.len(), spec: c.specifier.clone() });
            }
        }
    }
    component_paths.sort_by_key(|s| s.start);
    component_paths.dedup_by_key(|s| s.start);
    Ok(AstroOut { code, css: r.css, scripts, warnings: diags, islands, component_paths })
}

fn first_text(diags: &[Diag]) -> String {
    diags.iter().find(|d| d.severity == "error").or(diags.first()).map(|d| d.text.clone()).unwrap_or_else(|| "compile error".into())
}

fn convert(diags: &[Diagnostic], file: &str) -> Vec<Diag> {
    diags
        .iter()
        .map(|d| {
            let label = d.labels.first();
            Diag {
                severity: match d.severity {
                    DiagnosticSeverity::Error => "error",
                    DiagnosticSeverity::Warning => "warning",
                    DiagnosticSeverity::Information => "info",
                    DiagnosticSeverity::Hint => "hint",
                }
                .to_string(),
                text: d.text.clone(),
                hint: d.hint.clone(),
                file: file.to_string(),
                line: label.map(|l| l.line).unwrap_or(0),
                column: label.map(|l| l.column).unwrap_or(0),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{AstroOut, compile};

    fn compiled(source: &str) -> AstroOut {
        compile("src/pages/index.astro", source, None, &|_, src| Ok(src.to_string())).unwrap()
    }

    #[test]
    fn package_island_paths_are_left_for_serve_time() {
        let out = compiled(
            "---\nimport { Chart } from \"some-widgets\";\nimport Local from \"../components/Local.tsx\";\n---\n<Chart client:load />\n<Local client:load />\n",
        );
        assert!(out.code.contains(r#""client:component-path": "/src/components/Local.tsx""#), "{}", out.code);
        assert!(out.code.contains(r#""client:component-export": "Chart""#), "{}", out.code);
        let found: Vec<(&str, &str)> = out.component_paths.iter().map(|s| (s.spec.as_str(), &out.code[s.start..s.end])).collect();
        assert_eq!(found, vec![("some-widgets", "some-widgets")]);
    }

    #[test]
    fn a_client_only_package_island_is_recorded_once() {
        let out = compiled("---\nimport Widget from \"widgets\";\n---\n<Widget client:only=\"react\" />\n<Widget client:load />\n");
        assert_eq!(out.component_paths.len(), 2, "{:?}", out.component_paths);
        assert!(out.component_paths.iter().all(|s| &out.code[s.start..s.end] == "widgets"), "{}", out.code);
    }
}
