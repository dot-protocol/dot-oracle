//! Facts valid in time (3 Nov 2026, the competitor map's temporal-facts row): a record replaced later still held before its
//! successor was written; a record written with valid-until stops holding after it; `as_of` searches what was on record then.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Ranker, Scope, Store, Tuning, SEARCH_TUNING};

const DAY: i64 = 86_400_000;
const T0: i64 = 1_790_000_000_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn say(s: &Store, text: &str, tags: &[String], at: i64) -> String {
    let e = Envelope::sign(&sk(2), "ops", "rocky", Kind::Say, text, None, tags.to_vec(), at);
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &e, at).unwrap().id
}

#[test]
fn a_fact_holds_until_it_is_replaced_or_its_validity_ends_and_as_of_reads_the_record_then() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), T0 - DAY).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), T0 - DAY), T0 - DAY).unwrap();
    let thursday = say(&s, "the deploy day is thursday", &[], T0);
    let friday = say(&s, "the deploy day is friday now", &[format!("supersedes:{thursday}")], T0 + 10 * DAY);
    let pune = say(&s, "the office is in pune", &[format!("valid-until:{}", T0 + 5 * DAY)], T0 + DAY);
    let run = |q: &str, as_of: Option<i64>| {
        SEARCH_TUNING.with(|t| *t.borrow_mut() = Tuning::parse(None, None).with_as_of(as_of));
        s.search_in(q, None, None, 10, None, Ranker::Rrf, false).unwrap()
    };
    // Now: friday leads; thursday is replaced, since friday was written.
    let (now, engine) = run("deploy day", None);
    assert_eq!(now[0].obs.id, friday, "{engine}");
    let old = now.iter().find(|h| h.obs.id == thursday).expect("the replaced fact is kept");
    assert_eq!(old.invalid_since.as_deref(), Some(oracle_proto::iso_ms(T0 + 10 * DAY).as_str()));
    assert!(now[0].invalid_since.is_none());
    // As of day 3: friday was not written yet, and thursday still held.
    let (then, engine) = run("deploy day", Some(T0 + 3 * DAY));
    assert_eq!(then.iter().map(|h| h.obs.id.clone()).collect::<Vec<_>>(), vec![thursday.clone()], "{engine}");
    assert!(then[0].invalid_since.is_none() && then[0].superseded_by.is_none(), "replaced later, it held then");
    assert!(engine.contains("as of"), "{engine}");
    // The office: valid until day 5. On day 3 it holds; now it does not, and says since when.
    let (held, _) = run("office pune", Some(T0 + 3 * DAY));
    assert!(held[0].invalid_since.is_none());
    assert_eq!(held[0].valid_until.as_deref(), Some(oracle_proto::iso_ms(T0 + 5 * DAY).as_str()));
    let (ended, _) = run("office pune", None);
    assert_eq!(ended[0].obs.id, pune);
    assert_eq!(ended[0].invalid_since.as_deref(), Some(oracle_proto::iso_ms(T0 + 5 * DAY).as_str()));
    // Before anything was written: nothing on record.
    assert!(run("deploy day", Some(T0 - 1)).0.is_empty());
}
