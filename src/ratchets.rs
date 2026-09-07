//! Issues #93 and #94, made permanent. Three checks that read this crate's own source and its
//! `Cargo.toml`, because all three bugs were invisible to `cargo test`: the test profile has always
//! unwound, and a test calls a handler on a runtime with as many workers as the machine has cores.
//!
//! - `the_release_profile_unwinds` — `panic = "abort"` and `catch_unwind` are only ever correct
//!   apart. With both, three nets and the comments on them are dead code in the shipped binary.
//! - `no_lock_poison_is_fatal` — unwinding means a panicking holder poisons its lock, and
//!   `unwrap()` on the next caller turns one failed request into a permanent outage.
//! - `no_disk_or_process_work_on_an_async_worker` — two threads answer every request, and an
//!   import of 10,000 files holds one of them for 766 ms.
//!
//! What a green run does **not** prove:
//!
//! - **Nothing about a call behind a name.** The third check reads the text of each `async fn` in
//!   `src/http/`. `Store::remove_tenant` is a `remove_dir_all` and `Tenant::read` re-reads a
//!   `FileData::Disk` file, and neither spells `std::fs::` in the handler, so neither is caught.
//!   It stops the next `std::fs::` from being *written* into a handler; it does not certify the
//!   ones already reachable through a helper.
//! - **Nothing about a lock reached through a wrapper.** The second check reads five spellings. A
//!   `lock()` whose `unwrap` is on the next line, or behind a helper of its own, is invisible.
//! - **Nothing about `grass`, `oxc` or the other parsers actually panicking.** The first check
//!   proves the release profile can unwind, not that any particular panic is caught — the tests in
//!   `transform::tests` and `scss::tests` drive the nets, and they run under the test profile.
//! - **Nothing about test code**, `src/ratchets.rs` or `src/silent_failures.rs`, which are skipped,
//!   or about a masked `spawn_blocking` closure whose argument list contains an unbalanced
//!   parenthesis inside a string literal: the mask would run past its end and hide real lines.

use std::path::Path;

use crate::silent_failures::{production_lines, rust_files, src_dir};

fn manifest() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    std::fs::read_to_string(path).expect("Cargo.toml is readable")
}

/// Every `src/**.rs` but the two files that carry these rules as data.
fn sources() -> Vec<(String, String)> {
    let mut files = Vec::new();
    rust_files(&src_dir(), &mut files);
    files.sort();
    assert!(files.len() > 10, "the walk found only {} files, so it is not reading the crate", files.len());
    files
        .iter()
        .filter(|p| !p.ends_with("ratchets.rs") && !p.ends_with("silent_failures.rs"))
        .map(|p| {
            let name = p.strip_prefix(src_dir()).unwrap_or(p).display().to_string();
            (name, std::fs::read_to_string(p).expect("a readable source file"))
        })
        .collect()
}

/// Issue #93's acceptance test. Under `panic = "abort"` the panic runtime aborts before unwinding,
/// so `catch_unwind` never returns `Err` and `resume_unwind` is never reached: the nets in
/// `Engine::deadlined` and `Flights::flight_body` would be code that reads as protection and is
/// not. Either the profile unwinds or the nets go.
#[test]
fn the_release_profile_unwinds() {
    let manifest = manifest();
    let release = manifest.split("[profile.release]").nth(1).expect("a [profile.release] section");
    let release = release.split("\n[").next().unwrap_or(release);
    let aborts = release.lines().any(|l| l.trim_start().starts_with("panic") && l.contains("abort"));

    let nets: Vec<String> = sources()
        .iter()
        .flat_map(|(name, text)| {
            production_lines(text)
                .into_iter()
                .filter(|(_, line)| line.contains("catch_unwind") || line.contains("resume_unwind"))
                .map(|(number, line)| format!("  {name}:{number}: {line}"))
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        !nets.is_empty(),
        "no catch_unwind or resume_unwind is left in src/ — if the nets were deliberately removed, \
         put `panic = \"abort\"` back and delete this check with them"
    );
    assert!(
        !aborts,
        "[profile.release] sets panic = \"abort\", which aborts the process before unwinding, so \
         every one of these is dead code in the shipped binary — remove the setting or remove the nets:\n{}",
        nets.join("\n")
    );
}

/// Spellings that answer a poisoned lock by panicking. Two of the callers are `Drop` impls that run
/// while a compile unwinds (`Permit`, `Lead`), where a second panic is not a 500 but an abort.
const WEDGES: [&str; 4] = [".lock().unwrap()", ".lock().expect(", ".read().unwrap()", ".write().unwrap()"];

/// The two `Condvar` calls, which return a poison error of their own. `child.wait()` spells one of
/// them and is not a lock, so the panicking half has to be on the line as well.
const CONDVAR: [&str; 2] = ["wait_timeout(", ".wait("];

fn wedges_on_poison(line: &str) -> bool {
    WEDGES.iter().any(|w| line.contains(w))
        || (CONDVAR.iter().any(|c| line.contains(c)) && (line.contains(".unwrap()") || line.contains(".expect(")))
}

#[test]
fn no_lock_poison_is_fatal() {
    let found: Vec<String> = sources()
        .iter()
        .flat_map(|(name, text)| {
            production_lines(text)
                .into_iter()
                .filter(|(_, line)| wedges_on_poison(line))
                .map(|(number, line)| format!("  {name}:{number}: {line}"))
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        found.is_empty(),
        "a panic in one tenant's compile poisons whatever lock it was holding, and these hand every \
         later caller an Err — use `held`, `shared`, `exclusive`, `waited` or `waited_for` from \
         src/sync.rs, or say here why this lock is the one that should stop the daemon:\n{}",
        found.join("\n")
    );
}

/// Disk and process work. Every one of these on an async worker is `/health` waiting behind it.
/// The `Chats` methods are named one by one: `cap`, `window` and `forget` are memory and belong
/// wherever they are called.
const OFF_THE_WORKERS: [&str; 8] =
    ["std::fs::", "std::process::", "Command::new(", ".chats.load(", ".chats.save(", ".chats.list(", ".chats.delete(", ".chats.usage("];

/// Blanks out the argument of every `spawn_blocking(...)`, keeping the newlines so line numbers
/// still line up. Whatever a handler hands to the blocking pool is not work it does itself.
fn without_blocking_closures(body: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mark: Vec<char> = "spawn_blocking(".chars().collect();
    let mut out = chars.clone();
    let mut i = 0;
    while i + mark.len() <= chars.len() {
        if chars[i..i + mark.len()] != mark[..] {
            i += 1;
            continue;
        }
        let mut depth = 0usize;
        let mut end = i + mark.len() - 1;
        while end < chars.len() {
            match chars[end] {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            end += 1;
        }
        for c in out.iter_mut().take(end.min(chars.len())).skip(i) {
            if *c != '\n' {
                *c = ' ';
            }
        }
        i = end.max(i + 1);
    }
    out.into_iter().collect()
}

/// Every `async fn` declared at column zero in one file, as (name, first line number, body). Those
/// are the handlers and what they await; the test modules are indented inside `mod tests`, and the
/// scan stops at the first one anyway.
fn async_fns(text: &str) -> Vec<(String, usize, String)> {
    let text = text.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.starts_with("async fn ") && !line.starts_with("pub async fn ") {
            continue;
        }
        let name = line.split("fn ").nth(1).unwrap_or(line).split('(').next().unwrap_or(line).to_string();
        let end = (i + 1..lines.len()).find(|k| lines[*k] == "}").unwrap_or(lines.len().saturating_sub(1));
        out.push((name, i + 1, lines[i..=end].join("\n")));
    }
    out
}

#[test]
fn no_disk_or_process_work_on_an_async_worker() {
    let mut handlers = 0;
    let mut found = Vec::new();
    for (name, text) in sources().iter().filter(|(name, _)| name.starts_with("http/")) {
        for (function, at, body) in async_fns(text) {
            handlers += 1;
            for (offset, line) in without_blocking_closures(&body).lines().enumerate() {
                if OFF_THE_WORKERS.iter().any(|w| line.contains(w)) && !line.trim_start().starts_with("//") {
                    found.push(format!("  {name}:{}: {function}: {}", at + offset, line.trim()));
                }
            }
        }
    }
    assert!(handlers > 20, "only {handlers} async functions were read, so the scan is not finding the handlers");

    assert!(
        found.is_empty(),
        "two tokio workers answer every request on this daemon, and these do disk or process work on \
         one of them — put the call inside `tokio::task::spawn_blocking`, the way `api::write_file` \
         and `preview::module` already do:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_rules_catch_the_bugs_they_were_written_for() {
    // src/http/chats.rs and src/store.rs as #93 found them
    assert!(wedges_on_poison("let mut overlay = self.overlay.write().unwrap();"));
    assert!(wedges_on_poison("self.sizes.write().unwrap().entry(t.id.clone()).or_default().insert(chat.id.clone(), bytes);"));
    assert!(wedges_on_poison("let mut state = self.state.lock().unwrap();"));
    assert!(wedges_on_poison("done = flight.ready.wait_timeout(done, left).unwrap().0;"));
    // and not the two spellings that are not locks at all
    assert!(!wedges_on_poison("let status = match tokio::time::timeout(st.shots.deadline, child.wait()).await {"));
    assert!(!wedges_on_poison("let bytes = tenant.read(path).unwrap();"));

    // src/http/archive.rs and src/http/ai.rs as #94 found them: the call is the handler's own work
    let import = "async fn import(st: State) -> Response {\n    match t.write_many(f, &d, replace) {}\n    std::fs::write(p, b);\n}";
    assert_eq!(async_fns(import).len(), 1);
    assert!(without_blocking_closures(import).contains("std::fs::write"));

    // the same call handed to the blocking pool is not
    let fixed = "async fn import(st: State) -> Response {\n    spawn_blocking(move || std::fs::write(p, b)).await;\n}";
    assert!(!without_blocking_closures(fixed).contains("std::fs::write"));

    // a multi-line closure, and only the closure: the match arms below it are still read
    let block = "async fn write_file() {\n    let w = spawn_blocking(move || {\n        std::fs::write(p, b)\n    })\n    .await;\n    std::process::abort();\n}";
    let masked = without_blocking_closures(block);
    assert!(!masked.contains("std::fs::write"), "{masked}");
    assert!(masked.contains("std::process::abort"), "{masked}");
    assert_eq!(masked.lines().count(), block.lines().count(), "line numbers must survive the mask");
}
