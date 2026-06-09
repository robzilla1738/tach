//! The `http.get` / `http.post` plan tools — the ONLY network code in the
//! binary.
//!
//! Authority is a URL-glob allowlist on the goal (`allow { http.get
//! "https://api.example.com/**" }`), matched with the same glob engine as file
//! scopes, default-deny. HTTPS is required unless the matching glob itself
//! says `http://` (an explicit localhost/dev opt-in). Redirects are never
//! followed — a 3xx is an output, because auto-following would let an allowed
//! URL bounce to a forbidden one.
//!
//! Secrets never enter the store. A plan supplies sensitive headers as
//! `headers_env { authorization: "STRIPE_AUTH_HEADER" }` — the *name* of an
//! environment variable. The value is read at invocation, attached to the
//! request, and never serialized: receipts and events carry env-var names
//! only. Literal sensitive headers are rejected at check time (E0439) and
//! again here, defense in depth.

use crate::goal::GoalSpec;
use crate::patch::glob_match;
use crate::store;
use serde_json::{json, Map, Value};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// Default per-call timeout. Shorter than shell's: a single HTTP exchange that
/// takes longer than 30s is almost always a hang, and the plan can raise it.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Bytes of response body inlined into the receipt output; the full body is
/// always streamed to the artifact file.
pub const BODY_INLINE_CAP: usize = 1024 * 1024;

/// Header names whose values are credentials. A plan must supply these via
/// `headers_env`, never as literals — a literal would be frozen into the goal
/// source, the receipt input, and the event log.
pub const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "x_api_key",
    "x-api-key",
    "proxy_authorization",
    "proxy-authorization",
];

/// A minimal, deliberately strict URL split. Rejects anything surprising —
/// with a default-deny allowlist, strictness is always safe.
#[derive(Debug, PartialEq)]
pub struct ParsedUrl {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    /// Path + query, always with a leading `/`.
    pub path: String,
}

impl ParsedUrl {
    /// The canonical form authority globs match against:
    /// `scheme://host[:port]/path`.
    pub fn canonical(&self) -> String {
        match self.port {
            Some(p) => format!("{}://{}:{}{}", self.scheme, self.host, p, self.path),
            None => format!("{}://{}{}", self.scheme, self.host, self.path),
        }
    }
}

pub fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("`{url}` has no scheme — write https://…"))?;
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "scheme `{scheme}` is not supported (http/https only)"
        ));
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("`{url}` has no host"));
    }
    if authority.contains('@') {
        return Err("userinfo (`user@host`) in a URL is rejected".into());
    }
    if path.contains('#') {
        return Err("a fragment (`#…`) in a tool URL is rejected".into());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            let port: u16 = p
                .parse()
                .map_err(|_| format!("port `{p}` is out of range"))?;
            (h, Some(port))
        }
        Some(_) => return Err(format!("`{authority}` is not a valid host[:port]")),
        None => (authority, None),
    };
    if host.is_empty() {
        return Err(format!("`{url}` has no host"));
    }
    let host_ok = host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
    if !host_ok {
        return Err(format!(
            "host `{host}` contains characters outside [A-Za-z0-9.-]"
        ));
    }
    Ok(ParsedUrl {
        scheme: scheme.to_string(),
        host: host.to_ascii_lowercase(),
        port,
        path: path.to_string(),
    })
}

/// Is `url` granted for `tool` by the goal's URL globs? Default-deny. HTTPS is
/// required unless the *matching glob* itself starts `http://` — granting a
/// plaintext URL is an explicit, visible act in the goal source.
pub fn url_allowed(spec: &GoalSpec, tool: &str, url: &str) -> Result<(), String> {
    let parsed = parse_url(url)?;
    let globs = match tool {
        "http.get" => &spec.allow.http_get,
        "http.post" => &spec.allow.http_post,
        other => return Err(format!("`{other}` is not an http tool")),
    };
    let canonical = parsed.canonical();
    let matched = globs.iter().find(|g| glob_match(g, &canonical));
    match matched {
        None => Err(format!(
            "`{canonical}` matches none of the goal's `allow {{ {tool} [...] }}` globs"
        )),
        Some(g) => {
            if parsed.scheme == "http" && !g.starts_with("http://") {
                return Err(format!(
                    "`{canonical}` is plaintext http, but the matching glob `{g}` does not \
                     explicitly grant http:// — use https, or grant http:// in the goal"
                ));
            }
            Ok(())
        }
    }
}

/// Validate the call's headers shape and reject literal credentials. Returns
/// the literal headers (snake_case as written) and the env-var *names* for
/// the indirect ones.
pub fn check_headers(input: &Value) -> Result<(), String> {
    if let Some(h) = input.get("headers") {
        let obj = h
            .as_object()
            .ok_or_else(|| "`headers` must be a record of strings".to_string())?;
        for (k, v) in obj {
            if v.as_str().is_none() {
                return Err(format!("header `{k}` must be a string"));
            }
            let norm = k.to_ascii_lowercase();
            if SENSITIVE_HEADERS.contains(&norm.as_str()) {
                return Err(format!(
                    "header `{k}` is a credential and must not be a literal — use \
                     `headers_env {{ {k}: \"ENV_VAR_NAME\" }}` so the value never \
                     enters the goal source, receipts, or events"
                ));
            }
        }
    }
    if let Some(h) = input.get("headers_env") {
        let obj = h
            .as_object()
            .ok_or_else(|| "`headers_env` must be a record of env-var names".to_string())?;
        for (k, v) in obj {
            if v.as_str().is_none() {
                return Err(format!(
                    "`headers_env.{k}` must name an environment variable"
                ));
            }
        }
    }
    Ok(())
}

/// Invoke `http.get` / `http.post`. The caller has authorized the URL and
/// verified no receipt exists. Inner `Err` means the exchange did not complete
/// — for GET that is safe to retry; for POST the request may or may not have
/// reached the server (the inherent at-least-once window), which is why every
/// POST carries an `Idempotency-Key` derived from the receipt key, letting
/// idempotent APIs deduplicate a retry.
pub fn invoke_http(
    repo: &Path,
    run_id: &str,
    key: &str,
    tool: &str,
    input: &Value,
) -> io::Result<Result<Value, String>> {
    let url = match input.get("url").and_then(|v| v.as_str()) {
        Some(u) => u,
        None => return Ok(Err("`url` is required and must be a string".into())),
    };
    if let Err(e) = check_headers(input) {
        return Ok(Err(e));
    }
    let timeout_ms = input
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_TIMEOUT_MS);

    // Resolve env-named headers BEFORE any I/O: a missing variable must fail
    // with no receipt and no request, so exporting it and resuming is safe.
    let mut secret_headers: Vec<(String, String)> = Vec::new();
    let mut resolved_names: Vec<String> = Vec::new();
    if let Some(obj) = input.get("headers_env").and_then(|v| v.as_object()) {
        for (header, var) in obj {
            let var = var.as_str().unwrap_or_default();
            match std::env::var(var) {
                Ok(value) => {
                    secret_headers.push((header_wire_name(header), value));
                    resolved_names.push(header.clone());
                }
                Err(_) => {
                    return Ok(Err(format!(
                        "environment variable `{var}` (for header `{header}`) is not set — \
                         export it and resume; nothing was sent"
                    )))
                }
            }
        }
    }

    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_millis(timeout_ms))
        .build();
    let mut req = match tool {
        "http.get" => agent.get(url),
        "http.post" => agent.post(url),
        other => return Ok(Err(format!("`{other}` is not an http tool"))),
    };
    if let Some(obj) = input.get("headers").and_then(|v| v.as_object()) {
        for (k, v) in obj {
            req = req.set(&header_wire_name(k), v.as_str().unwrap_or_default());
        }
    }
    for (k, v) in &secret_headers {
        req = req.set(k, v);
    }
    if tool == "http.post" {
        // True exactly-once across the receipt-commit crash window, for APIs
        // that honor it (Stripe-class): the key is stable across retries.
        req = req.set("Idempotency-Key", key);
    }

    let start = Instant::now();
    let result = if tool == "http.post" {
        let body = input.get("body").and_then(|v| v.as_str()).unwrap_or("");
        req.send_string(body)
    } else {
        req.call()
    };
    let duration_ms = start.elapsed().as_millis() as u64;

    // A non-2xx status IS a response — the exchange completed and the plan
    // branches on it. Only a transport failure is a tool error.
    let response = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(t)) => {
            return Ok(Err(format!("transport failure: {t}")));
        }
    };

    let status = response.status();
    let mut headers = Map::new();
    for name in response.headers_names() {
        let norm = name.to_ascii_lowercase();
        // Never record cookies: a Set-Cookie is a credential the server minted.
        if norm == "set-cookie" {
            continue;
        }
        if let Some(v) = response.header(&name) {
            headers.insert(norm.replace('-', "_"), json!(v));
        }
    }

    // Stream the body to the artifact file, then inline the first chunk.
    let artifact_dir = store::artifacts_dir(repo, run_id);
    std::fs::create_dir_all(&artifact_dir)?;
    let body_path = artifact_dir.join(format!("{key}.body"));
    let mut reader = response.into_reader();
    let mut file = std::fs::File::create(&body_path)?;
    let mut body_bytes: u64 = 0;
    let mut inline = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => return Ok(Err(format!("reading response body failed: {e}"))),
        };
        file.write_all(&buf[..n])?;
        if inline.len() < BODY_INLINE_CAP {
            let take = n.min(BODY_INLINE_CAP - inline.len());
            inline.extend_from_slice(&buf[..take]);
        }
        body_bytes += n as u64;
    }
    file.sync_all()?;

    Ok(Ok(json!({
        "status": status,
        "ok": (200..300).contains(&status),
        "headers": Value::Object(headers),
        "body": String::from_utf8_lossy(&inline).into_owned(),
        "body_bytes": body_bytes,
        "body_artifact": rel(repo, &body_path),
        "duration_ms": duration_ms,
        "headers_env_resolved": resolved_names,
    })))
}

/// `content_type` (field-access-friendly snake_case in plan source) becomes
/// `Content-Type` on the wire; an already-dashed name passes through.
fn header_wire_name(name: &str) -> String {
    name.replace('_', "-")
}

fn rel(repo: &Path, p: &Path) -> String {
    p.strip_prefix(repo)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::{AllowSpec, BudgetSpec};

    fn spec(get: &[&str], post: &[&str]) -> GoalSpec {
        GoalSpec {
            name: "G".into(),
            success: None,
            budget: BudgetSpec::default(),
            allow: AllowSpec {
                http_get: get.iter().map(|s| s.to_string()).collect(),
                http_post: post.iter().map(|s| s.to_string()).collect(),
                ..AllowSpec::default()
            },
            require: vec![],
        }
    }

    #[test]
    fn url_parsing_is_strict() {
        let u = parse_url("https://api.example.com/v1/refunds?x=1").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "api.example.com");
        assert_eq!(u.port, None);
        assert_eq!(u.path, "/v1/refunds?x=1");
        assert_eq!(u.canonical(), "https://api.example.com/v1/refunds?x=1");

        let p = parse_url("http://127.0.0.1:8080").unwrap();
        assert_eq!(p.port, Some(8080));
        assert_eq!(p.path, "/", "a bare host canonicalizes with a root path");

        assert!(parse_url("ftp://x.test/").is_err(), "non-http scheme");
        assert!(parse_url("api.example.com/x").is_err(), "missing scheme");
        assert!(parse_url("https://user@host.test/").is_err(), "userinfo");
        assert!(parse_url("https://host.test/x#frag").is_err(), "fragment");
        assert!(parse_url("https://пример.test/").is_err(), "non-ascii host");
    }

    #[test]
    fn globs_are_default_deny_and_segment_aware() {
        let s = spec(&["https://api.example.com/**"], &[]);
        assert!(url_allowed(&s, "http.get", "https://api.example.com/v1/x").is_ok());
        assert!(url_allowed(&s, "http.get", "https://api.example.com/").is_ok());
        // A lookalike host must not match.
        assert!(url_allowed(&s, "http.get", "https://api.example.com.evil.test/").is_err());
        // The grant is per-method.
        assert!(url_allowed(&s, "http.post", "https://api.example.com/v1/x").is_err());
        // No grants at all → nothing allowed.
        let none = spec(&[], &[]);
        assert!(url_allowed(&none, "http.get", "https://api.example.com/").is_err());
    }

    #[test]
    fn plaintext_http_needs_an_explicit_http_glob() {
        // An https glob never grants an http URL…
        let s = spec(&["https://api.example.com/**"], &[]);
        assert!(url_allowed(&s, "http.get", "http://api.example.com/x").is_err());
        // …but an explicit http:// glob (localhost dev) does.
        let dev = spec(&["http://127.0.0.1:9000/**"], &[]);
        assert!(url_allowed(&dev, "http.get", "http://127.0.0.1:9000/x").is_ok());
        // Port is part of the identity: another port is another server.
        assert!(url_allowed(&dev, "http.get", "http://127.0.0.1:9001/x").is_err());
    }

    #[test]
    fn literal_credential_headers_are_rejected() {
        let bad =
            json!({ "url": "https://x.test/", "headers": { "authorization": "Bearer sk_live" } });
        let err = check_headers(&bad).unwrap_err();
        assert!(
            err.contains("headers_env"),
            "the error teaches the fix: {err}"
        );
        let bad2 = json!({ "url": "https://x.test/", "headers": { "Cookie": "session=1" } });
        assert!(check_headers(&bad2).is_err(), "case-insensitive");
        let ok = json!({ "url": "https://x.test/", "headers": { "content_type": "application/json" },
                         "headers_env": { "authorization": "MY_TOKEN" } });
        assert!(check_headers(&ok).is_ok());
    }

    #[test]
    fn header_names_go_to_wire_form() {
        assert_eq!(header_wire_name("content_type"), "content-type");
        assert_eq!(header_wire_name("x-api-version"), "x-api-version");
    }
}
