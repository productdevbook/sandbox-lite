mod check;
mod http;
mod resolve;
mod routes;
mod store;
mod transform;

#[cfg(test)]
mod proptests;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

struct Args {
    listen: String,
    data_dir: Option<PathBuf>,
    bases: Vec<(String, PathBuf)>,
    bases_dir: Option<PathBuf>,
    domain: String,
    cdn: String,
    cache_mb: usize,
    model: String,
    api_token: Option<String>,
    preview_secret: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "sandbox-lite {}\n\nUsage: sandbox-lite [options]\n       sandbox-lite check [--json] DIR...   compile every source file of an Astro project and print diagnostics + a feature census\n\n  --listen ADDR       bind address (default 127.0.0.1:4321)\n  --domain NAME       preview domain; tenants are served at http://<id>.NAME:PORT/ (default localhost)\n  --bases DIR         directory whose sub-directories are base projects (default ./examples if present)\n  --base NAME=PATH    add one base project (repeatable)\n  --data-dir DIR      where tenant edits are persisted (default ./data)\n  --no-persist        keep tenant edits in memory only\n  --cdn URL           where bare npm imports are fetched from in the browser (default https://esm.sh)\n  --cache-mb N        transform cache budget in MiB (default 64)\n  --model NAME        Claude model for the built-in chat (default claude-fable-5-1)\n  --api-token TOKEN   require `Authorization: Bearer TOKEN` (or ?token=) on /api/*\n  --preview-secret S  tenant hosts need a per-tenant token derived from S (the API hands it out)\n\nEnvironment: ANTHROPIC_API_KEY enables the chat endpoint; SANDBOX_LITE_API_TOKEN and SANDBOX_LITE_PREVIEW_SECRET are read as defaults for the two flags.",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut args = Args {
        listen: "127.0.0.1:4321".into(),
        data_dir: Some(PathBuf::from("data")),
        bases: Vec::new(),
        bases_dir: None,
        domain: "localhost".into(),
        cdn: "https://esm.sh".into(),
        cache_mb: 64,
        model: "claude-fable-5-1".into(),
        api_token: std::env::var("SANDBOX_LITE_API_TOKEN").ok().filter(|v| !v.is_empty()),
        preview_secret: std::env::var("SANDBOX_LITE_PREVIEW_SECRET").ok().filter(|v| !v.is_empty()),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--listen" => args.listen = value(),
            "--domain" => args.domain = value().to_ascii_lowercase(),
            "--bases" => args.bases_dir = Some(PathBuf::from(value())),
            "--base" => {
                let v = value();
                let Some((name, path)) = v.split_once('=') else { usage() };
                args.bases.push((name.to_string(), PathBuf::from(path)));
            }
            "--data-dir" => args.data_dir = Some(PathBuf::from(value())),
            "--no-persist" => args.data_dir = None,
            "--cdn" => args.cdn = value(),
            "--cache-mb" => args.cache_mb = value().parse().unwrap_or_else(|_| usage()),
            "--model" => args.model = value(),
            "--api-token" => args.api_token = Some(value()),
            "--preview-secret" => args.preview_secret = Some(value()),
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
    let store = store::Store::new(args.data_dir.clone());
    let mut bases = args.bases.clone();
    if let Some(dir) = &args.bases_dir {
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                for e in entries.flatten() {
                    if e.path().is_dir() {
                        bases.push((e.file_name().to_string_lossy().into_owned(), e.path()));
                    }
                }
            }
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
    let state = Arc::new(http::AppState {
        store,
        engine: transform::Engine::new(transform::Config { cdn: args.cdn.clone(), cache_bytes: args.cache_mb << 20 }),
        domain: args.domain.clone(),
        port,
        model: args.model.clone(),
        api_key,
        api_token: args.api_token.clone(),
        preview_secret: args.preview_secret.clone(),
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
        "sandbox-lite listening on http://{}\n  editor:  http://localhost:{port}/\n  preview: http://<tenant>.{}:{port}/\n  chat:    {}\n  auth:    api token {}, preview token {}",
        args.listen,
        args.domain,
        if state.api_key.is_some() { format!("enabled ({})", state.model) } else { "disabled (set ANTHROPIC_API_KEY)".to_string() },
        if state.api_token.is_some() { "required" } else { "off" },
        if state.preview_secret.is_some() { "required" } else { "off" }
    );
    let app = http::app(state);
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await.unwrap();
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
