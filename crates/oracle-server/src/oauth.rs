//! OAuth for the connectors, ported from the Oracle before this one (`tree_serve.py`, `/oauth/*` and `/.well-known/*`), so a connector that was
//! authorised there keeps its URL and its tokens and never reconnects.
//!
//! What is kept exactly: the discovery documents, the dynamic client registration reply, the connect-code form and its checks, PKCE S256, the code TTL
//! (600 s), the access token (`ocg_<b64url json>.<b64url hmac-sha256>`, keys sorted, compact, claims `iss` `aud` `sub` `scope` `iat` `exp` (+90 d) `jti`
//! `handle`, signed with the same secret file `legacy.rs` verifies with), and refresh tokens (`refresh_…`, the same grant).
//!
//! What is stricter or new: clients, codes and refresh tokens live in SQLite (the old server kept codes and refresh tokens in memory), a code is
//! deleted in the statement that reads it (so a replay finds nothing), `oracle:admin` is never granted, and the seat must be active in the key registry.
//! `/oauth/revoke` appends the token's `jti` to the file `legacy.rs` reads (`ORACLE_LEGACY_REVOKED_FILE`), so a revocation is immediate and durable.
//!
//! Secrets are read from files this process alone reads, never from the environment or the request log:
//! `ORACLE_LEGACY_OAUTH_SECRET_FILE` (the signing secret), `ORACLE_OAUTH_CONNECT_CODE_FILE` (the code typed on the form; without it `/oauth/authorize`
//! answers 503 `server_not_configured`), `ORACLE_OAUTH_ADMIN_TOKEN_FILE` (who may revoke someone else's token). `ORACLE_PUBLIC_ORIGIN` is the issuer.

use crate::http::{now_ms, App};
use crate::store::{Result, Store};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub const CODE_TTL_S: i64 = 600;
pub const ACCESS_TTL_S: i64 = 86_400 * 90;
const AUDIENCE: &str = "oracle-mcp";
/// Requestable scopes, lowest first. `oracle:mesh` and the legacy `mcp` are never requested through this flow.
const SCOPES: [&str; 3] = ["oracle:read", "oracle:write", "oracle:admin"];
const ADMIN: &str = "oracle:admin";
/// What the metadata documents advertise (the old server's list, `mcp` included).
const SCOPES_SUPPORTED: [&str; 4] = ["oracle:read", "oracle:write", "oracle:admin", "mcp"];
const DESCRIPTIONS: [(&str, &str, &str); 3] = [
    ("oracle:read", "Read knowledge", "Search, load context, read observations, stats, and health. No changes."),
    ("oracle:write", "Write knowledge", "Add observations, commit, synthesize, run web search/fetch. Cannot delete."),
    ("oracle:admin", "Administer", "Delete and overwrite observations, run maintenance. Full destructive control."),
];

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS oauth_clients (
  client_id  TEXT PRIMARY KEY,
  record     TEXT NOT NULL,
  created_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS oauth_codes (
  code       TEXT PRIMARY KEY,
  record     TEXT NOT NULL,
  expires_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS oauth_refresh (
  token      TEXT PRIMARY KEY,
  client_id  TEXT NOT NULL,
  handle     TEXT NOT NULL,
  scope      TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  revoked_ms INTEGER
) WITHOUT ROWID;
"#;

/// Configuration, read once. Any part may be absent; each route says what it does without it.
#[derive(Default)]
pub struct OAuth {
    secret: Option<Vec<u8>>,
    connect_code: Option<String>,
    admin_token: Option<String>,
    origin: String,
    revoked: Option<PathBuf>,
}

impl OAuth {
    pub fn from_env() -> OAuth {
        let var = |v: &str| std::env::var(v).ok().filter(|p| !p.trim().is_empty());
        let read = |v: &str| {
            let path = var(v)?;
            match std::fs::read_to_string(&path) {
                Ok(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
                Ok(_) => {
                    eprintln!("oauth: {path} is empty");
                    None
                }
                Err(e) => {
                    eprintln!("oauth: cannot read {path}: {e}");
                    None
                }
            }
        };
        OAuth {
            secret: read("ORACLE_LEGACY_OAUTH_SECRET_FILE").map(String::into_bytes),
            connect_code: read("ORACLE_OAUTH_CONNECT_CODE_FILE"),
            admin_token: read("ORACLE_OAUTH_ADMIN_TOKEN_FILE"),
            origin: var("ORACLE_PUBLIC_ORIGIN").unwrap_or_else(|| "http://127.0.0.1:8900".into()),
            revoked: var("ORACLE_LEGACY_REVOKED_FILE").map(PathBuf::from),
        }
    }

    pub fn new(secret: &str, connect_code: Option<&str>, admin_token: Option<&str>, origin: &str, revoked: Option<&std::path::Path>) -> OAuth {
        OAuth {
            secret: Some(secret.as_bytes().to_vec()),
            connect_code: connect_code.map(str::to_owned),
            admin_token: admin_token.map(str::to_owned),
            origin: origin.to_owned(),
            revoked: revoked.map(std::path::Path::to_path_buf),
        }
    }

    /// The token's `iss`: the public origin's host (and port), so a deployment names itself in ORACLE_PUBLIC_ORIGIN
    /// and the code names no host. Verification never reads `iss` (legacy.rs checks the HMAC, `aud` and expiry).
    pub fn issuer(&self) -> &str {
        let o = self.origin.split_once("://").map_or(self.origin.as_str(), |(_, rest)| rest);
        o.split('/').next().unwrap_or(o)
    }

    /// `_issue_oauth_access_token`: keys sorted (a `serde_json` map is a `BTreeMap`), compact, HMAC-SHA256 over the base64url body.
    pub fn mint(&self, client_id: &str, scope: &str, handle: &str, now_s: i64) -> Option<String> {
        let secret = self.secret.as_ref()?;
        let scope = if scope.trim().is_empty() { "oracle:read oracle:write" } else { scope.trim() };
        let mut p = Map::new();
        p.insert("iss".into(), json!(self.issuer()));
        p.insert("aud".into(), json!(AUDIENCE));
        p.insert("sub".into(), json!(if client_id.is_empty() { "chatgpt" } else { client_id }));
        p.insert("scope".into(), json!(scope));
        p.insert("iat".into(), json!(now_s));
        p.insert("exp".into(), json!(now_s + ACCESS_TTL_S));
        p.insert("jti".into(), json!(b64u(&random(16))));
        let h = normalise_handle(handle);
        if !h.is_empty() {
            p.insert("handle".into(), json!(h));
        }
        let body = b64u(serde_json::to_string(&Value::Object(p)).ok()?.as_bytes());
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
        mac.update(body.as_bytes());
        Some(format!("ocg_{body}.{}", b64u(&mac.finalize().into_bytes())))
    }

    /// Signature, audience and expiry of an `ocg_` token: its payload, else nothing.
    fn verify(&self, token: &str, now_s: i64) -> Option<Value> {
        let secret = self.secret.as_ref()?;
        let (body, sig) = token.strip_prefix("ocg_")?.split_once('.')?;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
        mac.update(body.as_bytes());
        mac.verify_slice(&crate::legacy::b64url_decode(sig)?).ok()?;
        let p: Value = serde_json::from_slice(&crate::legacy::b64url_decode(body)?).ok()?;
        (p["aud"] == AUDIENCE && p["exp"].as_i64().unwrap_or(0) > now_s).then_some(p)
    }
}

// ── small helpers ────────────────────────────────────────────────────────────────────────────────────────────────

fn random(n: usize) -> Vec<u8> {
    hex::decode(oracle_proto::random_hex(n)).unwrap_or_default()
}

pub fn b64u_decode(s: &str) -> Option<Vec<u8>> {
    crate::legacy::b64url_decode(s)
}

pub fn b64u(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len() * 4 / 3 + 3);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// `re.sub(r"[^a-z0-9_-]", "-", handle.strip().lower())`
fn normalise_handle(h: &str) -> String {
    h.trim().to_lowercase().chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' { c } else { '-' }).collect()
}

/// Python's `html.escape(s, quote=True)`.
use crate::naming::esc;

/// Python's `urllib.parse.quote_plus` (what `urlencode` uses).
fn quote_plus(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => o.push(b as char),
            b' ' => o.push('+'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

fn unquote_plus(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if b.get(i + 2).is_some() && (b[i + 1] as char).is_ascii_hexdigit() && (b[i + 2] as char).is_ascii_hexdigit() => {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(b'%'));
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `application/x-www-form-urlencoded` (or a query string), in order, every pair kept.
fn form_pairs(s: &str) -> Vec<(String, String)> {
    s.split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (unquote_plus(k), unquote_plus(v))
    }).collect()
}

/// Python's `dict(pairs)` over a query: the last value wins, the key keeps its first place.
fn last_wins(pairs: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in pairs {
        match out.iter_mut().find(|(ok, _)| ok == k) {
            Some(slot) => slot.1 = v.clone(),
            None => out.push((k.clone(), v.clone())),
        }
    }
    out
}

fn get<'a>(p: &'a [(String, String)], k: &str) -> &'a str {
    p.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.as_str()).unwrap_or("")
}

fn cors(mut r: Response) -> Response {
    r.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    r
}

fn json_reply(status: StatusCode, v: Value) -> Response {
    cors((status, axum::Json(v)).into_response())
}

fn oauth_err(status: StatusCode, error: &str, detail: &str) -> Response {
    if detail.is_empty() {
        json_reply(status, json!({ "error": error }))
    } else {
        json_reply(status, json!({ "error": error, "detail": detail }))
    }
}

fn redirect(to: &str) -> Response {
    let mut r = StatusCode::FOUND.into_response();
    if let Ok(v) = HeaderValue::from_str(to) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    r
}

/// Run a store call off the executor. A database failure is a 500 that says nothing more.
#[allow(clippy::result_large_err)] // the error is the axum Response itself
async fn db<T: Send + 'static>(app: &App, f: impl FnOnce(&Store) -> Result<T> + Send + 'static) -> std::result::Result<T, Response> {
    let store = app.store.clone();
    match tokio::task::spawn_blocking(move || f(&store)).await {
        Ok(Ok(v)) => Ok(v),
        _ => Err(oauth_err(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "")),
    }
}

// ── storage ──────────────────────────────────────────────────────────────────────────────────────────────────────

/// Store a client record exactly as registered (`client_secret` and all). Idempotent by `client_id`, so the ops import can be run twice.
pub fn put_client(c: &Connection, record: &Value, created_ms: i64) -> Result<()> {
    let id = record["client_id"].as_str().unwrap_or_default();
    c.execute(
        "INSERT INTO oauth_clients(client_id,record,created_ms) VALUES(?1,?2,?3) ON CONFLICT(client_id) DO UPDATE SET record=excluded.record",
        params![id, record.to_string(), created_ms],
    )?;
    Ok(())
}

pub fn client(c: &Connection, id: &str) -> Result<Option<Value>> {
    let j: Option<String> = c.query_row("SELECT record FROM oauth_clients WHERE client_id=?1", [id], |r| r.get(0)).optional()?;
    Ok(j.and_then(|j| serde_json::from_str(&j).ok()))
}

// ── discovery ────────────────────────────────────────────────────────────────────────────────────────────────────

pub async fn authorization_server(State(app): State<App>) -> Response {
    let o = &app.oauth.origin;
    json_reply(
        StatusCode::OK,
        json!({
            "issuer": o,
            "authorization_endpoint": format!("{o}/oauth/authorize"),
            "token_endpoint": format!("{o}/oauth/token"),
            "registration_endpoint": format!("{o}/oauth/register"),
            "scopes_supported": SCOPES_SUPPORTED,
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "token_endpoint_auth_methods_supported": ["none"],
            "code_challenge_methods_supported": ["S256"],
        }),
    )
}

/// RFC 9728. The path-specific document's `resource` is `origin/path/` and must not be collapsed to the origin: strict clients stop at a bare 401 otherwise.
pub async fn protected_resource(State(app): State<App>, uri: Uri) -> Response {
    let o = &app.oauth.origin;
    let path = uri.path().strip_prefix("/.well-known/oauth-protected-resource").unwrap_or("").trim_matches('/');
    let resource = if path.is_empty() { format!("{o}/") } else { format!("{o}/{path}/") };
    json_reply(
        StatusCode::OK,
        json!({
            "resource": resource,
            "authorization_servers": [o],
            "bearer_methods_supported": ["header"],
            "scopes_supported": SCOPES_SUPPORTED,
        }),
    )
}

// ── registration ─────────────────────────────────────────────────────────────────────────────────────────────────

/// RFC 7591. Anything is accepted; the reply carries a `client_` id and a STRING `secret_` (a validator in one connector insists on it), never checked at the token
/// endpoint: the flow is PKCE-only.
pub async fn register(State(app): State<App>, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
    let now = now_ms() / 1000;
    let record = json!({
        "client_id": format!("client_{}", b64u(&random(12))),
        "client_secret": format!("secret_{}", b64u(&random(18))),
        "client_secret_expires_at": 0,
        "client_name": body.get("client_name").cloned().unwrap_or_else(|| json!("anonymous")),
        "redirect_uris": body.get("redirect_uris").cloned().unwrap_or_else(|| json!([])),
        "grant_types": body.get("grant_types").cloned().unwrap_or_else(|| json!(["authorization_code"])),
        "response_types": body.get("response_types").cloned().unwrap_or_else(|| json!(["code"])),
        "token_endpoint_auth_method": "none",
        "scope": "mcp",
        "client_id_issued_at": now,
    });
    let stored = record.clone();
    match db(&app, move |s| s.with_conn(|c| put_client(c, &stored, now * 1000))).await {
        Ok(()) => json_reply(StatusCode::CREATED, record),
        Err(e) => e,
    }
}

// ── authorize ────────────────────────────────────────────────────────────────────────────────────────────────────

/// The scopes asked for, in rank order; nothing (or only the legacy `mcp`) means read and write.
fn requested(p: &[(String, String)]) -> Vec<&'static str> {
    let asked: Vec<&str> = get(p, "scope").split_whitespace().collect();
    let known: Vec<&'static str> = SCOPES.iter().copied().filter(|s| asked.contains(s)).collect();
    if known.is_empty() { vec!["oracle:read", "oracle:write"] } else { known }
}

fn consent_form(p: &[(String, String)], error: &str, client_name: &str) -> Response {
    let hidden: Vec<String> = p
        .iter()
        .filter(|(k, _)| !["oracle_code", "grant_scope", "decision"].contains(&k.as_str()))
        .map(|(k, v)| format!("<input type=\"hidden\" name=\"{}\" value=\"{}\">", esc(k), esc(v)))
        .collect();
    let err = if error.is_empty() { String::new() } else { format!("<p class=\"err\">{}</p>", esc(error)) };
    let rows: String = requested(p)
        .iter()
        .map(|s| {
            let (_, label, blurb) = DESCRIPTIONS.iter().find(|d| d.0 == *s).copied().unwrap_or((s, s, ""));
            let checked = if *s == "oracle:read" || *s == "oracle:write" { "checked" } else { "" };
            let danger = if *s == ADMIN { " class=\"danger\"" } else { "" };
            format!(
                "\n      <label class=\"scope\">\n        <input type=\"checkbox\" name=\"grant_scope\" value=\"{v}\" {checked}>\n        <span class=\"scope-body\"><span class=\"scope-name\"{danger}>{l}</span>\n        <span class=\"scope-blurb\">{b}</span>\n        <code>{v}</code></span>\n      </label>",
                v = esc(s), l = esc(label), b = esc(blurb)
            )
        })
        .collect();
    let client_line = if client_name.is_empty() {
        "<p class=\"client\">An application is requesting access to your Oracle.</p>".to_string()
    } else {
        format!("<p class=\"client\"><strong>{}</strong> is requesting access to your Oracle.</p>", esc(client_name))
    };
    let host = get(p, "redirect_uri").split_once("://").map(|(_, r)| r.split(['/', '?', '#']).next().unwrap_or("")).unwrap_or("");
    let redirect_line = if host.is_empty() { String::new() } else { format!("<p class=\"redirect\">Tokens will be delivered to <code>{}</code>.</p>", esc(host)) };
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>Authorize Oracle MCP</title>
  <style>
    body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 0; background: #0b0f14; color: #e5edf5; }}
    main {{ max-width: 480px; margin: 8vh auto; padding: 28px; }}
    h1 {{ font-size: 22px; margin: 0 0 4px; }}
    .client {{ color: #e5edf5; margin: 0 0 4px; }}
    .redirect {{ color: #8b97a3; font-size: 13px; margin: 0 0 20px; }}
    .err {{ color: #fecaca; background: #7f1d1d33; border: 1px solid #7f1d1d; padding: 8px 12px; border-radius: 6px; margin: 0 0 16px; }}
    fieldset {{ border: 1px solid #1f2a37; border-radius: 10px; padding: 8px 12px 14px; margin: 0 0 20px; }}
    legend {{ color: #8b97a3; font-size: 12px; text-transform: uppercase; letter-spacing: .06em; padding: 0 6px; }}
    label.scope {{ display: flex; gap: 12px; align-items: flex-start; padding: 12px 6px; border-bottom: 1px solid #141c26; cursor: pointer; }}
    label.scope:last-child {{ border-bottom: 0; }}
    label.scope input {{ margin-top: 3px; width: 18px; height: 18px; accent-color: #d7fb6e; }}
    .scope-body {{ display: flex; flex-direction: column; gap: 2px; }}
    .scope-name {{ font-weight: 700; color: #e5edf5; }}
    .scope-name.danger {{ color: #fca5a5; }}
    .scope-blurb {{ color: #b7c2cc; font-size: 13px; line-height: 1.4; }}
    code {{ color: #8b97a3; font-size: 11px; }}
    label.code {{ display: block; margin: 8px 0 6px; color: #b7c2cc; }}
    input[type=password], input[type=text] {{ box-sizing: border-box; width: 100%; padding: 12px; border: 1px solid #334155; border-radius: 6px; background: #111827; color: #fff; }}
    .actions {{ display: flex; gap: 10px; margin-top: 18px; }}
    button {{ padding: 11px 16px; border: 0; border-radius: 6px; font-weight: 700; cursor: pointer; }}
    button.approve {{ background: #d7fb6e; color: #111827; }}
    button.deny {{ background: transparent; color: #8b97a3; border: 1px solid #334155; }}
  </style>
</head>
<body>
  <main>
    <h1>Authorize Oracle MCP</h1>
    {client_line}
    {redirect_line}
    {err}
    <form method="post" action="/oauth/authorize">
      {hidden}
      <fieldset>
        <legend>Grant these permissions</legend>
        {rows}
      </fieldset>
      <label class="code" for="seat_handle">Oracle seat</label>
      <input id="seat_handle" name="seat_handle" type="text" autocomplete="username" placeholder="e.g. kin or emmy" value="{seat}">
      <label class="code" for="oracle_code">Connect code</label>
      <input id="oracle_code" name="oracle_code" type="password" autocomplete="one-time-code" autofocus>
      <div class="actions">
        <button class="approve" type="submit" name="decision" value="approve">Authorize</button>
        <button class="deny" type="submit" name="decision" value="deny" formnovalidate>Cancel</button>
      </div>
    </form>
  </main>
</body>
</html>"#,
        hidden = hidden.join("\n"),
        seat = esc(get(p, "seat_handle")),
    );
    let mut r = html.into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    r
}

/// `GET|POST /oauth/authorize`: a browser approval gated by the connect code, ending in a redirect with a single-use code.
pub async fn authorize(State(app): State<App>, method: Method, uri: Uri, body: Bytes) -> Response {
    let (params, checked): (Vec<(String, String)>, Vec<String>) = if method == Method::POST {
        let pairs = form_pairs(&String::from_utf8_lossy(&body));
        let checked = pairs.iter().filter(|(k, v)| k == "grant_scope" && SCOPES.contains(&v.as_str())).map(|(_, v)| v.clone()).collect();
        (last_wins(&pairs), checked)
    } else {
        (last_wins(&form_pairs(uri.query().unwrap_or(""))), vec![])
    };
    let redirect_uri = get(&params, "redirect_uri").to_string();
    if redirect_uri.is_empty() {
        return oauth_err(StatusCode::BAD_REQUEST, "invalid_request", "redirect_uri required");
    }
    // The redirect target is untrusted until it matches a registered one exactly; nothing below redirects before this passes.
    let client_id = get(&params, "client_id").to_string();
    let cid = client_id.clone();
    let rec = match db(&app, move |s| s.with_conn(|c| client(c, &cid))).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    let client_name = rec.as_ref().and_then(|r| r["client_name"].as_str()).unwrap_or("").to_string();
    match &rec {
        None => return oauth_err(StatusCode::BAD_REQUEST, "invalid_client", "client_id not registered; call /oauth/register first"),
        Some(r) => {
            let uris = r["redirect_uris"].as_array().cloned().unwrap_or_default();
            if uris.is_empty() {
                return oauth_err(StatusCode::BAD_REQUEST, "invalid_request", "client has no registered redirect_uris; re-register with redirect_uris set");
            }
            if !uris.iter().any(|u| u.as_str() == Some(redirect_uri.as_str())) {
                return oauth_err(StatusCode::BAD_REQUEST, "invalid_request", "redirect_uri not registered for this client (exact string match required)");
            }
        }
    }
    let challenge = get(&params, "code_challenge").trim().to_string();
    if challenge.is_empty() {
        return oauth_err(StatusCode::BAD_REQUEST, "invalid_request", "code_challenge required (PKCE S256 mandatory per RFC 9700 / OAuth 2.1)");
    }
    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    let state = get(&params, "state").to_string();
    if get(&params, "decision") == "deny" {
        let mut q = "error=access_denied".to_string();
        if !state.is_empty() {
            q.push_str(&format!("&state={}", quote_plus(&state)));
        }
        return redirect(&format!("{redirect_uri}{sep}{q}"));
    }
    let Some(connect) = app.oauth.connect_code.clone() else {
        return json_reply(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "server_not_configured", "detail": "The connect code is not configured; set ORACLE_OAUTH_CONNECT_CODE_FILE to enable OAuth authorization."}),
        );
    };
    let supplied = get(&params, "oracle_code").trim().to_string();
    if supplied.is_empty() {
        return consent_form(&params, "", &client_name);
    }
    if !same(&supplied, &connect) {
        return consent_form(&params, "Invalid connect code.", &client_name);
    }
    let seat = normalise_handle(get(&params, "seat_handle"));
    if seat.is_empty() {
        return consent_form(&params, "Choose the enrolled Oracle seat for this connection.", &client_name);
    }
    let h = seat.clone();
    let enrolled = match db(&app, move |s| s.active_keys(&h)).await {
        Ok(k) => !k.is_empty(),
        Err(e) => return e,
    };
    if !enrolled {
        return consent_form(&params, &format!("Seat '{seat}' is not enrolled in Oracle."), &client_name);
    }
    // What was ticked, of what was asked, and never admin: a client that registered anonymously must not be able to raise itself by ticking a box.
    let asked = requested(&params);
    let granted: Vec<&str> = asked.into_iter().filter(|s| checked.iter().any(|c| c == s) && *s != ADMIN).collect();
    if granted.is_empty() {
        return consent_form(&params, "Select at least one permission to grant.", &client_name);
    }
    let code = format!("code_{}", b64u(&random(18)));
    let now = now_ms();
    let record = json!({"redirect_uri": redirect_uri, "client_id": client_id, "code_challenge": challenge, "scope": granted.join(" "), "handle": seat}).to_string();
    let c = code.clone();
    if let Err(e) = db(&app, move |s| {
        s.with_conn(|conn| {
            conn.execute("DELETE FROM oauth_codes WHERE expires_ms < ?1", [now])?;
            conn.execute("INSERT INTO oauth_codes(code,record,expires_ms) VALUES(?1,?2,?3)", params![c, record, now + CODE_TTL_S * 1000])?;
            Ok(())
        })
    })
    .await
    {
        return e;
    }
    let mut q = format!("code={}", quote_plus(&code));
    if !state.is_empty() {
        q.push_str(&format!("&state={}", quote_plus(&state)));
    }
    redirect(&format!("{redirect_uri}{sep}{q}"))
}

// ── token ────────────────────────────────────────────────────────────────────────────────────────────────────────

/// A form body, else a JSON object; either read as string fields.
fn fields(headers: &HeaderMap, body: &Bytes) -> Option<Vec<(String, String)>> {
    let form = || last_wins(&form_pairs(&String::from_utf8_lossy(body)));
    let json = || -> Option<Vec<(String, String)>> {
        let v: Value = serde_json::from_slice(body).ok()?;
        Some(v.as_object()?.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))).collect())
    };
    let is_form = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.contains("x-www-form-urlencoded"));
    let first = if is_form { Some(form()).filter(|f| !f.is_empty()) } else { json() };
    first.or_else(|| if is_form { json() } else { Some(form()).filter(|f| !f.is_empty()) })
}

fn token_reply(access: String, scope: &str, refresh: Option<String>) -> Response {
    let mut v = json!({"access_token": access, "token_type": "Bearer", "expires_in": ACCESS_TTL_S, "scope": scope});
    if let Some(r) = refresh {
        v["refresh_token"] = json!(r);
    }
    json_reply(StatusCode::OK, v)
}

/// `POST /oauth/token`: `authorization_code` (PKCE S256) and `refresh_token`.
pub async fn token(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(p) = fields(&headers, &body) else {
        return oauth_err(StatusCode::BAD_REQUEST, "invalid_request", "");
    };
    let now = now_ms();
    match get(&p, "grant_type") {
        "authorization_code" => {
            let code = get(&p, "code").to_string();
            if code.is_empty() {
                return oauth_err(StatusCode::BAD_REQUEST, "invalid_request", "code required");
            }
            // Read and delete in one statement: a replayed code finds nothing.
            let c = code.clone();
            let taken = match db(&app, move |s| {
                s.with_conn(|conn| Ok(conn.query_row("DELETE FROM oauth_codes WHERE code=?1 RETURNING record, expires_ms", [c], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).optional()?))
            })
            .await
            {
                Ok(t) => t,
                Err(e) => return e,
            };
            let Some((record, expires_ms)) = taken else {
                return oauth_err(StatusCode::BAD_REQUEST, "invalid_grant", "code unknown or expired");
            };
            if expires_ms < now {
                return oauth_err(StatusCode::BAD_REQUEST, "invalid_grant", "code expired");
            }
            let rec: Value = serde_json::from_str(&record).unwrap_or_default();
            let challenge = rec["code_challenge"].as_str().unwrap_or("");
            if !challenge.is_empty() {
                let verifier = get(&p, "code_verifier").trim();
                if verifier.is_empty() {
                    return oauth_err(StatusCode::BAD_REQUEST, "invalid_grant", "code_verifier required (PKCE S256)");
                }
                if !verifier.is_ascii() || !same(&b64u(&Sha256::digest(verifier.as_bytes())), challenge) {
                    return oauth_err(StatusCode::BAD_REQUEST, "invalid_grant", "code_verifier mismatch");
                }
            }
            let scope = rec["scope"].as_str().filter(|s| !s.is_empty()).unwrap_or("oracle:read oracle:write").to_string();
            let (client_id, handle) = (rec["client_id"].as_str().unwrap_or("").to_string(), rec["handle"].as_str().unwrap_or("").to_string());
            let Some(access) = app.oauth.mint(&client_id, &scope, &handle, now / 1000) else {
                return oauth_err(StatusCode::SERVICE_UNAVAILABLE, "server_not_configured", "OAuth token signing secret unavailable");
            };
            let refresh = format!("refresh_{}", b64u(&random(24)));
            let (r, cid, h, sc) = (refresh.clone(), client_id, handle, scope.clone());
            if let Err(e) = db(&app, move |s| {
                s.with_conn(|conn| {
                    conn.execute("INSERT INTO oauth_refresh(token,client_id,handle,scope,created_ms) VALUES(?1,?2,?3,?4,?5)", params![r, cid, h, sc, now])?;
                    Ok(())
                })
            })
            .await
            {
                return e;
            }
            token_reply(access, &scope, Some(refresh))
        }
        "refresh_token" => {
            let rtok = get(&p, "refresh_token").trim().to_string();
            if rtok.is_empty() {
                return oauth_err(StatusCode::BAD_REQUEST, "invalid_grant", "refresh_token required");
            }
            let rt = rtok.clone();
            let row = match db(&app, move |s| {
                s.with_conn(|conn| {
                    Ok(conn
                        .query_row("SELECT client_id, handle, scope FROM oauth_refresh WHERE token=?1 AND revoked_ms IS NULL", [rt], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
                        .optional()?)
                })
            })
            .await
            {
                Ok(r) => r,
                Err(e) => return e,
            };
            // An unknown refresh token is refused, never quietly answered with a default token.
            let Some((client_id, handle, scope)) = row else {
                return oauth_err(StatusCode::BAD_REQUEST, "invalid_grant", "refresh_token unknown or expired");
            };
            let client_id = if client_id.is_empty() { "chatgpt-refresh".to_string() } else { client_id };
            let scope = if scope.is_empty() { "oracle:read oracle:write".to_string() } else { scope };
            match app.oauth.mint(&client_id, &scope, &handle, now / 1000) {
                Some(access) => token_reply(access, &scope, None),
                None => oauth_err(StatusCode::SERVICE_UNAVAILABLE, "server_not_configured", "OAuth token signing secret unavailable"),
            }
        }
        _ => oauth_err(StatusCode::BAD_REQUEST, "unsupported_grant_type", ""),
    }
}

// ── revoke ───────────────────────────────────────────────────────────────────────────────────────────────────────

/// RFC 7009. The caller presents the token itself as the bearer, or the admin token; an unknown token is answered 200 so nobody can probe which exist.
pub async fn revoke(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(p) = fields(&headers, &body) else {
        return oauth_err(StatusCode::BAD_REQUEST, "invalid_request", "");
    };
    let token = get(&p, "token").trim().to_string();
    if token.is_empty() {
        return json_reply(StatusCode::OK, json!({}));
    }
    let header_of = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let bearer = header_of("authorization").strip_prefix("Bearer ").map(|b| b.trim().to_string()).unwrap_or_default();
    let admin = app.oauth.admin_token.as_deref().filter(|a| !a.is_empty());
    let is_admin = admin.is_some_and(|a| same(&header_of("x-admin-token"), a) || (!bearer.is_empty() && same(&bearer, a)));
    if !(is_admin || (!bearer.is_empty() && same(&bearer, &token))) {
        return oauth_err(StatusCode::UNAUTHORIZED, "unauthorized", "present the token to revoke as Bearer, or supply X-Admin-Token");
    }
    let mut reply = json!({"revoked": true});
    if token.starts_with("refresh_") {
        let (t, now) = (token.clone(), now_ms());
        if let Err(e) = db(&app, move |s| s.with_conn(|c| Ok(c.execute("UPDATE oauth_refresh SET revoked_ms=?2 WHERE token=?1 AND revoked_ms IS NULL", params![t, now])?))).await {
            return e;
        }
    }
    if let Some(jti) = app.oauth.verify(&token, now_ms() / 1000).and_then(|p| p["jti"].as_str().map(str::to_owned)).filter(|j| !j.is_empty() && !j.contains(['\n', '\r'])) {
        // Durable means it is on disk where `legacy.rs` reads it; a caller told false should retry.
        let durable = app.oauth.revoked.as_ref().is_some_and(|path| {
            use std::io::Write;
            std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| writeln!(f, "{jti}")).is_ok()
        });
        reply["durable"] = json!(durable);
    }
    json_reply(StatusCode::OK, reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_with_the_decoder_legacy_uses() {
        for n in 0..40 {
            let data: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
            assert_eq!(crate::legacy::b64url_decode(&b64u(&data)).unwrap(), data);
        }
        assert_eq!(b64u(b"\xfb\xff\xfe"), "-__-");
    }

    #[test]
    fn the_form_helpers_match_pythons() {
        assert_eq!(quote_plus("a b&c=d/é~_.-"), "a+b%26c%3Dd%2F%C3%A9~_.-");
        assert_eq!(unquote_plus("a+b%26c%3Dd%2F%C3%A9%zz%"), "a b&c=d/é%zz%");
        assert_eq!(esc("<a href=\"x\">'&"), "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;");
        assert_eq!(normalise_handle("  Emmy Core!"), "emmy-core-");
    }
}
