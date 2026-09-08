//! Test-only loopback HTTP server: answers fixed bodies by request path, one
//! connection per request, and records the paths it served. Lets the adapter
//! and self-update flows run end to end (API JSON → sidecar → artifact) without
//! the network.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// A running stub; `base` is its `http://127.0.0.1:<port>` origin.
pub(crate) struct Stub {
    pub(crate) base: String,
    served: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    /// Bind a loopback port, build the routes from the resulting `base` (so bodies
    /// can embed absolute URLs back to the stub) and serve them until the test
    /// process exits. Unknown paths get a 404.
    pub(crate) fn start(routes_for: impl FnOnce(&str) -> Vec<(String, Vec<u8>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let routes = routes_for(&base);
        let served = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&served);
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { break };
                let path = request_path(&mut sock);
                // Record before answering, so a client that has its response has
                // its path logged.
                log.lock().unwrap().push(path.clone());
                let response = match routes.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => ok(body),
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = sock.write_all(&response);
            }
        });
        Self { base, served }
    }

    /// The request paths served so far, in order.
    pub(crate) fn served(&self) -> Vec<String> {
        self.served.lock().unwrap().clone()
    }
}

/// A `200 OK` response carrying `body`.
fn ok(body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// Read one request head off `sock` and return its path (query string dropped).
fn request_path(sock: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if sock.read(&mut byte).unwrap_or(0) == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf);
    head.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map_or("", |target| target.split('?').next().unwrap_or(target))
        .to_owned()
}
