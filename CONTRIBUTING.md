# Contributing

sandbox-lite is one Rust binary plus a handful of browser assets. There is no
Node.js in the build or at runtime: the Astro runtime the browser needs is
committed as `assets/astro.js`, and every file under `assets/` is embedded in
the binary with `include_str!`, so editing an asset means rebuilding.

## Verification happens in CI, not on your machine

Do not run the suite locally to decide whether a change is good. Write it,
format it, commit it, push the branch, and read the run: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test`, a release build,
`sandbox-lite check` over every example, an HTTP smoke test, `bench/mem.sh` and
the Playwright suite all run on every push, to any branch. A red run is the
feedback; fix the cause and push again.

The reasons are practical: a local pass proves nothing about the merged tree —
two branches green on their own broke `main` once already — and a machine that
happens to have a warm cargo cache, a stale binary or another daemon on the
port produces answers the reviewer cannot reproduce.

## Build and run

A current stable toolchain is required: the crate is edition 2024 and uses let
chains. CI runs the latest stable (`dtolnay/rust-toolchain@stable`). The
compiler (`astro_codegen`) and the oxc fork it builds on are git dependencies
pinned by revision in `Cargo.toml`, so the first build needs network access
and takes a while.

```sh
cargo build --release
./target/release/sandbox-lite --no-persist     # bases: ./examples/*, editor at http://localhost:4321/
```

`cargo run -- --no-persist` is the same without the release profile.
Without `--no-persist`, tenant edits are written under `./data` (gitignored).
Open the editor, create a tenant from a base, and the preview is at
`http://<id>.localhost:4321/`. `sandbox-lite --help` lists every flag; `check`
compiles a project without serving it:

```sh
./target/release/sandbox-lite check examples/*
./target/release/sandbox-lite check --json path/to/theme
```

`ARCHITECTURE.md` says what each module does and how a page is rendered; read
it before changing `src/transform/` or `assets/shell.js`.

## The bundled Astro runtime

`assets/astro.js` is Astro's server runtime plus the container API, bundled
for the browser by `scripts/build-runtime.sh` (needs `npm`; `ASTRO_VERSION`
and `ESBUILD_VERSION` select the versions, defaults in the script). The script
also refreshes `assets/viewtransitions.css` and writes the version to
`assets/astro.version`, which `/api/stats` reports as `astro`.

**The compiler and the runtime move together.** The compiler in `Cargo.toml`
(`astro_codegen`, a `rev` of `withastro/compiler-rs`) emits code that imports
helpers from `/__sl/astro.js` (`src/transform/astro.rs` passes that URL as
`internal_url`; `src/transform/markdown.rs` imports `createComponent`,
`render`, `renderComponent`, `unescapeHTML` from it). A compiler from one
Astro version calling into a runtime from another breaks every page. So a bump
is one change:

1. Move the `astro_codegen` `rev` (and the `withastro/oxc` revs it expects) in
   `Cargo.toml`.
2. Run `ASTRO_VERSION=x.y.z scripts/build-runtime.sh`; commit `assets/astro.js`,
   `assets/viewtransitions.css` and `assets/astro.version`.
3. `cargo test`, `sandbox-lite check examples/*`, and open each example in a
   browser — the runtime only runs there.

`assets/shell.js` and `assets/live.js` need no version bump when edited: the
ETag the daemon sends for `/__sl/astro.js`, `/__sl/shell.js` and
`/__sl/live.js` is a hash of the three files (`asset_version` in
`src/http/preview.rs`).

## Measuring memory

```sh
bench/mem.sh [tenants] [port]        # defaults: 200 tenants, port 4399
```

It starts the release binary with a temporary data dir, creates N tenants from
`examples/starter`, writes one page in each, fetches each tenant's shell,
routes, module graph and a content collection, and prints the daemon's RSS
before and after plus the transform-cache statistics from `/api/stats`. It
needs Linux (`/proc`), `curl` and `python3`. CI runs `bench/mem.sh 100 4398`.
Quote its numbers in the pull request, not in code comments.

## Before opening a pull request

CI (`.github/workflows/ci.yml`) runs exactly this; run it locally first:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release && ./target/release/sandbox-lite check examples/*
```

Formatting follows `rustfmt.toml` (140 columns). Clippy warnings are errors.
`check` must exit 0 for every example — it is the compile-level regression
test for the transform pipeline. CI also starts the daemon, creates a tenant
with `curl`, and fetches one module, `routes.json` and the shell.

Beyond what CI can see:

- If you touched `src/transform/`, `src/resolve.rs`, `assets/shell.js` or a
  shim, load the affected example in a browser. CI only `curl`s; rendering
  happens in the browser.
- If behaviour changed, change the document that describes it — `README.md`,
  `ARCHITECTURE.md`, `SECURITY.md` — and verify the sentence against the code,
  not against the issue that asked for the change.
- A new route on the tenant host is open to whoever can open the preview; a
  new route on the API host is behind `--api-token` unless you exempt it. Say
  in the PR which one you added and why.

The pull request template carries this list.

## Comments

Default: none. Let the code say it.

Write a comment only when it says something the code cannot, and keep it to a
line:

- why a choice was made, not what the code does;
- an outside constraint that makes correct code look wrong;
- an outside system's odd behaviour.

Two from this codebase: `src/transform/css.rs` explains why relative `url()`
targets are rewritten ("the CSS ends up in a `<style>` tag whose base URL is
the page, not the file"); `assets/shims/astro-content.js` explains why dates
are revived ("YAML parses bare dates as timestamps; the daemon serialises them
as strings").

Do not write comments that restate the code, comments that describe what the
code used to do, or measurements, run ids and PR numbers — those belong in the
commit message and the pull request. If the comment is longer than the code, or
the code already says it, delete it.

## Commits, issues, labels

- One concern per commit; the message says why. Commit messages so far use a
  `scope: what` first line (`ci: …`, `sandbox-lite: …`).
- Issues use the forms under `.github/ISSUE_TEMPLATE/`. `bug` and `feature`
  are applied by the forms; `security`, `ops` and `docs` by hand.
  `agent-task` marks an issue scoped tightly enough for an autonomous agent to
  implement as a PR; `needs-review` marks a PR waiting for a maintainer.
- Commenting `@claude` on an issue or pull request starts the assistant
  workflow (`.github/workflows/claude.yml`); a weekly audit
  (`.github/workflows/audit.yml`) opens issues for what it can demonstrate.
- Vulnerabilities go through `SECURITY.md`, not the issue tracker.
