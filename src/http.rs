//! Blocking HTTP via `ureq` (rustls + aws-lc-rs, installed in `main`).
//!
//! `[http].headers` are passed through verbatim with `${ENV}` expanded from the
//! process environment **at request time** — so rotated secrets take effect
//! without restating them on disk, and plaintext credentials never need to live
//! in `lode.toml`. Header *values* and URL query strings are never logged; only
//! the scheme/host/path of a URL appears in diagnostics (design §11, §16).

use std::io::Read;

use crate::error::{Error, Result};

/// Byte cap for in-memory fetches (manifests). Generous, but bounded so a
/// hostile/oversized response can't exhaust memory. Streaming downloads
/// ([`get_reader`]) are intentionally unbounded — they go to disk, not RAM.
const MANIFEST_LIMIT: u64 = 32 * 1024 * 1024;

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

/// Fetch a URL fully into memory (for manifests). Bounded by [`MANIFEST_LIMIT`].
/// `allow_insecure` waives the default HTTPS requirement (see [`enforce_scheme`]).
pub(crate) fn get_bytes(
    url: &str,
    headers: &[(String, String)],
    allow_insecure: bool,
) -> Result<Vec<u8>> {
    send(url, headers, allow_insecure)?
        .into_with_config()
        .limit(MANIFEST_LIMIT)
        .read_to_vec()
        .map_err(|e| Error::Http(format!("read body from {}: {e}", redact_url(url))))
}

/// Open a streaming reader over a URL (for artifact downloads). The reader owns
/// the connection (`'static`) and is unbounded; the caller streams it to disk.
/// `allow_insecure` waives the default HTTPS requirement (see [`enforce_scheme`]).
pub(crate) fn get_reader(
    url: &str,
    headers: &[(String, String)],
    allow_insecure: bool,
) -> Result<impl Read> {
    Ok(send(url, headers, allow_insecure)?.into_reader())
}

/// Issue a blocking GET, applying `headers`, and hand back the response body.
/// The URL scheme is enforced first ([`enforce_scheme`]); 4xx/5xx are surfaced as
/// errors (ureq's default `http_status_as_error`).
fn send(url: &str, headers: &[(String, String)], allow_insecure: bool) -> Result<ureq::Body> {
    enforce_scheme(url, allow_insecure)?;
    tracing::debug!(url = %redact_url(url), headers = headers.len(), "http get");
    let mut request = ureq::get(url);
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request
        .call()
        .map_err(|e| Error::Http(format!("GET {}: {e}", redact_url(url))))?;
    Ok(response.into_body())
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
        // the real host, so credentials are never mis-attributed to `github.com`.
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
