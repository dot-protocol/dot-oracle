//! Cited by (docs/CORTEX.md): who among Blaze-approved observation nodes points at a record. Each case is one way the
//! count could lie: drafts, duplicates, withdrawn links, archived or rejected nodes, cycles, families, the speaker,
//! the listing order and the snapshot watermark (Emmy's contract).
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::cortex::observation_nid;
use oracle_server::store::{Principal, Scope, Store};
use serde_json::{json, Value};

const NOW: i64 = 1_790_000_000_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

struct T {
    _d: tempfile::TempDir,
    s: Store,
    at: std::cell::Cell<i64>,
}

impl T {
    fn new() -> T {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("o.db").to_str().unwrap()).unwrap();
        s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "brain", &pubkey_hex(&sk(2)), NOW - 900), NOW - 900).unwrap();
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(3)), NOW - 900), NOW - 900).unwrap();
        s.gateway_add("brain", &["attest".to_string()], NOW - 800).unwrap();
        T { _d: d, s, at: std::cell::Cell::new(NOW) }
    }
    fn put(&self, room: &str, text: &str, cargo: Value, tags: &[&str]) -> String {
        self.put_as(2, "brain", room, text, cargo, tags)
    }
    /// A record signed by key `k` as `handle` (1 = blaze, the owner; 2 = brain, the registered attester; 3 = rocky).
    fn put_as(&self, k: u8, handle: &str, room: &str, text: &str, cargo: Value, tags: &[&str]) -> String {
        let at = self.at.get() + 1;
        self.at.set(at);
        let e = Envelope::sign(&sk(k), room, handle, Kind::Say, text, Some(cargo.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
        self.s.write(&Principal { handle: handle.into(), scope: Scope::Write }, &e, at).unwrap().id
    }
    /// An observation node as cortex/graph.py publishes it; `reply` set means the span is in Claude's words.
    fn node(&self, chat: Option<&str>, start: u64, reply: Option<&str>) -> (String, String) {
        let message = format!("{}/p{start}", chat.unwrap_or("OBS-cases-x"));
        let mut source = json!({"message": message, "start": start, "end": start + 10, "prompt": format!("p{start}"), "reply": reply});
        if let Some(c) = chat {
            source["chat"] = json!(c);
        }
        let id = self.put("cortex", "a quote", json!({"kind": "observation", "title": "a quote", "quote": "a quote", "source": source, "approved_by": "blaze"}), &["cortex:node"]);
        (id, observation_nid(&message, start, start + 10))
    }
    fn approve(&self, nid: &str) -> String {
        self.verdict(nid, "approve")
    }
    fn verdict(&self, nid: &str, v: &str) -> String {
        self.put("cortex", "verdict", json!({"target": nid, "verdict": v, "by": "blaze", "via": "cases-page"}), &["cortex:verdict"])
    }
    fn link(&self, from: &str, to: &str, kind: &str) -> String {
        self.put("cortex", kind, json!({"from": from, "to": to, "kind": kind}), &["cortex:link"])
    }
    fn unlink(&self, from: &str, to: &str, kind: &str) {
        self.put("cortex", kind, json!({"from": from, "to": to, "kind": kind, "removed": true}), &["cortex:link"]);
    }
    fn record(&self) -> String {
        self.put("axxis", "a hippocampus record", json!({}), &[])
    }
    fn cites(&self, id: &str) -> Option<Value> {
        self.s.citations(&[id.to_string()]).unwrap().0.remove(id)
    }
}

#[test]
fn an_approved_node_cites_a_record_with_speaker_family_kind_and_approval() {
    let t = T::new();
    let rec = t.record();
    let (node, nid) = t.node(Some("OBS-cases-chatA"), 0, None);
    let verdict = t.approve(&nid);
    t.link(&nid, &rec, "generates");
    let c = t.cites(&rec).expect("one citation");
    assert_eq!(c["status"], "measured");
    assert_eq!(c["count"], 1);
    assert_eq!(c["families"], 1);
    let by = &c["by"][0];
    assert_eq!(by["node"], node.as_str());
    assert_eq!(by["said_by"], "blaze");
    assert_eq!(by["chat"], "OBS-cases-chatA");
    assert_eq!(by["kinds"], json!(["generates"]));
    assert_eq!(by["approval"]["verdict"], verdict.as_str());
    assert_eq!(by["approval"]["by"], "blaze");
    assert_eq!(by["approval"]["class"], "attested", "the brain seat attests Blaze's approval in the page; it is not his own signature");
    assert_eq!(by["approval"]["via"], "cases-page");
    assert_eq!(c["attestation"], json!({"owner": 0, "attested": 1, "agent": 0, "unverified": 0}));
    assert_eq!(c["truncated"], false);
    assert!(c["through_seq"].as_i64().unwrap() > 0);
}

#[test]
fn a_draft_or_unapproved_node_counts_zero() {
    let t = T::new();
    let rec = t.record();
    let (_, nid) = t.node(Some("c"), 0, None);
    t.link(&nid, &rec, "about");
    assert!(t.cites(&rec).is_none(), "no verdict: a measured zero");
}

#[test]
fn a_node_rejected_after_approval_counts_zero() {
    let t = T::new();
    let rec = t.record();
    let (_, nid) = t.node(Some("c"), 0, None);
    t.approve(&nid);
    t.link(&nid, &rec, "about");
    t.verdict(&nid, "reject");
    assert!(t.cites(&rec).is_none(), "the newest verdict wins");
}

#[test]
fn duplicate_links_count_once_and_keep_every_kind() {
    let t = T::new();
    let rec = t.record();
    let (_, nid) = t.node(Some("c"), 0, None);
    t.approve(&nid);
    t.link(&nid, &rec, "generates");
    t.link(&nid, &rec, "generates");
    t.link(&nid, &rec, "about");
    let c = t.cites(&rec).unwrap();
    assert_eq!(c["count"], 1);
    assert_eq!(c["by"][0]["kinds"], json!(["about", "generates"]));
}

#[test]
fn a_withdrawn_link_and_an_archived_node_drop_out() {
    let t = T::new();
    let rec = t.record();
    let (_, a) = t.node(Some("c1"), 0, None);
    t.approve(&a);
    t.link(&a, &rec, "about");
    t.unlink(&a, &rec, "about");
    let (bnode, b) = t.node(Some("c2"), 20, None);
    t.approve(&b);
    t.link(&b, &rec, "about");
    t.put("cortex", "withdrawn", json!({}), &[&format!("archives:{bnode}")]);
    assert!(t.cites(&rec).is_none(), "withdrawn link and archived node: zero");
}

#[test]
fn a_cycle_counts_each_side_once_and_never_itself() {
    let t = T::new();
    let (anode, a) = t.node(Some("c1"), 0, None);
    let (bnode, b) = t.node(Some("c2"), 20, None);
    t.approve(&a);
    t.approve(&b);
    t.link(&a, &b, "generates");
    t.link(&b, &a, "refines");
    let ca = t.cites(&anode).unwrap();
    assert_eq!(ca["count"], 1);
    assert_eq!(ca["by"][0]["node"], bnode.as_str());
    let cb = t.cites(&bnode).unwrap();
    assert_eq!(cb["count"], 1);
    assert_eq!(cb["by"][0]["node"], anode.as_str());
}

#[test]
fn families_count_chats_and_unknown_chats_never_merge() {
    let t = T::new();
    let rec = t.record();
    for (chat, start) in [(Some("same"), 0), (Some("same"), 20), (None, 40), (None, 60)] {
        let (_, n) = t.node(chat, start, None);
        t.approve(&n);
        t.link(&n, &rec, "about");
    }
    let c = t.cites(&rec).unwrap();
    assert_eq!(c["count"], 4);
    assert_eq!(c["families"], 3, "one known chat, two unknown ones kept apart");
}

#[test]
fn claude_words_read_as_claude() {
    let t = T::new();
    let rec = t.record();
    let (_, n) = t.node(Some("c"), 0, Some("reply-uuid"));
    t.approve(&n);
    t.link(&n, &rec, "about");
    assert_eq!(t.cites(&rec).unwrap()["by"][0]["said_by"], "claude");
}

#[test]
fn the_list_puts_one_node_per_family_first_and_is_truncated_at_five() {
    let t = T::new();
    let rec = t.record();
    let mut ids = Vec::new();
    for i in 0..7u64 {
        let chat = if i < 5 { "busy".to_string() } else { format!("quiet{i}") };
        let (id, n) = t.node(Some(&chat), i * 20, None);
        t.approve(&n);
        t.link(&n, &rec, "about");
        ids.push((id, chat));
    }
    let c = t.cites(&rec).unwrap();
    assert_eq!(c["count"], 7);
    assert_eq!(c["families"], 3);
    assert_eq!(c["truncated"], true);
    let by: Vec<&str> = c["by"].as_array().unwrap().iter().map(|b| b["chat"].as_str().unwrap()).collect();
    assert_eq!(by.len(), 5);
    let mut first3: Vec<&str> = by[..3].to_vec();
    first3.sort();
    first3.dedup();
    assert_eq!(first3.len(), 3, "the first three are three different families: {by:?}");
}

#[test]
fn the_watermark_covers_every_record_written_before_the_read() {
    let t = T::new();
    let rec = t.record();
    let (_, n) = t.node(Some("c"), 0, None);
    t.approve(&n);
    t.link(&n, &rec, "about");
    let (map, through) = t.s.citations(std::slice::from_ref(&rec)).unwrap();
    let last = t.put("axxis", "later", json!({}), &[]);
    let _ = last;
    assert_eq!(map[&rec]["through_seq"].as_i64().unwrap(), through);
    let (_, later) = t.s.citations(&[rec]).unwrap();
    assert!(later > through, "a later read has a later watermark");
}

/// Emmy's review of 01c2b60: `signed_by == by` is attribution agreement, not owner
/// authority. Each approval is classed against the key registry and the gateways in the same snapshot.
fn cited_once(t: &T, cargo: Value, k: u8, handle: &str) -> Option<Value> {
    let (_, nid) = t.node(Some("chat-a"), 0, None);
    let mut v = cargo;
    v["target"] = json!(nid);
    v["verdict"] = json!("approve");
    t.put_as(k, handle, "cortex", "verdict", v, &["cortex:verdict"]);
    let rec = t.record();
    t.link(&nid, &rec, "about");
    t.cites(&rec)
}

#[test]
fn an_approval_signed_by_the_owner_key_is_owner_class() {
    let t = T::new();
    let c = cited_once(&t, json!({"by": "blaze"}), 1, "blaze").expect("an owner-signed approval counts");
    assert_eq!((c["count"].as_u64(), c["by"][0]["approval"]["class"].as_str()), (Some(1), Some("owner")));
    assert_eq!(c["attestation"]["owner"], 1);
}

#[test]
fn an_agent_approving_in_its_own_name_counts_zero() {
    let t = T::new();
    let c = cited_once(&t, json!({"by": "brain"}), 2, "brain").expect("reported, not hidden");
    assert_eq!(c["count"], 0, "brain signing {{by: brain, verdict: approve}} is not Blaze's approval");
    assert_eq!(c["by"], json!([]));
    assert_eq!(c["attestation"], json!({"owner": 0, "attested": 0, "agent": 1, "unverified": 0}));
}

#[test]
fn a_seat_that_is_no_registered_attester_cannot_sign_for_the_owner() {
    let t = T::new();
    let c = cited_once(&t, json!({"by": "blaze", "via": "cases-page"}), 3, "rocky").expect("reported");
    assert_eq!((c["count"].as_u64(), c["attestation"]["unverified"].as_u64()), (Some(0), Some(1)), "a forged by: blaze from rocky");
}

#[test]
fn an_attestation_names_the_page_and_an_owner() {
    let t = T::new();
    let c = cited_once(&t, json!({"by": "blaze"}), 2, "brain").expect("reported");
    assert_eq!(c["count"], 0, "no via: the attester names no page the owner acted in");
    let t = T::new();
    let c = cited_once(&t, json!({"by": "rocky", "via": "cases-page"}), 2, "brain").expect("reported");
    assert_eq!(c["count"], 0, "rocky holds no owner key, so the brain cannot attest an approval in his name");
}

#[test]
fn removing_the_attester_withdraws_its_attestations() {
    let t = T::new();
    let (_, nid) = t.node(Some("chat-a"), 0, None);
    t.approve(&nid);
    let rec = t.record();
    t.link(&nid, &rec, "about");
    assert_eq!(t.cites(&rec).unwrap()["count"], 1);
    assert!(t.s.gateway_remove("brain").unwrap());
    let c = t.cites(&rec).unwrap();
    assert_eq!((c["count"].as_u64(), c["attestation"]["unverified"].as_u64()), (Some(0), Some(1)));
}

#[test]
fn a_superseded_node_cites_nothing_and_its_replacement_counts() {
    let t = T::new();
    let rec = t.record();
    let (old_id, old) = t.node(Some("chat-a"), 0, None);
    t.approve(&old);
    t.link(&old, &rec, "about");
    // the same mark republished on a corrected span: a new node that supersedes the old node's record
    let msg = "chat-a/p0";
    let new_id = t.put("cortex", "a quote", json!({"kind": "observation", "title": "a quote", "quote": "a quote",
        "source": {"message": msg, "start": 2, "end": 12, "prompt": "p0", "chat": "chat-a"}, "approved_by": "blaze"}),
        &["cortex:node", &format!("supersedes:{old_id}")]);
    let new = observation_nid(msg, 2, 12);
    assert!(t.cites(&rec).is_none(), "the superseded node no longer cites");
    t.approve(&new);
    t.link(&new, &rec, "about");
    let c = t.cites(&rec).unwrap();
    assert_eq!((c["count"].as_u64(), c["by"][0]["node"].as_str()), (Some(1), Some(new_id.as_str())));
}

#[test]
fn a_revision_of_the_same_node_never_supersedes_it() {
    let t = T::new();
    let rec = t.record();
    let (old_id, nid) = t.node(Some("chat-a"), 0, None);
    t.put("cortex", "a quote", json!({"kind": "observation", "title": "a quote, retitled", "quote": "a quote",
        "source": {"message": "chat-a/p0", "start": 0, "end": 10, "prompt": "p0", "chat": "chat-a"}, "approved_by": "blaze"}),
        &["cortex:node", &format!("supersedes:{old_id}")]);
    t.approve(&nid);
    t.link(&nid, &rec, "about");
    assert_eq!(t.cites(&rec).unwrap()["count"], 1, "same nid: a revision, the node stands");
}

#[test]
fn authority_changes_recompute_citations_without_new_observations() {
    let t = T::new();
    let (_, nid) = t.node(Some("chat-a"), 0, None);
    t.approve(&nid);
    let rec = t.record();
    t.link(&nid, &rec, "about");
    let (before, seq_before) = t.s.citations(std::slice::from_ref(&rec)).unwrap();
    assert_eq!(before[&rec]["count"], 1);
    assert!(t.s.gateway_remove("brain").unwrap());
    let (after, seq_after) = t.s.citations(std::slice::from_ref(&rec)).unwrap();
    assert_eq!(after[&rec]["count"], 0);
    assert_eq!(seq_before, seq_after, "through_seq covers observations, not authority configuration");
    t.s.gateway_add("brain", &["attest".to_string()], NOW + 1000).unwrap();
    let (restored, seq_restored) = t.s.citations(std::slice::from_ref(&rec)).unwrap();
    assert_eq!(restored[&rec]["count"], 1, "current registration applies to historical attestations too");
    assert_eq!(seq_before, seq_restored);
}

/// Step 2 (Emmy): search hits carry cited_by from one read per response; an
/// unverified approval is reported, never counted; a withdrawn link drops out; a failed read leaves recall whole.
fn hits_for(t: &T, q: &str) -> Vec<oracle_server::store::SearchHit> {
    t.s.search_in(q, None, None, 10, None, oracle_server::store::Ranker::parse(None), false).unwrap().0
}

#[test]
fn search_hits_carry_cited_by_once_per_response() {
    let t = T::new();
    let rec = t.put("axxis", "the harness for humans record", json!({}), &[]);
    let (_, nid) = t.node(Some("chat-a"), 0, None);
    t.approve(&nid);
    t.link(&nid, &rec, "about");
    let mut hits = hits_for(&t, "harness humans record");
    let ids: Vec<String> = hits.iter().map(|h| h.obs.id.clone()).collect();
    let status = oracle_server::store::enrich_cited_by(&mut hits, t.s.citations(&ids));
    assert_eq!(status["status"], "measured");
    let h = hits.iter().find(|h| h.obs.id == rec).expect("the record is a hit");
    assert_eq!(h.cited_by.as_ref().unwrap()["count"], 1);
    t.unlink(&nid, &rec, "about");
    let mut hits = hits_for(&t, "harness humans record");
    let ids: Vec<String> = hits.iter().map(|h| h.obs.id.clone()).collect();
    oracle_server::store::enrich_cited_by(&mut hits, t.s.citations(&ids));
    assert!(hits.iter().find(|h| h.obs.id == rec).unwrap().cited_by.is_none(), "withdrawn: a measured zero, absent");
}

#[test]
fn an_unverified_approval_shows_on_the_hit_but_counts_zero() {
    let t = T::new();
    let rec = t.put("axxis", "the unverified citation record", json!({}), &[]);
    let (_, nid) = t.node(Some("chat-a"), 0, None);
    t.put_as(3, "rocky", "cortex", "verdict", json!({"target": nid, "verdict": "approve", "by": "blaze", "via": "cases-page"}), &["cortex:verdict"]);
    t.link(&nid, &rec, "about");
    let mut hits = hits_for(&t, "unverified citation record");
    let ids: Vec<String> = hits.iter().map(|h| h.obs.id.clone()).collect();
    oracle_server::store::enrich_cited_by(&mut hits, t.s.citations(&ids));
    let c = hits.iter().find(|h| h.obs.id == rec).unwrap().cited_by.clone().unwrap();
    assert_eq!((c["count"].as_u64(), c["attestation"]["unverified"].as_u64()), (Some(0), Some(1)));
}

#[test]
fn a_failed_citation_read_leaves_recall_whole_and_says_unavailable() {
    let t = T::new();
    t.put("axxis", "the isolated failure record", json!({}), &[]);
    let mut hits = hits_for(&t, "isolated failure record");
    let n = hits.len();
    assert!(n > 0, "the failure test must exercise real search hits");
    let before: Vec<_> = hits.iter().map(|h| (h.obs.id.clone(), h.score)).collect();
    hits[0].cited_by = Some(json!({"count": 99}));
    let status = oracle_server::store::enrich_cited_by(&mut hits, Err(oracle_server::store::StoreError::NotFound));
    assert_eq!(status["status"], "unavailable");
    assert_eq!(hits.len(), n, "every hit still returned");
    let after: Vec<_> = hits.iter().map(|h| (h.obs.id.clone(), h.score)).collect();
    assert_eq!(before, after, "enrichment failure preserves hit order and scores");
    assert!(hits.iter().all(|h| h.cited_by.is_none()), "unavailable is never a zero");
}

/// The projection is cached by its inputs (Store::cite_key); every input change must be seen on the next read.
#[test]
fn the_cache_never_serves_a_stale_answer() {
    let t = T::new();
    let rec = t.record();
    let (node_id, nid) = t.node(Some("chat-a"), 0, None);
    t.approve(&nid);
    t.link(&nid, &rec, "about");
    assert_eq!(t.cites(&rec).unwrap()["count"], 1, "built");
    assert_eq!(t.cites(&rec).unwrap()["count"], 1, "served from the cache");
    t.verdict(&nid, "reject");
    assert!(t.cites(&rec).is_none(), "a new verdict is seen");
    t.approve(&nid);
    assert_eq!(t.cites(&rec).unwrap()["count"], 1, "approved again");
    let (_, nid2) = t.node(Some("chat-b"), 0, None);
    t.approve(&nid2);
    t.link(&nid2, &rec, "generates");
    assert_eq!(t.cites(&rec).unwrap()["count"], 2, "a new link is seen");
    t.put("cortex", "archive", json!({}), &[&format!("archives:{node_id}")]);
    assert_eq!(t.cites(&rec).unwrap()["count"], 1, "an archive (no cortex row) is seen");
    assert!(t.s.gateway_remove("brain").unwrap());
    assert_eq!(t.cites(&rec).unwrap()["count"], 0, "an attester taken away is seen");
}
