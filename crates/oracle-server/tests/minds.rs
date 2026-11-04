//! Minds (Blaze, 3 Nov 2026: "minds is supposed to be a list of minds in oracle with their statements"): every mind ranked by how
//! much it has said; one mind's statements newest first, without its reactions or task heads, paged to the start of its record.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::{json, Value};

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

#[allow(clippy::too_many_arguments)] // a test seed: every field of a record, spelled out
fn put(s: &Store, who: &str, n: u8, kind: Kind, text: &str, cargo: Option<Value>, tags: &[&str], at: i64) -> String {
    let e = Envelope::sign(&key(n), "oracle", who, kind, text, cargo.map(|c| c.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &e, at).unwrap().id
}

#[test]
fn minds_are_ranked_by_what_they_said_and_each_opens_onto_its_statements() {
    let now = oracle_server::http::now_ms();
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&key(1)), now - 10_000).unwrap();
    for (h, n) in [("rocky", 3u8), ("noor", 4u8)] {
        s.apply_key_event(&KeyEvent::sign(&key(1), "blaze", KeyAction::Add, h, &pubkey_hex(&key(n)), now - 9_000), now - 9_000).unwrap();
    }
    let first = put(&s, "rocky", 3, Kind::Say, "the first thing I said", None, &[], now - 8_000);
    let second = put(&s, "rocky", 3, Kind::Say, "the second", None, &[], now - 7_000);
    put(&s, "rocky", 3, Kind::Say, "✔ done", None, &["react:done", "reply:OBS-x"], now - 6_000);
    put(&s, "rocky", 3, Kind::State, "running", Some(json!({"subject": "t", "state": "running", "owner": "rocky", "next_touch": "2026-11-04"})), &["subject:t"], now - 5_000);
    let third = put(&s, "rocky", 3, Kind::Say, "the third", None, &[], now - 4_000);
    put(&s, "noor", 4, Kind::Say, "noor speaks once", None, &[], now - 3_000);

    let l = oracle_server::minds::list(&s).unwrap();
    assert_eq!(l["schema"], "minds/1");
    let order: Vec<&str> = l["minds"].as_array().unwrap().iter().map(|m| m["handle"].as_str().unwrap()).collect();
    assert_eq!(order.first(), Some(&"rocky"), "the one who said most leads: {order:?}");
    assert!(order.contains(&"noor"));
    assert_eq!(l["minds"][0]["records"], 5, "the count is every record it wrote");

    let ids = |v: &Value| v["statements"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let p1 = oracle_server::minds::statements(&s, "blaze", "rocky", None, Some(2)).unwrap();
    assert_eq!(ids(&p1), vec![third.clone(), second.clone()], "newest first; the reaction and the task head are not statements");
    let next = p1["next"].as_i64().expect("more to read");
    let p2 = oracle_server::minds::statements(&s, "blaze", "rocky", Some(next), Some(2)).unwrap();
    assert_eq!(ids(&p2), vec![first.clone()]);
    assert!(p2["next"].is_null(), "the start of the record: no cursor");
    assert!(oracle_server::minds::statements(&s, "blaze", "", None, None).is_err(), "a handle is required");
}
