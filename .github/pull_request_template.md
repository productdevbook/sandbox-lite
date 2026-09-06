<!-- What changes and why. A measurement or a scenario beats an adjective. -->

Closes #

## Checklist

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test`
- [ ] `cargo build --release && ./target/release/sandbox-lite check examples/*`
- [ ] If `src/transform/`, `src/resolve.rs`, `assets/shell.js` or a shim changed: opened the affected example in a browser
- [ ] If behaviour changed: `README.md` / `ARCHITECTURE.md` / `SECURITY.md` updated and each sentence checked against the code
- [ ] New routes: said above whether they are on the tenant host (open to preview viewers) or the API host (behind `--api-token`)
- [ ] Comments only say what the code cannot (why, an outside constraint); no restated code, no changelog

## Noticed, did not fix

<!-- Anything you saw on the way that is out of scope for this PR. -->
