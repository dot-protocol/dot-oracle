//! The cortex widened (docs/CORTEX.md): nodes, marks and links are signed messages, folded on read through the store.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::cortex::{marks, node, nid_of, nodes};
use oracle_server::store::{Principal, Scope, Store};

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(3)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    s
}
fn put(s: &Store, tags: &[&str], cargo: serde_json::Value, ago: i64) -> String {
    let at = NOW - ago * MIN;
    let env = Envelope::sign(&sk(3), "brain", "rocky", Kind::Say, "cortex", Some(cargo.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap().id
}

#[test]
fn a_brain_case_mark_becomes_a_node_page_with_its_words_and_blazes_verdict() {
    let s = oracle();
    put(&s, &["cortex:node"], serde_json::json!({"kind": "entity", "title": "Replit"}), 60);
    put(&s, &["cortex:node"], serde_json::json!({"kind": "question", "title": "How do platforms deploy apps without setup?"}), 59);
    let (replit, q) = (nid_of("entity", "Replit"), nid_of("question", "How do platforms deploy apps without setup?"));
    let m = put(&s, &["cortex:mark"], serde_json::json!({"source": {"system": "timemachine", "message": "019ba025", "start": 120, "end": 230, "sha256": "ab"},
        "labels": ["Question"], "intents": ["Compete"], "project": "AXXIS", "note": "zero-setup deploy", "origin": {"system": "cases", "mark": 36}}), 50);
    put(&s, &["cortex:link"], serde_json::json!({"from": m, "to": replit, "kind": "about"}), 49);
    put(&s, &["cortex:link"], serde_json::json!({"from": m, "to": q, "kind": "asks"}), 48);
    put(&s, &["cortex:verdict"], serde_json::json!({"target": m, "verdict": "approve", "by": "blaze", "via": "cases-page"}), 40);
    // negative controls, listed not dropped: an unknown kind, a link from nothing
    put(&s, &["cortex:node"], serde_json::json!({"kind": "vibe", "title": "x"}), 30);
    put(&s, &["cortex:link"], serde_json::json!({"from": "OBS-brain-x", "to": q, "kind": "about"}), 29);

    let all = nodes(&s, Some("brain"), None, None, true).unwrap();
    assert_eq!(all["count"], 2);
    assert_eq!(all["rejected"], 2);
    let why = all["rejected_why"].as_array().expect("every rejected write is listed with its id and why");
    assert_eq!(why.len(), 2);
    assert!(why.iter().all(|r| r["id"].is_string() && r["why"].as_str().is_some_and(|w| !w.is_empty())), "{why:?}");
    let qs = nodes(&s, None, Some("question"), Some("deploy"), true).unwrap();
    assert_eq!(qs["nodes"][0]["title"], "How do platforms deploy apps without setup?");
    assert_eq!((qs["nodes"][0]["marks"].as_i64(), qs["nodes"][0]["approved"].as_i64(), qs["nodes"][0]["status"].as_str()), (Some(1), Some(1), Some("open")));
    let page = node(&s, &replit[..10]).unwrap().expect("a prefix of 8+ names the node");
    assert_eq!(page["marks"][0]["source"]["start"], 120);
    assert_eq!((page["marks"][0]["verdict"]["by"].as_str(), page["marks"][0]["verdict"]["signed_by"].as_str()), (Some("blaze"), Some("rocky")));
    assert!(node(&s, "0000000000").unwrap().is_none());
    assert_eq!(marks(&s, Some("brain"), Some("019ba025"), false).unwrap()["count"], 1);
    assert_eq!(marks(&s, Some("brain"), Some("019ba025"), true).unwrap()["count"], 0, "a mark with Blaze's verdict is not pending");
    assert_eq!(marks(&s, None, Some("nope"), false).unwrap()["count"], 0);
}

#[test]
fn observation_page_preserves_the_drafters_perspective_without_inventing_execution_state() {
    let s = oracle();
    let source = serde_json::json!({"message":"chat/message", "chat":"chat", "start":0,"end":18});
    let record = put(&s, &["cortex:node"], serde_json::json!({
        "kind":"observation", "title":"Build our own agent", "source":source, "quote":"Build our own agent",
        "note":"An agent builder is proposed; completion is not evidenced.", "drafted_by":"rocky",
        "speaker":"Blaze", "project":"AXXIS", "intents":["Build"], "labels":["Plan"]
    }), 10);
    let id = oracle_server::cortex::observation_nid("chat/message", 0, 18);
    put(&s, &["cortex:verdict"], serde_json::json!({"target":id,"verdict":"approve","by":"blaze","via":"cases-page"}), 9);
    let page = node(&s, &id).unwrap().unwrap();
    let n = &page["node"];
    assert_eq!(n["minted_by"], record);
    assert_eq!(n["note"], "An agent builder is proposed; completion is not evidenced.");
    assert_eq!(n["speaker"], "Blaze");
    assert_eq!(n["drafted_by"], "rocky");
    assert_eq!(n["project"], "AXXIS");
    assert_eq!(n["intents"], serde_json::json!(["Build"]));
    assert!(n["status"].is_null(), "approval does not mean a plan is open or completed");
    let listing = nodes(&s, None, Some("observation"), None, true).unwrap();
    assert_eq!(listing["nodes"][0]["verdict"], "approve");
    assert!(listing["nodes"][0]["status"].is_null());
}
