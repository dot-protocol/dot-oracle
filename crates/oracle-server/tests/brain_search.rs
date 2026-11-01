//! The cortex leads recall (Blaze, 25 Oct 2026: "every new case should compound"): a Blaze-approved cortex observation
//! ranks above first-hand messages with the same words and carries its typed links; an unapproved node still trails.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Ranker, Scope, Store};

const NOW: i64 = 1_790_000_000_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

#[allow(clippy::too_many_arguments)]
fn put(s: &Store, who: &str, key: u8, room: &str, text: &str, cargo: Option<serde_json::Value>, tags: &[&str], at: i64) -> String {
    let e = Envelope::sign(&sk(key), room, who, Kind::Say, text, cargo.map(|c| c.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &e, at).unwrap().id
}

#[test]
fn an_approved_cortex_observation_leads_and_brings_its_links() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "brain", &pubkey_hex(&sk(2)), NOW - 900), NOW - 900).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(3)), NOW - 900), NOW - 900).unwrap();
    // The brain seat attests Blaze's approvals made in the Brain cases page (docs/CORTEX.md "Cited by").
    s.gateway_add("brain", &["attest".to_string()], NOW - 800).unwrap();
    // Plenty of first-hand chatter with the words, written first so it would win on recency and count.
    for i in 0..5 {
        put(&s, "rocky", 3, "oracle", &format!("synthesis skill agent extracts knowledge note {i}"), None, &[], NOW + i);
    }
    let later = put(&s, "rocky", 3, "oracle", "ox manual shipped", None, &[], NOW + 10);
    let quote = "a synthesis skill: the agent extracts the knowledge itself";
    let node = put(&s, "brain", 2, "cortex", quote, Some(serde_json::json!({
        "approved_by": "blaze", "kind": "observation", "title": "rule seventeen", "labels": ["Oracle-core design"], "note": "the synthesis rule",
        "quote": quote, "source": {"message": "OBS-cases-20000126-aaaaaaaaaaaa", "start": 10, "end": 70}})), &["cortex:node", "speaker:blaze"], NOW + 20);
    let nid = oracle_server::cortex::observation_nid("OBS-cases-20000126-aaaaaaaaaaaa", 10, 70);
    put(&s, "brain", 2, "cortex", "generates", Some(serde_json::json!({"from": nid, "kind": "generates", "to": later})), &["cortex:link"], NOW + 21);
    // Approved means a verdict that verifies as the owner's (Emmy's review): here attested by the brain seat for Blaze.
    put(&s, "brain", 2, "cortex", "approve", Some(serde_json::json!({"target": nid, "verdict": "approve", "by": "blaze", "via": "cases-page"})), &["cortex:verdict"], NOW + 21);
    // An unapproved graph node with the same words: still under the first-hand records.
    let draft = put(&s, "brain", 2, "cortex", "synthesis skill agent extracts knowledge (draft)", Some(serde_json::json!({"kind": "concept"})), &["cortex:node"], NOW + 22);

    let (hits, _) = s.search_in("synthesis skill agent extracts knowledge", None, None, 10, None, Ranker::Rrf, false).unwrap();
    assert_eq!(hits[0].obs.id, node, "the approved observation leads");
    let brain = hits[0].brain.as_ref().expect("it carries the graph around it");
    assert_eq!(brain["out"][0]["kind"], "generates");
    assert_eq!(brain["out"][0]["to"], later.as_str());
    assert_eq!(brain["approved_by"], "blaze");
    let pos = |id: &str| hits.iter().position(|h| h.obs.id == id);
    assert!(pos(&draft).unwrap_or(99) > 5, "an unapproved node trails every first-hand record");
    assert!(hits.iter().filter(|h| h.obs.id != node).all(|h| h.brain.is_none()));
}

/// Control for the test above (Emmy's review): the same node with only `approved_by` in its cargo and no verdict that
/// verifies as the owner's does not lead; the cargo field alone is never an approval.
#[test]
fn a_cargo_approved_by_without_a_verified_verdict_does_not_lead() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "brain", &pubkey_hex(&sk(2)), NOW - 900), NOW - 900).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(3)), NOW - 900), NOW - 900).unwrap();
    for i in 0..5 {
        put(&s, "rocky", 3, "oracle", &format!("synthesis skill agent extracts knowledge note {i}"), None, &[], NOW + i);
    }
    let quote = "a synthesis skill: the agent extracts the knowledge itself";
    let node = put(&s, "brain", 2, "cortex", quote, Some(serde_json::json!({
        "approved_by": "blaze", "kind": "observation", "title": "rule seventeen", "labels": ["Oracle-core design"], "note": "the synthesis rule",
        "quote": quote, "source": {"message": "OBS-cases-20000126-aaaaaaaaaaaa", "start": 10, "end": 70}})), &["cortex:node", "speaker:blaze"], NOW + 20);
    let nid = oracle_server::cortex::observation_nid("OBS-cases-20000126-aaaaaaaaaaaa", 10, 70);
    // A verdict naming Blaze but signed by a seat that is not a registered attester: an agent's word, not the owner's.
    put(&s, "rocky", 3, "cortex", "approve", Some(serde_json::json!({"target": nid, "verdict": "approve", "by": "blaze", "via": "cases-page"})), &["cortex:verdict"], NOW + 21);
    let (hits, _) = s.search_in("synthesis skill agent extracts knowledge", None, None, 10, None, Ranker::Rrf, false).unwrap();
    assert_ne!(hits[0].obs.id, node, "an unverified approval does not lead");
}

/// Live cargo is often plain words ("room dialogue from icarus via /room/say"), not JSON. SQLite's json_extract
/// raises "malformed JSON" on it, and 08b4053 answered every search on the live store with a 500 for that.
#[test]
fn a_hit_whose_cargo_is_plain_words_still_searches() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "brain", &pubkey_hex(&sk(2)), NOW - 900), NOW - 900).unwrap();
    let raw = |who: &str, key: u8, room: &str, text: &str, cargo: &str, tags: &[&str], at: i64| {
        let e = Envelope::sign(&sk(key), room, who, Kind::Say, text, Some(cargo.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
        s.write(&Principal { handle: who.into(), scope: Scope::Write }, &e, at).unwrap().id
    };
    let said = raw("blaze", 1, "oracle", "worktree isolation for parallel agents", "room dialogue from icarus via /room/say", &[], NOW);
    let node = raw("brain", 2, "cortex", "worktree isolation for parallel agents (node)", "not json either", &["cortex:node"], NOW + 1);
    raw("brain", 2, "cortex", "worktree isolation link", "plain words on a link", &["cortex:link"], NOW + 2);
    let (hits, _) = s.search_in("worktree isolation parallel agents", None, None, 10, None, Ranker::Rrf, false).expect("plain-word cargo is not an error");
    assert!(hits.iter().any(|h| h.obs.id == said));
    assert!(hits.iter().any(|h| h.obs.id == node));
    assert!(hits.iter().all(|h| h.brain.is_none()), "no JSON, so no approval and no brain");
}

#[test]
fn malformed_link_shape_cannot_break_an_approved_search_hit() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    let source = "OBS-cases-20000126-aaaaaaaaaaaa";
    let nid = oracle_server::cortex::observation_nid(source, 0, 10);
    let node = put(&s, "blaze", 1, "cortex", "reliable propagation", Some(serde_json::json!({
        "approved_by":"blaze", "kind":"observation", "source":{"message":source,"start":0,"end":10}
    })), &["cortex:node"], NOW);
    let link = serde_json::json!({"from":nid,"to":"OBS-oracle-20000126-bbbbbbbbbbbb","kind":"generates"});
    put(&s, "blaze", 1, "cortex", "generates", Some(link.clone()), &["cortex:link"], NOW + 1);
    for (i, bad) in [
        serde_json::json!({"from":nid,"to":"missing-kind"}),
        serde_json::json!({"from":nid,"kind":[],"to":"bad-kind"}),
        serde_json::json!({"from":nid,"kind":"generates","to":17}),
        serde_json::json!({"from":nid,"kind":"unknown","to":"OBS-oracle-20000126-bbbbbbbbbbbb"}),
    ].into_iter().enumerate() {
        put(&s, "blaze", 1, "cortex", "malformed link", Some(bad), &["cortex:link"], NOW + 2 + i as i64);
    }
    let (hits, _) = s.search_in("reliable propagation", None, None, 10, None, Ranker::Rrf, false)
        .expect("valid JSON with malformed fields must not fail the whole search");
    let brain = hits.iter().find(|h| h.obs.id == node).unwrap().brain.as_ref().unwrap();
    assert_eq!(brain["out"], serde_json::json!([{"kind":"generates","to":link["to"]}]));
    // Valid withdrawals and later replacements still fold in event order.
    let mut removed = link.clone(); removed["removed"] = serde_json::json!(true);
    put(&s, "blaze", 1, "cortex", "withdraw", Some(removed), &["cortex:link"], NOW + 20);
    let (hits, _) = s.search_in("reliable propagation", None, None, 10, None, Ranker::Rrf, false).unwrap();
    assert_eq!(hits.iter().find(|h| h.obs.id == node).unwrap().brain.as_ref().unwrap()["out"], serde_json::json!([]));
    put(&s, "blaze", 1, "cortex", "restore", Some(link.clone()), &["cortex:link"], NOW + 21);
    let (hits, _) = s.search_in("reliable propagation", None, None, 10, None, Ranker::Rrf, false).unwrap();
    assert_eq!(hits.iter().find(|h| h.obs.id == node).unwrap().brain.as_ref().unwrap()["out"], serde_json::json!([{"kind":"generates","to":link["to"]}]));
}

fn store_with_brain() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "brain", &pubkey_hex(&sk(2)), NOW - 900), NOW - 900).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(3)), NOW - 900), NOW - 900).unwrap();
    (dir, s)
}

fn approved(s: &Store, quote: &str, n: u64, at: i64) -> String {
    put(s, "brain", 2, "cortex", quote, Some(serde_json::json!({
        "approved_by": "blaze", "kind": "observation", "labels": ["Vision"], "quote": quote,
        "source": {"message": "OBS-cases-20000215-bbbbbbbbbbbb", "start": n, "end": n + 40}})), &["cortex:node", "speaker:blaze"], at)
}

/// The graph's scaffolding (a link record "about orchestrator", a concept node "orchestrator") holds no knowledge: it
/// trails every first-hand record (Blaze, 26 Oct: recall answered with those and hid the February chats).
#[test]
fn graph_scaffolding_trails_first_hand_records() {
    let (_d, s) = store_with_brain();
    let mut said = Vec::new();
    for i in 0..4 {
        said.push(put(&s, "rocky", 3, "axxis", &format!("the orchestrator routes each ask to the right agent, case {i}"), None, &[], NOW + i));
    }
    let link = put(&s, "brain", 2, "cortex", "about orchestrator", Some(serde_json::json!({"from": "aa", "kind": "about", "to": "bb"})), &["cortex:link"], NOW + 10);
    let concept = put(&s, "brain", 2, "cortex", "orchestrator", Some(serde_json::json!({"kind": "concept", "title": "orchestrator"})), &["cortex:node"], NOW + 11);
    let (hits, _) = s.search_in("orchestrator", None, None, 10, None, Ranker::Rrf, false).unwrap();
    let pos = |id: &str| hits.iter().position(|h| h.obs.id == id).unwrap_or(99);
    let last_said = said.iter().map(|id| pos(id)).max().unwrap();
    assert!(last_said < 99, "every first-hand record is found");
    assert!(pos(&link) > last_said && pos(&concept) > last_said, "scaffolding trails: link {} concept {} said {last_said}", pos(&link), pos(&concept));
}

/// At most two approved observations go ahead of the ranking, even when more match as well as the first-hand record.
#[test]
fn only_two_approved_observations_lead() {
    let (_d, s) = store_with_brain();
    let said = put(&s, "rocky", 3, "axxis", "dragons as live agents with speech bubbles in the hallway", None, &[], NOW);
    let a = approved(&s, "dragons as live agents with speech bubbles in the hallway, one", 0, NOW + 1);
    let b = approved(&s, "dragons as live agents with speech bubbles in the hallway, two", 100, NOW + 2);
    let c = approved(&s, "dragons as live agents with speech bubbles in the hallway, three", 200, NOW + 3);
    let (hits, _) = s.search_in("dragons live agents speech bubbles hallway", None, None, 10, None, Ranker::Rrf, false).unwrap();
    let pos = |id: &str| hits.iter().position(|h| h.obs.id == id).unwrap_or(99);
    assert_eq!([pos(&a), pos(&b), pos(&c)].iter().filter(|p| **p < 2).count(), 2, "two approved observations lead");
    assert!(pos(&said) < 99, "the first-hand record is on the page");
}

/// One room fills at most half a page while another room has matches (26 Oct: 16 of 25 hits for "agents as dragons"
/// were #cortex). With a room named, the cap is off.
#[test]
fn one_room_fills_at_most_half_a_page() {
    let (_d, s) = store_with_brain();
    // More strong #cortex hits than a page of 6 x 2: the other room is still reached (the pool is at least SIDE).
    for i in 0..15 {
        put(&s, "brain", 2, "cortex", &format!("agents as dragons in a living world, extract {i}"), None, &[], NOW + i);
    }
    let older = put(&s, "rocky", 3, "illuminati", "dragons as agents, said on a bike in February", None, &[], NOW - 500);
    let (hits, _) = s.search_in("agents dragons", None, None, 6, None, Ranker::Rrf, false).unwrap();
    let pos = hits.iter().position(|h| h.obs.id == older).expect("the other room's record is on the page");
    assert!(pos <= 3, "it comes before the fourth #cortex hit, at {pos}");
    assert_eq!(hits.iter().take(pos).filter(|h| h.obs.channel == "cortex").count(), pos, "the three best #cortex hits keep their place");
    let (only, _) = s.search_in("agents dragons", Some("cortex"), None, 6, None, Ranker::Rrf, false).unwrap();
    assert_eq!(only.len(), 6, "a named room is not capped");
}

/// A weak approved match does not lead: on the 26 Oct canary an approved node that shared one word ("open") went ahead of
/// the record that answered "how about the open port".
#[test]
fn a_weakly_matching_approved_observation_does_not_lead() {
    let (_d, s) = store_with_brain();
    let answer = put(&s, "rocky", 3, "illuminati", "is the only risk via telegram? how about the open port on the mac mini", None, &[], NOW);
    let weak = approved(&s, "I want an open interface for everyone", 300, NOW + 1);
    let (hits, _) = s.search_in("how about the open port", None, None, 10, None, Ranker::Rrf, false).unwrap();
    let pos = |id: &str| hits.iter().position(|h| h.obs.id == id).unwrap_or(99);
    assert_eq!(pos(&answer), 0, "the answer comes first; the weak approved node is at {}", pos(&weak));
}

// Review: diversity must preserve the source-before-scaffolding invariant on a smaller page.
#[test]
fn room_diversity_does_not_promote_scaffolding_over_sources() {
    let (_d, s) = store_with_brain();
    let mut said = Vec::new();
    for i in 0..4 {
        said.push(put(&s, "rocky", 3, "axxis", &format!("the orchestrator routes each ask to the right agent, case {i}"), None, &[], NOW + i));
    }
    let link = put(&s, "brain", 2, "cortex", "about orchestrator", Some(serde_json::json!({"from": "aa", "kind": "about", "to": "bb"})), &["cortex:link"], NOW + 10);
    let concept = put(&s, "brain", 2, "cortex", "orchestrator", Some(serde_json::json!({"kind": "concept", "title": "orchestrator"})), &["cortex:node"], NOW + 11);
    let (hits, _) = s.search_in("orchestrator", None, None, 6, None, Ranker::Rrf, false).unwrap();
    let pos = |id: &str| hits.iter().position(|h| h.obs.id == id).unwrap_or(99);
    let last_said = said.iter().map(|id| pos(id)).max().unwrap();
    assert!(last_said < 99, "every first-hand record is found");
    assert!(pos(&link) > last_said && pos(&concept) > last_said, "scaffolding trails: link {} concept {} said {last_said}", pos(&link), pos(&concept));
}

/// Scaffolding that outranks every original must not use up the room cap's page: the cap orders first-hand records among
/// themselves (26 Oct canary, 3a1d28e: with scaffolding counted in, two canon-ox mail answers fell from 1 and 2 to 4 and 7).
#[test]
fn high_scoring_scaffolding_does_not_consume_the_room_budget() {
    let (_d, s) = store_with_brain();
    // Scaffolding that holds the query words most densely, so it would rank first.
    put(&s, "brain", 2, "cortex", "mail relay", Some(serde_json::json!({"kind": "concept", "title": "mail relay"})), &["cortex:node"], NOW);
    put(&s, "brain", 2, "cortex", "about mail relay", Some(serde_json::json!({"from": "aa", "kind": "about", "to": "bb"})), &["cortex:link"], NOW + 1);
    let mut axxis = Vec::new();
    for i in 0..3 {
        axxis.push(put(&s, "rocky", 3, "axxis", &format!("the mail relay on core sends the support emails, note {i}"), None, &[], NOW + 10 + i));
    }
    let other = put(&s, "rocky", 3, "illuminati", "how the mail relay was first set up for the support emails", None, &[], NOW - 100);
    let (hits, _) = s.search_in("mail relay", None, None, 4, None, Ranker::Rrf, false).unwrap();
    let pos = |id: &str| hits.iter().position(|h| h.obs.id == id).unwrap_or(99);
    assert!(pos(&other) <= 2, "the other room's original comes inside the cap (two #axxis then it), at {}", pos(&other));
    let first: Vec<_> = hits.iter().take(4).map(|h| h.obs.channel.as_str()).collect();
    assert!(!first.contains(&"cortex"), "no scaffolding in the first four: {first:?}");
}

/// The Cortex is what Blaze approved (his ruling via Noor, 28 Oct 2026): the node list shows by default only an
/// observation whose approval counts (attested for him by the registered brain seat) and the concept it links to; the
/// same verdict signed by a seat that is no attester stays out, and `all` lists it with its class, never as approved.
#[test]
fn the_cortex_node_list_is_what_blaze_approved_and_drafts_stay_behind_all() {
    use oracle_server::cortex::{nid_of, nodes, observation_nid};
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "brain", &pubkey_hex(&sk(2)), NOW - 900), NOW - 900).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(3)), NOW - 900), NOW - 900).unwrap();
    s.gateway_add("brain", &["attest".to_string()], NOW - 800).unwrap();
    let obs = |n: u8, quote: &str, at: i64| {
        put(&s, "brain", 2, "cortex", quote, Some(serde_json::json!({"kind": "observation", "title": quote, "quote": quote,
            "source": {"message": format!("OBS-cases-20260126-00000000000{n}"), "start": 0, "end": 9}})), &["cortex:node"], at);
        observation_nid(&format!("OBS-cases-20260126-00000000000{n}"), 0, 9)
    };
    put(&s, "brain", 2, "cortex", "concept", Some(serde_json::json!({"kind": "concept", "title": "state transfer"})), &["cortex:node"], NOW);
    let concept = nid_of("concept", "state transfer");
    // A concept replaced by a newer one: history, not current cortex, even though an approved observation links to it.
    let old_hub = put(&s, "brain", 2, "cortex", "concept", Some(serde_json::json!({"kind": "concept", "title": "transfer of state"})), &["cortex:node"], NOW);
    let old_concept = nid_of("concept", "transfer of state");
    put(&s, "brain", 2, "cortex", "concept", Some(serde_json::json!({"kind": "concept", "title": "state transfer, refined"})), &["cortex:node", &format!("supersedes:{old_hub}")], NOW);
    let (good, forged, draft) = (obs(1, "approved by blaze", NOW + 1), obs(2, "approved by no attester", NOW + 2), obs(3, "never reviewed", NOW + 3));
    put(&s, "brain", 2, "cortex", "about", Some(serde_json::json!({"from": good, "kind": "about", "to": concept})), &["cortex:link"], NOW + 4);
    put(&s, "brain", 2, "cortex", "about", Some(serde_json::json!({"from": good, "kind": "about", "to": old_concept})), &["cortex:link"], NOW + 4);
    put(&s, "brain", 2, "cortex", "approve", Some(serde_json::json!({"target": good, "verdict": "approve", "by": "blaze", "via": "cases-page"})), &["cortex:verdict"], NOW + 5);
    put(&s, "rocky", 3, "cortex", "approve", Some(serde_json::json!({"target": forged, "verdict": "approve", "by": "blaze", "via": "cases-page"})), &["cortex:verdict"], NOW + 6);

    let cortex = nodes(&s, None, None, None, false).unwrap();
    let nids: Vec<&str> = cortex["nodes"].as_array().unwrap().iter().filter_map(|n| n["nid"].as_str()).collect();
    assert_eq!(cortex["approved_observations"], 1);
    assert!(nids.contains(&good.as_str()) && nids.contains(&concept.as_str()), "the approved observation and the concept it links to: {nids:?}");
    assert!(!nids.contains(&forged.as_str()), "a verdict from a seat that is no attester is not Blaze's approval");
    assert!(!nids.contains(&draft.as_str()), "a draft is hippocampus, not cortex");
    assert!(!nids.contains(&old_concept.as_str()), "a superseded concept is history, not current cortex");
    // The filtered count follows the filter; the total stays the whole cortex.
    let concepts = nodes(&s, None, Some("concept"), None, false).unwrap();
    assert_eq!((concepts["approved_observations"].as_u64(), concepts["approved_observations_total"].as_u64()), (Some(0), Some(1)));
    let every = nodes(&s, None, None, None, true).unwrap();
    assert_eq!(every["count"], 6);
    let old_row = every["nodes"].as_array().unwrap().iter().find(|n| n["nid"] == old_concept.as_str()).cloned().unwrap();
    assert!(old_row["superseded_by"].is_string() && old_row["cortex"] == false, "{old_row}");
    let of = |nid: &str| every["nodes"].as_array().unwrap().iter().find(|n| n["nid"] == nid).cloned().unwrap();
    assert_eq!((of(&good)["approval"]["class"].as_str(), of(&good)["cortex"].as_bool()), (Some("attested"), Some(true)));
    assert_eq!((of(&forged)["approval"]["class"].as_str(), of(&forged)["approval"]["counts"].as_bool(), of(&forged)["cortex"].as_bool()), (Some("unverified"), Some(false), Some(false)));
    assert!(of(&draft)["approval"].is_null());
}

/// Reading #cortex whole is its readings (Blaze, 31 Oct: oracle_read of #cortex gave "approved by Blaze" and "about
/// ranking" rows and no reading). The links, verdicts and concept nodes are folded and counted; the approved reading says
/// so; asking for them by tag still lists them; a room that is not #cortex never folds.
#[test]
fn a_cortex_room_read_folds_the_graph_and_marks_what_was_approved() {
    use oracle_server::http::recent_rows_folding;
    use oracle_server::projection::View;
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "brain", &pubkey_hex(&sk(2)), NOW - 900), NOW - 900).unwrap();
    s.gateway_add("brain", &["attest".to_string()], NOW - 800).unwrap();
    let quote = "the hour after the stream is when the audience decides";
    let node = put(&s, "brain", 2, "cortex", quote, Some(serde_json::json!({"kind": "observation", "title": "the hour after",
        "quote": quote, "source": {"message": "OBS-cases-20000126-bbbbbbbbbbbb", "start": 0, "end": 52}})), &["cortex:node"], NOW + 1);
    let nid = oracle_server::cortex::observation_nid("OBS-cases-20000126-bbbbbbbbbbbb", 0, 52);
    let other = put(&s, "brain", 2, "cortex", "an unreviewed reading", Some(serde_json::json!({"kind": "observation", "title": "unreviewed",
        "quote": "an unreviewed reading", "source": {"message": "OBS-cases-20000126-cccccccccccc", "start": 0, "end": 21}})), &["cortex:node"], NOW + 2);
    let concept = put(&s, "brain", 2, "cortex", "streaming", Some(serde_json::json!({"kind": "concept", "title": "streaming"})), &["cortex:node"], NOW + 3);
    put(&s, "brain", 2, "cortex", "about streaming", Some(serde_json::json!({"from": nid, "kind": "about", "to": concept})), &["cortex:link"], NOW + 4);
    put(&s, "brain", 2, "cortex", "approved by Blaze", Some(serde_json::json!({"target": nid, "verdict": "approve", "by": "blaze", "via": "cases-page"})), &["cortex:verdict"], NOW + 5);
    let view = View::parse(None, None);
    let approved = s.approved_index().unwrap();

    let (rows, _, _, folded) = recent_rows_folding(&s, Some("cortex"), 20, None, None, None, &view, NOW + 10, Some(&approved)).unwrap();
    let ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(ids, [other.as_str(), node.as_str()], "only the readings, newest first");
    assert_eq!(folded, 3, "the concept, the link and the verdict are counted");
    assert_eq!(rows[1]["approved"], true, "the approved reading says so");
    assert!(rows[0].get("approved").is_none(), "an unreviewed reading carries no mark");

    // Control: without folding the room reads as stored; asked by tag, the links are there.
    let (all, _, _, none) = recent_rows_folding(&s, Some("cortex"), 20, None, None, None, &view, NOW + 10, None).unwrap();
    assert_eq!((all.len(), none), (5, 0));
    let (links, _, _, _) = recent_rows_folding(&s, Some("cortex"), 20, None, Some("cortex:link"), None, &view, NOW + 10, None).unwrap();
    assert_eq!(links.len(), 1);
    // The room index pages (AXXIS browse) fold the same records and mark the same reading; scaffold=true lists them all.
    use oracle_server::roomread::{messages, threads, PageQ};
    let page = messages(&s, "cortex", &PageQ::default()).unwrap();
    let ids: Vec<&str> = page["items"].as_array().unwrap().iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(ids, [other.as_str(), node.as_str()]);
    assert_eq!(page["items"][1]["approved"], true);
    assert!(page["folded"].is_string());
    let whole = messages(&s, "cortex", &PageQ { scaffold: true, ..PageQ::default() }).unwrap();
    assert_eq!(whole["items"].as_array().unwrap().len(), 5);
    assert!(whole.get("folded").is_none());
    let roots: Vec<String> = threads(&s, "cortex", &PageQ::default()).unwrap()["threads"].as_array().unwrap().iter()
        .filter_map(|t| t["root"]["id"].as_str().map(str::to_owned)).collect();
    assert_eq!(roots, [other.clone(), node.clone()]);
    // A concept node in another room is not the cortex's scaffolding.
    let elsewhere = put(&s, "brain", 2, "oracle", "streaming", Some(serde_json::json!({"kind": "concept"})), &["cortex:node"], NOW + 6);
    let o = s.get(&elsewhere).unwrap();
    assert!(!oracle_server::store::is_scaffold(&o));
}

/// A node holds what Blaze approved (Noor, 31 Oct 2026, read every entity as "0 approved marks", the old claim-mark
/// count): `readings` counts the approved readings that link to it; an unapproved reading and a said-by link do not count.
#[test]
fn a_concept_counts_the_approved_readings_that_link_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("o.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "brain", &pubkey_hex(&sk(2)), NOW - 900), NOW - 900).unwrap();
    s.gateway_add("brain", &["attest".to_string()], NOW - 800).unwrap();
    put(&s, "brain", 2, "cortex", "concept: business models", Some(serde_json::json!({"kind": "concept", "title": "business models"})), &["cortex:node"], NOW);
    let concept = oracle_server::cortex::nid_of("concept", "business models");
    let reading = |i: u8, quote: &str| {
        let msg = format!("OBS-cases-20260126-{i:012}");
        put(&s, "brain", 2, "cortex", quote, Some(serde_json::json!({"kind": "observation", "title": quote, "quote": quote,
            "source": {"message": msg, "start": 0, "end": quote.len()}})), &["cortex:node"], NOW + i as i64);
        oracle_server::cortex::observation_nid(&msg, 0, quote.len() as u64)
    };
    let (a, b) = (reading(1, "the escrow that also learns"), reading(2, "a draft nobody approved"));
    for (from, kind) in [(&a, "about"), (&b, "about"), (&a, "said-by")] {
        put(&s, "brain", 2, "cortex", kind, Some(serde_json::json!({"from": from, "kind": kind, "to": concept})), &["cortex:link"], NOW + 10);
    }
    put(&s, "brain", 2, "cortex", "approved by Blaze", Some(serde_json::json!({"target": a, "verdict": "approve", "by": "blaze", "via": "cases-page"})), &["cortex:verdict"], NOW + 11);
    let v = oracle_server::cortex::nodes(&s, None, Some("concept"), None, false).unwrap();
    let n = v["nodes"].as_array().unwrap().iter().find(|n| n["title"] == "business models").expect("the concept an approved reading links to is in the cortex");
    assert_eq!(n["readings"], 1, "one approved reading; the draft and the said-by link do not count: {n}");
}
