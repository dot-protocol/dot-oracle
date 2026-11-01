//! Round 50 asks 6 and 7: each search hit carries its links as ids, and a search says when nothing is on record.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Ranker, Scope, Store};
use serde_json::json;

const NOW: i64 = 1_790_000_000_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn oracle(dir: &std::path::Path) -> Store {
    let s = Store::open(dir.join("oracle.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 1000), NOW - 1000).unwrap();
    s
}

fn say(s: &Store, text: &str, tags: Vec<String>, at: i64) -> String {
    let env = Envelope::sign(&sk(2), "dot", "rocky", Kind::Say, text, None, tags, at);
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap().id
}

#[test]
fn a_hit_lists_what_it_answers_and_what_answered_replied_to_cited_and_marked_it() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let ask = say(&s, "first message", vec![], NOW);
    let q = say(&s, "zebrafish lantern question", vec![format!("reply:{ask}"), "ref:https://example.org/x".into()], NOW + 1);
    let r1 = say(&s, "a plain reply", vec![format!("reply:{q}")], NOW + 2);
    let close = say(&s, "the answer", vec![format!("reply:{q}"), format!("closes:{q}")], NOW + 3);
    let cite = say(&s, "see the earlier one", vec![format!("ref:{q}")], NOW + 4);
    say(&s, "● seen", vec![format!("reply:{q}"), "react:seen".into()], NOW + 5);
    say(&s, "✔ go", vec![format!("reply:{q}"), "react:go".into()], NOW + 6);

    let (hits, _) = s.search_ranked("zebrafish lantern", None, None, 5, None, Ranker::Rrf).unwrap();
    let hit = hits.iter().find(|h| h.obs.id == q).expect("the question is a hit");
    let e = &hit.edges;
    assert_eq!(e.reply_to.as_deref(), Some(ask.as_str()));
    assert_eq!(e.refs, vec!["https://example.org/x".to_string()]);
    assert_eq!(e.replies, vec![r1.clone()], "the closing reply is listed once, under answered_by; marks are not replies");
    assert_eq!(e.answered_by, vec![close]);
    assert_eq!(e.cited_by, vec![cite]);
    assert_eq!(e.marks, 2);
    let v = serde_json::to_value(hit).unwrap();
    assert_eq!(v["edges"]["marks"], 2);

    // A record nobody links to, linking nowhere, carries no edges field at all.
    let (hits, _) = s.search_ranked("plain reply", None, None, 5, None, Ranker::Rrf).unwrap();
    let lone = hits.iter().find(|h| h.obs.id == r1).unwrap();
    assert!(serde_json::to_value(lone).unwrap().get("edges").is_some_and(|e| e["reply_to"] == json!(q)));
    let (hits, _) = s.search_ranked("first message", None, None, 5, None, Ranker::Rrf).unwrap();
    let first = hits.iter().find(|h| h.obs.id == ask).unwrap();
    assert_eq!(first.edges.replies, vec![q.clone()]);
}

#[test]
fn many_replies_keep_the_newest_twenty() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let root = say(&s, "okapi ledger root", vec![], NOW);
    let ids: Vec<String> = (0..25).map(|i| say(&s, &format!("reply number {i}"), vec![format!("reply:{root}")], NOW + 1 + i)).collect();
    let (hits, _) = s.search_ranked("okapi ledger", None, None, 5, None, Ranker::Rrf).unwrap();
    let e = &hits.iter().find(|h| h.obs.id == root).unwrap().edges;
    assert_eq!(e.replies.len(), oracle_server::store::EDGE_MAX);
    assert_eq!(e.replies.first(), Some(&ids[5]));
    assert_eq!(e.replies.last(), Some(&ids[24]), "each list ends with the newest");
}

#[test]
fn the_prior_verdict_says_found_new_none_or_unclear() {
    let v = |all: i64, before: i64, best: Option<f64>| {
        Store::prior(&json!({"all_words": all, "all_words_before": before, "earliest_ms": if all > 0 { json!(NOW) } else { json!(null) }, "best_meaning": best}))["verdict"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(v(3, 2, Some(0.2)), "found");
    assert_eq!(v(3, 0, Some(0.9)), "new");
    assert_eq!(v(0, 0, Some(0.31)), "none");
    assert_eq!(v(0, 0, None), "none");
    assert_eq!(v(0, 0, Some(0.55)), "unclear");
    let none = Store::prior(&json!({"all_words": 0, "all_words_before": 0, "earliest_ms": null, "best_meaning": 0.31}));
    assert!(none["says"].as_str().unwrap().starts_with("no prior"), "{none}");
}

#[test]
fn a_search_reports_its_prior_through_the_strength_it_already_measures() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    say(&s, "quokka harbour notes", vec![], NOW - 3 * 86_400_000);
    let day_ago = NOW - 86_400_000;
    assert_eq!(Store::prior(&s.strength("quokka harbour", None, day_ago, None).unwrap())["verdict"], "found");
    assert_eq!(Store::prior(&s.strength("sourdough hydration", None, day_ago, None).unwrap())["verdict"], "none");
    say(&s, "axolotl fresh today", vec![], NOW);
    assert_eq!(Store::prior(&s.strength("axolotl fresh", None, day_ago, None).unwrap())["verdict"], "new");
}

#[test]
fn a_live_message_cannot_carry_an_import_attestation_tag() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    for tag in ["signed:ok", "cargo-signed:ok"] {
        let env = Envelope::sign(&sk(2), "dot", "rocky", Kind::Say, "trust me", None, vec![tag.into()], NOW);
        let r = s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, NOW);
        assert!(matches!(r, Err(oracle_server::store::StoreError::Rejected(ref m)) if m.contains("import attestation")), "{tag}: {r:?}");
    }
    // A look-alike is an ordinary tag.
    say(&s, "fine", vec!["signed:pending".into()], NOW);
}
