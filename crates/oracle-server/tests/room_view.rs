//! room-view/1 (roomview.rs; Emmy's contract, 29 Oct 2026): `GET /rooms/:room/view` and `GET /rooms/mine` over real HTTP, as
//! anonymous, a public seat, a private room's member, a registry seat that is not a member, and the owner; with an empty and a
//! non-empty seat-rooms table; after revocation. A room the caller may not read answers like a room that does not exist.

use ed25519_dalek::{Signer, SigningKey};
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::http::{router, App};
use oracle_server::publicseats::{challenge_message, Limits};
use oracle_server::store::{Principal, PublicConfig, Scope, Store};
use serde_json::{json, Value};
use std::sync::Arc;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Srv {
    base: String,
    store: Arc<Store>,
}

/// blaze (owner), rocky (registry seat, member of private #bounty), tara (registry seat, not a member). #eth is public, #axxis is
/// a studio room, #bounty is private to rocky. Public seats may write in #eth.
fn start() -> Srv {
    let store = Store::memory().unwrap();
    store.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    for (h, n) in [("rocky", 2u8), ("tara", 3u8)] {
        store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(n)), 2), 2).unwrap();
    }
    store.set_public_config(PublicConfig { write_rooms: vec!["eth".into()], limits: Limits { seat_spacing_ms: 1, ..Limits::default() } });
    let blaze = Principal { handle: "blaze".into(), scope: Scope::Write };
    for (room, text) in [("eth", "a public claim about ETH"), ("axxis", "studio internals"), ("bounty", "BOUNTY SECRET: the moonshot list")] {
        let env = Envelope::sign(&sk(1), room, "blaze", Kind::Say, text, None, vec![], 5);
        store.write(&blaze, &env, 5).unwrap();
    }
    let cargo = json!({"schema": "private-rooms/1", "rooms": {"bounty": ["rocky"]}}).to_string();
    let env = Envelope::sign(&sk(1), "oracle", "blaze", Kind::Say, "[Private rooms] bounty", Some(cargo), vec!["schema:private-rooms/1".into()], 6);
    store.write(&blaze, &env, 6).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut app = App::new(store);
    app.self_url = base.clone();
    app.public_rooms = Arc::new(oracle_server::public::parse_rooms("eth"));
    let store = app.store.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
            axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), router(app)).await.unwrap();
        })
    });
    Srv { base, store }
}

fn get(base: &str, path: &str, token: Option<&str>) -> (u16, Value, Option<String>) {
    let mut req = ureq::get(&format!("{base}{path}"));
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let (code, r) = match req.call() {
        Ok(r) => (r.status(), r),
        Err(ureq::Error::Status(c, r)) => (c, r),
        Err(e) => panic!("{e}"),
    };
    let cache = r.header("cache-control").map(str::to_string);
    (code, r.into_json().unwrap_or(json!({})), cache)
}

impl Srv {
    fn token(&self, h: &str) -> String {
        self.store.mint_token(h, Scope::Write, h, 7).unwrap()
    }
    fn public_seat(&self) -> (String, String) {
        let (_, ch, _) = get(&self.base, "/public/register/challenge", None);
        let nonce = ch["nonce"].as_str().unwrap().to_string();
        let key = sk(9);
        let pk = pubkey_hex(&key);
        let sig = hex(&key.sign(&challenge_message(&nonce, &pk)).to_bytes());
        let v: Value = ureq::post(&format!("{}/public/register", self.base))
            .send_json(json!({"pubkey": pk, "nonce": nonce, "signature": sig, "name": "Ada"}))
            .unwrap()
            .into_json()
            .unwrap();
        (v["seat"].as_str().unwrap().to_string(), v["token"].as_str().unwrap().to_string())
    }
    fn rooms_of(&self, token: &str) -> Vec<String> {
        let (code, v, _) = get(&self.base, "/rooms/mine", Some(token));
        assert_eq!(code, 200, "{v}");
        v["rooms"].as_array().unwrap().iter().map(|r| r["room"].as_str().unwrap().to_string()).collect()
    }
}

#[test]
fn anonymous_gets_nothing() {
    let s = start();
    for p in ["/rooms/eth/view", "/rooms/bounty/view", "/rooms/mine"] {
        let (code, v, _) = get(&s.base, p, None);
        assert_eq!(code, 401, "{p}: {v}");
    }
}

#[test]
fn a_member_reads_its_private_room_whole_and_privately() {
    let s = start();
    let rocky = s.token("rocky");
    let (code, v, cache) = get(&s.base, "/rooms/bounty/view", Some(&rocky));
    assert_eq!(code, 200, "{v}");
    assert_eq!((v["schema"].as_str(), v["visibility"].as_str()), (Some("room-view/1"), Some("private")));
    assert_eq!(v["capabilities"], json!({"read": true, "write": true}));
    assert!(v["messages"].to_string().contains("BOUNTY SECRET"));
    assert_eq!(cache.as_deref(), Some("private, no-store"));
    assert!(s.rooms_of(&rocky).contains(&"bounty".to_string()));
    // A studio room reads as studio, a listed public room as public.
    assert_eq!(get(&s.base, "/rooms/axxis/view", Some(&rocky)).1["visibility"], "studio");
    assert_eq!(get(&s.base, "/rooms/eth/view", Some(&rocky)).1["visibility"], "public");
}

#[test]
fn a_private_room_is_absent_for_everyone_else_including_the_owner() {
    let s = start();
    for who in ["tara", "blaze"] {
        let t = s.token(who);
        let (code, v, cache) = get(&s.base, "/rooms/bounty/view", Some(&t));
        let (missing, _, _) = get(&s.base, "/rooms/no-such-room/view", Some(&t));
        assert_eq!((code, missing), (404, 404), "{who}: a private room answers like a missing one: {v}");
        assert!(!v.to_string().contains("private") && !v.to_string().contains("BOUNTY"), "{who}: {v}");
        assert_eq!(cache.as_deref(), Some("private, no-store"));
        assert!(!s.rooms_of(&t).contains(&"bounty".to_string()), "{who} does not discover #bounty");
    }
}

#[test]
fn a_public_seat_sees_only_its_public_room_with_an_empty_scope_table() {
    let s = start();
    let (seat, token) = s.public_seat();
    let (code, v, cache) = get(&s.base, "/rooms/eth/view", Some(&token));
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["visibility"], "public");
    assert_eq!(cache.as_deref(), Some("private, no-store"), "kept through the seat-scope check");
    for room in ["axxis", "bounty", "no-such-room"] {
        let (code, v, _) = get(&s.base, &format!("/rooms/{room}/view"), Some(&token));
        assert!(code == 404 || code == 403, "{room} gave {code}: {v}");
        assert!(!v.to_string().contains("internals") && !v.to_string().contains("BOUNTY"), "{room}: {v}");
    }
    assert_eq!(s.rooms_of(&token), vec!["eth".to_string()]);
    // Revocation holds on the next request.
    assert!(s.store.revoke_public(&seat, 8).unwrap());
    assert_eq!(get(&s.base, "/rooms/eth/view", Some(&token)).0, 401);
    assert_eq!(get(&s.base, "/rooms/mine", Some(&token)).0, 401);
}

#[test]
fn a_scoped_registry_seat_keeps_to_its_table() {
    let s = start();
    let cargo = json!({"schema": "seat-rooms/1", "seats": {"tara": {"write": ["axxis"], "read": []}}}).to_string();
    let env = Envelope::sign(&sk(1), "oracle", "blaze", Kind::Say, "[Seat rooms] tara", Some(cargo), vec!["schema:seat-rooms/1".into()], 9);
    s.store.write(&Principal { handle: "blaze".into(), scope: Scope::Write }, &env, 9).unwrap();
    let tara = s.token("tara");
    assert_eq!(get(&s.base, "/rooms/axxis/view", Some(&tara)).0, 200);
    let (code, _, _) = get(&s.base, "/rooms/eth/view", Some(&tara));
    assert!(code == 404 || code == 403, "outside its table: {code}");
    assert_eq!(s.rooms_of(&tara), vec!["axxis".to_string()]);
}

#[test]
fn membership_is_intersected_with_the_seat_scope() {
    // Emmy's review of 97627eb: a private room's member with an explicit scope that excludes the room must not read or discover
    // it, and a read-only grant must not be advertised writable.
    let s = start();
    let blaze = Principal { handle: "blaze".into(), scope: Scope::Write };
    let put = |text: &str, cargo: Value, tag: &str, at: i64| {
        let env = Envelope::sign(&sk(1), "oracle", "blaze", Kind::Say, text, Some(cargo.to_string()), vec![tag.into()], at);
        s.store.write(&blaze, &env, at).unwrap();
    };
    put("[Private rooms] bounty", json!({"schema": "private-rooms/1", "rooms": {"bounty": ["rocky", "tara"]}}), "schema:private-rooms/1", 10);
    put("[Seat rooms] tara", json!({"schema": "seat-rooms/1", "seats": {"tara": {"write": [], "read": ["axxis"]}}}), "schema:seat-rooms/1", 11);
    let tara = s.token("tara");
    let (code, v, _) = get(&s.base, "/rooms/bounty/view", Some(&tara));
    assert!(code == 404 || code == 403, "a member outside its scope: {code} {v}");
    assert!(!v.to_string().contains("BOUNTY"));
    assert_eq!(s.rooms_of(&tara), vec!["axxis".to_string()], "and does not discover it");
    let (code, v, _) = get(&s.base, "/rooms/axxis/view", Some(&tara));
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["capabilities"]["write"], false, "a read-only grant is not advertised writable");
    // Within its scope, read-only: the member reads the private room and may not write there.
    put("[Seat rooms] tara", json!({"schema": "seat-rooms/1", "seats": {"tara": {"write": [], "read": ["axxis", "bounty"]}}}), "schema:seat-rooms/1", 12);
    let (code, v, _) = get(&s.base, "/rooms/bounty/view", Some(&tara));
    assert_eq!((code, v["visibility"].as_str(), v["capabilities"]["write"].as_bool()), (200, Some("private"), Some(false)), "{v}");
}

#[test]
fn another_rooms_content_never_rides_in_on_a_reference() {
    // A record in #bounty replying to one in #axxis: the member who may not read #axxis sees only the opaque id.
    let s = start();
    let blaze = Principal { handle: "blaze".into(), scope: Scope::Write };
    let secret = Envelope::sign(&sk(1), "axxis", "blaze", Kind::Say, "AXXIS ONLY: the studio plan", None, vec![], 20);
    let secret_id = s.store.write(&blaze, &secret, 20).unwrap().id;
    let cargo = json!({"schema": "seat-rooms/1", "seats": {"rocky": {"write": ["bounty"], "read": []}}}).to_string();
    let scope = Envelope::sign(&sk(1), "oracle", "blaze", Kind::Say, "[Seat rooms] rocky", Some(cargo), vec!["schema:seat-rooms/1".into()], 21);
    s.store.write(&blaze, &scope, 21).unwrap();
    let rocky = s.token("rocky");
    let reply = Envelope::sign(&sk(2), "bounty", "rocky", Kind::Say, "follows up the studio plan", None, vec![format!("reply:{secret_id}")], 22);
    s.store.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &reply, 22).unwrap();
    let (code, v, _) = get(&s.base, "/rooms/bounty/view", Some(&rocky));
    assert_eq!(code, 200, "{v}");
    let body = v.to_string();
    assert!(body.contains("follows up the studio plan"));
    assert!(!body.contains("AXXIS ONLY"), "the referenced record's content is never projected: {body}");
}
