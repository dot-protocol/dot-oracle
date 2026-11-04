//! Answerable (Blaze, 3 Nov 2026: "add check_answerable and partial state to oracle", after NVIDIA Auto Ontology's pre-flight):
//! before anyone recalls, Oracle says yes, partial or no, which words are on record, and whether what holds them is approved.
use std::collections::BTreeSet;

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::{json, Value};

const NOW: i64 = 1_790_000_000_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

#[allow(clippy::too_many_arguments)] // a test seed: every field of a record, spelled out
fn put(s: &Store, who: &str, key: u8, room: &str, text: &str, cargo: Option<Value>, tags: &[&str], at: i64) -> String {
    let e = Envelope::sign(&sk(key), room, who, Kind::Say, text, cargo.map(|c| c.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &e, at).unwrap().id
}

fn seeded() -> (tempfile::TempDir, Store, String) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    for (h, n) in [("brain", 2u8), ("rocky", 3u8)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(n)), NOW - 900), NOW - 900).unwrap();
    }
    s.gateway_add("brain", &["attest".to_string()], NOW - 800).unwrap();
    put(&s, "rocky", 3, "dot", "the lattice bootstrap runs in ten seconds on one thread", None, &[], NOW);
    put(&s, "rocky", 3, "secret", "zebra crossing plans", None, &[], NOW + 1);
    let quote = "the lattice bootstrap beats OpenFHE";
    let node = put(&s, "brain", 2, "cortex", quote, Some(json!({"kind": "observation", "title": "bootstrap result", "quote": quote,
        "source": {"message": "OBS-cases-20000126-aaaaaaaaaaaa", "start": 0, "end": 35}})), &["cortex:node"], NOW + 2);
    let nid = oracle_server::cortex::observation_nid("OBS-cases-20000126-aaaaaaaaaaaa", 0, 35);
    put(&s, "brain", 2, "cortex", "approve", Some(json!({"target": nid, "verdict": "approve", "by": "blaze", "via": "cases-page"})), &["cortex:verdict"], NOW + 3);
    (dir, s, node)
}

#[test]
fn a_question_is_answerable_partly_answerable_or_not_and_says_which_words_and_whether_approved() {
    let (_dir, s, node) = seeded();
    let none = BTreeSet::new();
    let check = |q: &str, room: Option<&str>, hidden: &BTreeSet<String>| oracle_server::answerable::check(&s, q, room, hidden).unwrap();

    let yes = check("what is the lattice bootstrap", None, &none);
    assert_eq!(yes["schema"], "answerable/1");
    assert_eq!(yes["verdict"], "yes", "{yes}");
    assert_eq!(yes["all_words"]["approved"], json!([node]), "the approved reading that holds every word is named");
    assert_eq!(yes["words"][0], json!({"word": "lattice", "records": 2, "approved": 1}));

    let unapproved = check("lattice bootstrap thread", None, &none);
    assert_eq!(unapproved["verdict"], "partial", "every word is on record, but only in a record nobody approved: {unapproved}");
    assert!(unapproved["says"].as_str().unwrap().contains("none is approved"));
    assert_eq!(unapproved["all_words"]["records"], 1);

    let half = check("lattice quasar", None, &none);
    assert_eq!(half["verdict"], "partial");
    assert_eq!(half["missing"], json!(["quasar"]));
    assert_eq!(half["says"], "on record: lattice; not on record: quasar");

    let apart = check("openfhe thread", None, &none);
    assert_eq!(apart["verdict"], "partial", "each word is on record, in different records: {apart}");
    assert!(apart["says"].as_str().unwrap().starts_with("each word is on record, but no record holds them all"), "{apart}");

    let no = check("quasar nebula", None, &none);
    assert_eq!(no["verdict"], "no");
    assert_eq!(no["missing"], json!(["quasar", "nebula"]));

    assert_eq!(check("what is it and how", None, &none)["verdict"], "no", "no content words: nothing to look up");

    // A room the caller may not read is out of every count, so the answer does not leak what it holds.
    assert_eq!(check("zebra crossing", None, &none)["verdict"], "partial", "control: a member sees the record");
    let hidden: BTreeSet<String> = ["secret".to_string()].into();
    let outside = check("zebra crossing", None, &hidden);
    assert_eq!(outside["verdict"], "no", "{outside}");
    assert_eq!(outside["words"][0]["records"], 0);

    // One room: counts only there.
    let dot = check("lattice bootstrap", Some("dot"), &none);
    assert_eq!(dot["verdict"], "partial", "in #dot the words are held, the approved reading is in #cortex: {dot}");
    assert!(dot["graph"].is_null(), "with a room named the graph is left out");
}
