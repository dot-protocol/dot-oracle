//! `by_room`: where words are said (Noor, 2026-10-08: "find every Oppenheimer observation across all 79 rooms").
//! Real signed writes on the real store.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::roomread::{search, SearchQ};
use oracle_server::store::{Principal, Scope, Store};

const NOW: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 100 * DAY).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 100 * DAY), NOW - 100 * DAY).unwrap();
    s
}
fn say(s: &Store, room: &str, who: (u8, &str), text: &str, days: i64) {
    let at = NOW + days * DAY;
    let env = Envelope::sign(&sk(who.0), room, who.1, Kind::Say, text, None, vec![], at);
    s.write(&Principal { handle: who.1.into(), scope: Scope::Write }, &env, at).unwrap();
}

#[test]
fn by_room_counts_every_match_per_room_with_its_first_and_last_date_most_first() {
    let s = oracle();
    let r = (2, "rocky");
    say(&s, "illuminati", r, "Oppenheimer takes the seat", 1);
    say(&s, "illuminati", r, "Oppenheimer on the stop button", 3);
    say(&s, "illuminati", r, "Oppenheimer again", 5);
    say(&s, "matrix", r, "Oppenheimer in the matrix", 2);
    say(&s, "astra", r, "a bank statement", 2);
    let page = search(&s, &SearchQ { q: "Oppenheimer".into(), limit: Some(1), by_room: true, ..Default::default() }).unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 1, "the page stays its own size");
    let rooms = page["rooms"].as_array().unwrap();
    assert_eq!(rooms.len(), 2, "{rooms:?}");
    assert_eq!((rooms[0]["room"].as_str(), rooms[0]["count"].as_i64()), (Some("illuminati"), Some(3)));
    assert_eq!((rooms[0]["first_ms"].as_i64(), rooms[0]["last_ms"].as_i64()), (Some(NOW + DAY), Some(NOW + 5 * DAY)));
    assert_eq!((rooms[1]["room"].as_str(), rooms[1]["count"].as_i64()), (Some("matrix"), Some(1)));
    // Without by_room the page is as before; with words that match nothing, no rooms.
    assert!(search(&s, &SearchQ { q: "Oppenheimer".into(), ..Default::default() }).unwrap().get("rooms").is_none());
    let none = search(&s, &SearchQ { q: "Feynman".into(), by_room: true, ..Default::default() }).unwrap();
    assert_eq!(none["rooms"].as_array().map(Vec::len), Some(0));
    // Held to the same room as the page.
    let one = search(&s, &SearchQ { q: "Oppenheimer".into(), room: Some("matrix".into()), by_room: true, ..Default::default() }).unwrap();
    assert_eq!(one["rooms"].as_array().unwrap().len(), 1);
}
