//! Record codes and type addresses (Blaze, 31 Oct 2026, plan.oracle.search T7). A record's code, the first twelve hex of
//! sha256 of its signature (the tail of its id), names it alone on every door, and six or more of its digits do when they
//! are unique; a private room refuses a code exactly as it refuses the full id. A type address `domain.form.topic` is a
//! signed tag routed by prefix, renamed by the record's author with a retype that stays in the ledger. A real server on a
//! loopback port, real signed writes.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, sha_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::http::{router, App};
use oracle_server::store::{LegacyRow, Principal, Scope, Store};
use serde_json::{json, Value};
use std::sync::Arc;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn key(who: &str) -> u8 {
    match who { "blaze" => 1, "rocky" => 2, "tara" => 3, _ => 4 }
}
fn write(s: &Store, room: &str, who: &str, text: &str, tags: &[&str], at: i64) -> String {
    let env = Envelope::sign(&sk(key(who)), room, who, Kind::Say, text, None, tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}

struct Srv { base: String, store: Arc<Store>, blaze: String, rocky: String, tara: String }

fn start() -> Srv {
    let store = Store::memory().unwrap();
    store.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    for h in ["rocky", "tara"] {
        store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(key(h))), 2), 2).unwrap();
    }
    let tok = |h: &str| store.mint_token(h, Scope::Write, h, 3).unwrap();
    let (blaze, rocky, tara) = (tok("blaze"), tok("rocky"), tok("tara"));
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
    Srv { base, store, blaze, rocky, tara }
}

impl Srv {
    fn get(&self, token: &str, path: &str) -> (u16, String) {
        match ureq::get(&format!("{}{path}", self.base)).set("Authorization", &format!("Bearer {token}")).call() {
            Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        }
    }
    fn json(&self, token: &str, path: &str) -> Value {
        let (c, body) = self.get(token, path);
        assert_eq!(c, 200, "{path}: {body}");
        serde_json::from_str(&body).unwrap()
    }
}

fn code(id: &str) -> &str {
    &id[id.len() - 12..]
}

#[test]
fn a_record_s_code_names_it_on_every_door() {
    let s = start();
    let id = write(&s.store, "rocky", "rocky", "the record with a code", &[], 10);
    let reply = write(&s.store, "rocky", "tara", "a reply to it", &[&format!("reply:{id}")], 11);
    let c = code(&id);
    let sig = s.store.get(&id).unwrap().sig;
    // The code is the first twelve of sha256(signature): the same on every node, never chosen by a server.
    assert_eq!(c, &sha_hex(sig.as_bytes())[..12]);
    for path in [format!("/search?id={c}"), format!("/search?id={}", &c[..6]),
                 format!("/search?id={}", &sha_hex(sig.as_bytes())[..30]), format!("/search?id={}", c.to_uppercase())] {
        let v = s.json(&s.rocky, &path);
        assert_eq!(v["id"], json!(id), "{path}");
    }
    assert!(s.get(&s.rocky, &format!("/observations/{c}/content")).1.contains("the record with a code"));
    let t = s.json(&s.rocky, &format!("/threads/{c}"));
    assert!(t.to_string().contains(&reply), "the thread of a code is the record's thread: {t}");
    // The old id works as ever; a code that names nothing, or a longer code that disagrees with the whole hash, is not found.
    assert_eq!(s.json(&s.rocky, &format!("/search?id={id}"))["id"], json!(id));
    assert_eq!(s.get(&s.rocky, "/search?id=000000000000").0, 404);
    let wrong_tail = format!("{c}{}", if sha_hex(sig.as_bytes()).as_bytes()[12] == b'0' { "1" } else { "0" });
    assert_eq!(s.get(&s.rocky, &format!("/search?id={wrong_tail}")).0, 404);
}

#[test]
fn a_code_that_names_two_records_is_refused_with_both_named() {
    let s = start();
    let rows: Vec<LegacyRow> = ["abcdef000001", "abcdef000002"].iter().map(|tail| LegacyRow {
        id: format!("OBS-archive-20000101-{tail}"), channel: "archive".into(), author: "kin".into(), kind: "say".into(),
        content: format!("legacy {tail}"), cargo: None, tags: vec![], created_ms: 5, pubkey: String::new(), sig: String::new(),
        news: false, source: None, repair_legacy: false,
    }).collect();
    s.store.import_legacy(&rows).unwrap();
    let (c, body) = s.get(&s.rocky, "/search?id=abcdef");
    assert_eq!(c, 409, "{body}");
    assert!(body.contains("abcdef000001") || body.contains("conflict"), "{body}");
    assert_eq!(s.json(&s.rocky, "/search?id=abcdef000002")["id"], json!("OBS-archive-20000101-abcdef000002"));
    assert!(s.store.resolve_id("abcdef").unwrap_err().to_string().contains("names 2 records"));
}

#[test]
fn a_private_room_refuses_a_code_as_it_refuses_the_full_id() {
    let s = start();
    let secret = write(&s.store, "tara-notes", "tara", "PRIVATE_NOTE the code must not open it", &[], 10);
    let cargo = json!({"schema": "private-rooms/1", "rooms": {"tara-notes": ["tara", "blaze"]}}).to_string();
    let env = Envelope::sign(&sk(1), "oracle", "blaze", Kind::Say, "[Private rooms] test", Some(cargo), vec!["schema:private-rooms/1".into()], 20);
    s.store.write(&Principal { handle: "blaze".into(), scope: Scope::Write }, &env, 20).unwrap();
    let c = code(&secret);
    for path in [format!("/observations/{c}/content"), format!("/threads/{c}"),
                 format!("/search?id={c}"), format!("/search?id={}", &c[..8]), format!("/search?id={secret}")] {
        let (st, body) = s.get(&s.rocky, &path);
        assert_eq!(st, 403, "{path}: {body}");
        assert!(!body.contains("PRIVATE_NOTE"), "{path} leaked: {body}");
    }
    assert!(s.get(&s.tara, &format!("/search?id={c}")).1.contains("PRIVATE_NOTE"), "a member reads it by its code");
}

#[test]
fn a_type_address_routes_by_prefix_and_is_renamed_only_by_its_author() {
    let s = start();
    let a = write(&s.store, "oracle", "rocky", "short ids decided", &["type:oracle.decision.search-short-ids"], 10);
    let b = write(&s.store, "oracle", "rocky", "plans are the one place", &["type:oracle.decision.plans-one-place"], 11);
    write(&s.store, "oracle", "rocky", "a neighbour, not a child", &["type:oracle.decisions"], 12);
    write(&s.store, "memory", "tara", "a lesson", &["type:memory.lesson"], 13);
    let v = s.json(&s.rocky, "/types?prefix=oracle.decision");
    assert_eq!(v["count"], json!(2), "{v}");
    let got: Vec<&str> = v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(got, vec![b.as_str(), a.as_str()], "newest first, and oracle.decisions is not under oracle.decision");
    assert_eq!(v["items"][0]["code"], json!(code(&b)));
    assert_eq!(s.json(&s.rocky, "/types?prefix=oracle")["count"], json!(3));
    assert_eq!(s.json(&s.rocky, "/types")["count"], json!(4));

    // Someone else's retype is kept as a proposal; the author's own renames it, and both stay in its history.
    write(&s.store, "oracle", "tara", "Retype: tara's idea", &[&format!("retype:{a}"), "type:memory.idea"], 20);
    assert_eq!(s.json(&s.rocky, "/types?prefix=oracle.decision")["count"], json!(2), "a retype by another seat changes nothing");
    write(&s.store, "oracle", "rocky", "Retype: the author's own", &[&format!("retype:{a}"), "type:oracle.ids.short-codes"], 21);
    let v = s.json(&s.rocky, "/types?prefix=oracle.decision");
    assert_eq!(v["count"], json!(1), "{v}");
    let moved = s.json(&s.rocky, "/types?prefix=oracle.ids");
    assert_eq!(moved["items"][0]["id"], json!(a));
    let hist = moved["items"][0]["retypes"].as_array().unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!((hist[0]["applied"].clone(), hist[1]["applied"].clone()), (json!(false), json!(true)));
    assert_eq!(hist[1]["from"], json!("oracle.decision.search-short-ids"));
    assert_eq!(s.json(&s.rocky, "/types?prefix=memory.idea")["count"], json!(0));

    // A prefix that is not a type address is refused, never read as a pattern.
    assert_ne!(s.get(&s.rocky, "/types?prefix=Oracle%25").0, 200);
}

#[test]
fn a_private_room_s_typed_records_are_left_out_for_a_non_member() {
    let s = start();
    write(&s.store, "tara-notes", "tara", "PRIVATE_TYPED", &["type:oracle.decision.secret"], 10);
    write(&s.store, "oracle", "rocky", "open", &["type:oracle.decision.open"], 11);
    let cargo = json!({"schema": "private-rooms/1", "rooms": {"tara-notes": ["tara", "blaze"]}}).to_string();
    let env = Envelope::sign(&sk(1), "oracle", "blaze", Kind::Say, "[Private rooms] test", Some(cargo), vec!["schema:private-rooms/1".into()], 20);
    s.store.write(&Principal { handle: "blaze".into(), scope: Scope::Write }, &env, 20).unwrap();
    let (c, body) = s.get(&s.rocky, "/types?prefix=oracle.decision");
    assert_eq!(c, 200, "{body}");
    assert!(!body.contains("PRIVATE_TYPED") && !body.contains("tara-notes") && !body.contains("oracle.decision.secret"), "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["count"], json!(1));
    assert_eq!(s.json(&s.tara, "/types?prefix=oracle.decision")["count"], json!(2), "a member sees both");
    let _ = &s.blaze;
}

#[test]
fn a_type_address_has_a_shape() {
    for ok in ["decision", "oracle.decision.search-short-ids", "a.b.c.d.e.f", "x402.ledger"] {
        assert!(oracle_proto::type_addr_ok(ok), "{ok}");
    }
    for bad in ["", "Oracle.decision", "oracle..decision", "oracle.decision.", ".oracle", "a.b.c.d.e.f.g", "oracle decision", "-x.y", "oracle%", &"x".repeat(121)] {
        assert!(!oracle_proto::type_addr_ok(bad), "{bad:?}");
    }
}
