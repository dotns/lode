//! lodetest — a static, std-only multi-tool used ONLY by lode's docker-compose
//! integration test (`tests/src/integration/compose.test.ts`). It exists because
//! lode's release image is `gcr.io/distroless/static` — no libc, no shell, no
//! real `bun` — so every in-container helper must be a self-contained static
//! binary. It has two modes:
//!
//!   * `lodetest serve <root> [port]` — a tiny read-fresh HTTP file/manifest
//!     server (the local "fileserver" image). Each request reads from disk, so the
//!     test publishes a new manifest/artifact just by writing the file. Used over a
//!     fixed container IP (no DNS) so a static binary needs no NSS resolver.
//!
//!   * `lodetest [run] <script.(ts|js)> [args]` — a stand-in `bun` runtime that
//!     runs the web-bun app contract (the same contract as `tests/apps/web-bun`):
//!       - `GET /version` -> the app's version (`LODE_ACTIVE_VERSION` wins, else the
//!         script's baked `BUILD_VERSION`); `GET /healthz` -> `200 ok`
//!       - readiness: when `LODE_DATA_DIR` is set, atomically writes `state.json`
//!         field `ready = $LODE_INSTANCE` (preserving lode's fields)
//!       - graceful stop: SIGTERM/SIGINT -> drain + `exit(0)` sub-second
//!       - bad mode: script `BUILD_BAD="1"` or env `LODE_APP_BAD=1` -> `exit(1)` on
//!         startup (the crashing-v0.0.3 rollback artifact)
//!       - update-by-app-exit: when `$LODE_DATA_DIR/please_exit_update` appears, the
//!         app writes `state.target=<its contents>` and `exit(0)` — exercising
//!         lode's "child wrote a target then exited -> relaunch the new version"
//!         path inside the container.
//!
//! Uses ONLY std. The single `extern "C"` block is the libc `signal(2)` shim (std
//! has no signal API and the graceful-stop contract needs SIGTERM); its handler
//! only stores into an `AtomicBool` (async-signal-safe).

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

// POSIX signal numbers (identical on Linux and macOS).
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

type SigHandler = extern "C" fn(i32);
extern "C" {
    fn signal(signum: i32, handler: SigHandler) -> *const ();
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_signals() {
    unsafe {
        signal(SIGTERM, on_signal);
        signal(SIGINT, on_signal);
    }
}

fn port_from_env() -> u16 {
    env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("serve") {
        let root = args.get(1).cloned().unwrap_or_else(|| ".".to_string());
        let port = args
            .get(2)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or_else(port_from_env);
        serve(&root, port);
        return;
    }

    // `lodetest get <url>` — a minimal HTTP/1.0 GET client (IP host, no DNS). The
    // integration test runs it via `docker exec <fileserver> /lodetest get ...` to
    // probe the lode services container-to-container (the test host can't route to
    // the compose network). Prints the body; exits 0 on 2xx, else 1.
    if args.first().map(String::as_str) == Some("get") {
        match args.get(1) {
            Some(url) => process::exit(http_get(url)),
            None => {
                eprintln!("usage: lodetest get <url>");
                process::exit(2);
            }
        }
    }

    run_app(&args);
}

/// Minimal HTTP/1.0 GET over a raw TCP socket to `http://HOST:PORT/PATH` (HOST must
/// be an IP — no name resolution, so the static binary needs no NSS). Writes the
/// response body to stdout; returns 0 on a 2xx status, else 1.
fn http_get(url: &str) -> i32 {
    let rest = match url.strip_prefix("http://") {
        Some(r) => r,
        None => {
            eprintln!("[get] only http:// is supported: {url}");
            return 2;
        }
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(80)),
        None => (authority, 80),
    };
    let mut stream = match TcpStream::connect((host, port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[get] connect {host}:{port}: {e}");
            return 1;
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).and_then(|()| stream.flush()).is_err() {
        return 1;
    }
    let mut raw = Vec::new();
    if stream.read_to_end(&mut raw).is_err() {
        return 1;
    }
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some(hb) => hb,
        None => (text.as_ref(), ""),
    };
    print!("{body}");
    let _ = std::io::stdout().flush();
    let ok = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .is_some_and(|c| (200..300).contains(&c));
    i32::from(!ok)
}

// ---------------------------------------------------------------------------
// serve mode — read-fresh static HTTP file/manifest server
// ---------------------------------------------------------------------------

fn serve(root: &str, port: u16) {
    install_signals();
    let root = fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[fileserver] bind {addr} failed: {e}");
            process::exit(1);
        }
    };
    if listener.set_nonblocking(true).is_err() {
        eprintln!("[fileserver] set_nonblocking failed");
        process::exit(1);
    }
    println!("[fileserver] serving {} on {addr}", root.display());

    while !SHUTDOWN.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let root = root.clone();
                // A thread per connection: several lode containers fetch the
                // manifest / artifacts / runtime concurrently at startup.
                thread::spawn(move || serve_conn(stream, &root));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                eprintln!("[fileserver] accept error: {e}");
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
    println!("[fileserver] SIGTERM — exiting 0");
}

fn serve_conn(mut stream: TcpStream, root: &Path) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut buf = [0u8; 4096];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let mut parts = req.lines().next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("");
    let raw_path = parts.next().unwrap_or("/");
    if method != "GET" && method != "HEAD" {
        let _ = write_response(&mut stream, "405 Method Not Allowed", b"method not allowed", false);
        return;
    }

    // Strip query/fragment, then resolve under root (defeating `..` traversal).
    let path = raw_path.split(['?', '#']).next().unwrap_or("/");
    let rel = path.trim_start_matches('/');
    match safe_join(root, rel) {
        Some(file) if file.is_file() => match fs::read(&file) {
            Ok(body) => {
                let _ = write_response(&mut stream, "200 OK", &body, method == "HEAD");
            }
            Err(_) => {
                let _ = write_response(&mut stream, "500 Internal Server Error", b"read error", false);
            }
        },
        _ => {
            let _ = write_response(&mut stream, "404 Not Found", b"not found", method == "HEAD");
        }
    }
}

/// Join `rel` under `root`, rejecting absolute paths and any `..`/root component so
/// a request can never escape the served directory.
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    for comp in rel_path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => return None,
        }
    }
    Some(root.join(rel_path))
}

fn write_response(stream: &mut TcpStream, status: &str, body: &[u8], head_only: bool) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    if !head_only {
        stream.write_all(body)?;
    }
    stream.flush()
}

// ---------------------------------------------------------------------------
// app / bun-runtime mode — the web-bun contract
// ---------------------------------------------------------------------------

fn run_app(args: &[String]) {
    // The script path is the last `.ts`/`.js` arg (lode runs us via
    // `run = "bun app.ts"`; `bun run app.ts` also works). It is read for the
    // baked BUILD_* directives.
    let script = args
        .iter()
        .rev()
        .find(|a| a.ends_with(".ts") || a.ends_with(".js"))
        .cloned();
    let script_text = script
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();

    let version = resolve_version(&script_text);

    // `bun <script> version` (or --version/-v): print version and exit (parity with
    // the real apps' `lode version` passthrough).
    if args.iter().any(|a| a == "version" || a == "--version" || a == "-v") {
        println!("{version}");
        return;
    }

    let baked_bad = baked_value(&script_text, "BUILD_BAD").as_deref() == Some("1");
    let runtime_bad = env::var("LODE_APP_BAD").map(|v| v == "1").unwrap_or(false);
    if baked_bad || runtime_bad {
        eprintln!("[bun] bad mode (baked={baked_bad} LODE_APP_BAD={runtime_bad}) — crashing on startup, exit 1");
        process::exit(1);
    }

    install_signals();

    let port = port_from_env();
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[bun] failed to bind {addr}: {e}");
            process::exit(1);
        }
    };
    if listener.set_nonblocking(true).is_err() {
        eprintln!("[bun] set_nonblocking failed");
        process::exit(1);
    }

    let instance = env::var("LODE_INSTANCE").unwrap_or_else(|_| "none".to_string());
    let data_dir = env::var("LODE_DATA_DIR").unwrap_or_else(|_| "unset".to_string());
    println!(
        "[bun] starting version={version} pid={} instance={instance} data_dir={data_dir} addr={addr}",
        process::id()
    );

    announce_ready();

    // Serve, while watching for the update-by-app-exit trigger. The single-threaded
    // poll loop is plenty for the test's health/version probes.
    while !SHUTDOWN.load(Ordering::SeqCst) {
        if let Some(target) = update_on_exit_target() {
            set_state_field("target", &target);
            println!("[bun] update-on-exit: wrote state.target={target}; exiting 0");
            process::exit(0);
        }
        match listener.accept() {
            Ok((stream, _)) => handle_app(stream, &version),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }

    println!("[bun] SIGTERM/SIGINT received — cleaning up");
    println!("[bun] cleanup done, exiting 0");
    process::exit(0);
}

fn handle_app(mut stream: TcpStream, version: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 1024];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
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

/// `LODE_ACTIVE_VERSION` (injected by lode) wins; else the script's baked
/// `BUILD_VERSION`; else a dev default.
fn resolve_version(script_text: &str) -> String {
    match env::var("LODE_ACTIVE_VERSION") {
        Ok(v) if !v.is_empty() => v,
        _ => baked_value(script_text, "BUILD_VERSION").unwrap_or_else(|| "0.0.0-dev".to_string()),
    }
}

/// Extract a baked constant from the script: `const NAME = "VALUE";` (or
/// `NAME="VALUE"`). Quotes either side; first match wins.
fn baked_value(script_text: &str, name: &str) -> Option<String> {
    for line in script_text.lines() {
        let line = line.trim();
        let after_name = line.strip_prefix("const ").unwrap_or(line);
        let Some(rest) = after_name.strip_prefix(name) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim();
        let start = rest.find('"')? + 1;
        let end = rest[start..].find('"')? + start;
        return Some(rest[start..end].to_string());
    }
    None
}

/// The version named by the update-by-app-exit trigger file, if present. The test
/// drops `$LODE_DATA_DIR/please_exit_update` containing the desired version; the
/// app then writes `state.target` and exits, the same as a real app deciding to
/// upgrade itself.
fn update_on_exit_target() -> Option<String> {
    let dir = env::var("LODE_DATA_DIR").ok().filter(|d| !d.is_empty())?;
    let path = Path::new(&dir).join("please_exit_update");
    let v = fs::read_to_string(&path).ok()?;
    // One-shot: remove the trigger so the version lode relaunches us on does not
    // re-fire it (which would write target == current and make lode mirror-exit).
    let _ = fs::remove_file(&path);
    let v = v.trim().to_string();
    if v.is_empty() {
        return None;
    }
    // Belt-and-suspenders: never request the version we are already running.
    if env::var("LODE_ACTIVE_VERSION").unwrap_or_default() == v {
        return None;
    }
    Some(v)
}

/// Readiness handshake: write `state.json` field `ready = $LODE_INSTANCE` when
/// `LODE_DATA_DIR` is set, so `supervise.readiness = "state"` works.
fn announce_ready() {
    let Some(inst) = env::var("LODE_INSTANCE").ok() else {
        return;
    };
    if env::var("LODE_DATA_DIR").ok().filter(|d| !d.is_empty()).is_some() {
        set_state_field("ready", &inst);
        println!("[bun] ready: wrote state.ready={inst}");
    }
}

/// Atomically set a string field in `$LODE_DATA_DIR/state.json`, preserving lode's
/// own fields (replace the value if the key exists, else insert after `{`, else
/// write a minimal object), via temp + rename. No-op if `LODE_DATA_DIR` is unset.
fn set_state_field(key: &str, value: &str) {
    let Some(dir) = env::var("LODE_DATA_DIR").ok().filter(|d| !d.is_empty()) else {
        return;
    };
    let state_path = Path::new(&dir).join("state.json");
    let content = match fs::read_to_string(&state_path) {
        Ok(existing) if existing.contains(&format!("\"{key}\"")) => {
            replace_field(&existing, key, value).unwrap_or_else(|| minimal(key, value))
        }
        Ok(existing) if !existing.trim().is_empty() => {
            inject_field(&existing, key, value).unwrap_or_else(|| minimal(key, value))
        }
        _ => minimal(key, value),
    };
    let tmp = state_path.with_extension(format!("{key}.{}", process::id()));
    if fs::write(&tmp, &content)
        .and_then(|()| fs::rename(&tmp, &state_path))
        .is_err()
    {
        let _ = fs::remove_file(&tmp);
        eprintln!("[bun] state write failed for {key}");
    }
}

fn minimal(key: &str, value: &str) -> String {
    format!("{{\n  \"{key}\": \"{value}\"\n}}\n")
}

/// Replace the value of an existing `"key"` (runs to the next `,` or `}`), keeping
/// every other field byte-for-byte.
fn replace_field(content: &str, key: &str, value: &str) -> Option<String> {
    let k = content.find(&format!("\"{key}\""))?;
    let colon = content[k..].find(':')? + k;
    let end_rel = content[colon + 1..].find([',', '}'])?;
    let end = colon + 1 + end_rel;
    let mut out = String::with_capacity(content.len() + value.len());
    out.push_str(&content[..=colon]);
    out.push_str(&format!(" \"{value}\""));
    out.push_str(&content[end..]);
    Some(out)
}

/// Insert `"key": "value"` into an existing JSON object that lacks the key.
fn inject_field(content: &str, key: &str, value: &str) -> Option<String> {
    let brace = content.find('{')?;
    let after = &content[brace + 1..];
    let needs_comma = after.trim_start().starts_with('"');
    let mut out = String::with_capacity(content.len() + value.len() + 16);
    out.push_str(&content[..=brace]);
    out.push_str(&format!(
        "\n  \"{key}\": \"{value}\"{}",
        if needs_comma { "," } else { "" }
    ));
    out.push_str(after);
    Some(out)
}
