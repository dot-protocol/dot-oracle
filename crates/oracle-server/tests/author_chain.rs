//! The author chain (docs/CHAIN.md): a write that names `prev` is stored only while it is the author's head; the chain
//! reads back as signed and checks whole offline; old envelopes are untouched.
use ed25519_dalek::SigningKey;
use oracle_proto::{check_chain, link_of, pubkey_hex, Envelope, KeyAction, KeyEvent, Kind, GENESIS_PREV};
use oracle_server::store::{Principal, Scope, Store, StoreError};

const NOW: i64 = 1_790_000_000_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn oracle(dir: &std::path::Path) -> Store {
    let s = Store::open(dir.join("oracle.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 1000), NOW - 1000).unwrap();
    // A second device of the same seat.
    s.apply_key_event(&KeyEvent::sign(&sk(2), "rocky", KeyAction::Add, "rocky", &pubkey_hex(&sk(3)), NOW - 1000), NOW - 1000).unwrap();
    s
}

fn rocky() -> Principal {
    Principal { handle: "rocky".into(), scope: Scope::Write }
}

fn env(key: u8, room: &str, text: &str, at: i64) -> Envelope {
    Envelope::sign(&sk(key), room, "rocky", Kind::Say, text, None, vec![], at)
}

/// Sign over the head the store reports, the way ox does.
fn chained(s: &Store, key: u8, room: &str, text: &str, at: i64) -> Result<String, StoreError> {
    let (head, _) = s.chain_head("rocky").unwrap();
    let e = env(key, room, text, at).chain_to(&head, &sk(key), at);
    s.write(&rocky(), &e, at).map(|r| r.id)
}

#[test]
fn a_seat_chain_is_linear_across_rooms_and_devices_and_checks_whole() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    assert_eq!(s.chain_head("rocky").unwrap(), (GENESIS_PREV.to_string(), None), "no native write yet: genesis");
    // A write from before chains existed: stored exactly as before, and the first chained write names it.
    let old = s.write(&rocky(), &env(2, "dot", "before chains", NOW), NOW).unwrap().id;
    let a = chained(&s, 2, "dot", "one", NOW + 1).unwrap();
    let b = chained(&s, 3, "oracle", "two, from the other device, in another room", NOW + 2).unwrap();
    let c = chained(&s, 2, "dot", "three", NOW + 3).unwrap();
    // blaze's writes are not rocky's chain.
    s.write(&Principal { handle: "blaze".into(), scope: Scope::Write }, &Envelope::sign(&sk(1), "dot", "blaze", Kind::Say, "mine", None, vec![], NOW + 4), NOW + 4).unwrap();

    let recs = s.chain("rocky", 0, 100).unwrap();
    assert_eq!(recs.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), [old.as_str(), &a, &b, &c]);
    let r = check_chain(&recs);
    assert!(r.breaks.is_empty(), "{r:?}");
    assert_eq!((r.chained, r.unchained, r.first_chained.as_deref()), (3, 1, Some(a.as_str())));
    assert_eq!(recs[2].pubkey, pubkey_hex(&sk(3)), "each record carries the device key that verified it");
    // The stored envelope is the signed one: the stranger's verification of one row is unchanged.
    assert_eq!(s.envelope_of(&b).unwrap(), recs[2].envelope);
    assert_eq!(s.chain_head("rocky").unwrap().0, link_of(&recs[3].envelope.sig));
    assert_eq!(s.chain("rocky", recs[1].seq, 100).unwrap().len(), 2, "after= pages");
}

#[test]
fn a_write_over_a_stale_head_is_refused_and_signs_again() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let (head, _) = s.chain_head("rocky").unwrap();
    // Two devices read the same head; the first to arrive is stored.
    let first = env(2, "dot", "laptop", NOW).chain_to(&head, &sk(2), NOW);
    let second = env(3, "dot", "phone", NOW).chain_to(&head, &sk(3), NOW);
    s.write(&rocky(), &first, NOW).unwrap();
    // Negative control: the second names a head that is gone, so it would fork the chain. Refused, nothing stored.
    match s.write(&rocky(), &second, NOW + 1) {
        Err(StoreError::Conflict(m)) => assert!(m.contains("chain head"), "{m}"),
        other => panic!("a stale prev must be a conflict: {other:?}"),
    }
    assert_eq!(s.chain("rocky", 0, 10).unwrap().len(), 1);
    // Signed again over the new head, the same write (same client_op) is stored once.
    let (head2, _) = s.chain_head("rocky").unwrap();
    let again = second.clone().chain_to(&head2, &sk(3), NOW + 2);
    assert_eq!(again.client_op, second.client_op);
    s.write(&rocky(), &again, NOW + 2).unwrap();
    assert!(check_chain(&s.chain("rocky", 0, 10).unwrap()).breaks.is_empty());
    // A genesis claim after the chain began is a stale head too.
    let late = env(2, "dot", "pretends to be first", NOW + 3).chain_to(GENESIS_PREV, &sk(2), NOW + 3);
    assert!(matches!(s.write(&rocky(), &late, NOW + 3), Err(StoreError::Conflict(_))));
}
