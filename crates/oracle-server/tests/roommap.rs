//! A room's map (docs/ROOM-MAP.md): the written map from the State beside what the index computes, and the line the brief prints.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::roommap::{map_of, words_of};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::json;

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    for (h, n) in [("rocky", 3u8), ("emmy", 4u8), ("noor", 5u8)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(n)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    }
    s
}
fn put(s: &Store, who: &str, key: u8, room: &str, tags: &[&str], cargo: Option<serde_json::Value>, ago: i64) -> String {
    let at = NOW - ago * MIN;
    let env = Envelope::sign(&sk(key), room, who, Kind::Say, "m", cargo.map(|c| c.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}

#[test]
fn a_room_with_a_written_map_prints_its_first_line_then_the_index_facts() {
    let s = oracle();
    for i in 0..5 {
        put(&s, "rocky", 3, "axxis", &[], None, 100 - i);
    }
    put(&s, "emmy", 4, "axxis", &[], None, 50);
    put(&s, "blaze", 1, "axxis", &[], None, 45);
    put(&s, "noor", 5, "axxis", &[], None, 44);
    let cargo = json!({"schema": "room-context/1", "channel": "axxis", "parents": [], "fields": {"purpose": "the OS",
        "map": "The OS room: dev↔prod drift, CI, ops records.\nHeads: the board (ox tasks --room axxis). Skip: build logs."}});
    let head = put(&s, "rocky", 3, "axxis", &["room-context:v1"], Some(cargo), 40);
    let terms = json!({"terms": [{"term": "ci", "rooms": {"axxis": 9, "oracle": 1}}, {"term": "graph", "rooms": {"oracle": 7}}, {"word": "drift", "rooms": {"axxis": 3}}, {"term": "never", "rooms": {"axxis": 0}}]});
    let m = map_of(&s, "axxis", Some(&terms), NOW).unwrap();
    assert_eq!(m["schema"], "room-map/1");
    assert_eq!(m["written"]["text"].as_str().unwrap().lines().next().unwrap(), "The OS room: dev↔prod drift, CI, ops records.");
    assert_eq!(m["written"]["head"], head);
    assert_eq!(m["written"]["by"], "rocky");
    assert_eq!(m["written"]["at_ms"], NOW - 40 * MIN);
    assert_eq!(m["computed"]["records"], 9, "every record of the room, the State revision too");
    assert_eq!(m["computed"]["first_ms"], NOW - 100 * MIN);
    assert_eq!(m["computed"]["last_ms"], NOW - 40 * MIN);
    assert_eq!(m["computed"]["seats"], json!([{"seat": "rocky", "n": 6}, {"seat": "blaze", "n": 1}, {"seat": "emmy", "n": 1}, {"seat": "noor", "n": 1}]), "by count, then name");
    assert_eq!(m["computed"]["words"], json!([{"word": "ci", "n": 9}, {"word": "drift", "n": 3}]), "the room's own counts, read from term (the ranking's key) or word; a word it never said is absent");
    assert_eq!(m["computed"]["state_fields"], json!(["purpose"]));
    assert_eq!(m["computed"]["open_threads"], 9, "every root not answered or settled; counted, not printed");
    assert_eq!(m["line"], "#axxis  The OS room: dev↔prod drift, CI, ops records. · 9 records · last 40m ago · seats rocky 6, blaze 1, emmy 1 · words ci drift", "three seats in the line, all five in the computed map");
    // Negative controls: a room without a map says so and keeps its facts; a room with nothing has zeros and no 'last'.
    let o = map_of(&s, "oracle", Some(&terms), NOW).unwrap();
    assert!(o["written"].is_null());
    assert_eq!(o["computed"]["records"], 0);
    assert_eq!(o["line"], "#oracle  no map yet · 0 records · words graph ci");
    put(&s, "emmy", 4, "oracle", &[], None, 10);
    let o = map_of(&s, "oracle", None, NOW).unwrap();
    assert_eq!(o["line"], "#oracle  no map yet · 1 record · last 10m ago · seats emmy 1", "no ranking file: no words, not an error");
    assert_eq!(words_of(&terms, "nowhere", 8), vec![]);
}

#[test]
fn a_later_revision_without_the_map_keeps_it_and_a_long_first_line_is_cut() {
    let s = oracle();
    let long = "x".repeat(200);
    let c1 = json!({"schema": "room-context/1", "channel": "r", "parents": [], "fields": {"map": format!("{long}\nmore")}});
    let h1 = put(&s, "rocky", 3, "r", &["room-context:v1"], Some(c1), 30);
    // the client merges fields (oracle_mcp::state_revision); the server stores what it is given: a revision that carries the map keeps it
    let c2 = json!({"schema": "room-context/1", "channel": "r", "parents": [h1], "fields": {"current": "now", "map": format!("{long}\nmore")}});
    put(&s, "rocky", 3, "r", &["room-context:v1"], Some(c2), 20);
    let m = map_of(&s, "r", None, NOW).unwrap();
    assert_eq!(m["computed"]["state_fields"], json!(["current"]));
    let line = m["line"].as_str().unwrap();
    assert!(line.starts_with(&format!("#r  {}…", "x".repeat(139))), "{line}");
    assert!(line.ends_with(" · 2 records · last 20m ago · seats rocky 2"), "{line}");
}
