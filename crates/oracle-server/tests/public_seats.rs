//! Public seats and limits (publicseats.rs; plan.oracle.public-oracle S1 + S2). A stranger mints a seat with a key they
//! hold, over HTTP, and writes in the public write rooms only, within the limits; the feature is dark until the owner names
//! the rooms. A real server on a loopback port, real signed writes.

use ed25519_dalek::{Signer, SigningKey};
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::http::{router, App};
use oracle_server::publicseats::{challenge_message, Limits};
use oracle_server::store::{PublicConfig, Store};
use serde_json::{json, Value};
use std::sync::Arc;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

struct Srv {
    base: String,
    store: Arc<Store>,
}

fn start(rooms: &[&str], limits: Limits) -> Srv {
    let store = Store::memory().unwrap();
    store.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), 2), 2).unwrap();
    store.set_public_config(PublicConfig { write_rooms: rooms.iter().map(|r| r.to_string()).collect(), limits });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut app = App::new(store);
    app.self_url = base.clone();
    let store = app.store.clone();
    let served = app.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(l, router(served)).await.unwrap();
        })
    });
    Srv { base, store }
}

fn call(req: ureq::Request, body: Option<Value>) -> (u16, Value, Option<String>) {
    let r = match body {
        Some(b) => req.send_json(b),
        None => req.call(),
    };
    match r {
        Ok(r) => { let ra = r.header("retry-after").map(str::to_string); (r.status(), r.into_json().unwrap_or(json!({})), ra) }
        Err(ureq::Error::Status(c, r)) => { let ra = r.header("retry-after").map(str::to_string); (c, r.into_json().unwrap_or(json!({})), ra) }
        Err(e) => panic!("{e}"),
    }
}

impl Srv {
    fn challenge(&self) -> (u16, Value) {
        let (c, v, _) = call(ureq::get(&format!("{}/public/register/challenge", self.base)), None);
        (c, v)
    }
    fn register_with(&self, key: &SigningKey, name: &str, ip: &str, tamper: bool) -> (u16, Value, Option<String>) {
        let (_, ch) = self.challenge();
        let nonce = ch["nonce"].as_str().unwrap_or("").to_string();
        let pk = pubkey_hex(key);
        let mut msg = challenge_message(&nonce, &pk);
        if tamper {
            msg.push(b'!');
        }
        let sig = hex(&key.sign(&msg).to_bytes());
        call(ureq::post(&format!("{}/public/register", self.base)).set("X-Forwarded-For", ip),
             Some(json!({"pubkey": pk, "nonce": nonce, "signature": sig, "name": name})))
    }
    fn register(&self, key: &SigningKey, name: &str) -> (String, String) {
        let (code, v, _) = self.register_with(key, name, "198.51.100.1", false);
        assert_eq!(code, 201, "{v}");
        (v["seat"].as_str().unwrap().to_string(), v["token"].as_str().unwrap().to_string())
    }
    fn write_as(&self, token: &str, key: &SigningKey, seat: &str, room: &str, text: &str, at: i64) -> Result<String, String> {
        let who = self.store.authenticate(token).map_err(|e| e.to_string())?;
        let env = Envelope::sign(key, room, seat, Kind::Say, text, None, vec![], at);
        self.store.write(&who, &env, at).map(|r| r.id).map_err(|e| e.to_string())
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn roomy() -> Limits {
    Limits { seat_spacing_ms: 1, ..Limits::default() }
}

#[test]
fn dark_until_the_owner_names_rooms() {
    let s = start(&[], Limits::default());
    assert_eq!(s.challenge().0, 404, "no write rooms: no challenge");
    let (code, _, _) = s.register_with(&sk(9), "Ada", "198.51.100.1", false);
    assert_eq!(code, 404, "no write rooms: no registration");
}

#[test]
fn a_stranger_mints_a_seat_and_writes_only_in_the_public_rooms() {
    let s = start(&["eth", "riemann"], roomy());
    let key = sk(9);
    let (seat, token) = s.register(&key, "  Ada   Lovelace ");
    assert!(seat.starts_with("p-") && seat.len() == 18, "{seat}");
    let now = oracle_server::http::now_ms();
    assert!(s.write_as(&token, &key, &seat, "eth", "a check, from outside", now).is_ok());
    let other = s.write_as(&token, &key, &seat, "axxis", "into a studio room", now + 10).unwrap_err();
    assert!(other.contains("writes only in"), "{other}");
    // A registry seat is untouched by any of this.
    assert!(s.store.seat_rooms_for("rocky").unwrap().is_none());
    // The seat's envelope must be signed by its key: another key under the same token is refused.
    let forged = s.write_as(&token, &sk(10), &seat, "eth", "forged", now + 20).unwrap_err();
    assert!(forged.contains("signature"), "{forged}");
}

#[test]
fn a_private_room_stays_closed_to_public_seats() {
    let s = start(&["eth", "secret"], roomy());
    // The owner makes #secret private to rocky (privaterooms.rs), even though it is (wrongly) listed as a write room.
    let cargo = json!({"schema": "private-rooms/1", "rooms": {"secret": ["rocky"]}}).to_string();
    let env = Envelope::sign(&sk(1), "oracle", "blaze", Kind::Say, "[Private rooms] test", Some(cargo), vec!["schema:private-rooms/1".into()], 5);
    s.store.write(&oracle_server::store::Principal { handle: "blaze".into(), scope: oracle_server::store::Scope::Write }, &env, 5).unwrap();
    let key = sk(9);
    let (seat, token) = s.register(&key, "Ada");
    let e = s.write_as(&token, &key, &seat, "secret", "let me in", oracle_server::http::now_ms()).unwrap_err();
    assert!(e.contains("private"), "{e}");
}

#[test]
fn the_same_key_is_the_same_seat_and_old_tokens_retire() {
    let s = start(&["eth"], Limits { tokens_per_seat: 2, seat_spacing_ms: 1, ..Limits::default() });
    let key = sk(9);
    let (a, t1) = s.register(&key, "Ada");
    let (b, t2) = s.register(&key, "Ada L.");
    let (c, t3) = s.register(&key, "Ada L.");
    assert_eq!((a.as_str(), b.as_str()), (c.as_str(), c.as_str()), "one key, one seat");
    assert!(s.store.authenticate(&t1).is_err(), "the oldest token retires past the cap");
    assert!(s.store.authenticate(&t2).is_ok() && s.store.authenticate(&t3).is_ok());
}

#[test]
fn bad_signatures_and_spent_challenges_are_refused() {
    let s = start(&["eth"], roomy());
    let (code, v, _) = s.register_with(&sk(9), "Ada", "198.51.100.1", true);
    assert_eq!(code, 401, "{v}");
    // Replaying a challenge that was already spent fails, even with a good signature.
    let (_, ch) = s.challenge();
    let nonce = ch["nonce"].as_str().unwrap();
    let key = sk(9);
    let pk = pubkey_hex(&key);
    let body = json!({"pubkey": pk, "nonce": nonce, "signature": hex(&key.sign(&challenge_message(nonce, &pk)).to_bytes()), "name": "Ada"});
    let (first, _, _) = call(ureq::post(&format!("{}/public/register", s.base)), Some(body.clone()));
    let (again, _, _) = call(ureq::post(&format!("{}/public/register", s.base)), Some(body));
    assert_eq!((first, again), (201, 401));
    let (bad_name, _, _) = s.register_with(&sk(11), "x@y.z", "198.51.100.1", false);
    assert_eq!(bad_name, 422);
}

#[test]
fn writes_pass_their_limits_and_a_retry_is_not_counted_twice() {
    let s = start(&["eth"], Limits { seat_spacing_ms: 60_000, ..Limits::default() });
    let key = sk(9);
    let (seat, token) = s.register(&key, "Ada");
    let who = s.store.authenticate(&token).unwrap();
    let now = oracle_server::http::now_ms();
    let env = Envelope::sign(&key, "eth", &seat, Kind::Say, "first", None, vec![], now);
    let first = s.store.write(&who, &env, now).unwrap();
    // The same envelope again is the same write (idempotent), not a second one against the limit.
    let retry = s.store.write(&who, &env, now + 5).unwrap();
    assert_eq!(first.id, retry.id);
    assert!(retry.duplicate);
    let e = s.write_as(&token, &key, &seat, "eth", "second, too soon", now + 10).unwrap_err();
    assert!(e.contains("limited") && e.contains("one write at a time"), "{e}");
}

#[test]
fn registrations_are_limited_per_address_with_retry_after() {
    let s = start(&["eth"], Limits { register_per_ip_hour: 2, ..Limits::default() });
    assert_eq!(s.register_with(&sk(21), "A", "203.0.113.5", false).0, 201);
    assert_eq!(s.register_with(&sk(22), "B", "203.0.113.5", false).0, 201);
    let (code, v, retry) = s.register_with(&sk(23), "C", "203.0.113.5", false);
    assert_eq!(code, 429, "{v}");
    assert!(retry.and_then(|r| r.parse::<i64>().ok()).is_some_and(|r| r > 0 && r <= 3600));
    // Another address is its own bucket, and re-registering an existing key is not a new registration.
    assert_eq!(s.register_with(&sk(24), "D", "203.0.113.6", false).0, 201);
    assert_eq!(s.register_with(&sk(21), "A", "203.0.113.5", false).0, 201);
}

#[test]
fn a_revoked_seat_loses_its_tokens_and_cannot_come_back() {
    let s = start(&["eth"], roomy());
    let key = sk(9);
    let (seat, token) = s.register(&key, "Ada");
    assert!(s.store.revoke_public(&seat, oracle_server::http::now_ms()).unwrap());
    assert!(s.store.authenticate(&token).is_err());
    let (code, _, _) = s.register_with(&key, "Ada again", "198.51.100.9", false);
    assert_eq!(code, 403);
}

#[test]
fn a_public_seat_reads_only_its_public_rooms() {
    // Found live on 29 Oct 2026: with no owner seat-rooms table, a public seat's reads were unscoped, so it read every room.
    let s = start(&["eth"], roomy());
    let blaze = oracle_server::store::Principal { handle: "blaze".into(), scope: oracle_server::store::Scope::Write };
    for (room, text) in [("eth", "a public claim"), ("axxis", "studio internals")] {
        let env = Envelope::sign(&sk(1), room, "blaze", Kind::Say, text, None, vec![], 7);
        s.store.write(&blaze, &env, 7).unwrap();
    }
    let key = sk(9);
    let (_, token) = s.register(&key, "Ada");
    let get = |path: &str| call(ureq::get(&format!("{}{path}", s.base)).set("Authorization", &format!("Bearer {token}")), None);
    let (ok, v, _) = get("/rooms/eth/messages");
    assert_eq!(ok, 200, "{v}");
    assert!(v.to_string().contains("a public claim"));
    // The other doors a token opens: the MCP endpoint and the live stream must not carry another room either.
    let mcp = ureq::post(&format!("{}/mcp", s.base)).set("Authorization", &format!("Bearer {token}")).set("Accept", "application/json, text/event-stream")
        .send_json(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "oracle_read", "arguments": {"room": "axxis"}}}));
    let mcp_body = match mcp { Ok(r) => r.into_string().unwrap_or_default(), Err(ureq::Error::Status(_, r)) => r.into_string().unwrap_or_default(), Err(e) => panic!("{e}") };
    assert!(!mcp_body.contains("studio internals"), "/mcp leaked: {mcp_body}");
    // Positive control: the same call by a registry seat that may read #axxis does carry it, so the check above is not vacuous.
    let rocky = s.store.mint_token("rocky", oracle_server::store::Scope::Read, "control", 9).unwrap();
    let ctl = ureq::post(&format!("{}/mcp", s.base)).set("Authorization", &format!("Bearer {rocky}")).set("Accept", "application/json, text/event-stream")
        .send_json(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "oracle_read", "arguments": {"room": "axxis"}}}));
    let ctl_body = match ctl { Ok(r) => r.into_string().unwrap_or_default(), Err(ureq::Error::Status(_, r)) => r.into_string().unwrap_or_default(), Err(e) => panic!("{e}") };
    assert!(ctl_body.contains("studio internals"), "control: a reader of #axxis sees it through /mcp: {ctl_body}");
    let stream = ureq::get(&format!("{}/stream", s.base)).set("Authorization", &format!("Bearer {token}")).timeout(std::time::Duration::from_millis(800)).call();
    if let Ok(r) = stream {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut r.into_reader(), &mut buf);
        assert!(!buf.contains("studio internals"), "/stream leaked: {buf}");
    }
    for path in ["/rooms/axxis/messages", "/rooms/axxis/state", "/fold", "/search?q=internals", "/inbox", "/threads"] {
        let (code, v, _) = get(path);
        assert!(code == 403 || code == 404, "{path} gave {code}: {v}");
        assert!(!v.to_string().contains("studio internals"), "{path} leaked: {v}");
    }
}

#[test]
fn public_scope_holds_across_alternate_routes_and_view_as() {
    let s = start(&["eth"], roomy());
    let owner = oracle_server::store::Principal { handle: "blaze".into(), scope: oracle_server::store::Scope::Write };
    let public = Envelope::sign(&sk(1), "eth", "blaze", Kind::Say, "PUBLIC CONTROL", None, vec![], 71);
    s.store.write(&owner, &public, 71).unwrap();
    let private = Envelope::sign(&sk(1), "axxis", "blaze", Kind::Say, "PRIVATE ROUTE CANARY", Some("PRIVATE CARGO CANARY".into()), vec![], 72);
    let id = s.store.write(&owner, &private, 72).unwrap().id;
    let (_, token) = s.register(&sk(17), "Route probe");
    let get = |path: &str| call(ureq::get(&format!("{}{path}", s.base)).set("Authorization", &format!("Bearer {token}")), None);
    // Positive controls: this token reads its room; an authorized registry seat reads the actual private record.
    assert!(get("/rooms/eth/messages").1.to_string().contains("PUBLIC CONTROL"));
    let reader = s.store.mint_token("rocky", oracle_server::store::Scope::Read, "control", 73).unwrap();
    let (code, value, _) = call(ureq::get(&format!("{}/search?id={id}", s.base)).set("Authorization", &format!("Bearer {reader}")), None);
    assert_eq!(code, 200);
    assert!(value.to_string().contains("PRIVATE ROUTE CANARY"));
    for path in [
        format!("/search?id={id}"), format!("/observations/{id}/content"),
        format!("/observations/{id}/cargo"), format!("/threads/{id}"),
        "/ask?room=axxis".into(), "/public/search?q=PRIVATE".into(),
        "/public/room/axxis".into(), "/sync/log".into(), "/sync/keys".into(),
        "/files".into(), "/keys?history=true".into(), "/channels".into(),
        "/states".into(), "/graph".into(), "/asks?seat=blaze".into(),
    ] {
        let (code, value, _) = get(&path);
        assert!(matches!(code, 200 | 403 | 404), "unexpected {code} at {path}");
        let body = value.to_string();
        assert!(!body.contains("PRIVATE ROUTE CANARY") && !body.contains("PRIVATE CARGO CANARY"), "private data through {path}: {body}");
        if !path.starts_with("/search?id=") { assert_ne!(code, 200, "non-allowlisted route opened: {path}"); }
    }
    for path in ["/whoami", "/rooms/axxis/messages", "/rooms/axxis/view"] {
        let (code, value, _) = call(ureq::get(&format!("{}{path}",s.base))
            .set("Authorization", &format!("Bearer {token}"))
            .set("x-oracle-view-as", "blaze"), None);
        assert!(matches!(code, 401 | 403), "public seat impersonated owner through {path}: {code} {value}");
    }
}
