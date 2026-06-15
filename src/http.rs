//! Blocking HTTP via `ureq` (rustls + aws-lc-rs, installed in `main`).
//!
//! `[http].headers` are passed through verbatim with `${ENV}` expanded from the
//! process environment **at request time** — so rotated secrets take effect
//! without restating them on disk, and plaintext credentials never need to live
//! in `lode.toml`. Header *values* and URL query strings are never logged; only
//! the scheme/host/path of a URL appears in diagnostics (design §11, §16).
//!
//! Every fetch goes through one shared [`struct@AGENT`] with explicit per-phase
//! timeouts (lode is a single-threaded PID-1 supervisor — nothing may block
//! forever, see [`TIMEOUTS`]), and redirects are followed **manually** so
//! credentials are re-decided on every hop (the redirect policy lives on
//! [`send`]).

use std::io::Read;
use std::sync::LazyLock;
use std::time::Duration;

use crate::error::{Error, Result};

/// Byte cap for in-memory fetches (manifests). Generous, but bounded so a
/// hostile/oversized response can't exhaust memory. Streaming downloads
/// ([`get_reader`]) are intentionally unbounded — they go to disk, not RAM.
const MANIFEST_LIMIT: u64 = 32 * 1024 * 1024;

/// Per-phase timeouts for the shared agent. A struct (rather than loose
/// constants in [`agent_with`]) so tests can build an agent with tiny values
/// against a deliberately silent server; production uses [`TIMEOUTS`].
struct TimeoutCfg {
    /// TCP connect + TLS handshake.
    connect: Duration,
    /// Writing the request head (lode sends no request bodies).
    send_request: Duration,
    /// Awaiting the response status line + headers.
    recv_response: Duration,
}

/// Production timeout policy. lode's supervise loop is a single-threaded,
/// synchronous PID-1 tick (no async, no worker threads): a manifest or artifact
/// fetch that hangs would block signal handling, child reaping and restarts
/// indefinitely. `ureq` 3.x defaults **every** timeout to `None`, so each phase
/// is capped here explicitly; on top of these, every request also carries an
/// end-to-end cap chosen by fetch kind ([`SMALL_FETCH_CAP`] /
/// [`STREAM_FETCH_CAP`]). All caps apply per redirect hop (each hop is its own
/// request), so the worst case is `(1 + MAX_REDIRECT_HOPS) ×` the cap — slow,
/// but firmly bounded.
const TIMEOUTS: TimeoutCfg = TimeoutCfg {
    connect: Duration::from_secs(10),
    send_request: Duration::from_secs(30),
    recv_response: Duration::from_secs(30),
};

/// End-to-end cap (DNS through the last body byte) for in-memory fetches:
/// manifests, GitHub API JSON and `.sig` sidecars are small documents, so a
/// minute is generous.
const SMALL_FETCH_CAP: Duration = Duration::from_mins(1);

/// End-to-end cap for streamed artifact/runtime downloads. NOT `None` — a dead
/// peer must never hang the supervisor — but generous enough for multi-GB
/// artifacts on slow links: one hour.
const STREAM_FETCH_CAP: Duration = Duration::from_hours(1);

/// Maximum redirect hops [`send`] follows before giving up.
const MAX_REDIRECT_HOPS: u32 = 5;

/// The shared agent: every fetch in the crate goes through it, so the timeout
/// policy and the manual-redirect setting can't be bypassed by a new call site.
static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| agent_with(&TIMEOUTS));

/// Build an agent from `timeouts`. Auto-follow is disabled (`max_redirects(0)`)
/// because [`send`] follows redirects manually to re-decide credentials per hop.
fn agent_with(timeouts: &TimeoutCfg) -> ureq::Agent {
    ureq::config::Config::builder()
        .timeout_connect(Some(timeouts.connect))
        .timeout_send_request(Some(timeouts.send_request))
        .timeout_recv_response(Some(timeouts.recv_response))
        .max_redirects(0)
        .build()
        .new_agent()
}

/// Resolve `[http].headers` (each `"Name: Value"`) into name/value pairs, with
/// `${ENV}` references expanded from the current process environment.
///
/// Done per request (not at config load) so secrets are read fresh and never
/// persisted into [`crate::config::Config`]. A malformed line, an empty name, an
/// unterminated `${…}`, or a reference to an unset variable is an error. Error
/// messages never echo a header value.
pub(crate) fn expand_headers(raw: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(raw.len());
    for line in raw {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| Error::Http("malformed header (expected \"Name: Value\")".to_owned()))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Http("malformed header: empty name".to_owned()));
        }
        out.push((name.to_owned(), expand_env(value.trim())?));
    }
    Ok(out)
}

/// Expand `${NAME}` references in a header value from the process environment.
fn expand_env(input: &str) -> Result<String> {
    expand_with(input, |var| std::env::var(var).ok())
}

/// `${NAME}` expansion against an arbitrary `lookup` (the env in production; a
/// fixture in tests — keeping `lode` free of `unsafe` `set_var` calls). Literal
/// text passes through; `$` not followed by `{` is literal. Errors carry only the
/// *variable name* (not secret) — never the resolved value.
fn expand_with(input: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| Error::Http("unterminated ${...} in header value".to_owned()))?;
        let var = &after[..end];
        let value = lookup(var)
            .ok_or_else(|| Error::Http(format!("header references unset env var ${{{var}}}")))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Fetch a URL fully into memory (for manifests). Bounded by [`MANIFEST_LIMIT`]
/// and time-capped by [`SMALL_FETCH_CAP`]. `allow_insecure` waives the default
/// HTTPS requirement (see [`enforce_scheme`]); `attach` decides per redirect hop
/// whether `headers` ride to that hop's host (see [`send`]).
pub(crate) fn get_bytes(
    url: &str,
    headers: &[(String, String)],
    allow_insecure: bool,
    attach: &dyn Fn(&str) -> bool,
) -> Result<Vec<u8>> {
    send(
        &AGENT,
        url,
        headers,
        allow_insecure,
        attach,
        SMALL_FETCH_CAP,
    )?
    .into_with_config()
    .limit(MANIFEST_LIMIT)
    .read_to_vec()
    .map_err(|e| Error::Http(format!("read body from {}: {e}", redact_url(url))))
}

/// Open a streaming reader over a URL (for artifact downloads). The reader owns
/// the connection (`'static`) and is byte-unbounded (the caller streams it to
/// disk) but time-capped by [`STREAM_FETCH_CAP`]. `allow_insecure` waives the
/// default HTTPS requirement (see [`enforce_scheme`]); `attach` decides per
/// redirect hop whether `headers` ride to that hop's host (see [`send`]).
pub(crate) fn get_reader(
    url: &str,
    headers: &[(String, String)],
    allow_insecure: bool,
    attach: &dyn Fn(&str) -> bool,
) -> Result<impl Read> {
    Ok(send(
        &AGENT,
        url,
        headers,
        allow_insecure,
        attach,
        STREAM_FETCH_CAP,
    )?
    .into_reader())
}

/// Issue a blocking GET via `agent`, following up to [`MAX_REDIRECT_HOPS`]
/// redirects **manually**, and hand back the final response body. 4xx/5xx are
/// surfaced as errors (ureq's default `http_status_as_error`); `cap` is the
/// end-to-end time limit applied to each hop's request.
///
/// **Redirect policy** (the project's): auto-follow is disabled on the agent
/// and each `301/302/303/307/308` hop is re-validated from scratch — the
/// `Location` is resolved against the current URL ([`resolve_location`]), the
/// scheme rule re-runs on the hop's URL ([`enforce_scheme`], so e.g. an
/// `https → remote http` downgrade is refused mid-chain), and the
/// caller-supplied `attach` predicate re-decides from the hop's **host**
/// whether the custom headers ride along. ureq itself only strips
/// `Authorization`/`Cookie` cross-host, but `[http].headers` carry arbitrary
/// credential headers (`PRIVATE-TOKEN`, `X-Api-Key`, …) — so a hop whose host
/// the predicate rejects gets **none** of them (warned, host only; the body is
/// still fetched). Exceeding [`MAX_REDIRECT_HOPS`] is an error carrying the
/// hop count and the last URL.
fn send(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    allow_insecure: bool,
    attach: &dyn Fn(&str) -> bool,
    cap: Duration,
) -> Result<ureq::Body> {
    let mut current = url.to_owned();
    let mut hops = 0u32;
    loop {
        // The scheme rule re-runs on EVERY hop, not just the initial URL.
        enforce_scheme(&current, allow_insecure)?;
        let host = url_host(&current);
        let attach_here = host.is_some_and(attach);
        if !attach_here && !headers.is_empty() {
            // Headers withheld from this hop's host. Only the host is logged.
            tracing::warn!(
                host = host.unwrap_or("?"),
                hops,
                "custom headers not attached to non-allowlisted host"
            );
        }
        let hop_headers: &[(String, String)] = if attach_here { headers } else { &[] };
        tracing::debug!(url = %redact_url(&current), hops, headers = hop_headers.len(), "http get");
        let mut request = agent
            .get(&current)
            .config()
            .timeout_global(Some(cap))
            .build();
        for (name, value) in hop_headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let response = request.call().map_err(|e| {
            let via = if hops == 0 {
                String::new()
            } else {
                format!(" (redirect hop {hops} from {})", redact_url(url))
            };
            Error::Http(format!("GET {}{via}: {e}", redact_url(&current)))
        })?;
        if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Ok(response.into_body());
        }
        if hops == MAX_REDIRECT_HOPS {
            return Err(Error::Http(format!(
                "GET {}: more than {MAX_REDIRECT_HOPS} redirects (last hop {})",
                redact_url(url),
                redact_url(&current)
            )));
        }
        let location = response
            .headers()
            .get(ureq::http::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                Error::Http(format!(
                    "GET {}: redirect without a usable Location header",
                    redact_url(&current)
                ))
            })?;
        current = resolve_location(&current, location)?;
        hops += 1;
    }
}

/// Resolve a redirect `Location` against the current URL. An absolute
/// `http(s)://…` is taken as-is; `//authority/…` keeps the current scheme;
/// `/path` keeps scheme + authority; anything else replaces the last segment of
/// the current path. No `..` normalisation (hand-rolled — no URL crate per the
/// dependency policy — and real-world redirect targets are overwhelmingly
/// absolute). An empty `Location`, or a base without `scheme://`, is an error.
fn resolve_location(base: &str, location: &str) -> Result<String> {
    let location = location.trim();
    if location.is_empty() {
        return Err(Error::Http(format!(
            "redirect from {} carries an empty Location",
            redact_url(base)
        )));
    }
    if has_http_scheme(location) {
        return Ok(location.to_owned());
    }
    let (scheme, rest) = base.split_once("://").ok_or_else(|| {
        Error::Http(format!(
            "cannot resolve redirect Location against {}",
            redact_url(base)
        ))
    })?;
    if let Some(tail) = location.strip_prefix("//") {
        return Ok(format!("{scheme}://{tail}"));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if location.starts_with('/') {
        return Ok(format!("{scheme}://{authority}{location}"));
    }
    // Relative path: swap out everything after the base path's last '/'.
    let path = rest[authority.len()..]
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    let dir = path.rfind('/').map_or("/", |i| &path[..=i]);
    Ok(format!("{scheme}://{authority}{dir}{location}"))
}

/// Whether `s` starts with `http://` or `https://` (ASCII case-insensitive).
fn has_http_scheme(s: &str) -> bool {
    ["http://", "https://"].iter().any(|prefix| {
        s.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    })
}

/// Attach-predicate allowing only the exact host of `url` (ASCII
/// case-insensitive) — the minimum same-origin rule for API/manifest fetches:
/// credentials follow same-host redirects but never leave the host the operator
/// configured. A hostless `url` yields a predicate that allows nothing.
pub(crate) fn same_host_as(url: &str) -> impl Fn(&str) -> bool {
    let initial = url_host(url).map(str::to_owned);
    move |host: &str| {
        initial
            .as_deref()
            .is_some_and(|i| i.eq_ignore_ascii_case(host))
    }
}

/// Refuse a non-HTTPS fetch unless explicitly waived. Remote manifests, GitHub API
/// calls, artifacts and runtime downloads must travel over TLS by default so
/// `[http].headers` credentials and payloads are never sent in the clear (design
/// §11/§16). Two escapes: `allow_insecure` (operator opt-out via
/// `[http].allow_insecure` / `--allow-insecure-http`), and an always-on carve-out
/// for plain `http` to a loopback host — local dev and the test harness are not
/// "remote". The rejected URL is redacted to scheme/host/path.
fn enforce_scheme(url: &str, allow_insecure: bool) -> Result<()> {
    if allow_insecure || scheme_allowed(url) {
        return Ok(());
    }
    Err(Error::Http(format!(
        "refusing non-HTTPS URL {}; set [http].allow_insecure=true to override",
        redact_url(url)
    )))
}

/// Whether `url` may be fetched without an insecure opt-in: any `https` URL, or an
/// `http` URL whose host is loopback. Scheme compare is case-insensitive.
fn scheme_allowed(url: &str) -> bool {
    let (scheme, after) = url.split_once("://").unwrap_or(("", url));
    if scheme.eq_ignore_ascii_case("https") {
        return true;
    }
    scheme.eq_ignore_ascii_case("http") && is_loopback(after)
}

/// Is the authority of `after` (the text following `scheme://`) a loopback host?
/// Drops any `user:pass@` userinfo and `:port`, and understands an `[::1]` IPv6
/// literal. Loopback = `localhost`, `::1`, or a numeric host in `127.0.0.0/8`.
fn is_loopback(after: &str) -> bool {
    // Authority ends at the first '/', '?' or '#'.
    let authority = after.split(['/', '?', '#']).next().unwrap_or(after);
    // Userinfo (everything up to and including the last '@') is not the host.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    // IPv6 literal: `[::1]` (optionally `]:port`).
    if let Some(rest) = host_port.strip_prefix('[') {
        return rest.split(']').next() == Some("::1");
    }
    let host = host_port.split(':').next().unwrap_or(host_port);
    host.eq_ignore_ascii_case("localhost") || is_ipv4_loopback(host)
}

/// A literal IPv4 address in `127.0.0.0/8` (e.g. `127.0.0.1`). The `127.` prefix is
/// honoured only for an all-numeric host, so a DNS name like `127.example.com`
/// (which could resolve anywhere) is NOT treated as loopback.
fn is_ipv4_loopback(host: &str) -> bool {
    host.strip_prefix("127.").is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    })
}

/// The loggable form of a URL: everything up to (but excluding) the query string
/// or fragment, so credentials carried as `?token=…` are never written to logs.
fn redact_url(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// The host of an `http`/`https` URL — the authority's hostname, with the scheme,
/// any `userinfo@`, the `:port`, and the path/query/fragment all stripped.
/// Hand-rolled (no URL crate per the dependency policy); returns a borrow, so it
/// is *not* lowercased — callers compare with [`str::eq_ignore_ascii_case`].
/// `None` when `url` carries no recognizable `http(s)://` host.
///
/// Userinfo is stripped *before* the port so a credential-stuffed authority like
/// `https://github.com:@evil.example/…` resolves to `evil.example` (not the
/// spoofed `github.com`) and is correctly treated as cross-origin.
pub(crate) fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    // The authority is everything before the first '/', '?' or '#'.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // Drop any `userinfo@` (taking the part after the *last* '@')…
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    // …then any `:port`.
    let host = host_port.split(':').next().unwrap_or(host_port);
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture environment, so expansion is tested without touching real env
    /// (which would need `unsafe set_var`, forbidden crate-wide).
    fn fixture(var: &str) -> Option<String> {
        match var {
            "RELEASE_TOKEN" => Some("s3cr3t".to_owned()),
            "API_KEY" => Some("abc123".to_owned()),
            _ => None,
        }
    }

    #[test]
    fn expands_env_at_request_time() {
        assert_eq!(
            expand_with("Bearer ${RELEASE_TOKEN}", fixture).unwrap(),
            "Bearer s3cr3t"
        );
        // Multiple references in one value.
        assert_eq!(
            expand_with("${RELEASE_TOKEN}:${API_KEY}", fixture).unwrap(),
            "s3cr3t:abc123"
        );
        // A bare `$` (not `${`) is literal.
        assert_eq!(expand_with("price is $5", fixture).unwrap(), "price is $5");
    }

    #[test]
    fn unset_env_var_is_an_error() {
        assert!(expand_with("${LODE_DEFINITELY_UNSET}", fixture).is_err());
    }

    #[test]
    fn unterminated_reference_rejected() {
        assert!(expand_with("${UNTERMINATED", fixture).is_err());
    }

    #[test]
    fn malformed_header_lines_rejected() {
        // Parsing (no `${…}`) fails before any env access, so real env is untouched.
        assert!(expand_headers(&["no-colon-here".to_owned()]).is_err());
        assert!(expand_headers(&[" : value".to_owned()]).is_err());
    }

    #[test]
    fn plain_headers_pass_through() {
        // No `${…}` → no env access; literal value preserved.
        let headers = expand_headers(&["X-Api-Key: literal-value".to_owned()]).unwrap();
        assert_eq!(
            headers[0],
            ("X-Api-Key".to_owned(), "literal-value".to_owned())
        );
    }

    #[test]
    fn url_host_extracts_authority_hostname() {
        assert_eq!(
            url_host("https://h.example/p/manifest.json"),
            Some("h.example")
        );
        assert_eq!(url_host("http://h.example"), Some("h.example"));
        // Port, query and fragment are all stripped.
        assert_eq!(
            url_host("https://h.example:8443/p?x=1#f"),
            Some("h.example")
        );
        assert_eq!(url_host("http://127.0.0.1:9090/payload"), Some("127.0.0.1"));
        // Userinfo is stripped *before* the port — a spoofed authority resolves to
        // the real host, so credentials are never misattributed to `github.com`.
        assert_eq!(
            url_host("https://github.com:@evil.example/x"),
            Some("evil.example")
        );
        assert_eq!(url_host("https://user:pass@h.example/x"), Some("h.example"));
        // Non-http(s) or hostless inputs yield None.
        assert_eq!(url_host("ftp://h.example/x"), None);
        assert_eq!(url_host("https:///just-a-path"), None);
        assert_eq!(url_host("not-a-url"), None);
    }

    #[test]
    fn https_is_allowed_and_remote_http_is_rejected() {
        assert!(enforce_scheme("https://releases.example.com/m.json", false).is_ok());
        // Scheme compare is case-insensitive.
        assert!(enforce_scheme("HTTPS://releases.example.com/m.json", false).is_ok());
        // Plain http to a real host is refused by default.
        assert!(enforce_scheme("http://releases.example.com/m.json", false).is_err());
        // A URL with no scheme is not https/loopback-http → refused.
        assert!(enforce_scheme("releases.example.com/m.json", false).is_err());
    }

    #[test]
    fn loopback_http_is_always_allowed() {
        for url in [
            "http://localhost/m.json",
            "http://localhost:8080/m.json",
            "http://127.0.0.1:3000/manifest.json",
            "http://127.0.0.1/x",
            "http://127.1.2.3/x",                // 127.0.0.0/8
            "http://[::1]:9000/x",               // IPv6 loopback literal
            "http://user:pass@127.0.0.1:3000/x", // userinfo is stripped
        ] {
            assert!(
                enforce_scheme(url, false).is_ok(),
                "loopback http should be allowed: {url}"
            );
        }
    }

    #[test]
    fn lookalike_loopback_hosts_are_still_remote() {
        // A DNS name that merely starts with `127.` or contains `localhost` could
        // resolve anywhere, so it is NOT a loopback carve-out.
        assert!(enforce_scheme("http://127.example.com/x", false).is_err());
        assert!(enforce_scheme("http://localhost.evil.com/x", false).is_err());
        assert!(enforce_scheme("http://notlocalhost/x", false).is_err());
    }

    #[test]
    fn allow_insecure_permits_remote_http() {
        assert!(enforce_scheme("http://releases.example.com/m.json", true).is_ok());
        // Even an exotic scheme is waived once the operator opts in.
        assert!(enforce_scheme("http://10.0.0.5:8080/m.json", true).is_ok());
    }

    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};

    /// Read one request head (through the blank line) off `sock`.
    fn read_head(sock: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            if sock.read(&mut byte).unwrap() == 0 {
                break;
            }
            buf.push(byte[0]);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Serve exactly one request on `listener`: write `response` back and return
    /// the received request head via the join handle.
    fn serve_one(listener: TcpListener, response: String) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let head = read_head(&mut sock);
            sock.write_all(response.as_bytes()).unwrap();
            head
        })
    }

    /// A `302 Found` response head pointing at `location`.
    fn redirect_to(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    const OK_BODY: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

    /// P1-5: a server whose backlog completes the TCP handshake but never writes
    /// a response must produce a timeout error, not hang the (single-threaded,
    /// PID-1) caller forever. Pre-fix — bare `ureq::get`, all timeouts `None` —
    /// this test hangs indefinitely.
    #[test]
    fn silent_server_times_out_instead_of_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let agent = agent_with(&TimeoutCfg {
            connect: Duration::from_millis(500),
            send_request: Duration::from_millis(500),
            recv_response: Duration::from_millis(250),
        });
        let started = std::time::Instant::now();
        let result = send(
            &agent,
            &format!("http://127.0.0.1:{port}/m.json"),
            &[],
            false,
            &|_: &str| true,
            Duration::from_secs(2),
        );
        assert!(result.is_err(), "a silent server must time out, not hang");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timed out too slowly: {:?}",
            started.elapsed()
        );
        drop(listener);
    }

    /// P2-10: a cross-host redirect (different host STRING — `127.0.0.1` vs
    /// `localhost`) must not carry the custom headers to the new host when the
    /// attach predicate rejects it; the body is still fetched (strip, not fail).
    #[test]
    fn cross_host_redirect_drops_custom_headers() {
        // Target server B is reached via the host string `localhost`.
        let b = TcpListener::bind("127.0.0.1:0").unwrap();
        let port_b = b.local_addr().unwrap().port();
        let tb = serve_one(b, OK_BODY.to_owned());
        // Initial server A under `127.0.0.1` bounces to B under `localhost`.
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let port_a = a.local_addr().unwrap().port();
        let ta = serve_one(a, redirect_to(&format!("http://localhost:{port_b}/x")));

        let headers = vec![("X-Api-Key".to_owned(), "secret".to_owned())];
        // Only the initial host may carry the credential.
        let attach = |host: &str| host.eq_ignore_ascii_case("127.0.0.1");
        let body = get_bytes(
            &format!("http://127.0.0.1:{port_a}/start"),
            &headers,
            false,
            &attach,
        )
        .unwrap();

        // The hop is followed (body arrives), just without credentials…
        assert_eq!(body, b"ok");
        // …server A (allowed) saw the header; server B (cross-host) did NOT.
        assert!(
            ta.join()
                .unwrap()
                .to_ascii_lowercase()
                .contains("x-api-key")
        );
        let head_b = tb.join().unwrap();
        assert!(
            !head_b.to_ascii_lowercase().contains("x-api-key"),
            "credential leaked across hosts: {head_b}"
        );
    }

    /// The positive control for the predicate: a redirect to an allowed host
    /// keeps the custom headers (same-host here, both hops `127.0.0.1`).
    #[test]
    fn allowed_host_redirect_keeps_custom_headers() {
        let b = TcpListener::bind("127.0.0.1:0").unwrap();
        let port_b = b.local_addr().unwrap().port();
        let tb = serve_one(b, OK_BODY.to_owned());
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let port_a = a.local_addr().unwrap().port();
        let ta = serve_one(a, redirect_to(&format!("http://127.0.0.1:{port_b}/x")));

        let headers = vec![("X-Api-Key".to_owned(), "secret".to_owned())];
        let attach = |host: &str| host.eq_ignore_ascii_case("127.0.0.1");
        let body = get_bytes(
            &format!("http://127.0.0.1:{port_a}/start"),
            &headers,
            false,
            &attach,
        )
        .unwrap();

        assert_eq!(body, b"ok");
        ta.join().unwrap();
        assert!(
            tb.join()
                .unwrap()
                .to_ascii_lowercase()
                .contains("x-api-key")
        );
    }

    /// More than [`MAX_REDIRECT_HOPS`] redirects is an error, not a loop.
    #[test]
    fn redirects_past_the_hop_cap_error_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            // Initial request + MAX_REDIRECT_HOPS follows; the last served
            // redirect trips the cap client-side.
            for _ in 0..=MAX_REDIRECT_HOPS {
                let (mut sock, _) = listener.accept().unwrap();
                let _ = read_head(&mut sock);
                sock.write_all(redirect_to(&format!("http://127.0.0.1:{port}/again")).as_bytes())
                    .unwrap();
            }
        });
        let err = get_bytes(
            &format!("http://127.0.0.1:{port}/start"),
            &[],
            false,
            &|_: &str| true,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("redirects"),
            "unexpected error: {err}"
        );
        server.join().unwrap();
    }

    /// The scheme rule re-runs per hop: a loopback-http server redirecting to
    /// REMOTE plain http is refused mid-chain (no request leaves for it).
    #[test]
    fn redirect_hop_to_remote_http_is_refused() {
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = a.local_addr().unwrap().port();
        let ta = serve_one(a, redirect_to("http://releases.example.com/x"));
        let err = get_bytes(
            &format!("http://127.0.0.1:{port}/m.json"),
            &[],
            false,
            &|_: &str| true,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("refusing non-HTTPS"),
            "scheme rule must re-run per hop: {err}"
        );
        ta.join().unwrap();
    }

    #[test]
    fn resolve_location_handles_all_forms() {
        let base = "https://a.example/dir/file?q=1";
        // Absolute (scheme case-insensitive) → taken as-is.
        assert_eq!(
            resolve_location(base, "https://b.example/z").unwrap(),
            "https://b.example/z"
        );
        assert_eq!(
            resolve_location(base, "HTTP://b.example/z").unwrap(),
            "HTTP://b.example/z"
        );
        // Scheme-relative keeps the base scheme.
        assert_eq!(
            resolve_location(base, "//cdn.example/z").unwrap(),
            "https://cdn.example/z"
        );
        // Host-relative keeps scheme + authority.
        assert_eq!(
            resolve_location(base, "/z?tok=1").unwrap(),
            "https://a.example/z?tok=1"
        );
        // Path-relative replaces the last segment (base query dropped).
        assert_eq!(
            resolve_location(base, "z2").unwrap(),
            "https://a.example/dir/z2"
        );
        // A pathless base resolves relative targets at the root.
        assert_eq!(
            resolve_location("https://a.example", "z").unwrap(),
            "https://a.example/z"
        );
        // Empty Location or a scheme-less base is an error.
        assert!(resolve_location(base, "").is_err());
        assert!(resolve_location("no-scheme", "/z").is_err());
    }

    #[test]
    fn same_host_as_allows_only_the_initial_host() {
        let p = same_host_as("https://api.example:443/v1/releases");
        assert!(p("api.example"));
        assert!(p("API.EXAMPLE")); // case-insensitive
        assert!(!p("evil.example"));
        assert!(!p("api.example.evil")); // exact match, not prefix
        // A hostless URL yields a predicate that allows nothing.
        let none = same_host_as("not-a-url");
        assert!(!none("api.example"));
    }

    #[test]
    fn redact_url_strips_query_and_fragment() {
        assert_eq!(
            redact_url("https://h.example/p/manifest.json?token=abc&x=1"),
            "https://h.example/p/manifest.json"
        );
        assert_eq!(
            redact_url("https://h.example/p#frag"),
            "https://h.example/p"
        );
        assert_eq!(redact_url("https://h.example/p"), "https://h.example/p");
    }
}
