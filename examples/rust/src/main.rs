//! lode demo (Rust). See ../README.md.
//!
//! Conforms to the lode app contract via the SDK (../../sdks/lode.rs, included
//! below as `mod lode`) and shows the three things an app does under lode:
//!   1. START   — bind $PORT and serve; lode runs this binary as its child.
//!   2. READ    — read lode-injected env via the SDK (active_version / instance_id
//!                / data dir) + passthrough host env (PORT, operator [env]).
//!   3. UPGRADE — (a) PASSIVE: mark_ready() + install_term_handler(), so lode's
//!                update/rollback is seamless; (b) ACTIVE: the endpoints below call
//!                request_update / reboot / hold / release.
//!
//! Standalone (no lode): LODE_DIR is unset, so `Lode::from_env()` is `None`
//! and the request endpoints reply 503 — you still get a working server.

// The single-file SDK, referenced in place (no copy). It needs serde + serde_json
// (see Cargo.toml); an app may use ordinary libraries — lode does not constrain them.
#[path = "../../../sdks/lode.rs"]
mod lode;

use std::env;
use std::process;
use std::time::Duration;

use serde_json::json;

use lode::Lode;

const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

fn version() -> String {
    lode::active_version().unwrap_or_else(|| BUILD_VERSION.to_string())
}

fn log(msg: &str) {
    println!("[demo-rust] {msg}");
}

fn main() {
    // `lode version` passthrough (exec = "./lode-demo-rust").
    if let Some(arg) = env::args().nth(1) {
        if arg == "version" || arg == "--version" || arg == "-v" {
            println!("{}", version());
            return;
        }
    }

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let addr = format!("0.0.0.0:{port}");

    // START: bind.
    let server = match tiny_http::Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[demo-rust] bind {addr}: {e}");
            process::exit(1);
        }
    };

    // The SDK handle — None when run standalone (LODE_DIR unset).
    let lode = Lode::from_env().ok();

    // UPGRADE (passive): graceful stop. The SDK flips a flag on SIGTERM/SIGINT; the
    // accept loop notices within one recv timeout and exits(0) within stop_timeout.
    lode::install_term_handler();

    log(&format!(
        "starting version={} pid={} instance={} data_dir={} addr={addr}",
        version(),
        process::id(),
        lode::instance_id(),
        env::var("LODE_DIR").unwrap_or_else(|_| "unset".into()),
    ));

    // UPGRADE (passive): announce readiness so lode (readiness="state") commits us.
    match &lode {
        Some(l) => match l.mark_ready() {
            Ok(()) => log(&format!("ready: state.ready={}", lode::instance_id())),
            Err(e) => log(&format!("readiness skipped: {e}")),
        },
        None => log("readiness skipped (standalone)"),
    }

    while !lode::terminating() {
        match server.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(req)) => handle(req, lode.as_ref()),
            Ok(None) => {} // timed out — re-check the shutdown flag
            Err(e) => {
                eprintln!("[demo-rust] recv error: {e}");
                break;
            }
        }
    }
    log("SIGTERM/SIGINT received — cleanup done, exiting 0");
    process::exit(0);
}

fn handle(req: tiny_http::Request, lode: Option<&Lode>) {
    let method = req.method().as_str().to_string();
    let path = req.url().split('?').next().unwrap_or("/").to_string();

    // Run an SDK request, or 503 when not supervised by lode.
    let ask = |f: &dyn Fn(&Lode) -> lode::Result<()>, ok: &str| match lode {
        Some(l) => match f(l) {
            Ok(()) => (200, format!("{ok}\n")),
            Err(e) => (503, format!("{e}\n")),
        },
        None => (503, "not running under lode (LODE_DIR unset)\n".to_string()),
    };

    let (code, ctype, body) = match (method.as_str(), path.as_str()) {
        ("GET", "/healthz") => (200, "text/plain; charset=utf-8", "ok\n".to_string()),
        ("GET", "/version") => (200, "text/plain; charset=utf-8", format!("{}\n", version())),
        ("GET", "/env") => (200, "application/json", env_json()), // READ
        ("POST", "/upgrade") => {
            let (c, b) = ask(
                &|l| l.request_update("latest"),
                "requested update to latest",
            );
            (c, "text/plain; charset=utf-8", b)
        }
        ("POST", "/restart") => {
            let (c, b) = ask(&|l| l.reboot().map(|_| ()), "requested restart");
            (c, "text/plain; charset=utf-8", b)
        }
        ("POST", "/hold") => {
            let (c, b) = ask(&|l| l.hold(), "held (lode will not (re)start the app)");
            (c, "text/plain; charset=utf-8", b)
        }
        ("POST", "/release") => {
            let (c, b) = ask(&|l| l.release(), "released");
            (c, "text/plain; charset=utf-8", b)
        }
        _ => (404, "text/plain; charset=utf-8", "not found\n".to_string()),
    };

    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes())
        .expect("static header");
    let resp = tiny_http::Response::from_string(body)
        .with_status_code(code)
        .with_header(header);
    let _ = req.respond(resp);
}

// READ: surface the env lode injected + passthrough host/operator env.
fn env_json() -> String {
    json!({
        "version": version(),                          // LODE_ACTIVE_VERSION or baked
        "instance": lode::instance_id(),               // unique id per launch
        "dataDir": env::var("LODE_DIR").ok(),     // where state.json lives
        "port": env::var("PORT").unwrap_or_else(|_| "8080".into()), // host env passthrough
        "greeting": env::var("APP_GREETING").ok(),     // operator [env] / host -e
    })
    .to_string()
}
