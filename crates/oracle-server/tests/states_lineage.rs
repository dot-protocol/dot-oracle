//! Card states-lineage-chain: `GET /states?subject=&history=<n>`, a head's revisions newest first with `current` and `superseded_by` computed at read time
//! (ported from graph-Oracle PR #14, with Emmy's limit+1 fix: `truncated` only when more exist). Real signed writes on the real store.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::lineage::chain;
use oracle_server::store::{Principal, Scope, Store, StoreError};
use serde_json::Value;

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    }
    s
}
/// One revision of `subject`: a state write, optionally superseding earlier revisions by id.
#[allow(clippy::too_many_arguments)]
fn rev(s: &Store, room: &str, who: &str, subject: &str, state: &str, note: &str, supersedes: &[&str], mins: i64) -> String {
    let at = NOW + mins * MIN;
    let cargo = format!(r#"{{"subject":"{subject}","state":"{state}","owner":"{who}","next_touch":"2026-12-01"}}"#);
    let mut tags = vec![format!("subject:{subject}")];
    tags.extend(supersedes.iter().map(|p| format!("supersedes:{p}")));
    let env = Envelope::sign(&sk(if who == "rocky" { 2 } else { 3 }), room, who, Kind::State, note, Some(cargo), tags, at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn ids(v: &Value) -> Vec<String> {
    v["revisions"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_owned()).collect()
}
fn rejected(r: Result<Value, StoreError>) -> String {
    match r {
        Err(StoreError::Rejected(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_chain_of_exactly_limit_revisions_is_not_truncated_and_one_more_is() {
    let s = oracle();
    let mut all = vec![];
    for i in 0..5 {
        all.push(rev(&s, "core", "rocky", "core.ship", if i == 4 { "done" } else { "running" }, &format!("revision {i}"), &[], i));
    }
    let v = chain(&s, "core.ship", 5).unwrap();
    assert_eq!((v["count"].as_i64(), v["truncated"].as_bool(), v["limit"].as_i64()), (Some(5), Some(false), Some(5)), "exactly the limit: nothing more exists, so nothing is truncated");
    assert_eq!(ids(&v), all.iter().rev().cloned().collect::<Vec<_>>(), "newest first");
    let v = chain(&s, "core.ship", 4).unwrap();
    assert_eq!((v["count"].as_i64(), v["truncated"].as_bool()), (Some(4), Some(true)), "one more exists than the limit: trimmed, and it says so");
    assert_eq!(ids(&v), all.iter().rev().take(4).cloned().collect::<Vec<_>>());
    let v = chain(&s, "core.ship", 200).unwrap();
    assert_eq!(v["truncated"], false);
    assert_eq!(chain(&s, "core.ship", 1).unwrap()["truncated"], true);
}

#[test]
fn a_replaced_revision_says_by_whom_and_the_newest_is_current() {
    let s = oracle();
    let a = rev(&s, "core", "rocky", "core.ship", "running", "first", &[], 0);
    let b = rev(&s, "core", "dinesh", "core.ship", "blocked", "second", &[], 1);
    let c = rev(&s, "core", "rocky", "core.ship", "done", "third", &[], 2);
    let v = chain(&s, "core.ship", 10).unwrap();
    let r = |id: &str| v["revisions"].as_array().unwrap().iter().find(|x| x["id"] == id).unwrap().clone();
    assert_eq!((r(&c)["current"].clone(), r(&c)["superseded_by"].clone()), (true.into(), serde_json::json!([])));
    assert_eq!((r(&b)["current"].clone(), r(&b)["superseded_by"].clone()), (false.into(), serde_json::json!([c])), "a head is replaced by the next write to its subject");
    assert_eq!(r(&a)["superseded_by"], serde_json::json!([b]));
    assert_eq!(v["heads"], serde_json::json!([c]));
    assert_eq!((r(&b)["state"].as_str(), r(&b)["owner"].as_str(), r(&b)["room"].as_str(), r(&b)["author"].as_str(), r(&b)["note"].as_str()), (Some("blocked"), Some("dinesh"), Some("core"), Some("dinesh"), Some("second")));
    assert_eq!(r(&c)["signed"], true, "signed is the same verified-when-written rule as every list");
    // another subject's revisions are not in the chain
    rev(&s, "core", "rocky", "core.other", "running", "unrelated", &[], 3);
    assert_eq!(ids(&chain(&s, "core.ship", 10).unwrap()), vec![c.clone(), b.clone(), a.clone()]);
}

#[test]
fn a_fork_shows_both_heads_ordered_by_seq_and_a_merge_ends_it() {
    let s = oracle();
    let root = rev(&s, "core", "rocky", "core.plan", "running", "the plan", &[], 0);
    // two revisions written off the same parent, neither knowing the other
    let left = rev(&s, "core", "rocky", "core.plan", "running", "rocky's branch", &[&root], 1);
    let right = rev(&s, "core", "dinesh", "core.plan", "blocked", "dinesh's branch", &[&root], 2);
    let v = chain(&s, "core.plan", 10).unwrap();
    let r = |id: &str| v["revisions"].as_array().unwrap().iter().find(|x| x["id"] == id).unwrap().clone();
    assert_eq!((r(&left)["current"].clone(), r(&right)["current"].clone()), (true.into(), true.into()), "both branches are heads");
    assert_eq!(v["heads"], serde_json::json!([left, right]), "heads are ordered by seq, oldest first, so the order is stable");
    assert_eq!(v["fork"], true);
    assert_eq!(r(&root)["superseded_by"], serde_json::json!([left, right]), "the parent was replaced twice");
    assert_eq!(ids(&v), vec![right.clone(), left.clone(), root.clone()], "the chain itself is newest first");
    // a revision that supersedes both branches merges them: one head again
    let merged = rev(&s, "core", "rocky", "core.plan", "done", "merged", &[&left, &right], 3);
    let v = chain(&s, "core.plan", 10).unwrap();
    assert_eq!((v["heads"].clone(), v["fork"].clone()), (serde_json::json!([merged]), false.into()));
    assert_eq!(v["revisions"][2]["superseded_by"], serde_json::json!([merged]), "a branch is replaced by the merge");
}

#[test]
fn bad_subjects_and_limits_are_refused_and_an_unknown_subject_is_not_found() {
    let s = oracle();
    rev(&s, "core", "rocky", "core.ship", "running", "x", &[], 0);
    assert!(rejected(chain(&s, "core.ship", 0)).contains("history"));
    assert!(rejected(chain(&s, "core.ship", 201)).contains("1 to 200"));
    assert!(rejected(chain(&s, "", 5)).contains("subject"));
    assert!(matches!(chain(&s, "core.nothing", 5), Err(StoreError::NotFound)));
    // a message that only mentions a subject in a tag is not a revision: it has to be a state write
    let at = NOW + 9 * MIN;
    let env = Envelope::sign(&sk(2), "core", "rocky", Kind::Say, "talking about core.ship", None, vec!["subject:core.ship".into()], at);
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap();
    assert_eq!(chain(&s, "core.ship", 10).unwrap()["count"], 1);
}
