//! web-rust — a minimal, dependency-free HTTP service used by lode's integration
//! tests. It implements the SAME language-agnostic lode app contract as
//! the sibling `tests/apps/web-bun/app.ts`:
//!
//!   * binds an HTTP server to `$PORT` (default 8080)
//!   * `GET /version`  -> the app's own version (plain text body)
//!   * `GET /healthz`  -> `200 ok`
//!   * self-reports its version: `LODE_ACTIVE_VERSION` (injected by lode) wins,
//!     else the baked `BUILD_VERSION` (see build.rs)
//!   * graceful stop: on SIGTERM / SIGINT it drains and `exit(0)` sub-second,
//!     well within `supervise.stop_timeout` (design §8)
//!   * optional readiness handshake: when `LODE_DATA_DIR` is set it atomically
//!     (temp + rename) writes `state.json` field `ready = $LODE_INSTANCE`,
//!     preserving lode's own fields — this is what makes `readiness = "state"`
//!     work (design §7/§8, integration §2)
//!   * "bad" mode for rollback tests: baked `BUILD_BAD=1` or runtime
//!     `LODE_APP_BAD=1` -> `exit(1)` immediately on startup (crash within
//!     `health_grace`)
//!
//! Uses ONLY the Rust standard library (no external crates) so it compiles into
//! a small, self-contained, static-friendly binary. The single `extern "C"`
//! block is the libc `signal(2)` shim — std has no signal API, and the
//! graceful-stop contract requires catching SIGTERM. The handler only stores
//! into an `AtomicBool` (async-signal-safe); the accept loop polls it.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

// POSIX signal numbers (identical on Linux and macOS).
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

// libc `signal(2)`: std exposes no signal API, so we declare the one symbol we
// need. It is always available (libc is linked into every Rust program).
type SigHandler = extern "C" fn(i32);
extern "C" {
    fn signal(signum: i32, handler: SigHandler) -> *const ();
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

// Only async-signal-safe work: a single atomic store. The accept loop notices.
extern "C" fn on_signal(_sig: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn log(msg: &str) {
    println!("[web-rust] {msg}");
}

fn resolve_version() -> String {
    // lode injects LODE_ACTIVE_VERSION = what it installed; it wins so the
    // self-reported version always matches. Standalone -> the baked build value.
    match env::var("LODE_ACTIVE_VERSION") {
        Ok(v) if !v.is_empty() => v,
        _ => env!("BUILD_VERSION").to_string(),
    }
}

fn main() {
    let version = resolve_version();

    // `web-rust version` (or --version/-v) just prints the version and exits 0 —
    // handy for standalone testing and `lode version` passthrough when the
    // operator sets exec = "./web-rust".
    if let Some(arg) = env::args().nth(1) {
        if arg == "version" || arg == "--version" || arg == "-v" {
            println!("{version}");
            return;
        }
    }

    // Bad mode (rollback testing): crash immediately on startup so the new
    // version never survives health_grace and lode rolls back. Baked (a real
    // "bad v0.0.3" artifact) or forced at runtime without rebuilding.
    let baked_bad = env!("BUILD_BAD") == "1";
    let runtime_bad = env::var("LODE_APP_BAD").map(|v| v == "1").unwrap_or(false);
    if baked_bad || runtime_bad {
        eprintln!(
            "[web-rust] bad mode (baked={baked_bad} LODE_APP_BAD={runtime_bad}) — crashing on startup, exit 1"
        );
        process::exit(1);
    }

    // Install graceful-stop handlers before we start serving.
    unsafe {
        signal(SIGTERM, on_signal);
        signal(SIGINT, on_signal);
    }

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let addr = format!("0.0.0.0:{port}");

    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[web-rust] failed to bind {addr}: {e}");
            process::exit(1);
        }
    };
    // Non-blocking accept so the loop can poll the shutdown flag and exit fast.
    if let Err(e) = listener.set_nonblocking(true) {
        eprintln!("[web-rust] set_nonblocking failed: {e}");
        process::exit(1);
    }

    let instance = env::var("LODE_INSTANCE").unwrap_or_else(|_| "none".to_string());
    let data_dir = env::var("LODE_DATA_DIR").unwrap_or_else(|_| "unset".to_string());
    log(&format!(
        "starting version={version} pid={} instance={instance} data_dir={data_dir} addr={addr}",
        process::id()
    ));

    // Readiness handshake: once the listener is bound we can serve, so announce.
    announce_ready();

    serve(&listener, &version);

    // Reached only via SIGTERM/SIGINT.
    log("SIGTERM/SIGINT received — cleaning up");
    log("cleanup done, exiting 0");
    process::exit(0);
}

fn serve(listener: &TcpListener, version: &str) {
    while !SHUTDOWN.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _addr)) => handle(stream, version),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No pending connection — nap briefly, then re-check shutdown.
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[web-rust] accept error: {e}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn handle(mut stream: TcpStream, version: &str) {
    // A slow/silent client must not wedge the single-threaded loop.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 1024];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    // Request line: "<METHOD> <PATH> HTTP/1.1".
    let path = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");

    let (status, body) = match path {
        "/version" => ("200 OK", version),
        "/healthz" => ("200 OK", "ok"),
        _ => ("404 Not Found", "not found"),
    };

    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// When `supervise.readiness = "state"`, lode marks us running/good only after
/// we self-report ready. We atomically (temp + rename) set `state.json` field
/// `ready = $LODE_INSTANCE`, preserving lode's own fields. No-op when
/// `LODE_DATA_DIR` is unset (standalone runs).
fn announce_ready() {
    let data_dir = match env::var("LODE_DATA_DIR") {
        Ok(d) if !d.is_empty() => d,
        _ => return,
    };
    let inst = env::var("LODE_INSTANCE").unwrap_or_default();
    let state_path = Path::new(&data_dir).join("state.json");

    let content = match fs::read_to_string(&state_path) {
        Ok(existing) if existing.contains("\"ready\"") => {
            replace_ready(&existing, &inst).unwrap_or_else(|| minimal_state(&inst))
        }
        Ok(existing) if !existing.trim().is_empty() => {
            inject_ready(&existing, &inst).unwrap_or_else(|| minimal_state(&inst))
        }
        _ => minimal_state(&inst),
    };

    // Atomic temp + rename, so lode never reads a half-written state.json.
    let tmp = state_path.with_extension(format!("ready.{}", process::id()));
    if let Err(e) = fs::write(&tmp, &content).and_then(|()| fs::rename(&tmp, &state_path)) {
        let _ = fs::remove_file(&tmp);
        eprintln!("[web-rust] ready write failed: {e}");
        return;
    }
    log(&format!(
        "ready: wrote state.ready={inst} -> {}",
        state_path.display()
    ));
}

fn minimal_state(inst: &str) -> String {
    format!("{{\n  \"ready\": \"{inst}\"\n}}\n")
}

/// Replace the value of an existing `"ready"` key with `"<inst>"`, keeping every
/// other field byte-for-byte. The value runs up to the next `,` or `}`.
fn replace_ready(content: &str, inst: &str) -> Option<String> {
    let key = content.find("\"ready\"")?;
    let colon = content[key..].find(':')? + key;
    let rest = &content[colon + 1..];
    let end_rel = rest.find([',', '}'])?;
    let end = colon + 1 + end_rel;
    let mut out = String::with_capacity(content.len() + inst.len());
    out.push_str(&content[..=colon]);
    out.push_str(&format!(" \"{inst}\""));
    out.push_str(&content[end..]);
    Some(out)
}

/// Insert a `"ready"` field into an existing JSON object that lacks one,
/// preserving the other fields.
fn inject_ready(content: &str, inst: &str) -> Option<String> {
    let brace = content.find('{')?;
    let after = &content[brace + 1..];
    let needs_comma = after.trim_start().starts_with('"');
    let mut out = String::with_capacity(content.len() + inst.len() + 16);
    out.push_str(&content[..=brace]);
    out.push_str(&format!(
        "\n  \"ready\": \"{inst}\"{}",
        if needs_comma { "," } else { "" }
    ));
    out.push_str(after);
    Some(out)
}
