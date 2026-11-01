//! docs/ARCHIVE.md: an archived room leaves search but stays readable; an expired record leaves the view.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};

const NOW: i64 = 1_792_000_000_000;

fn sk(n: u8) -> SigningKey { SigningKey::from_bytes(&[n; 32]) }
fn oracle(dir: &std::path::Path) -> Store {
    let s = Store::open(dir.join("oracle.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 1000), NOW - 1000).unwrap();
    s
}
#[allow(clippy::too_many_arguments)]
fn put(s: &Store, key: &SigningKey, who: &str, room: &str, text: &str, cargo: Option<String>, tags: Vec<String>, at: i64) -> String {
    let env = Envelope::sign(key, room, who, Kind::Say, text, cargo, tags, at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn archive(s: &Store, room: &str, why: &str, at: i64) -> String {
    let cargo = serde_json::json!({"schema": "room-context/1", "channel": room, "parents": [], "fields": {"purpose": "a room", "archived": why}}).to_string();
    put(s, &sk(1), "blaze", room, "State", Some(cargo), vec!["room-context:v1".into()], at)
}
fn ids(hits: &[oracle_server::store::SearchHit]) -> Vec<String> { hits.iter().map(|h| h.obs.id.clone()).collect() }

/// The same words in a live room and in an archived one: search returns the live one and says what it left out;
/// include_archived returns both; the archived room still reads by room and by id. Control: the live room is untouched.
#[test]
fn an_archived_room_leaves_search_but_not_the_room_or_the_id() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let live = put(&s, &sk(2), "rocky", "notes", "the lantern and the channel capacity", None, vec![], NOW);
    let old = put(&s, &sk(2), "rocky", "old-mail", "the lantern and the channel capacity, a letter", None, vec![], NOW + 1);
    let both = ids(&s.search("lantern capacity", None, None, 10).unwrap());
    assert!(both.contains(&live) && both.contains(&old), "before the archive both are found: {both:?}");
    assert_eq!(s.not_considered().unwrap(), (0, 0));

    let head = archive(&s, "old-mail", "the old mail, dead since July", NOW + 2);
    let after = ids(&s.search("lantern capacity", None, None, 10).unwrap());
    assert!(after.contains(&live) && !after.contains(&old), "the archived room is out of search: {after:?}");
    assert_eq!(s.not_considered().unwrap().0, 1, "one room not considered");
    assert_eq!(s.archived_rooms().unwrap().get("old-mail").map(String::as_str), Some("the old mail, dead since July"));
    let all = ids(&s.search_in("lantern capacity", None, None, 10, None, oracle_server::store::Ranker::Rrf, true).unwrap().0);
    assert!(all.contains(&old), "include_archived finds it again: {all:?}");
    let read = s.recent(Some("old-mail"), 10, None).unwrap();
    assert!(read.iter().any(|o| o.id == old), "the room still reads");
    assert!(s.get(&old).is_ok(), "the id still reads");

    // a later revision without the field takes the room out of the archive
    let cargo = serde_json::json!({"schema": "room-context/1", "channel": "old-mail", "parents": [head], "fields": {"purpose": "a room"}}).to_string();
    put(&s, &sk(1), "blaze", "old-mail", "State", Some(cargo), vec!["room-context:v1".into()], NOW + 3);
    assert_eq!(s.not_considered().unwrap(), (0, 0));
    assert!(ids(&s.search("lantern capacity", None, None, 10).unwrap()).contains(&old));
}

/// A record with a past `expires:` leaves search and the room read; the same words without it stay (control);
/// the id still reads and the author chain still holds it.
#[test]
fn an_expired_record_leaves_the_view_but_not_the_chain() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let real_now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    let gone = put(&s, &sk(2), "rocky", "astra", "Astra 404 alert, the lantern", None, vec![format!("expires:{}", real_now - 1000)], NOW);
    let kept = put(&s, &sk(2), "rocky", "astra", "Astra 404 alert, the lantern, still live", None, vec![format!("expires:{}", real_now + 3_600_000)], NOW + 1);
    let plain = put(&s, &sk(2), "rocky", "astra", "the lantern, a person's message", None, vec![], NOW + 2);
    let found = ids(&s.search("lantern", None, None, 10).unwrap());
    assert!(!found.contains(&gone) && found.contains(&kept) && found.contains(&plain), "{found:?}");
    let read: Vec<String> = s.recent(Some("astra"), 10, None).unwrap().into_iter().map(|o| o.id).collect();
    assert!(!read.contains(&gone) && read.contains(&kept) && read.contains(&plain), "{read:?}");
    assert!(s.get(&gone).is_ok(), "the id still reads");
    let chain = s.chain("rocky", 0, 100).unwrap();
    assert_eq!(chain.len(), 3, "the chain holds every record, expired or not");
    assert_eq!(oracle_server::store::expires_of(&["expires:12".to_string()]), Some(12));
    assert!(oracle_server::store::expires_of(&["expires:soon".to_string()]).is_none());
}
