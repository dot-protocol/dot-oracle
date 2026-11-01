//! The graph of Oracle (docs/GRAPH.md): rooms and seats joined by who writes where; the cortex joined by its marks.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::cortex::nid_of;
use oracle_server::graph::{core, cortex};
use oracle_server::store::{Principal, Scope, Store};

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    for (h, n) in [("rocky", 3u8), ("emmy", 4u8)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(n)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    }
    s
}
fn put(s: &Store, who: &str, key: u8, room: &str, tags: &[&str], cargo: Option<serde_json::Value>, ago: i64) -> String {
    let at = NOW - ago * MIN;
    let env = Envelope::sign(&sk(key), room, who, Kind::Say, "m", cargo.map(|c| c.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn has_edge(g: &serde_json::Value, a: &str, b: &str, kind: &str) -> Option<i64> {
    g["edges"].as_array().unwrap().iter()
        .find(|e| e["kind"] == kind && ((e["source"] == a && e["target"] == b) || (e["source"] == b && e["target"] == a)))
        .and_then(|e| e["weight"].as_i64())
}

#[test]
fn the_core_view_joins_seats_to_the_rooms_they_write_in_and_rooms_by_crossing_replies() {
    let s = oracle();
    for i in 0..4 { put(&s, "rocky", 3, "axxis", &[], None, 100 - i); }
    let root = put(&s, "emmy", 4, "dot", &[], None, 90);
    put(&s, "rocky", 3, "axxis", &[&format!("reply:{root}")], None, 80);
    put(&s, "emmy", 4, "axxis", &[], None, 70);
    let g = core(&s, 2).unwrap();
    assert_eq!(has_edge(&g, "seat:rocky", "room:axxis", "writes"), Some(5));
    assert_eq!(has_edge(&g, "seat:emmy", "room:axxis", "writes"), None, "one message is under min 2");
    assert_eq!(has_edge(&g, "room:axxis", "room:dot", "reply"), Some(1), "a reply across rooms joins the rooms");
    let kinds: Vec<&str> = g["nodes"].as_array().unwrap().iter().filter_map(|n| n["kind"].as_str()).collect();
    assert!(kinds.contains(&"room") && kinds.contains(&"seat"));
}

#[test]
fn the_cortex_view_joins_nodes_marked_together_and_keeps_claims_with_their_edges() {
    let s = oracle();
    put(&s, "rocky", 3, "brain", &["cortex:node"], Some(serde_json::json!({"kind": "entity", "title": "Replit"})), 60);
    put(&s, "rocky", 3, "brain", &["cortex:node"], Some(serde_json::json!({"kind": "concept", "title": "zero-setup deployment"})), 59);
    put(&s, "rocky", 3, "brain", &["cortex:node"], Some(serde_json::json!({"kind": "concept", "title": "agent memory"})), 58);
    let (r, z, a) = (nid_of("entity", "Replit"), nid_of("concept", "zero-setup deployment"), nid_of("concept", "agent memory"));
    let m1 = put(&s, "rocky", 3, "brain", &["cortex:mark"], Some(serde_json::json!({"source": {"message": "u40", "start": 0, "end": 9}, "labels": ["Question"]})), 50);
    let m2 = put(&s, "rocky", 3, "brain", &["cortex:mark"], Some(serde_json::json!({"source": {"message": "u40", "start": 10, "end": 20}, "labels": ["Plan"]})), 49);
    for (m, n) in [(&m1, &r), (&m1, &z), (&m2, &a)] {
        put(&s, "rocky", 3, "brain", &["cortex:link"], Some(serde_json::json!({"from": m, "to": n, "kind": "about"})), 40);
    }
    put(&s, "emmy", 4, "mathematics", &["claim:mint"], Some(serde_json::json!({"statement": "A", "type": "conjecture", "status": "open"})), 30);
    put(&s, "emmy", 4, "mathematics", &["claim:mint"], Some(serde_json::json!({"statement": "B", "type": "conjecture", "status": "open"})), 29);
    let (ca, cb) = (oracle_server::claims::cid_of("A"), oracle_server::claims::cid_of("B"));
    put(&s, "emmy", 4, "mathematics", &["claim:edge"], Some(serde_json::json!({"from": &ca[..12], "to": &cb[..12], "kind": "supports"})), 20);
    let g = cortex(&s, false).unwrap();
    assert_eq!(g["nodes"].as_array().unwrap().len(), 5);
    assert_eq!(has_edge(&g, &format!("node:{r}"), &format!("node:{z}"), "same-mark"), Some(1), "one mark links both");
    assert_eq!(has_edge(&g, &format!("node:{r}"), &format!("node:{a}"), "same-source"), Some(1), "marks on the same message");
    assert_eq!(has_edge(&g, &format!("node:{r}"), &format!("node:{a}"), "same-mark"), None, "different marks are not same-mark");
    assert_eq!(has_edge(&g, &format!("claim:{ca}"), &format!("claim:{cb}"), "supports"), Some(1));
    // where each node comes from and who made it (Blaze, 18 Oct: segment the claims by who made them; the Brain cases apart)
    let by_id = |id: String| g["nodes"].as_array().unwrap().iter().find(|n| n["id"] == id).cloned().unwrap();
    let (claim, node) = (by_id(format!("claim:{ca}")), by_id(format!("node:{r}")));
    assert_eq!((claim["origin"].as_str(), claim["author"].as_str()), (Some("claims"), Some("emmy")));
    assert_eq!((node["origin"].as_str(), node["by"].clone()), (Some("brain"), serde_json::json!(["rocky"])), "the drafter of the marks that hold it");
}

#[test]
fn drafts_count_only_when_asked_and_rejected_never_count() {
    let s = oracle();
    put(&s, "rocky", 3, "brain", &["cortex:node"], Some(serde_json::json!({"kind": "entity", "title": "Replit"})), 60);
    put(&s, "rocky", 3, "brain", &["cortex:node"], Some(serde_json::json!({"kind": "concept", "title": "agent memory"})), 59);
    let (r, a) = (nid_of("entity", "Replit"), nid_of("concept", "agent memory"));
    let approved = put(&s, "rocky", 3, "brain", &["cortex:mark"], Some(serde_json::json!({"source": {"message": "u1", "start": 0, "end": 5}, "labels": ["Claim"]})), 50);
    let draft = put(&s, "rocky", 3, "brain", &["cortex:mark"], Some(serde_json::json!({"source": {"message": "u2", "start": 0, "end": 5}, "labels": ["Claim"]})), 49);
    let rejected = put(&s, "rocky", 3, "brain", &["cortex:mark"], Some(serde_json::json!({"source": {"message": "u3", "start": 0, "end": 5}, "labels": ["Claim"]})), 48);
    for m in [&approved, &draft, &rejected] {
        for n in [&r, &a] {
            put(&s, "rocky", 3, "brain", &["cortex:link"], Some(serde_json::json!({"from": m, "to": n, "kind": "about"})), 40);
        }
    }
    put(&s, "emmy", 4, "brain", &["cortex:verdict"], Some(serde_json::json!({"target": approved, "verdict": "approve", "by": "blaze", "via": "cases-page"})), 30);
    put(&s, "emmy", 4, "brain", &["cortex:verdict"], Some(serde_json::json!({"target": rejected, "verdict": "reject", "by": "blaze", "via": "cases-page"})), 30);
    let node = |g: &serde_json::Value, id: &str| g["nodes"].as_array().unwrap().iter().find(|n| n["id"] == format!("node:{id}")).cloned().unwrap();
    let g = cortex(&s, true).unwrap();
    assert_eq!(g["marks"], "approved");
    let n = node(&g, &r);
    assert_eq!((n["approved"].as_i64(), n["drafts"].as_i64(), n["rejected"].as_i64(), n["size"].as_i64()), (Some(1), Some(1), Some(1), Some(1)), "counts are always reported; size is the approved");
    assert_eq!(has_edge(&g, &format!("node:{r}"), &format!("node:{a}"), "same-mark"), Some(1), "only the approved mark joins");
    assert_eq!(n["by"], serde_json::json!(["rocky"]), "a draft or rejected mark names no maker in the approved view");
    let g = cortex(&s, false).unwrap();
    assert_eq!(node(&g, &r)["size"].as_i64(), Some(2), "all: drafts count too");
    assert_eq!(has_edge(&g, &format!("node:{r}"), &format!("node:{a}"), "same-mark"), Some(2), "all: the draft joins, the rejected never does");
}

#[test]
fn the_seo_view_joins_ranked_words_to_the_rooms_that_said_them() {
    let terms = serde_json::json!({"generated_at": "2026-10-18T08:00:00Z", "terms": [
        {"term": "recall", "rank": 1, "weight": 90.5, "count": 12, "chats_count": 1, "oracle_count": 11, "documents": 6, "common": false, "rooms": {"axxis": 7, "oracle": 4, "chats": 1}},
        {"term": "graph", "rank": 2, "weight": 40.0, "count": 5, "documents": 3, "common": false, "rooms": {"oracle": 5}},
        {"term": "the", "rank": 3, "weight": 1.0, "count": 900, "documents": 800, "common": true, "rooms": {"axxis": 900}}]});
    let g = oracle_server::graph::seo(&terms, 2);
    let ids: Vec<&str> = g["nodes"].as_array().unwrap().iter().filter_map(|n| n["id"].as_str()).collect();
    assert_eq!(ids, ["word:recall", "word:graph", "room:axxis", "room:chats", "room:oracle"], "top 2 words, then the rooms they were said in");
    assert_eq!(has_edge(&g, "word:recall", "room:axxis", "said"), Some(7));
    let recall = g["nodes"].as_array().unwrap().iter().find(|n| n["id"] == "word:recall").unwrap();
    assert_eq!((recall["size"].as_i64(), recall["chats_count"].as_i64(), recall["oracle_count"].as_i64(), recall["count"].as_i64()), (Some(1), Some(1), Some(11), Some(12)), "a word's size is the Time Machine's count; the rooms' count rides beside it");
    assert_eq!(has_edge(&g, "word:graph", "room:axxis", "said"), None, "a room that never said the word is not joined");
    let oracle = g["nodes"].as_array().unwrap().iter().find(|n| n["id"] == "room:oracle").unwrap();
    assert_eq!(oracle["size"], 9, "a room's size is every ranked word said in it");
    assert_eq!(g["generated_at"], "2026-10-18T08:00:00Z");
}

#[test]
fn an_archived_mark_leaves_the_cortex_graph() {
    let s = oracle();
    put(&s, "rocky", 3, "brain", &["cortex:node"], Some(serde_json::json!({"kind": "entity", "title": "Replit"})), 60);
    let r = nid_of("entity", "Replit");
    let m = put(&s, "rocky", 3, "brain", &["cortex:mark"], Some(serde_json::json!({"source": {"message": "u1", "start": 0, "end": 5}, "labels": ["Claim"]})), 50);
    put(&s, "rocky", 3, "brain", &["cortex:link"], Some(serde_json::json!({"from": m, "to": r, "kind": "about"})), 40);
    put(&s, "emmy", 4, "brain", &["cortex:verdict"], Some(serde_json::json!({"target": m, "verdict": "approve", "by": "blaze", "via": "cases-page"})), 30);
    let node = |g: &serde_json::Value| g["nodes"].as_array().unwrap().iter().find(|n| n["id"] == format!("node:{r}")).cloned().unwrap();
    assert_eq!(node(&cortex(&s, true).unwrap())["approved"], 1, "approved and live");
    put(&s, "rocky", 3, "brain", &[&format!("archives:{m}")], None, 20); // ox archive brain <m>
    assert_eq!(node(&cortex(&s, true).unwrap())["approved"], 0, "archived: out of the view, still in the ledger");
}

/// Brain cases since 25 Oct: an approved mark is published as an observation node with Blaze's verdict and joined to
/// the concept it is about by a cortex:link, with no cortex:mark at all. The view held none of them (Blaze, 25 Oct:
/// "the graph is not working, it should show all the nodes and relations we created today").
#[test]
fn a_published_observation_is_held_by_its_verdict_and_holds_what_it_links_to() {
    let s = oracle();
    let ob = |start: u64, quote: &str| serde_json::json!({"kind": "observation", "title": quote, "quote": quote,
        "source": {"message": "OBS-cases-20001025-aaaaaaaaaaaa", "start": start, "end": start + 10}, "drafted_by": "rocky"});
    put(&s, "rocky", 3, "cortex", &["cortex:node"], Some(ob(0, "many agents on one codebase")), 50);
    put(&s, "rocky", 3, "cortex", &["cortex:node"], Some(ob(20, "a rejected one")), 49);
    put(&s, "rocky", 3, "cortex", &["cortex:node"], Some(serde_json::json!({"kind": "concept", "title": "parallel agents"})), 48);
    put(&s, "rocky", 3, "cortex", &["cortex:node"], Some(serde_json::json!({"kind": "concept", "title": "nobody links here"})), 47);
    let a = oracle_server::cortex::observation_nid("OBS-cases-20001025-aaaaaaaaaaaa", 0, 10);
    let r = oracle_server::cortex::observation_nid("OBS-cases-20001025-aaaaaaaaaaaa", 20, 30);
    let concept = nid_of("concept", "parallel agents");
    put(&s, "rocky", 3, "cortex", &["cortex:link"], Some(serde_json::json!({"from": a, "to": concept, "kind": "about"})), 46);
    put(&s, "blaze", 1, "cortex", &["cortex:verdict"], Some(serde_json::json!({"target": a, "verdict": "approve"})), 45);
    put(&s, "blaze", 1, "cortex", &["cortex:verdict"], Some(serde_json::json!({"target": r, "verdict": "reject"})), 44);
    put(&s, "rocky", 3, "cortex", &["cortex:link"], Some(serde_json::json!({"from": a, "to": r, "kind": "about"})), 43);
    let g = cortex(&s, true).unwrap();
    let held = |nid: &str| g["nodes"].as_array().unwrap().iter().find(|n| n["id"] == format!("node:{nid}")).unwrap()["held"].as_bool().unwrap();
    assert!(held(&a), "approved by Blaze: held");
    let by = g["nodes"].as_array().unwrap().iter().find(|n| n["id"] == format!("node:{a}")).unwrap()["by"].clone();
    assert_eq!(by, serde_json::json!(["rocky"]), "the drafter rides on the node (the Drafted by chips)");
    assert!(held(&concept), "linked from an approved node: held");
    assert!(!held(&r), "rejected: never held");
    assert!(!held(&nid_of("concept", "nobody links here")), "no approval and no approved link: not held");
    // An unweighed link draws as related, 2 on the 1-4 scale.
    assert_eq!(has_edge(&g, &format!("node:{a}"), &format!("node:{concept}"), "about"), Some(2));
}

/// A Belief under the decision it underpins (Blaze, 26 Oct): a `supports` link is a cortex edge like the others, not dropped.
#[test]
fn a_supports_link_is_an_edge() {
    let s = oracle();
    put(&s, "rocky", 3, "cortex", &["cortex:node"], Some(serde_json::json!({"kind": "concept", "title": "vcs have no clue"})), 50);
    put(&s, "rocky", 3, "cortex", &["cortex:node"], Some(serde_json::json!({"kind": "concept", "title": "revenue first"})), 49);
    let (b, d) = (nid_of("concept", "vcs have no clue"), nid_of("concept", "revenue first"));
    put(&s, "rocky", 3, "cortex", &["cortex:link"], Some(serde_json::json!({"from": b, "to": d, "kind": "supports"})), 48);
    let g = cortex(&s, false).unwrap();
    assert!(has_edge(&g, &format!("node:{b}"), &format!("node:{d}"), "supports").is_some(), "the premise points at its conclusion");
}
