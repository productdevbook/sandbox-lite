//! Issue #48, made permanent: a check that reads this crate's own source for the shape of that bug
//! — a fallible operation whose error is thrown away and replaced with a value — on the code that
//! answers requests.
//!
//! The rule is deliberately narrow. `unwrap_or` over an `Option` from string or JSON navigation
//! (`as_str`, `strip_prefix`, `find`) is not error handling at all, and flagging it would bury the
//! twenty lines that matter under a hundred that do not. So a line is flagged only when it names a
//! source that can genuinely fail *and* a form that discards what it failed with. Test code is out
//! of scope: the scan ends at the file's own `#[cfg(test)] mod`, and skips the item any other
//! `#[cfg(test)]` applies to rather than the rest of the file.
//!
//! `TOLERATED` may only shrink. Each entry carries the reason the default is the right answer
//! there, and adding one is a decision to be argued for in review — not a way to get to green.
//!
//! What a green run does **not** prove:
//!
//! - That no failure is swallowed. It proves that no line matches one of these nine `FALLIBLE`
//!   spellings together with one of these seven `DISCARDS`. A fallible call behind a helper's own
//!   name, or a discard spelled across two lines, is invisible to a rule that reads text a line at
//!   a time — as is any operation nobody has added to `FALLIBLE`.
//! - Anything about code a `#[cfg(test)]` guards, `silent_failures.rs` itself, or a crate this one
//!   depends on. Only `src/**.rs` is read. It reads too much in one direction as well: the modules
//!   `main.rs` compiles only under test carry no `#[cfg(test)]` of their own, so `proptests.rs` and
//!   `shared_fixtures.rs` are scanned as production. A catch there is a confusing message, not a
//!   missed swallow.
//! - That a tolerated line is still right. `TOLERATED` is keyed by text, so the reason is checked
//!   by a reader in review and by nothing else.
//!
//! `DISCARDS` grows when a form gets past it. `Result::into_iter().flatten()` did: it reads as
//! iteration rather than as error handling, and it is how `Chats::count` and `Chats::usage` turned
//! a directory that could not be listed into a tenant holding nothing.

use std::path::{Path, PathBuf};

/// Operations whose failure carries information the caller needs.
const FALLIBLE: [&str; 9] =
    ["serde_json::from_", "serde_yaml::from_", "std::fs::", "spawn_blocking", "Result::ok", ".read(", "read_text(", ".send(", ".text()"];

/// Forms that answer with a value instead of the failure.
const DISCARDS: [&str; 7] =
    ["let _ =", "unwrap_or_default()", "unwrap_or_else(", "unwrap_or(", ".ok()", "is_ok()", "into_iter().flatten()"];

/// Every line the rule catches that is a deliberate fallback, with the reason. Keyed by the line's
/// text so that moving code around does not silently re-tolerate something else.
const TOLERATED: [(&str, &str); 8] = [
    (
        "if path.ends_with(\".astro\") && tenant.read_text(&path).ok().flatten().is_some_and(|t| scss::uses_sass(&t)) {",
        "a census counter in `check`; the same file is compiled two lines later and reports the read failure there",
    ),
    (
        "let stderr = std::fs::read_to_string(&log).unwrap_or_default();",
        "chrome's log, read only to enrich a screenshot failure already being returned; no log is no extra detail",
    ),
    (
        "let _ = tx.send(Ok(Event::default().event(event).data(data.to_string()))).await;",
        "the SSE receiver is gone because the client disconnected, which is not a failure of the reply",
    ),
    (
        "let body = resp.text().await.unwrap_or_default();",
        "the body of an upstream response that already failed; its status is what is reported",
    ),
    (
        "return Err(Fail::new(StatusCode::BAD_GATEWAY, serde_json::from_str(&body).unwrap_or(Value::String(body))));",
        "an upstream error body that is not JSON is passed through verbatim, so the diagnostic survives either way",
    ),
    (
        "let status = std::fs::read_to_string(\"/proc/self/status\").ok()?;",
        "there is no /proc off Linux; `rss_kb` is an Option and /metrics omits the gauge",
    ),
    ("let _ = self.events.send(format!(", "a broadcast with no subscribers, which is the normal state of a tenant nobody is previewing"),
    (
        "self.read().unwrap_or_else(PoisonError::into_inner)",
        "`RwLock::read`, not `Tenant::read` — `.read(` is in FALLIBLE for the second. Taking the data \
         back from a poisoned lock is this daemon's answer to a panicking holder, argued in src/sync.rs",
    ),
];

pub fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

pub fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("src is readable") {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The production lines of one file as (1-based line number, trimmed text). The scan ends at the
/// file's own test module — a `#[cfg(test)]` at column zero over a `mod` that opens a body — and
/// skips only the item any other `#[cfg(test)]` applies to: a test-only `impl`, a `mod x;`
/// declaration, a `static` inside a `thread_local!`, a statement inside a function. Stopping at the
/// attribute itself, as this did until #83, left everything below `transform/mod.rs`'s first
/// indented one unread.
pub fn production_lines(text: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "#[cfg(test)]" {
            out.push((i + 1, lines[i].trim().to_string()));
            i += 1;
            continue;
        }
        let attr = indent(lines[i]);
        let mut item = i + 1;
        while lines.get(item).is_some_and(|line| line.trim_start().starts_with("#[")) {
            item += 1;
        }
        match lines.get(item) {
            None => break,
            Some(line) if attr == 0 && line.trim_start().starts_with("mod ") && !line.trim_end().ends_with(';') => break,
            Some(_) => i = end_of_item(&lines, item, attr) + 1,
        }
    }
    out
}

/// The last line of the item starting at `lines[at]`, under an attribute indented by `attr`. A
/// block ends at the `}` rustfmt puts back at the attribute's own indentation; anything else ends
/// at its `;`. Reading the end wrong resumes the scan inside test code, where a catch is a visible
/// false positive rather than a silent gap.
fn end_of_item(lines: &[&str], at: usize, attr: usize) -> usize {
    let last = lines.len().saturating_sub(1);
    for (i, line) in lines.iter().enumerate().skip(at) {
        if line.matches('{').count() > line.matches('}').count() {
            let closes = |k: &usize| lines[*k].trim_start().starts_with('}') && indent(lines[*k]) <= attr;
            return (i + 1..lines.len()).find(closes).unwrap_or(last);
        }
        if line.trim_end().ends_with(';') {
            return i;
        }
    }
    last
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_swallow(line: &str) -> bool {
    FALLIBLE.iter().any(|f| line.contains(f)) && DISCARDS.iter().any(|d| line.contains(d))
}

#[test]
fn no_new_swallowed_failures() {
    let mut files = Vec::new();
    rust_files(&src_dir(), &mut files);
    files.sort();
    assert!(files.len() > 10, "the walk found only {} files, so it is not reading the crate", files.len());

    let mut scanned = 0;
    let mut found = Vec::new();
    for path in &files {
        if path.ends_with("silent_failures.rs") {
            continue;
        }
        let text = std::fs::read_to_string(path).expect("a readable source file");
        let lines = production_lines(&text);
        scanned += lines.len();
        for (number, line) in lines {
            if is_swallow(&line) {
                found.push((path.strip_prefix(src_dir()).unwrap_or(path).display().to_string(), number, line));
            }
        }
    }
    assert!(scanned > 7000, "only {scanned} lines of production code were read, so the scan is stopping short of the test modules");

    let unexplained: Vec<String> = found
        .iter()
        .filter(|(_, _, line)| !TOLERATED.iter().any(|(tolerated, _)| tolerated == line))
        .map(|(file, number, line)| format!("  {file}:{number}: {line}"))
        .collect();
    assert!(
        unexplained.is_empty(),
        "these discard a failure on a path that answers a request — turn each into an error the caller \
         sees, or add it to TOLERATED in src/silent_failures.rs with the reason the default is right:\n{}",
        unexplained.join("\n")
    );

    // The list may only shrink: an entry nothing matches any more is a fix that was made, and
    // leaving it behind lets the next swallow of that shape in without review.
    let stale: Vec<&str> =
        TOLERATED.iter().map(|(line, _)| *line).filter(|tolerated| !found.iter().any(|(_, _, line)| line == tolerated)).collect();
    assert!(stale.is_empty(), "TOLERATED entries that match nothing any more — delete them:\n  {}", stale.join("\n  "));
}

#[test]
fn a_cfg_test_item_does_not_hide_the_rest_of_the_file() {
    let numbers = |text: &str| production_lines(text).iter().map(|(number, _)| *number).collect::<Vec<_>>();

    // an attribute inside a function skips its statement, not the rest of the file
    let inside = "fn a() {\n    #[cfg(test)]\n    SPAWNED.set(1);\n    let b = 2;\n}\n";
    assert_eq!(numbers(inside), vec![1, 4, 5]);

    // a test-only `impl` in the middle of a file skips its block, and the file goes on
    let block = "fn a() {}\n\n#[cfg(test)]\nimpl Engine {\n    fn for_tests() {}\n}\n\nfn b() {}\n";
    assert_eq!(numbers(block), vec![1, 2, 7, 8]);

    // `#[cfg(test)] mod x;` declares a module, it does not open one: main.rs goes on below it
    let declared = "#[cfg(test)]\nmod proptests;\n\nfn main() {}\n";
    assert_eq!(numbers(declared), vec![3, 4]);

    // and the test module itself ends the scan
    let module = "fn a() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n";
    assert_eq!(numbers(module), vec![1, 2]);
}

#[test]
fn the_rule_catches_the_bug_it_was_written_for() {
    // src/http/preview.rs before this change, verbatim.
    assert!(is_swallow("let out = out.ok().and_then(Result::ok).unwrap_or_else(content::empty_collection);"));
    assert!(is_swallow("let out = tokio::task::spawn_blocking(move || check_tenant(&st2, &t)).await.unwrap_or_else(|e| json!({}));"));
    assert!(is_swallow("Some(\"yaml\" | \"yml\") => serde_yaml::from_str::<Value>(&text).ok(),"));
    // src/http/chats.rs as #58 left it: a chats directory that cannot be listed read as no chats,
    // which stopped the cap being enforced and understated the tenant's usage in /api/stats.
    assert!(is_swallow("Some(dir) => std::fs::read_dir(dir).into_iter().flatten().flatten().filter(is_chat_file).count(),"));
    // and not the Option navigation that makes up most of the crate's `unwrap_or`s
    assert!(!is_swallow("let stem = rest.rsplit_once('.').map(|(s, _)| s).unwrap_or(rest);"));
    assert!(!is_swallow("let base_url = co.get(\"baseUrl\").and_then(|b| b.as_str()).unwrap_or(\".\");"));
}
