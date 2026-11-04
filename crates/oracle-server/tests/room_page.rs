//! The room page (`GET /rooms/:ch/page`, room-page/1; Blaze, 3 Nov 2026: "a page is a state of the room, and it changes and
//! updates as the room's messages come"): composed from a seeded store, each section holds what the room's records say, and
//! a new record changes the page on the next read with nothing rewritten. Negative controls: another room's records never
//! appear; an answered decide no longer waits; a decision older than the window drops out.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::{json, Value};

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

#[allow(clippy::too_many_arguments)] // a test seed: every field of a record, spelled out
fn put(s: &Store, who: &str, n: u8, room: &str, kind: Kind, text: &str, cargo: Option<Value>, tags: &[&str], at: i64) -> String {
    let e = Envelope::sign(&key(n), room, who, kind, text, cargo.map(|c| c.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &e, at).unwrap().id
}

#[test]
fn a_room_page_is_the_rooms_live_state() {
    let now = oracle_server::http::now_ms();
    let (day, min) = (86_400_000i64, 60_000i64);
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&key(1)), now - 40 * day).unwrap();
    s.apply_key_event(&KeyEvent::sign(&key(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&key(3)), now - 39 * day), now - 39 * day).unwrap();
    let state = put(&s, "rocky", 3, "oracle", Kind::Say, "State updated", Some(json!({"schema": "room-context/1", "channel": "oracle", "parents": [],
        "fields": {"purpose": "Shared memory", "current": "recall measured"}})), &["room-context:v1"], now - 60 * min);
    put(&s, "rocky", 3, "oracle", Kind::Say, "a note after the State", None, &[], now - 50 * min);
    let decided = put(&s, "blaze", 1, "oracle", Kind::Say, "Recall first. Then the rest.", None, &["intent:decision"], now - 40 * min);
    put(&s, "blaze", 1, "oracle", Kind::Say, "An old decision.", None, &["intent:decision"], now - 31 * day);
    put(&s, "rocky", 3, "oracle", Kind::State, "ranking on the canary", Some(json!({"subject": "oracle-recall", "state": "running", "owner": "rocky", "next_touch": "2026-11-04"})), &["subject:oracle-recall"], now - 30 * min);
    put(&s, "rocky", 3, "oracle", Kind::State, "shipped", Some(json!({"subject": "oracle-old", "state": "done", "owner": "rocky"})), &["subject:oracle-old"], now - 29 * min);
    let ask = put(&s, "rocky", 3, "oracle", Kind::Say, "@blaze Approve the page? Options: 1) yes 2) no", Some(json!({"options": ["yes", "no"], "recommendation": {"option": 1, "label": "yes", "why": "it is right"}})), &["to:blaze", "ask:decide"], now - 20 * min);
    put(&s, "rocky", 3, "oracle", Kind::Say, "digest: what moved today", None, &["ask:read"], now - 19 * min);
    put(&s, "rocky", 3, "elsewhere", Kind::Say, "another room's decision", None, &["intent:decision"], now - 10 * min);

    let page = oracle_server::room_page::compose(&s, "oracle", now, Some(0)).unwrap();
    assert_eq!(page["schema"], "room-page/1");
    assert_eq!(page["state"]["id"], state.as_str());
    assert_eq!(page["state"]["fields"]["purpose"], "Shared memory");
    assert!(page["state"]["records_since"].as_i64().unwrap() >= 5, "the room moved past its State: {}", page["state"]);
    let ids = |v: &Value| v["items"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(ids(&page["decided"]), vec![decided.clone()], "the 31-day-old decision and another room's are left out");
    assert_eq!(page["decided"]["items"][0]["text"], "Recall first.");
    let building: Vec<&str> = page["building"]["items"].as_array().unwrap().iter().map(|x| x["subject"].as_str().unwrap()).collect();
    assert_eq!(building, vec!["oracle-recall"], "a done head is not building");
    assert_eq!(ids(&page["waiting"]), vec![ask.clone()], "the open decide waits");
    assert!(page["who"]["items"].as_array().unwrap().iter().any(|w| w["seat"] == "blaze"));
    assert_eq!(page["freshness"]["meaning"]["on"], true);
    assert!(page["freshness"]["meaning"]["behind"].as_i64().unwrap() > 0, "nothing indexed: the room's records are behind");
    assert_eq!(page["reading"], Value::Null);

    // Live: an answer to the decide and a seat's reading change the next read; nothing was rewritten.
    put(&s, "blaze", 1, "oracle", Kind::Say, "yes", None, &[&format!("reply:{ask}"), "choice:1"], now - 5 * min);
    let reading = put(&s, "rocky", 3, "oracle", Kind::Say, "# the room page", None, &["page:room", "room:oracle"], now - 4 * min);
    let page = oracle_server::room_page::compose(&s, "oracle", now, None).unwrap();
    assert!(ids(&page["waiting"]).is_empty(), "answered, it no longer waits: {}", page["waiting"]);
    assert_eq!(page["decided"]["items"][0]["choice"], "1", "the pick is a decision of the room");
    assert_eq!(page["reading"]["id"], reading.as_str());
    assert_eq!(page["reading"]["records_since"], 0);
    assert_eq!(page["freshness"]["meaning"]["on"], false);
}
