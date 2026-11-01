//! The claims fold (Round 43): mint, edge and status are signed messages; the table is folded on read.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::claims::{cid_of, read};
use oracle_server::store::{Principal, Scope, Store};

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "noor", &pubkey_hex(&sk(2)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    s
}
fn put(s: &Store, tag: &str, cargo: serde_json::Value, ago: i64) -> String {
    let at = NOW - ago * MIN;
    let env = Envelope::sign(&sk(2), "core", "noor", Kind::Say, "claim", Some(cargo.to_string()), vec![tag.to_string()], at);
    let mut env = env;
    env.channel = "mathematics".into();
    let env = Envelope::sign(&sk(2), &env.channel.clone(), "noor", Kind::Say, "claim", Some(cargo.to_string()), vec![tag.to_string()], at);
    s.write(&Principal { handle: "noor".into(), scope: Scope::Write }, &env, at).unwrap().id
}

#[test]
fn claims_are_minted_linked_and_moved_by_signed_messages() {
    let s = oracle();
    let wrong = "The Zenodo token is missing.";
    let right = "The Zenodo token is present, set 2026-05-15.";
    put(&s, "claim:mint", serde_json::json!({"statement": wrong, "type": "external", "status": "open", "row": "C16"}), 50);
    put(&s, "claim:mint", serde_json::json!({"statement": right, "type": "external", "status": "verified", "sources": ["OBS-coordination-20260628-327336841"]}), 40);
    let (w, r) = (cid_of(wrong), cid_of(right));
    put(&s, "claim:edge", serde_json::json!({"from": &r[..12], "to": &w[..12], "kind": "supersedes"}), 30);
    put(&s, "claim:status", serde_json::json!({"cid": &w[..10], "status": "refuted", "why": "Asa, 28 Jun"}), 20);
    // Negative controls: a second mint of the same words (spacing differs) and an edge to nothing are rejected, and said so.
    put(&s, "claim:mint", serde_json::json!({"statement": "The  Zenodo token is missing. ", "type": "external", "status": "open"}), 10);
    put(&s, "claim:edge", serde_json::json!({"from": &r[..12], "to": "deadbeefdeadbeef", "kind": "supports"}), 5);
    let all = read(&s, Some("mathematics"), None, None).unwrap();
    assert_eq!(all.claims.len(), 2);
    let old = all.claims.iter().find(|c| c.cid == w).unwrap();
    assert_eq!((old.status.as_str(), old.history.len(), old.superseded_by.as_deref(), old.row.as_deref()), ("refuted", 2, Some(r.as_str()), Some("C16")));
    let new = all.claims.iter().find(|c| c.cid == r).unwrap();
    assert_eq!((new.out.len(), new.out[0].kind.as_str(), new.sources.len()), (1, "supersedes", 1));
    assert_eq!(all.rejected.len(), 2, "{:?}", all.rejected);
    assert_eq!(read(&s, None, Some("verified"), None).unwrap().claims.len(), 1);
    assert_eq!(read(&s, None, None, Some(&r[..8])).unwrap().claims.len(), 1);
}

#[test]
fn the_hallway_lists_each_room_with_its_oldest_open_ask() {
    let s = oracle();
    let at = NOW - 30 * MIN;
    let ask = |text: &str, tags: Vec<String>, ago: i64| {
        let env = Envelope::sign(&sk(2), "mathematics", "noor", Kind::Say, text, None, tags, NOW - ago * MIN);
        s.write(&Principal { handle: "noor".into(), scope: Scope::Write }, &env, NOW - ago * MIN).unwrap().id
    };
    let first = ask("first: which seats are live?", vec!["ask:reply".into(), "to:blaze".into()], 40);
    ask("second: and when?", vec!["ask:reply".into(), "to:blaze".into()], 35);
    ask("a note", vec![], 33);
    let v = oracle_server::hallway::read(&s, 14, at).unwrap();
    let room = v["rooms"].as_array().unwrap().iter().find(|r| r["room"] == "mathematics").unwrap().clone();
    assert_eq!((room["open_asks"].as_i64(), room["oldest_open_ask"]["id"].as_str()), (Some(2), Some(first.as_str())), "{room}");
    assert_eq!(room["oldest_open_ask"]["to"][0], "blaze");
    // Negative control: no State signed, and the hallway says so rather than inventing one.
    assert_eq!((room["has_state"].as_bool(), room["current"].is_null()), (Some(false), true));
}

#[test]
fn search_folds_copies_of_one_body_into_one_hit() {
    let s = oracle();
    let turn = "THREE SENSES OF CHANNEL: Shannon's pipe, the Oracle's ledger, and the self-addressing loom; each one carries a different meaning of capacity.";
    let say = |text: String, ago: i64| {
        let env = Envelope::sign(&sk(2), "mathematics", "noor", Kind::Say, &text, None, vec![], NOW - ago * MIN);
        s.write(&Principal { handle: "noor".into(), scope: Scope::Write }, &env, NOW - ago * MIN).unwrap().id
    };
    let a = say(format!("[CLAUDE-EXPORT · blaze · 2026-05-13 · room:a]\n{turn}"), 30);
    let b = say(format!("[CLAUDE-EXPORT · blaze · 2026-05-23 · room:b]\n{turn}"), 20);
    let other = say("Channel capacity in the other sense: how many seats one room can hold before the floor gets noisy, measured in rounds.".into(), 10);
    let hits = s.search("channel capacity", None, None, 10).unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.obs.id.as_str()).collect();
    assert_eq!(hits.len(), 2, "two bodies, three records: {ids:?}");
    let folded = hits.iter().find(|h| h.obs.id == a || h.obs.id == b).unwrap();
    assert_eq!(folded.same_as.len(), 1, "the copy is named, not dropped");
    assert!(ids.contains(&other.as_str()), "negative control: a different body stays its own hit");
}

/// Cascading refutation (TASK-OAI-1): ETH refuted moves what rests on it, directly or through a lemma, to standing open; a claim
/// re-verified after the fall, a claim that rests on nothing refuted, and the signed status itself stay as they were.
#[test]
fn a_refuted_root_leaves_what_rests_on_it_standing_open_until_re_verified() {
    let s = oracle();
    let eth = "ETH: 3-SAT needs time 2^(delta n) for some delta > 0.";
    let lemma = "Lemma: k-Clique needs n^(Omega(k)) time, assuming ETH.";
    let far = "Graph result resting on the k-Clique lemma.";
    let again = "A result that cites ETH and was re-checked after the refutation.";
    let free = "An unconditional result.";
    for (st, ago) in [(eth, 90), (lemma, 89), (far, 88), (again, 87), (free, 86)] {
        put(&s, "claim:mint", serde_json::json!({"statement": st, "type": "external", "status": "verified"}), ago);
    }
    let (e, l, f, a, u) = (cid_of(eth), cid_of(lemma), cid_of(far), cid_of(again), cid_of(free));
    put(&s, "claim:edge", serde_json::json!({"from": &l[..12], "to": &e[..12], "kind": "cites"}), 80);
    put(&s, "claim:edge", serde_json::json!({"from": &l[..12], "to": &f[..12], "kind": "supports"}), 79);
    put(&s, "claim:edge", serde_json::json!({"from": &a[..12], "to": &e[..12], "kind": "cites"}), 78);
    put(&s, "claim:edge", serde_json::json!({"from": &u[..12], "to": &f[..12], "kind": "contradicts"}), 77);
    put(&s, "claim:status", serde_json::json!({"cid": &e[..12], "status": "refuted", "why": "OpenAI 378, Lean"}), 50);
    put(&s, "claim:status", serde_json::json!({"cid": &a[..12], "status": "verified", "why": "re-checked: needs only P != NP"}), 40);
    let all = read(&s, Some("mathematics"), None, None).unwrap();
    let get = |cid: &str| all.claims.iter().find(|c| c.cid == cid).unwrap();
    assert_eq!((get(&e).status.as_str(), get(&e).standing.as_str()), ("refuted", "refuted"));
    let lm = get(&l);
    assert_eq!((lm.status.as_str(), lm.standing.as_str()), ("verified", "open"), "the signed status is untouched; the standing moves");
    assert!(lm.fallen.as_ref().unwrap().why.starts_with(&format!("root claim {} refuted", &e[..12])));
    let fr = get(&f).fallen.as_ref().expect("two steps down, through the lemma");
    assert_eq!((fr.root.as_str(), fr.path.len()), (e.as_str(), 2));
    assert_eq!(get(&a).standing, "verified", "re-verified after the fall: it stands");
    assert_eq!(get(&u).standing, "verified", "contradicts is not resting on");
    // Control: the root back to open lifts the cascade.
    put(&s, "claim:status", serde_json::json!({"cid": &e[..12], "status": "open", "why": "control"}), 10);
    let after = read(&s, Some("mathematics"), None, None).unwrap();
    assert!(after.claims.iter().all(|c| c.fallen.is_none()), "no refuted root, nothing fallen");
}

#[test]
fn a_source_is_added_to_a_minted_claim_once_and_only_from_its_room() {
    // Blaze, 30 Oct 2026: claims editable through Oracle. #io's claims named their papers only in the words; a source is
    // added beside the claim, so its cid and every link to it stay.
    let s = oracle();
    let words = "Indistinguishability obfuscation from LPN over F_p, DLIN and constant-locality PRGs (Jain, Lin, Sahai, EUROCRYPT 2022).";
    put(&s, "claim:mint", serde_json::json!({"statement": words, "type": "external", "status": "verified"}), 50);
    let cid = cid_of(words);
    put(&s, "claim:source", serde_json::json!({"cid": &cid[..12], "sources": ["https://eprint.iacr.org/2021/1334"]}), 40);
    put(&s, "claim:source", serde_json::json!({"cid": &cid[..12], "sources": ["https://eprint.iacr.org/2021/1334", " https://doi.org/10.1007/978-3-031-06944-4_23 "]}), 30);
    // Negative controls: no claim with that cid, a source with no list, and a write from another room are refused, and said so.
    put(&s, "claim:source", serde_json::json!({"cid": "deadbeefdeadbeef", "sources": ["https://example.org"]}), 20);
    put(&s, "claim:source", serde_json::json!({"cid": &cid[..12]}), 15);
    let at = NOW - 10 * MIN;
    let env = Envelope::sign(&sk(2), "kakeya", "noor", Kind::Say, "claim", Some(serde_json::json!({"cid": &cid[..12], "sources": ["https://elsewhere.example"]}).to_string()), vec!["claim:source".into()], at);
    s.write(&Principal { handle: "noor".into(), scope: Scope::Write }, &env, at).unwrap();
    let all = read(&s, None, None, Some(&cid[..12])).unwrap();
    let c = &all.claims[0];
    assert_eq!(c.sources, vec!["https://eprint.iacr.org/2021/1334".to_string(), "https://doi.org/10.1007/978-3-031-06944-4_23".to_string()]);
    assert_eq!(c.cid, cid, "the claim keeps its cid");
    assert_eq!(all.rejected.len(), 3, "{:?}", all.rejected);
}
