//! View as another seat (docs/VIEW-AS.md): the owner, or an admin token, reads as any seat through the header
//! `x-oracle-view-as`, with read scope only; a seat that is not the owner may not; every use is audited; the MCP door
//! passes it on, so a tool answers as that seat. A real server on a loopback port.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::http::{now_ms, router, App};
use oracle_server::store::{Scope, Store};
use serde_json::{json, Value};

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

struct Srv {
    base: String,
    app: App,
    blaze: String,
    rocky: String,
}

fn start() -> Srv {
    let store = Store::memory().unwrap();
    store.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), 2), 2).unwrap();
    // The AXXIS web door holds a read token for Blaze: the owner's seat, read scope.
    let blaze = store.mint_token("blaze", Scope::Read, "web door", 3).unwrap();
    let rocky = store.mint_token("rocky", Scope::Write, "rocky", 3).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut app = App::new(store);
    app.self_url = base.clone();
    let served = app.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(l, router(served)).await.unwrap();
        })
    });
    Srv { base, app, blaze, rocky }
}

impl Srv {
    fn get(&self, token: &str, path: &str, view: Option<&str>) -> (u16, Value) {
        let mut req = ureq::get(&format!("{}{path}", self.base)).set("Authorization", &format!("Bearer {token}"));
        if let Some(v) = view {
            req = req.set("x-oracle-view-as", v);
        }
        match req.call() {
            Ok(r) => (r.status(), r.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(c, r)) => (c, r.into_json().unwrap_or(Value::Null)),
            Err(e) => panic!("{e}"),
        }
    }
    fn post(&self, token: &str, path: &str, body: &Value, view: Option<&str>) -> (u16, Value) {
        let mut req = ureq::post(&format!("{}{path}", self.base)).set("Authorization", &format!("Bearer {token}")).set("Accept", "application/json, text/event-stream");
        if let Some(v) = view {
            req = req.set("x-oracle-view-as", v);
        }
        match req.send_json(body.clone()) {
            Ok(r) => (r.status(), r.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(c, r)) => (c, r.into_json().unwrap_or(Value::Null)),
            Err(e) => panic!("{e}"),
        }
    }
}

#[test]
fn the_owner_reads_as_a_seat_and_cannot_write_as_it() {
    let s = start();
    let (code, me) = s.get(&s.blaze, "/whoami", Some("rocky"));
    assert_eq!((code, me["handle"].as_str(), me["scope"].as_str()), (200, Some("rocky"), Some("read")), "{me}");
    // Without the header, the same token is Blaze.
    assert_eq!(s.get(&s.blaze, "/whoami", None).1["handle"], "blaze");
    // A write while viewing as rocky is refused, even with a validly signed envelope of rocky's.
    let at = now_ms();
    let env = Envelope::sign(&sk(2), "core", "rocky", Kind::Say, "not mine to send", None, vec![], at);
    let (code, _) = s.post(&s.blaze, "/observations", &serde_json::to_value(&env).unwrap(), Some("rocky"));
    assert_eq!(code, 403, "viewing as a seat is read only");
    // The use is audited.
    let audit = s.app.store.cross_seat_audit(10).unwrap();
    assert!(audit.iter().any(|a| a.caller == "blaze" && a.route == "view-as" && a.target == "rocky"));
}

#[test]
fn only_the_owner_or_an_admin_may_view_as_and_only_a_real_seat() {
    let s = start();
    // Negative control: rocky is a seat, not the owner, and its token is not admin.
    assert_eq!(s.get(&s.rocky, "/whoami", Some("blaze")).0, 403);
    // A seat that does not exist is refused, not invented.
    let (code, _) = s.get(&s.blaze, "/whoami", Some("nobody"));
    assert!((400..500).contains(&code), "{code}");
    // An admin token of a non-owner seat may.
    let admin = s.app.store.mint_token("rocky", Scope::Admin, "ops", 4).unwrap();
    assert_eq!(s.get(&admin, "/whoami", Some("blaze")).1["handle"], "blaze");
}

#[test]
fn an_mcp_tool_answers_as_the_seat_viewed() {
    let s = start();
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "oracle_brief", "arguments": {}}});
    let text = |v: &Value| v["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    let (code, as_rocky) = s.post(&s.blaze, "/mcp", &call, Some("rocky"));
    assert_eq!(code, 200, "{as_rocky}");
    assert!(text(&as_rocky).contains("ox brief · rocky"), "{}", text(&as_rocky));
    let (_, as_me) = s.post(&s.blaze, "/mcp", &call, None);
    assert!(text(&as_me).contains("ox brief · blaze"), "{}", text(&as_me));
    // A write tool through the door while viewing is refused with a reason.
    let say = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "oracle_say", "arguments": {"room": "core", "text": "hi"}}});
    let (_, r) = s.post(&s.blaze, "/mcp", &say, Some("rocky"));
    assert!(r["result"]["isError"] == json!(true) || r["error"].is_object(), "{r}");
}
