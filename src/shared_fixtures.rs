//! Issue #51, made permanent: a check that reads this crate's own source for a test fixture that
//! builds `AppState`, a transform `Config` or an `Engine` field by field. Each such literal is a
//! place a field added to the type has to be edited, and missing one is how `Config::sass_timeout`
//! turned `main` red — two pull requests, each green on its own branch.
//!
//! Test code builds these through `AppState::for_tests` and `Engine::for_tests*`, and a fixture that
//! needs a different value says only that value: `AppState { store, ..AppState::for_tests() }`. So
//! the rule, inside a `#[cfg(test)] mod` and over the whole of a module `main.rs` compiles only
//! under test: a literal of one of these types must hand the rest of its fields to the shared
//! constructor with `..`, and `Engine::new` and `Engine::with_gate` must not be named at all. Both
//! are named once, in the `#[cfg(test)] impl` blocks that sit outside the test modules and are the
//! one place a new argument is edited.
//!
//! Production code is out of scope. `main.rs` names every field of `AppState` and every field of
//! `Config` once, on purpose: that literal is what the flags mean, and the compiler stops there
//! anyway when a field is added.
//!
//! `TOLERATED` may only shrink. Each entry carries the reason spelling the fields out is right
//! there, and adding one is a decision to be argued for in review — not a way to get to green.

use std::path::Path;

use crate::silent_failures::{rust_files, src_dir};

/// Types a fixture used to spell out. A literal of one of these in test code must carry a `..`.
const SHARED: [&str; 2] = ["AppState", "Config"];

/// The engine constructors, whose argument lists a fixture used to repeat.
const ENGINE_CTORS: [&str; 2] = ["Engine::new(", "Engine::with_gate("];

/// Every site the rule catches that is right as it stands, with the reason it is.
const TOLERATED: [(&str, &str); 0] = [];

/// The test code of one file as `(1-based line number, text)`: everything from the `#[cfg(test)]`
/// that opens a test module, or every line when the file is a module compiled only under test.
fn test_lines(text: &str, whole_file: bool) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let opens_test_mod = |i: usize| {
        lines[i].trim() == "#[cfg(test)]"
            && lines.get(i + 1).is_some_and(|next| next.trim_start().starts_with("mod ") && !next.trim_end().ends_with(';'))
    };
    let start = if whole_file {
        0
    } else {
        match (0..lines.len()).find(|i| opens_test_mod(*i)) {
            Some(i) => i,
            None => return Vec::new(),
        }
    };
    lines[start..].iter().enumerate().map(|(i, line)| (start + i + 1, (*line).to_string())).collect()
}

/// `Type {` on `line`, opening a literal: not the tail of a longer name, and not a definition, an
/// `impl` header or a return type.
fn opens_literal(line: &str, ty: &str) -> bool {
    let trimmed = line.trim_start();
    if ["struct ", "pub struct ", "impl ", "fn ", "pub fn "].iter().any(|item| trimmed.starts_with(item)) {
        return false;
    }
    let needle = format!("{ty} {{");
    line.match_indices(&needle).any(|(at, _)| {
        let before = &line[..at];
        !before.chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_') && !before.trim_end().ends_with("->")
    })
}

/// The literal that opens on `lines[i]`: that line alone when its braces close on it, otherwise down
/// to the line that closes it at the opening line's indentation. rustfmt puts the close there, and a
/// brace count would have to know which braces are inside a string.
fn literal(lines: &[(usize, String)], i: usize) -> String {
    let open = &lines[i].1;
    if open.matches('{').count() == open.matches('}').count() {
        return open.clone();
    }
    let indent = open.len() - open.trim_start().len();
    let mut body = open.clone();
    for (_, line) in &lines[i + 1..] {
        body.push('\n');
        body.push_str(line);
        if line.trim_start().starts_with('}') && line.len() - line.trim_start().len() <= indent {
            break;
        }
    }
    body
}

/// Whether a literal hands its remaining fields to a constructor. The `..` has to be the literal's
/// own last element — a nested `Config { cache_bytes, ..Config::default() }` inside a state literal
/// that lists all fifteen of its own does not make that state literal safe, and reading the body for
/// any `..` at all is how this rule first missed every one of them.
fn delegates(body: &str) -> bool {
    match body.lines().count() {
        1 => body.contains(".."),
        _ => body.lines().any(|line| line.trim_start().starts_with("..")),
    }
}

/// Every construction in `lines` that a field added to one of these types would break.
fn field_by_field(lines: &[(usize, String)]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, (number, line)) in lines.iter().enumerate() {
        let calls_ctor = ENGINE_CTORS.iter().any(|ctor| line.contains(ctor));
        let spells_out = SHARED.iter().any(|ty| opens_literal(line, ty)) && !delegates(&literal(lines, i));
        if calls_ctor || spells_out {
            out.push((*number, line.trim().to_string()));
        }
    }
    out
}

/// The modules `main.rs` compiles only under test; every line of those files is test code.
fn test_only_modules() -> Vec<String> {
    let text = std::fs::read_to_string(src_dir().join("main.rs")).expect("main.rs is readable");
    let lines: Vec<&str> = text.lines().collect();
    lines
        .windows(2)
        .filter(|pair| pair[0].trim() == "#[cfg(test)]")
        .filter_map(|pair| pair[1].trim().strip_prefix("mod ").and_then(|name| name.strip_suffix(';')).map(str::to_string))
        .collect()
}

#[test]
fn no_fixture_lists_the_fields_of_a_shared_type() {
    let mut files = Vec::new();
    rust_files(&src_dir(), &mut files);
    files.sort();
    assert!(files.len() > 10, "the walk found only {} files, so it is not reading the crate", files.len());

    let whole_file = test_only_modules();
    assert!(!whole_file.is_empty(), "no `#[cfg(test)] mod x;` in main.rs, so the test-only modules are not being read");

    let mut scanned = 0;
    let mut found = Vec::new();
    for path in &files {
        if path.ends_with("shared_fixtures.rs") {
            continue;
        }
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(path).expect("a readable source file");
        let lines = test_lines(&text, whole_file.contains(&stem));
        scanned += lines.len();
        for (number, line) in field_by_field(&lines) {
            found.push((relative(path), number, line));
        }
    }
    assert!(scanned > 1000, "only {scanned} lines of test code were read, so the scan is not finding the test modules");

    let unexplained: Vec<String> = found
        .iter()
        .filter(|(_, _, line)| !TOLERATED.iter().any(|(tolerated, _)| tolerated == line))
        .map(|(file, number, line)| format!("  {file}:{number}: {line}"))
        .collect();
    assert!(
        unexplained.is_empty(),
        "these fixtures name fields or constructor arguments a later pull request will add to — build them with \
         `AppState::for_tests()` / `Engine::for_tests*()` and say only the values that matter, or add the line to \
         TOLERATED in src/shared_fixtures.rs with the reason:\n{}",
        unexplained.join("\n")
    );

    // The list may only shrink: an entry nothing matches any more is a fixture that was fixed, and
    // leaving it behind lets the next one of that shape in without review.
    let stale: Vec<&str> =
        TOLERATED.iter().map(|(line, _)| *line).filter(|tolerated| !found.iter().any(|(_, _, line)| line == tolerated)).collect();
    assert!(stale.is_empty(), "TOLERATED entries that match nothing any more — delete them:\n  {}", stale.join("\n  "));
}

#[test]
fn the_rule_catches_the_fixtures_it_was_written_for() {
    // src/http/archive.rs before this change, cut short.
    let was = numbered(&[
        "        let metrics = Arc::new(crate::metrics::Metrics::default());",
        "        let state = Arc::new(AppState {",
        "            store,",
        "            engine: Engine::new(Config { cache_bytes: 1 << 20, ..Config::default() }, metrics.clone()),",
        "            chats: crate::http::chats::Chats::default(),",
        "            started: Instant::now(),",
        "        });",
    ]);
    assert_eq!(field_by_field(&was).len(), 2, "the state literal and the `Engine::new` inside it");

    // and what replaced it, plus the two forms that say only what they need
    assert!(field_by_field(&numbered(&["        let state = Arc::new(AppState { store, ..AppState::for_tests() });"])).is_empty());
    assert!(field_by_field(&numbered(&["        Config { cache_bytes: 8 << 20, max_source_bytes, ..Config::default() }"])).is_empty());
    assert_eq!(field_by_field(&numbered(&["        Engine::with_gate(cfg, metrics, gate)"])).len(), 1);

    // a definition, an impl header and a return type are not literals, and neither is a longer name
    assert!(field_by_field(&numbered(&["pub struct AppState {", "    pub store: Store,", "}"])).is_empty());
    assert!(field_by_field(&numbered(&["impl Default for Config {", "    fn default() -> Config {", "    }", "}"])).is_empty());
    assert!(field_by_field(&numbered(&["    fn capped(max: usize) -> Config {", "        cfg", "    }"])).is_empty());
    assert!(field_by_field(&numbered(&["    let c = SassConfig { timeout, threads };"])).is_empty());
}

#[test]
fn only_test_code_is_read() {
    // `#[cfg(test)] mod proptests;` declares a module, it does not open one: what follows it in
    // main.rs is the daemon's own `AppState`, which names every field on purpose.
    let declared = "#[cfg(test)]\nmod proptests;\n\nlet state = Arc::new(http::AppState {\n    store,\n});\n";
    assert!(test_lines(declared, false).is_empty());
    assert_eq!(test_lines(declared, true).len(), 6);

    let module = "fn serve() {}\n\n#[cfg(test)]\nmod tests {\n    fn state() {}\n}\n";
    assert_eq!(test_lines(module, false).first().map(|(number, _)| *number), Some(3));
}

fn relative(path: &Path) -> String {
    path.strip_prefix(src_dir()).unwrap_or(path).display().to_string()
}

fn numbered(lines: &[&str]) -> Vec<(usize, String)> {
    lines.iter().enumerate().map(|(i, line)| (i + 1, (*line).to_string())).collect()
}
