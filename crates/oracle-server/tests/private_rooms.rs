//! Private rooms (privaterooms.rs): a room the owner makes private is read and written by its members only. Every other seat, a studio
//! seat included, is refused the room's routes and writes, and finds none of its records in any other answer. A real server on a
//! loopback port, real signed writes.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::http::{now_ms, router, App};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::{json, Value};
use std::sync::Arc;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn key(who: &str) -> u8 {
    match who { "blaze" => 1, "rocky" => 2, "tara" => 3, "imran" => 5, _ => 4 }
}
fn write(s: &Store, room: &str, who: &str, text: &str, tags: &[&str], at: i64) -> Result<String, String> {
    let env = Envelope::sign(&sk(key(who)), room, who, Kind::Say, text, None, tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).map(|r| r.id).map_err(|e| e.to_string())
}
fn private(s: &Store, who: &str, table: Value, at: i64) -> Result<String, String> {
    let cargo = json!({"schema": "private-rooms/1", "rooms": table}).to_string();
    let env = Envelope::sign(&sk(key(who)), "oracle", who, Kind::Say, "[Private rooms] test", Some(cargo), vec!["schema:private-rooms/1".into()], at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).map(|r| r.id).map_err(|e| e.to_string())
}

struct Srv { base: String, store: Arc<Store>, blaze: String, rocky: String, tara: String, imran: String }

fn start() -> Srv {
    let store = Store::memory().unwrap();
    store.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    for h in ["rocky", "tara", "imran"] {
        store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(key(h))), 2), 2).unwrap();
    }
    let tok = |h: &str| store.mint_token(h, Scope::Write, h, 3).unwrap();
    let (blaze, rocky, tara, imran) = (tok("blaze"), tok("rocky"), tok("tara"), tok("imran"));
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
    Srv { base, store, blaze, rocky, tara, imran }
}

impl Srv {
    fn get(&self, token: &str, path: &str) -> (u16, String) {
        match ureq::get(&format!("{}{path}", self.base)).set("Authorization", &format!("Bearer {token}")).call() {
            Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
            Err(e) => panic!("{e}"),
        }
    }
    fn post(&self, token: &str, env: &Envelope) -> u16 {
        match ureq::post(&format!("{}/observations", self.base)).set("Authorization", &format!("Bearer {token}")).send_json(serde_json::to_value(env).unwrap()) {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(c, _)) => c,
            Err(e) => panic!("{e}"),
        }
    }
}

#[test]
fn a_private_room_is_closed_to_every_non_member_and_open_to_its_members() {
    let s = start();
    let hello = write(&s.store, "imran", "tara", "PRIVATE_SALARY 18 lakh, notice 60 days", &[], 10).unwrap();
    write(&s.store, "rocky", "rocky", "public note in rocky", &[&format!("reply:{hello}")], 11).unwrap();
    // Only the owner's record counts.
    assert!(private(&s.store, "rocky", json!({"imran": ["rocky"]}), 15).is_ok());
    assert!(s.store.private_rooms_table().unwrap().is_empty(), "a non-owner's table changes nothing");
    private(&s.store, "blaze", json!({"imran": ["imran", "tara", "blaze"]}), 20).unwrap();

    // Members read and write.
    for t in [&s.imran, &s.tara, &s.blaze] {
        let (c, body) = s.get(t, "/rooms/imran/messages");
        assert!(c == 200 && body.contains("PRIVATE_SALARY"), "{body}");
    }
    assert_eq!(s.post(&s.imran, &Envelope::sign(&sk(5), "imran", "imran", Kind::Say, "hi Tara", None, vec![], now_ms())), 200);

    // A studio seat that is not a member: routes, queries and ids of the room refused; writes refused.
    for path in ["/rooms/imran/messages", "/rooms/imran/state", "/rooms/imran/threads", "/search?channel=imran&q=salary",
                 &format!("/observations/{hello}/content"), &format!("/threads/{hello}"), &format!("/search?id={hello}")] {
        assert_eq!(s.get(&s.rocky, path).0, 403, "{path}");
    }
    assert_eq!(s.post(&s.rocky, &Envelope::sign(&sk(2), "imran", "rocky", Kind::Say, "in his room", None, vec![], now_ms())), 403);
    assert!(write(&s.store, "imran", "rocky", "around the route", &[], 40).is_err());

    // Every other answer carries none of it: search, fold, threads, hallway, asks, the room list, the stream replay.
    for path in ["/search?q=salary", "/search?q=PRIVATE_SALARY", "/fold", "/threads", "/hallway", "/asks?seat=tara", "/channels", "/states", "/cards", "/graph", "/presence"] {
        let (c, body) = s.get(&s.rocky, path);
        assert!(!body.contains("PRIVATE_SALARY") && !body.contains(&hello[hello.len() - 12..]), "{path} ({c}) leaked: {body}");
    }
    // Rocky's own rooms still work.
    let (c, body) = s.get(&s.rocky, "/rooms/rocky/messages");
    assert!(c == 200 && body.contains("public note in rocky"), "{body}");

    // Opening the room again restores the studio read.
    private(&s.store, "blaze", json!({"tara-notes": ["tara"]}), 50).unwrap();
    assert_eq!(s.get(&s.rocky, "/rooms/imran/messages").0, 200);
}

#[test]
fn an_unreadable_table_fails_closed() {
    let s = start();
    write(&s.store, "imran", "tara", "x", &[], 10).unwrap();
    s.store.put_private_rooms_raw_for_tests("not json").unwrap();
    assert_eq!(s.get(&s.rocky, "/rooms/rocky/messages").0, 503);
    assert!(write(&s.store, "rocky", "rocky", "y", &[], 11).is_err());
}

#[test]
fn a_private_room_is_never_public_even_when_the_public_list_names_it() {
    // Emmy's review of 8f3b819: anonymous public routes carry no seat, so the private table is enforced at their source.
    let store = Store::memory().unwrap();
    store.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "tara", &pubkey_hex(&sk(3)), 2), 2).unwrap();
    let hello = write(&store, "imran", "tara", "PRIVATE_SALARY public test", &["claim:mint"], 10).unwrap(); // a cortex record: public until private
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut app = App::new(store);
    app.self_url = base.clone();
    app.public_rooms = Arc::new(oracle_server::public::parse_rooms("imran"));
    let store = app.store.clone();
    let served = app.clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move {
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(l, router(served)).await.unwrap();
        })
    });
    let anon = |path: &str| match ureq::get(&format!("{base}{path}")).call() {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
        Err(e) => panic!("{e}"),
    };
    // The negative control: public before it is private.
    assert_eq!(anon(&format!("/public/obs/{hello}")).0, 200);
    private(&store, "blaze", json!({"imran": ["imran", "tara", "blaze"]}), 20).unwrap();
    assert_eq!(anon(&format!("/public/obs/{hello}")).0, 404);
    assert_eq!(anon(&format!("/public/obs/{hello}/envelope")).0, 404);
    let (_, body) = anon("/public/search?q=PRIVATE_SALARY");
    assert!(!body.contains("PRIVATE_SALARY"), "{body}");
}

/// Signed over the author's head as ox signs (docs/CHAIN.md), so the records are linked and the check below means something.
fn chained(s: &Store, room: &str, who: &str, text: &str, at: i64) -> String {
    let (head, _) = s.chain_head(who).unwrap();
    let env = Envelope::sign(&sk(key(who)), room, who, Kind::Say, text, None, vec![], at).chain_to(&head, &sk(key(who)), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}

#[test]
fn a_chain_through_a_room_that_went_private_is_whole_with_a_withheld_link() {
    // oracle.chain-placeholders (found 4 Nov 2026 testing ox verify): a seat wrote in a room that later went private
    // without it. Its own chain used to come back with those records dropped by the private-rooms guard, so the chain
    // check showed a break that was not a fork. Now the record is a withheld link: its place, prev and sig, nothing else.
    let s = start();
    chained(&s.store, "rocky", "rocky", "before, in my room", 10);
    let inside = chained(&s.store, "imran", "rocky", "WITHHELD_WORDS written while the room was open", 11);
    chained(&s.store, "rocky", "rocky", "after, in my room again", 12);
    private(&s.store, "blaze", json!({"imran": ["imran", "tara", "blaze"]}), 13).unwrap();

    let (code, body) = s.get(&s.rocky, "/chain/rocky");
    assert_eq!(code, 200, "{body}");
    assert!(!body.contains("WITHHELD_WORDS") && !body.contains("imran") && !body.contains(&inside), "the link names nothing of the room: {body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    let recs: Vec<oracle_proto::ChainRecord> = serde_json::from_value(v["records"].clone()).unwrap();
    assert_eq!(recs.len(), 3, "the withheld record keeps its place: {body}");
    assert!(recs[1].withheld && !recs[0].withheld && !recs[2].withheld);
    let report = oracle_proto::check_chain(&recs);
    assert!(report.breaks.is_empty(), "{report:?}");
    assert_eq!((report.chained, report.withheld, report.from_genesis), (3, 1, true));

    // Negative controls: the same face with the withheld line removed breaks, and so does one whose withheld sig was
    // changed (the next record's prev no longer names it), so a server cannot drop or invent a link and still pass.
    let mut dropped = recs.clone();
    dropped.remove(1);
    assert_eq!(oracle_proto::check_chain(&dropped).breaks.len(), 1, "a dropped link is a break");
    let mut forged = recs.clone();
    forged[1].envelope.sig = "00".repeat(64);
    assert_eq!(oracle_proto::check_chain(&forged).breaks.len(), 1, "an invented link is a break");

    // A member reads the same chain whole: nothing is withheld from the room's own members (the owner here, with an
    // admin token, since a chain is read by its author or an admin).
    let admin = s.store.mint_token("blaze", Scope::Admin, "chain-test", 14).unwrap();
    let (_, whole) = s.get(&admin, "/chain/rocky");
    let recs: Vec<oracle_proto::ChainRecord> = serde_json::from_value(serde_json::from_str::<Value>(&whole).unwrap()["records"].clone()).unwrap();
    assert!(recs.iter().all(|r| !r.withheld) && recs[1].id == inside, "{whole}");
    assert_eq!(oracle_proto::check_chain(&recs).withheld, 0);
}
