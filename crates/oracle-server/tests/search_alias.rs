//! Identifier aliases in search (Rocky, 2026-10-07): a quoted observation id or its hex tail finds the record
//! first. Keyword search alone cannot: the id is not part of the indexed text. Measured on the live archive's
//! held-out set, the two identifier queries went from 0/2 to 2/2 in the top 5.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn oracle(dir: &std::path::Path) -> Store {
    let s = Store::open(&dir.join("oracle.db").to_string_lossy()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    s.apply_key_event(
        &KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 1000 * MIN),
        NOW - 1000 * MIN,
    )
    .unwrap();
    s
}

fn say(s: &Store, room: &str, text: &str, mins: i64) -> String {
    let at = NOW + mins * MIN;
    let env = Envelope::sign(&sk(2), room, "rocky", Kind::Say, text, None, vec![], at);
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap().id
}

#[test]
fn an_id_or_its_hex_tail_finds_the_record_first() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let target = say(&s, "mathematics", "QSB: CPU co-grinder as a second power budget", 0);
    say(&s, "mathematics", "the co-grinder budget is discussed here too", 1);
    let tail = target.rsplit('-').next().unwrap().to_string();
    assert!(tail.len() >= 12, "ids end in a long hex tail: {target}");

    let (hits, engine) = s.search_with(&tail, None, None, 5, None).unwrap();
    assert_eq!(hits.first().map(|h| h.obs.id.clone()), Some(target.clone()), "tail {tail} -> {engine}");
    assert!(engine.starts_with("id alias"), "{engine}");

    let (hits, _) = s.search_with(&target, None, None, 5, None).unwrap();
    assert_eq!(hits.first().map(|h| h.obs.id.clone()), Some(target.clone()));

    // The room filter still holds: the same tail scoped to another room finds nothing by alias.
    let (hits, _) = s.search_with(&tail, Some("ops"), None, 5, None).unwrap();
    assert!(hits.iter().all(|h| h.obs.id != target));
}

#[test]
fn ordinary_words_are_not_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    say(&s, "mathematics", "deadbeef is not an id, and neither is a word", 0);
    let (hits, engine) = s.search_with("deadbeef", None, None, 5, None).unwrap();
    assert!(!engine.contains("id alias"), "{engine}");
    assert_eq!(hits.len(), 1);
    let (_, engine) = s.search_with("co-grinder budget", None, None, 5, None).unwrap();
    assert!(!engine.contains("id alias"), "{engine}");
}
