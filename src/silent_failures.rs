//! Issue #48, made permanent: a check that reads this crate's own source for the shape of that bug
//! — a fallible operation whose error is thrown away and replaced with a value — on the code that
//! answers requests.
//!
//! The rule is deliberately narrow. `unwrap_or` over an `Option` from string or JSON navigation
//! (`as_str`, `strip_prefix`, `find`) is not error handling at all, and flagging it would bury the
//! twenty lines that matter under a hundred that do not. So a line is flagged only when it names a
//! source that can genuinely fail *and* a form that discards what it failed with. Test code is out
//! of scope: everything from a file's first `#[cfg(test)]` is skipped.
//!
//! `TOLERATED` may only shrink. Each entry carries the reason the default is the right answer
//! there, and adding one is a decision to be argued for in review — not a way to get to green.
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
        "return Err(Fail { status: StatusCode::BAD_GATEWAY, error: serde_json::from_str(&body).unwrap_or(Value::String(body)) });",
        "an upstream error body that is not JSON is passed through verbatim, so the diagnostic survives either way",
    ),
    (
        "let status = std::fs::read_to_string(\"/proc/self/status\").ok()?;",
        "there is no /proc off Linux; `rss_kb` is an Option and /metrics omits the gauge",
    ),
    (
        "None => self.mem.read().unwrap().get(&t.id).map(|m| m.values().cloned().collect()).unwrap_or_default(),",
        "an Option: a tenant with no conversations yet genuinely has an empty list",
    ),
    ("let _ = self.events.send(format!(", "a broadcast with no subscribers, which is the normal state of a tenant nobody is previewing"),
];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("src is readable") {
        let path = entry.expect("a readable directory entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The lines of `path` before its first `#[cfg(test)]`, as (1-based line number, trimmed text).
fn production_lines(path: &Path) -> Vec<(usize, String)> {
    let text = std::fs::read_to_string(path).expect("a readable source file");
    text.lines()
        .take_while(|line| !line.trim_start().starts_with("#[cfg(test)]"))
        .enumerate()
        .map(|(i, line)| (i + 1, line.trim().to_string()))
        .collect()
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

    let mut found = Vec::new();
    for path in &files {
        if path.ends_with("silent_failures.rs") {
            continue;
        }
        for (number, line) in production_lines(path) {
            if is_swallow(&line) {
                found.push((path.strip_prefix(src_dir()).unwrap_or(path).display().to_string(), number, line));
            }
        }
    }

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
