use std::path::Path;

use oxc_allocator::Allocator;
use oxc_codegen::Codegen;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};

use super::{BuildError, Diag};

/// Byte range of an import specifier's text (quotes excluded).
#[derive(Clone, Debug)]
pub struct SpecRef {
    pub start: usize,
    pub end: usize,
    pub spec: String,
}

pub fn transform(path: &str, source: &str) -> Result<String, BuildError> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let ret = Parser::new(&allocator, source, source_type).parse();
    if !ret.errors.is_empty() {
        return Err(oxc_errors(path, source, &ret.errors));
    }
    let mut program = ret.program;
    let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();
    let options = TransformOptions::default();
    let out = Transformer::new(&allocator, Path::new(path), &options).build_with_scoping(scoping, &mut program);
    if !out.errors.is_empty() {
        return Err(oxc_errors(path, source, &out.errors));
    }
    Ok(Codegen::new().build(&program).code)
}

pub fn scan(code: &str) -> Vec<SpecRef> {
    scan_checked(code).unwrap_or_default()
}

pub fn scan_checked(code: &str) -> Option<Vec<SpecRef>> {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, code, SourceType::mjs()).parse();
    if !ret.errors.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for (name, reqs) in ret.module_record.requested_modules.iter() {
        for r in reqs.iter() {
            push(&mut out, code, r.span.start as usize, r.span.end as usize, Some(name.as_str()));
        }
    }
    for d in ret.module_record.dynamic_imports.iter() {
        push(&mut out, code, d.module_request.start as usize, d.module_request.end as usize, None);
    }
    out.sort_by_key(|s| s.start);
    out.dedup_by_key(|s| s.start);
    Some(out)
}

/// Names a module exports, so generated code does not export them twice.
pub fn exports(code: &str) -> Vec<String> {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, code, SourceType::mjs()).parse();
    ret.module_record.exported_bindings.keys().map(|k| k.to_string()).collect()
}

fn push(out: &mut Vec<SpecRef>, code: &str, start: usize, end: usize, name: Option<&str>) {
    let Some(slice) = code.get(start..end) else { return };
    let quoted =
        slice.len() >= 2 && ((slice.starts_with('"') && slice.ends_with('"')) || (slice.starts_with('\'') && slice.ends_with('\'')));
    let (start, end, inner) = if quoted { (start + 1, end - 1, &slice[1..slice.len() - 1]) } else { (start, end, slice) };
    if inner.contains('\\') || (!quoted && name.is_none()) {
        return;
    }
    let spec = name.map(|n| n.to_string()).unwrap_or_else(|| inner.to_string());
    out.push(SpecRef { start, end, spec });
}

fn oxc_errors(path: &str, source: &str, errors: &[oxc_diagnostics::OxcDiagnostic]) -> BuildError {
    let diagnostics: Vec<Diag> = errors
        .iter()
        .map(|e| {
            let offset = e.labels.as_ref().and_then(|l| l.first()).map(|l| l.offset()).unwrap_or(0);
            let (line, column) = line_col(source, offset);
            Diag {
                severity: "error".into(),
                text: e.message.to_string(),
                hint: e.help.as_ref().map(|h| h.to_string()).unwrap_or_default(),
                file: path.to_string(),
                line,
                column,
            }
        })
        .collect();
    let message = format!("{path}: {}", diagnostics.first().map(|d| d.text.clone()).unwrap_or_default());
    BuildError::compile(message, diagnostics)
}

pub fn line_col(source: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(source.len());
    let before = &source[..offset];
    let line = before.matches('\n').count() as u32 + 1;
    let column = before.rsplit('\n').next().map(|l| l.chars().count()).unwrap_or(0) as u32 + 1;
    (line, column)
}
