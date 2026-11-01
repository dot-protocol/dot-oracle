//! Seat rooms (seatrooms.rs): a seat the owner confines to rooms writes only in its write rooms, reads only its rooms, and is refused every
//! route that is not one room's; an unscoped seat is untouched. A real server on a loopback port, real signed writes.

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
    match who { "blaze" => 1, "rocky" => 2, "tara" => 3, _ => 4 }
}
fn write(s: &Store, room: &str, who: &str, text: &str, cargo: Option<String>, tags: &[&str], at: i64) -> Result<String, String> {
    let env = Envelope::sign(&sk(key(who)), room, who, Kind::Say, text, cargo, tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).map(|r| r.id).map_err(|e| e.to_string())
}
fn scope(s: &Store, who: &str, table: Value, at: i64) -> Result<String, String> {
    let cargo = json!({"schema": "seat-rooms/1", "seats": table}).to_string();
    write(s, "oracle", who, "[Seat rooms] test", Some(cargo), &["schema:seat-rooms/1"], at)
}

struct Srv { base: String, store: Arc<Store>, tara: String, rocky: String }

fn start() -> Srv {
    let store = Store::memory().unwrap();
    store.init_owner("blaze", &pubkey_hex(&sk(1)), 1).unwrap();
    for h in ["rocky", "tara"] {
        store.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(key(h))), 2), 2).unwrap();
    }
    let tara = store.mint_token("tara", Scope::Write, "tara", 3).unwrap();
    let rocky = store.mint_token("rocky", Scope::Write, "rocky", 3).unwrap();
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
    Srv { base, store, tara, rocky }
}

impl Srv {
    fn get(&self, token: &str, path: &str) -> (u16, Value) {
        match ureq::get(&format!("{}{path}", self.base)).set("Authorization", &format!("Bearer {token}")).call() {
            Ok(r) => (r.status(), r.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(c, r)) => (c, r.into_json().unwrap_or(Value::Null)),
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
fn a_scoped_seat_reads_and_writes_its_rooms_only_and_an_unscoped_seat_is_untouched() {
    let s = start();
    let mine = write(&s.store, "imran", "rocky", "welcome", None, &[], 10).unwrap();
    let page = write(&s.store, "career", "rocky", "market map", None, &[], 11).unwrap();
    let secret = write(&s.store, "blaze", "rocky", "the founder's secret", None, &[], 12).unwrap();
    write(&s.store, "blaze", "rocky", "cross-room reply text", None, &[&format!("reply:{mine}")], 13).unwrap();
    scope(&s.store, "blaze", json!({"tara": {"write": ["imran"], "read": ["career"]}}), 20).unwrap();
    assert_eq!(s.store.seat_rooms_for("tara").unwrap().unwrap().write.len(), 1);

    // Writes: its write room only, through the route and through the store.
    assert_eq!(s.post(&s.tara, &Envelope::sign(&sk(3), "imran", "tara", Kind::Say, "hello Imran", None, vec![], now_ms())), 200);
    assert_eq!(s.post(&s.tara, &Envelope::sign(&sk(3), "blaze", "tara", Kind::Say, "posted elsewhere", None, vec![], now_ms())), 403);
    assert_eq!(s.post(&s.tara, &Envelope::sign(&sk(3), "career", "tara", Kind::Say, "read-only room", None, vec![], now_ms())), 403);
    assert!(write(&s.store, "astra", "tara", "around the route", None, &[], 33).is_err());

    // Reads: its rooms; a message, a search or a thread from another room is refused or scrubbed.
    assert_eq!(s.get(&s.tara, "/rooms/imran/messages").0, 200);
    assert_eq!(s.get(&s.tara, "/rooms/career/messages").0, 200);
    for path in ["/rooms/blaze/messages", "/rooms/astra/state", "/channels", "/presence", "/states", "/graph", "/cortex", "/cards", "/tasks",
                 "/search?q=secret", "/search?channel=blaze&q=x", "/asks?seat=rocky", "/hallway", "/keys", "/fold", "/files", "/terms"] {
        assert_eq!(s.get(&s.tara, path).0, 403, "{path}");
    }
    assert_eq!(s.get(&s.tara, &format!("/search?id={secret}")).0, 403);
    assert_eq!(s.get(&s.tara, &format!("/search?id={page}")).0, 200);
    // Derived answers are closed to a scoped seat until projections are scoped at their sources: threads, thread lists, asks.
    assert_eq!(s.get(&s.tara, &format!("/threads/{mine}")).0, 403);
    assert_eq!(s.get(&s.tara, "/threads?channel=imran&roots=rocky&scope=room&filter=all").0, 403);
    assert_eq!(s.get(&s.tara, "/asks").0, 403);
    // The room feed is raw signed records only: no derived field (state, ask, intent), nothing from another room.
    let (code, feed) = s.get(&s.tara, "/rooms/imran/messages");
    assert_eq!(code, 200);
    for item in feed["items"].as_array().unwrap() {
        assert_eq!(item.get("channel").or_else(|| item.get("room")).and_then(|v| v.as_str()), Some("imran"), "{item}");
        assert!(item.get("state").is_none() && item.get("ask").is_none() && item.get("intent").is_none(), "derived field in {item}");
    }
    assert!(!feed.to_string().contains("cross-room reply text"));
    assert_eq!(s.get(&s.tara, "/whoami").0, 200);

    // An unscoped seat is exactly as before.
    assert_eq!(s.get(&s.rocky, "/rooms/blaze/messages").0, 200);
    assert_eq!(s.get(&s.rocky, "/channels").0, 200);
}

#[test]
fn only_the_owner_sets_the_table_and_a_rebuilt_index_keeps_it() {
    let s = start();
    scope(&s.store, "blaze", json!({"tara": {"write": ["imran"]}}), 20).unwrap();
    // Rocky's record, valid or not, changes nothing.
    scope(&s.store, "rocky", json!({"rocky": {"write": ["rocky"]}}), 21).unwrap();
    assert!(s.store.seat_rooms_for("rocky").unwrap().is_none());
    assert!(s.store.seat_rooms_for("tara").unwrap().is_some());
    // An invalid owner record changes nothing either.
    write(&s.store, "oracle", "blaze", "[Seat rooms] broken", Some("{\"schema\":\"seat-rooms/1\",\"seats\":{\"tara\":{}}}".into()), &[], 22).unwrap();
    assert!(s.store.seat_rooms_for("tara").unwrap().is_some());
    s.store.reindex().unwrap();
    assert!(s.store.seat_rooms_for("tara").unwrap().is_some(), "an index rebuild unscoped the seat");
    assert_eq!(s.get(&s.tara, "/rooms/blaze/messages").0, 403);
    // The owner clears it: an ordinary seat again.
    scope(&s.store, "blaze", json!({}), 30).unwrap();
    assert!(s.store.seat_rooms_for("tara").unwrap().is_none());
    assert_eq!(s.get(&s.tara, "/rooms/blaze/messages").0, 200);
}

#[test]
fn an_unreadable_scope_table_fails_closed() {
    let s = start();
    scope(&s.store, "blaze", json!({"tara": {"write": ["imran"]}}), 20).unwrap();
    s.store.put_seat_scope_raw_for_tests("{not json").unwrap();
    // Reads: an outage, never the unscoped handler, for the scoped seat and for everyone while the policy cannot be read.
    assert_eq!(s.get(&s.tara, "/rooms/blaze/messages").0, 503);
    assert_eq!(s.get(&s.rocky, "/rooms/blaze/messages").0, 503);
    // Writes: refused.
    assert!(write(&s.store, "blaze", "tara", "while the table is broken", None, &[], 30).is_err());
}

// Emmy's regression: the projected thread list omits raw descendants, so their room labels
// cannot help a response-only detector; the summary must never reach the scoped seat.
#[test]
fn emmy_projected_thread_list_never_exposes_foreign_summary() {
    let s = start();
    let now = now_ms();
    let mine = write(&s.store, "imran", "rocky", "own-room question", None, &["intent:question", "ask:reply", "to:tara"], now - 1000).unwrap();
    write(&s.store, "blaze", "blaze", "PRIVATE_FOREIGN_SUMMARY_SENTINEL", None, &[&format!("reply:{mine}"), "intent:summary"], now - 900).unwrap();
    scope(&s.store, "blaze", json!({"tara": {"write": ["imran"]}}), now - 800).unwrap();
    for path in ["/threads?channel=imran&roots=rocky&scope=room&filter=all", "/rooms/imran/threads", "/rooms/imran/messages", "/rooms/imran/state"] {
        let (status, body) = s.get(&s.tara, path);
        assert!(status == 200 || status == 403, "{path}: unexpected status {status}: {body}");
        assert!(!body.to_string().contains("PRIVATE_FOREIGN_SUMMARY_SENTINEL"), "{path}: forbidden summary reached the scoped seat: {body}");
    }
}
