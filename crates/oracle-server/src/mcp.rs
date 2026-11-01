//! `POST /mcp` — the Oracle's MCP tools over HTTP, for a client that can only reach a URL.
//!
//! Streamable HTTP, stateless: every request carries the bearer, every response is one JSON
//! body. The tools are `oracle-mcp`'s, the same ones `ox mcp` serves over stdio. Here the door
//! is the server's own HTTP API, called back on the loopback with the caller's bearer, so a
//! tool sees exactly what that token may see and nothing else. Writes are signed with a key
//! this server holds for the seat (`$ORACLE_HOME/hosted/<handle>.key`), when the owner has
//! placed one there; a seat without a hosted key can read here and must write with `ox`.
//!
//! The old Oracle's tool names are listed at this door (a connector's habits were formed
//! there), so pointing a public `/mcp` door here changes nothing for its clients.

use crate::http::{now_ms, App};
use crate::store::{Principal, Scope};
use anyhow::{bail, Result};
use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use oracle_mcp::{Door, Written};
use oracle_proto::{keyfile, pubkey_hex, Envelope, Kind};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

/// The server as a door for one authenticated caller.
pub struct Loopback {
    base: String,
    token: String,
    /// The seat this caller reads as (docs/VIEW-AS.md), passed on with every call back to the server.
    view_as: Option<String>,
    who: Principal,
    hosted_key: Option<PathBuf>,
    agent: ureq::Agent,
}

impl Loopback {
    pub fn new(app: &App, token: String, who: Principal) -> Loopback {
        let hosted_key = app
            .hosted_keys
            .as_ref()
            .map(|d| d.join(format!("{}.key", who.handle)))
            .filter(|p| p.is_file());
        Loopback {
            base: app.self_url.clone(),
            token,
            view_as: None,
            who,
            hosted_key,
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(20))
                .build(),
        }
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let mut req = self
            .agent
            .request(method, &format!("{}{}", self.base, path))
            .set("Authorization", &format!("Bearer {}", self.token));
        if let Some(seat) = &self.view_as {
            req = req.set(crate::http::VIEW_AS, seat);
        }
        let resp = match body {
            Some(b) => req.send_json(b),
            None => req.call(),
        };
        match resp {
            Ok(r) => Ok(r.into_json()?),
            Err(ureq::Error::Status(code, r)) => {
                let detail = r
                    .into_json::<Value>()
                    .ok()
                    .and_then(|v| v["detail"].as_str().map(str::to_string))
                    .unwrap_or_default();
                bail!("server refused ({code}): {detail}")
            }
            Err(e) => bail!("Oracle unreachable ({e})"),
        }
    }
}

impl Door for Loopback {
    fn handle(&self) -> &str {
        &self.who.handle
    }
    fn get(&self, path: &str) -> Result<Value> {
        self.call("GET", path, None)
    }
    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.call("POST", path, Some(body))
    }
    fn get_bytes(&self, path: &str) -> Result<(String, Vec<u8>)> {
        let mut req = self
            .agent
            .request("GET", &format!("{}{}", self.base, path))
            .set("Authorization", &format!("Bearer {}", self.token));
        if let Some(seat) = &self.view_as {
            req = req.set(crate::http::VIEW_AS, seat);
        }
        match req.call() {
            Ok(r) => {
                let ctype = r.header("Content-Type").unwrap_or("application/octet-stream").to_string();
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut r.into_reader(), &mut bytes)?;
                if bytes.len() > 64 * 1024 * 1024 {
                    bail!("file larger than 64 MB");
                }
                Ok((ctype, bytes))
            }
            Err(ureq::Error::Status(code, _)) => bail!("server refused ({code})"),
            Err(e) => bail!("Oracle unreachable ({e})"),
        }
    }
    fn state(&self, room: &str) -> Result<Value> {
        oracle_mcp::state::read(self, room, None)
    }
    fn state_as_of(&self, room: &str, as_of_seq: i64) -> Result<Value> {
        oracle_mcp::state::read(self, room, Some(as_of_seq))
    }
    fn write(&self, kind: Kind, room: &str, text: &str, cargo: Option<String>, tags: Vec<String>, op_id: Option<String>) -> Result<Written> {
        if self.who.scope < Scope::Write {
            bail!("this token can read but not write");
        }
        let Some(path) = &self.hosted_key else {
            bail!(
                "NOT written — the server holds no signing key for '{}'. Writes over HTTP need a hosted key \
                 ($ORACLE_HOME/hosted/{}.key, placed by the owner); until then write with `ox`.",
                self.who.handle,
                self.who.handle
            );
        };
        let text = text.trim_end();
        if text.trim().is_empty() {
            bail!("nothing to say");
        }
        let sk = keyfile::load(path).map_err(|e| anyhow::anyhow!("hosted key unreadable: {e}"))?;
        let mut env = Envelope::sign(&sk, room, &self.who.handle, kind, text, cargo, tags, now_ms());
        // The caller's op_id is this write's identity here: the same seat and op_id sign the same client_op, which the store answers with the
        // original observation (`duplicate`) instead of storing a second one (Grok, 2026-10-17: two calls under one op_id were stored twice,
        // because this door dropped the op_id and every call got a random client_op). docs/mcp.md, "A retry is the same write".
        if let Some(op) = op_id.as_deref() {
            if !op_id_ok(op) {
                bail!("op_id must be 8-64 of [A-Za-z0-9_-], the same on every retry");
            }
            env.client_op = format!("mcp:{}:{op}", self.who.handle);
            let at = env.ts_ms;
            env = env.resign(&sk, at);
        }
        env.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
        let v = self.call("POST", "/observations", Some(&serde_json::to_value(&env)?))?;
        // A duplicate is the same write only if it says the same thing: an op_id reused for another write is refused, never answered with the
        // first write's receipt as if this one had landed.
        if let (Some(op), true) = (op_id.as_deref(), v["duplicate"] == json!(true)) {
            let first = self.get(&format!("/search?id={}", oracle_mcp::enc(v["id"].as_str().unwrap_or_default())))?;
            if first["content"].as_str() != Some(env.content.as_str()) || first["channel"].as_str() != Some(room) || first["kind"].as_str() != Some(kind.as_str()) {
                bail!("op_id {op} was already used for a different write ({}); use a new op_id for a new write", v["id"].as_str().unwrap_or("?"));
            }
        }
        Ok(Written::Stored(v))
    }
    fn identity(&self) -> String {
        let key = match &self.hosted_key {
            Some(p) => keyfile::load(p)
                .map(|k| format!("hosted key {}…", &pubkey_hex(&k)[..8]))
                .unwrap_or_else(|_| "hosted key unreadable".into()),
            None => "no hosted key (reads only here; write with ox)".into(),
        };
        format!("{key} · {} · via POST /mcp", self.base)
    }
}

/// A caller's operation id: 8-64 of [A-Za-z0-9_-] (the rule `ox` and the tool descriptions state).
fn op_id_ok(op: &str) -> bool {
    (8..=64).contains(&op.len()) && op.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// This process's boot: a new value on every start, carried in the session ids it issues.
/// True the first time a stale session id is seen (it is then told to start again); false after, so a client that
/// keeps sending it is served rather than locked out. Bounded: past 10,000 ids the set starts over.
fn told_stale(id: &str) -> bool {
    static TOLD: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    let mut told = TOLD.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if told.len() > 10_000 {
        told.clear();
    }
    told.insert(id.to_string())
}

fn boot_id() -> &'static str {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOOT.get_or_init(|| oracle_proto::sha_hex(format!("{:?}{}", std::time::SystemTime::now(), std::process::id()).as_bytes())[..8].to_string())
}

fn json_response(status: StatusCode, body: Value, session: Option<HeaderValue>) -> Response {
    let mut r = (status, axum::Json(body)).into_response();
    if let Some(s) = session {
        r.headers_mut().insert("mcp-session-id", s);
    }
    r
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// One HTTP request: a JSON-RPC message or a batch of them. Authentication first, so an
/// anonymous request learns nothing beyond 401. Notifications alone are acknowledged with 202.
pub async fn post(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let mut session = headers.get("mcp-session-id").cloned();
    let initializing = serde_json::from_slice::<Value>(&body).is_ok_and(|m| {
        m["method"] == "initialize" || m.as_array().is_some_and(|a| a.iter().any(|x| x["method"] == "initialize"))
    });
    // A session this server issued before its last start is over: 404 makes a client start a new session (MCP
    // streamable HTTP), and the new session lists the tools again, so a release reaches every seat without a manual
    // reconnect (Jobs, 2026-10-10: oracle_claim was live and the connector still did not list it). Said once per
    // session, and never to an initialize: Grok Bot's client re-initialized carrying the old id and was refused on
    // every call after a release (2026-10-10 13:20-13:40Z). Sessions hold no state here, so serving one is safe.
    let stale = session.as_ref().and_then(|v| v.to_str().ok()).is_some_and(|s| s.starts_with("oc-") && !s.starts_with(&format!("oc-{}-", boot_id())));
    if stale && !initializing && told_stale(session.as_ref().and_then(|v| v.to_str().ok()).unwrap_or("")) {
        return json_response(StatusCode::NOT_FOUND, rpc_error(Value::Null, -32001, "session ended with a server restart; start a new session (initialize)".into()), None);
    }
    let who = match crate::http::principal(&app, &headers, Scope::Read).await {
        Ok(w) => w,
        Err(e) => return e.into_response(),
    };
    let token = token.unwrap_or_default();
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                rpc_error(Value::Null, -32700, format!("parse error: {e}")),
                session,
            )
        }
    };
    let (batch, msgs) = match msg {
        Value::Array(a) => (true, a),
        other => (false, vec![other]),
    };
    // A new session for an initialize, and for a stale one being served: the reply carries this boot's id.
    if stale || msgs.iter().any(|m| m["method"] == "initialize") {
        session = HeaderValue::from_str(&format!("oc-{}-{}", boot_id(), oracle_proto::sha_hex(format!("{:?}{}", std::time::SystemTime::now(), std::process::id()).as_bytes()).get(..16).unwrap_or("0"))).ok();
    }
    let mut door = Loopback::new(&app, token, who);
    door.view_as = crate::http::view_as(&headers);
    let replies = tokio::task::spawn_blocking(move || {
        msgs.iter()
            .filter_map(|m| {
                if !m.is_object() {
                    return Some(rpc_error(Value::Null, -32600, "invalid request".into()));
                }
                oracle_mcp::handle(&door, m, true)
            })
            .collect::<Vec<Value>>()
    })
    .await
    .unwrap_or_else(|e| vec![rpc_error(Value::Null, -32603, format!("internal: {e}"))]);
    if replies.is_empty() {
        let mut r = StatusCode::ACCEPTED.into_response();
        if let Some(s) = session {
            r.headers_mut().insert("mcp-session-id", s);
        }
        return r;
    }
    let body = if batch { Value::Array(replies) } else { replies.into_iter().next().unwrap_or(Value::Null) };
    json_response(StatusCode::OK, body, session)
}

/// `GET /mcp`: this server pushes nothing; a client that opens a listening stream is told so.
pub async fn get() -> Response {
    let mut r = (StatusCode::METHOD_NOT_ALLOWED, "MCP over HTTP here is request/response: POST JSON-RPC to this URL").into_response();
    r.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
    r
}

/// `DELETE /mcp`: a client ending a session; there is none to end.
pub async fn delete() -> Response {
    StatusCode::OK.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::router;
    use crate::store::Store;
    use oracle_proto::{generate_key, KeyAction, KeyEvent};

    /// A real server on a loopback port: a fresh store, the owner, one seat with a hosted key.
    async fn serve() -> (String, String, String, PathBuf) {
        let dir = std::env::temp_dir().join(format!("oracle-mcp-test-{}", oracle_proto::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(dir.join("oracle.db").to_str().unwrap()).unwrap();
        let owner = generate_key();
        s.init_owner("blaze", &pubkey_hex(&owner), 1).unwrap();
        let rocky = generate_key();
        let ev = KeyEvent::sign(&owner, "blaze", KeyAction::Add, "rocky", &pubkey_hex(&rocky), 1);
        s.apply_key_event(&ev, 1).unwrap();
        let write_tok = s.mint_token("rocky", Scope::Write, "t", 1).unwrap();
        let kin = generate_key();
        let ev = KeyEvent::sign(&owner, "blaze", KeyAction::Add, "kin", &pubkey_hex(&kin), 1);
        s.apply_key_event(&ev, 1).unwrap();
        let read_tok = s.mint_token("kin", Scope::Read, "t", 1).unwrap();
        let hosted = dir.join("hosted");
        keyfile::create(&hosted.join("rocky.key"), &rocky).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let mut app = App::new(s);
        app.self_url = url.clone();
        app.hosted_keys = Some(hosted);
        tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });
        (url, write_tok, read_tok, dir)
    }

    fn rpc(url: &str, tok: &str, body: Value) -> (u16, Value) {
        let r = ureq::post(&format!("{url}/mcp/"))
            .set("Authorization", &format!("Bearer {tok}"))
            .set("Accept", "application/json, text/event-stream")
            .send_json(&body);
        match r {
            Ok(r) => (r.status(), r.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(c, r)) => (c, r.into_json().unwrap_or(Value::Null)),
            Err(e) => panic!("{e}"),
        }
    }
    fn text(v: &Value) -> String {
        v["result"]["content"][0]["text"].as_str().unwrap_or("").to_string()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_session_from_before_a_restart_is_ended_so_the_client_lists_the_tools_again() {
        let (url, tok, _r, _d) = serve().await;
        let call = |session: Option<&str>, body: Value| {
            let mut req = ureq::post(&format!("{url}/mcp/")).set("Authorization", &format!("Bearer {tok}")).set("Accept", "application/json, text/event-stream");
            if let Some(s) = session {
                req = req.set("Mcp-Session-Id", s);
            }
            match req.send_json(&body) {
                Ok(r) => (r.status(), r.header("mcp-session-id").map(str::to_owned)),
                Err(ureq::Error::Status(c, r)) => (c, r.header("mcp-session-id").map(str::to_owned)),
                Err(e) => panic!("{e}"),
            }
        };
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let (c, issued) = call(None, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}));
        let issued = issued.expect("initialize issues a session id");
        assert_eq!((c, issued.starts_with("oc-")), (200, true));
        assert_eq!(call(Some(&issued), list.clone()).0, 200, "this boot's session is served");
        assert_eq!(call(Some("oc-00000000-feedfacecafebeef"), list.clone()).0, 404, "a session from an earlier boot is ended");
        let (c, fresh) = call(Some("oc-00000000-feedfacecafebeef"), list.clone());
        assert_eq!(c, 200, "said once: a client that keeps the old id is served, never locked out");
        assert!(fresh.is_some_and(|f| f.starts_with("oc-") && f != "oc-00000000-feedfacecafebeef"), "and handed this boot's session");
        let init = json!({"jsonrpc": "2.0", "id": 9, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}});
        assert_eq!(call(Some("oc-00000000-0123456789abcdef"), init).0, 200, "an initialize carrying an old id starts a new session");
        // Controls: no session, or a session the client named itself, are served as before.
        assert_eq!(call(None, list.clone()).0, 200);
        assert_eq!(call(Some("client-chosen-session"), list).0, 200);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_http_door_serves_the_same_tools_signed_with_a_hosted_key() {
        let (url, tok, read_tok, _d) = serve().await;
        // No bearer: 401 and nothing learned.
        let r = ureq::post(&format!("{url}/mcp")).send_json(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}));
        assert!(matches!(r, Err(ureq::Error::Status(401, _))));
        // Both spellings of the path, initialize names the seat, legacy names are listed.
        let (c, v) = rpc(&url, &tok, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}));
        assert_eq!(c, 200);
        assert_eq!(v["result"]["serverInfo"]["title"], "Oracle · rocky");
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
        let (_, v) = rpc(&url, &tok, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
        let names: Vec<&str> = v["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        for n in ["oracle_say", "oracle_about", "room_say", "oracle_thread"] {
            assert!(names.contains(&n), "{n} listed at the HTTP door");
        }
        // A write is signed with the hosted key and lands as rocky.
        let (_, v) = rpc(&url, &tok, json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "room_say", "arguments": {"channel": "hustler", "content": "hello from a URL @kin", "tags": ["kind:note"]}}}));
        assert_eq!(v["result"]["isError"], false, "{v}");
        assert!(text(&v).starts_with("signed:ok  OBS-hustler-"), "{v}");
        let (_, v) = rpc(&url, &tok, json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "oracle_about", "arguments": {"room": "hustler", "terms": "hello URL"}}}));
        assert!(text(&v).contains("hello from a URL"), "{v}");
        // The read is what the token may read; the brief says how this door signs.
        let (_, v) = rpc(&url, &tok, json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "oracle_state", "arguments": {}}}));
        assert!(text(&v).contains("hosted key"), "{v}");
        assert!(text(&v).contains("via POST /mcp"));
        // A read-only token, or a seat without a hosted key, cannot write here — and is told why.
        let (_, v) = rpc(&url, &read_tok, json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "oracle_say", "arguments": {"room": "hustler", "text": "x"}}}));
        assert_eq!(v["result"]["isError"], true);
        assert!(text(&v).contains("read but not write"), "{v}");
        let (_, v) = rpc(&url, &read_tok, json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "oracle_read", "arguments": {"room": "hustler"}}}));
        assert!(text(&v).contains("hello from a URL"), "a reader reads: {v}");
        // Notifications alone → 202; a batch → an array; GET → 405 with Allow.
        let r = ureq::post(&format!("{url}/mcp")).set("Authorization", &format!("Bearer {tok}"))
            .send_json(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).unwrap();
        assert_eq!(r.status(), 202);
        let (_, v) = rpc(&url, &tok, json!([{"jsonrpc": "2.0", "id": 8, "method": "ping"}, {"jsonrpc": "2.0", "id": 9, "method": "ping"}]));
        assert_eq!(v.as_array().map(|a| a.len()), Some(2));
        let r = ureq::get(&format!("{url}/mcp")).set("Authorization", &format!("Bearer {tok}")).call();
        match r {
            Err(ureq::Error::Status(405, r)) => assert_eq!(r.header("allow"), Some("POST, DELETE")),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_seat_without_a_hosted_key_reads_but_is_sent_to_ox_for_writes() {
        let (url, tok, _r, d) = serve().await;
        std::fs::remove_file(d.join("hosted/rocky.key")).unwrap();
        let (_, v) = rpc(&url, &tok, json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "oracle_say", "arguments": {"room": "hustler", "text": "x"}}}));
        assert_eq!(v["result"]["isError"], true);
        assert!(text(&v).contains("no signing key for 'rocky'"), "{v}");
        assert!(text(&v).contains("hosted/rocky.key"));
        let (_, v) = rpc(&url, &tok, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "oracle_task", "arguments": {}}}));
        assert_eq!(text(&v), "no state heads");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retry_under_one_op_id_is_one_observation_and_a_reused_op_id_is_refused() {
        let (url, tok, _r, _d) = serve().await;
        let call = |id: u64, args: Value| rpc(&url, &tok, json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "oracle_say", "arguments": args}}));
        let count = || {
            let (_, v) = rpc(&url, &tok, json!({"jsonrpc": "2.0", "id": 90, "method": "tools/call", "params": {"name": "oracle_read", "arguments": {"room": "hustler"}}}));
            text(&v).matches("retry me once").count()
        };
        let args = json!({"room": "hustler", "text": "retry me once", "op_id": "grok-retry-0001"});
        let (_, first) = call(1, args.clone());
        assert_eq!(first["result"]["isError"], false, "{first}");
        let (_, again) = call(2, args.clone());
        assert_eq!(again["result"]["isError"], false, "a retry is answered, not refused: {again}");
        let id = |v: &Value| text(v).split_whitespace().nth(1).unwrap_or_default().to_string();
        assert_eq!(id(&first), id(&again), "the retry returns the original observation");
        assert_eq!(count(), 1, "one observation for one op_id");
        // The same op_id for another text is refused, and nothing new is stored.
        let (_, other) = call(3, json!({"room": "hustler", "text": "something else entirely", "op_id": "grok-retry-0001"}));
        assert_eq!(other["result"]["isError"], true, "{other}");
        assert!(text(&other).contains("already used for a different write"), "{other}");
        // A malformed op_id is refused before anything is signed.
        let (_, bad) = call(4, json!({"room": "hustler", "text": "retry me once", "op_id": "x"}));
        assert_eq!(bad["result"]["isError"], true, "{bad}");
        // Control: without an op_id two calls are two writes, as before.
        let plain = json!({"room": "hustler", "text": "retry me once"});
        call(5, plain.clone());
        call(6, plain);
        assert_eq!(count(), 3, "no op_id: every call is a new write");
    }
}
