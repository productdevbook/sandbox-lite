mod check;
mod http;
mod metrics;
mod resolve;
mod routes;
mod store;
mod transform;

#[cfg(test)]
mod proptests;
#[cfg(test)]
mod silent_failures;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Args {
    listen: String,
    data_dir: Option<PathBuf>,
    bases: Vec<(String, PathBuf)>,
    bases_dir: Option<PathBuf>,
    watch_bases: u64,
    domain: String,
    cdn: String,
    cache_mb: usize,
    max_source_kb: usize,
    sass_timeout_ms: u64,
    compile_timeout_ms: u64,
    max_compiles: usize,
    model: String,
    api_token: Option<String>,
    preview_secret: Option<String>,
    tenant_quota_mb: u64,
    cookie_samesite: Option<http::SameSite>,
    chrome: Option<PathBuf>,
    chrome_jobs: usize,
    chat_window: usize,
    chats_per_tenant: usize,
}

fn usage() -> ! {
    eprintln!(
        "sandbox-lite {}\n\nUsage: sandbox-lite [options]\n       sandbox-lite check [--json] DIR...   compile every source file of an Astro project and print diagnostics + a feature census\n\n  --listen ADDR        bind address (default 127.0.0.1:4321)\n  --domain NAME        preview domain; tenants are served at http://<id>.NAME:PORT/ (default localhost)\n  --bases DIR          directory whose sub-directories are base projects (default ./examples if present)\n  --base NAME=PATH     add one base project (repeatable)\n  --watch-bases N      re-read every base project when its files change, polled every N seconds (default: off)\n  --data-dir DIR       where tenant edits are persisted (default ./data)\n  --no-persist         keep tenant edits in memory only\n  --cdn URL            where bare npm imports are fetched from in the browser (default https://esm.sh)\n  --cache-mb N         transform cache budget in MiB (default 64)\n  --max-source-kb N    largest .astro/.ts/.js/.mdx/.scss file the compilers accept, in KiB (default 64)\n  --sass-timeout-ms N  deadline for one Sass compile, after which the request fails (default 5000)\n  --compile-timeout-ms N\n                       deadline for one compile of any kind; past it the request is answered with a\n                       diagnostic and the compiler thread, which cannot be interrupted, is abandoned\n                       and counted under `compiles` in /api/stats (default 10000)\n  --max-compiles N     compiles that may run at once; one that waits too long for a slot is refused with 503 (default: one per core)\n  --model NAME         Claude model for the built-in chat (default claude-fable-5-1)\n  --api-token TOKEN    require `Authorization: Bearer TOKEN` (or ?token=) on /api/*\n  --preview-secret S   tenant hosts need a per-tenant token derived from S (the API hands it out)\n  --chrome PATH        chrome or chromium binary for the chat's screenshot tool (default: off)\n  --chrome-jobs N      screenshots that may run at once; a call that waits longer than 10s is told the tool is busy (default {})\n  --chat-window N      turns of a conversation replayed to the model in full; older ones are folded into a stored summary (default {})\n  --chats-per-tenant N conversations a tenant may keep; saving past it drops the least recently updated (default {})\n  --tenant-quota-mb N  edited files a tenant may hold, in MiB; a write past it is refused with 413 (default 64)\n  --cookie-samesite lax|none\n                       SameSite of the preview cookie; none also sets Secure. With --preview-secret the\n                       default is none, because a framed preview is cross-site and a browser\n                       drops a Lax cookie there; otherwise lax\n\nEnvironment: ANTHROPIC_API_KEY enables the chat endpoint; SANDBOX_LITE_API_TOKEN, SANDBOX_LITE_PREVIEW_SECRET, SANDBOX_LITE_TENANT_QUOTA_MB, SANDBOX_LITE_MAX_COMPILES, SANDBOX_LITE_COMPILE_TIMEOUT_MS, SANDBOX_LITE_CHROME, SANDBOX_LITE_CHROME_JOBS, SANDBOX_LITE_CHAT_WINDOW and SANDBOX_LITE_CHATS_PER_TENANT are read as defaults for those flags; SANDBOX_LITE_ANTHROPIC_BASE points the chat at another Messages API endpoint (default https://api.anthropic.com).",
        env!("CARGO_PKG_VERSION"),
        http::ai::DEFAULT_CHROME_JOBS,
        http::chats::DEFAULT_WINDOW_TURNS,
        http::chats::DEFAULT_MAX_CHATS
    );
    std::process::exit(2)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().filter(|v| !v.is_empty()).map_or(default, |v| v.parse().unwrap_or_else(|_| usage()))
}

fn parse_args() -> Args {
    let mut args = Args {
        listen: "127.0.0.1:4321".into(),
        data_dir: Some(PathBuf::from("data")),
        bases: Vec::new(),
        bases_dir: None,
        watch_bases: 0,
        domain: "localhost".into(),
        cdn: "https://esm.sh".into(),
        cache_mb: 64,
        max_source_kb: 64,
        sass_timeout_ms: transform::scss::DEFAULT_TIMEOUT_MS,
        compile_timeout_ms: std::env::var("SANDBOX_LITE_COMPILE_TIMEOUT_MS")
            .ok()
            .filter(|v| !v.is_empty())
            .map_or(transform::DEFAULT_COMPILE_TIMEOUT_MS, |v| v.parse().unwrap_or_else(|_| usage())),
        max_compiles: std::env::var("SANDBOX_LITE_MAX_COMPILES")
            .ok()
            .filter(|v| !v.is_empty())
            .map_or_else(transform::default_max_compiles, |v| v.parse().unwrap_or_else(|_| usage())),
        model: "claude-fable-5-1".into(),
        api_token: std::env::var("SANDBOX_LITE_API_TOKEN").ok().filter(|v| !v.is_empty()),
        preview_secret: std::env::var("SANDBOX_LITE_PREVIEW_SECRET").ok().filter(|v| !v.is_empty()),
        tenant_quota_mb: std::env::var("SANDBOX_LITE_TENANT_QUOTA_MB")
            .ok()
            .filter(|v| !v.is_empty())
            .map_or(64, |v| v.parse().unwrap_or_else(|_| usage())),
        cookie_samesite: None,
        chrome: std::env::var("SANDBOX_LITE_CHROME").ok().filter(|v| !v.is_empty()).map(PathBuf::from),
        chrome_jobs: env_usize("SANDBOX_LITE_CHROME_JOBS", http::ai::DEFAULT_CHROME_JOBS),
        chat_window: env_usize("SANDBOX_LITE_CHAT_WINDOW", http::chats::DEFAULT_WINDOW_TURNS),
        chats_per_tenant: env_usize("SANDBOX_LITE_CHATS_PER_TENANT", http::chats::DEFAULT_MAX_CHATS),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--listen" => args.listen = value(),
            "--domain" => args.domain = value().to_ascii_lowercase(),
            "--bases" => args.bases_dir = Some(PathBuf::from(value())),
            "--watch-bases" => args.watch_bases = value().parse().unwrap_or_else(|_| usage()),
            "--base" => {
                let v = value();
                let Some((name, path)) = v.split_once('=') else { usage() };
                args.bases.push((name.to_string(), PathBuf::from(path)));
            }
            "--data-dir" => args.data_dir = Some(PathBuf::from(value())),
            "--no-persist" => args.data_dir = None,
            "--cdn" => args.cdn = value(),
            "--cache-mb" => args.cache_mb = value().parse().unwrap_or_else(|_| usage()),
            "--max-source-kb" => args.max_source_kb = value().parse().unwrap_or_else(|_| usage()),
            "--sass-timeout-ms" => args.sass_timeout_ms = value().parse().unwrap_or_else(|_| usage()),
            "--compile-timeout-ms" => args.compile_timeout_ms = value().parse().unwrap_or_else(|_| usage()),
            "--max-compiles" => args.max_compiles = value().parse().unwrap_or_else(|_| usage()),
            "--model" => args.model = value(),
            "--api-token" => args.api_token = Some(value()),
            "--preview-secret" => args.preview_secret = Some(value()),
            "--tenant-quota-mb" => args.tenant_quota_mb = value().parse().unwrap_or_else(|_| usage()),
            "--cookie-samesite" => args.cookie_samesite = Some(value().parse().unwrap_or_else(|_| usage())),
            "--chrome" => args.chrome = Some(PathBuf::from(value())),
            "--chrome-jobs" => args.chrome_jobs = value().parse().unwrap_or_else(|_| usage()),
            "--chat-window" => args.chat_window = value().parse().unwrap_or_else(|_| usage()),
            "--chats-per-tenant" => args.chats_per_tenant = value().parse().unwrap_or_else(|_| usage()),
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    if args.bases_dir.is_none() && args.bases.is_empty() && PathBuf::from("examples").is_dir() {
        args.bases_dir = Some(PathBuf::from("examples"));
    }
    args
}

fn check_command(argv: &[String]) -> ! {
    let json = argv.iter().any(|a| a == "--json");
    let dirs: Vec<PathBuf> = argv.iter().filter(|a| !a.starts_with("--")).map(PathBuf::from).collect();
    if dirs.is_empty() {
        eprintln!("usage: sandbox-lite check [--json] DIR...");
        std::process::exit(2);
    }
    std::process::exit(check::run(&dirs, json));
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(|a| a.as_str()) == Some("check") {
        check_command(&argv[1..]);
    }
    let args = parse_args();
    let store =
        store::Store::new(args.data_dir.clone(), args.tenant_quota_mb.saturating_mul(1 << 20)).with_bases_dir(args.bases_dir.clone());
    // `--base NAME=PATH` and the `--bases` scan are contained by the one rule `POST /api/bases`
    // applies, so a path outside `--bases` cannot become a base whichever way it arrives.
    let mut bases = Vec::new();
    for (name, path) in &args.bases {
        match store.base_root(path) {
            Ok(root) => bases.push((name.clone(), root)),
            Err(e) => {
                eprintln!("cannot add base {name}: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Some(dir) = &args.bases_dir {
        match store.bases_in_dir() {
            Ok(found) => bases.extend(found),
            Err(e) => {
                eprintln!("cannot read bases dir {}: {e}", dir.display());
                std::process::exit(1);
            }
        }
    }
    for (name, path) in &bases {
        match store::Base::load(name, path) {
            Ok(b) => {
                eprintln!("base {name}: {} files, {} KiB", b.file_count(), b.bytes() / 1024);
                store.add_base(b);
            }
            Err(e) => {
                eprintln!("cannot load base {name} from {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    }
    match store.restore() {
        Ok(n) if n > 0 => eprintln!("restored {n} tenants from {}", args.data_dir.as_ref().unwrap().display()),
        Ok(_) => {}
        Err(e) => {
            eprintln!("cannot restore tenants: {e}");
            std::process::exit(1);
        }
    }
    let port = args.listen.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(80);
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty());
    let metrics = Arc::new(metrics::Metrics::default());
    let state = Arc::new(http::AppState {
        store,
        engine: transform::Engine::new(
            transform::Config {
                cdn: args.cdn.clone(),
                cache_bytes: args.cache_mb << 20,
                max_source_bytes: args.max_source_kb << 10,
                sass_timeout: Duration::from_millis(args.sass_timeout_ms),
                compile_timeout: Duration::from_millis(args.compile_timeout_ms),
                max_compiles: args.max_compiles,
            },
            metrics.clone(),
        ),
        metrics,
        chats: http::chats::Chats::new(args.chats_per_tenant, args.chat_window),
        domain: args.domain.clone(),
        port,
        model: args.model.clone(),
        api_key,
        api_base: std::env::var("SANDBOX_LITE_ANTHROPIC_BASE")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "https://api.anthropic.com".to_string()),
        api_token: args.api_token.clone(),
        preview_secret: args.preview_secret.clone(),
        cookie_samesite: args.cookie_samesite.unwrap_or(http::SameSite::default_for(args.preview_secret.as_deref())),
        chrome: args.chrome.clone(),
        shots: http::ai::Shots::new(args.chrome_jobs),
        started: Instant::now(),
    });
    let listener = match tokio::net::TcpListener::bind(&args.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot bind {}: {e}", args.listen);
            std::process::exit(1);
        }
    };
    eprintln!(
        "sandbox-lite listening on http://{}\n  editor:  http://localhost:{port}/\n  preview: http://<tenant>.{}:{port}/\n  chat:    {}, screenshot tool {}\n  auth:    api token {}, preview token {}",
        args.listen,
        args.domain,
        if state.api_key.is_some() { format!("enabled ({})", state.model) } else { "disabled (set ANTHROPIC_API_KEY)".to_string() },
        match &state.chrome {
            Some(path) => format!("via {} ({} at a time)", path.display(), args.chrome_jobs),
            None => "off (set --chrome)".to_string(),
        },
        if state.api_token.is_some() { "required" } else { "off" },
        if state.preview_secret.is_some() { "required" } else { "off" }
    );
    if args.watch_bases > 0 {
        eprintln!("  watch:   base projects re-read when they change, polled every {}s", args.watch_bases);
        spawn_base_watcher(state.clone(), Duration::from_secs(args.watch_bases));
    }
    let app = http::app(state);
    http::serve(listener, app, shutdown_signal()).await;
}

/// Polls every base root on an interval and reloads the ones whose files changed. One walk per
/// base per tick, on the blocking pool: the walk stats every file the base holds.
fn spawn_base_watcher(state: Arc<http::AppState>, period: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let state = state.clone();
            match tokio::task::spawn_blocking(move || state.store.reload_changed_bases()).await {
                Ok(names) => {
                    for name in names {
                        eprintln!("watch: reloaded base {name}");
                    }
                }
                Err(e) => eprintln!("watch: {e}"),
            }
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
