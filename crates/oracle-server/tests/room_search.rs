//! Step 3d of docs/ROOM-INDEX.md, route 5: search as a page of the room's own items. A room, an author and a class are asked for by name; matches page newest
//! first by seq like every list, or come back ranked (the best `limit`, and it says when there were more). Real signed writes on the real store.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::roomread::{search, SearchQ};
use oracle_server::store::{LegacyRow, Principal, Scope, Store, StoreError};
use serde_json::Value;

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn key_of(who: &str) -> u8 {
    match who {
        "blaze" => 1,
        "rocky" => 2,
        "dinesh" => 3,
        _ => 4,
    }
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3), ("kepler", 4)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    }
    s
}
fn say(s: &Store, room: &str, who: &str, text: &str, tags: &[&str], mins: i64) -> String {
    let at = NOW + mins * MIN;
    let env = Envelope::sign(&sk(key_of(who)), room, who, Kind::Say, text, None, tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn ids(v: &Value) -> Vec<String> {
    v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_owned()).collect()
}
fn q(text: &str) -> SearchQ {
    SearchQ { q: text.into(), ..Default::default() }
}
fn refused(r: Result<Value, StoreError>) -> String {
    match r {
        Err(StoreError::Rejected(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_search_is_scoped_to_the_room_the_author_and_the_class_it_is_asked_for() {
    let s = oracle();
    let core1 = say(&s, "core", "rocky", "the needle is in core", &[], 0);
    let core2 = say(&s, "core", "dinesh", "another needle in core", &["intent:status"], 1);
    let ops = say(&s, "ops", "rocky", "a needle in ops", &[], 2);
    s.import_legacy(&[LegacyRow {
        id: "OBS-core-old".into(), channel: "core".into(), author: "rocky".into(), kind: "say".into(), content: "an old needle".into(), cargo: None, tags: vec![],
        created_ms: NOW - 9000 * MIN, pubkey: String::new(), sig: String::new(), news: false, source: None, repair_legacy: false,
    }]).unwrap();
    let all = search(&s, &q("needle")).unwrap();
    assert_eq!(all["count"], 4, "no room named: the whole ledger");
    let core = search(&s, &SearchQ { room: Some("core".into()), ..q("needle") }).unwrap();
    let mut got = ids(&core);
    got.sort();
    let mut want = vec![core1.clone(), core2.clone(), "OBS-core-old".to_string()];
    want.sort();
    assert_eq!(got, want, "a room's search is that room's: its history too, never ops");
    assert!(ids(&core).iter().all(|i| *i != ops));
    let by_author = search(&s, &SearchQ { room: Some("core".into()), author: Some("dinesh".into()), ..q("needle") }).unwrap();
    assert_eq!(ids(&by_author), vec![core2.clone()]);
    let by_class = search(&s, &SearchQ { room: Some("core".into()), class: vec!["status".into()], ..q("needle") }).unwrap();
    assert_eq!(ids(&by_class), vec![core2.clone()]);
    let two_classes = search(&s, &SearchQ { room: Some("core".into()), class: vec!["status".into(), "history".into()], ..q("needle") }).unwrap();
    assert_eq!(two_classes["count"], 2, "a class list is an OR: the status and the history");
    assert_eq!(search(&s, &SearchQ { room: Some("core".into()), class: vec!["history".into()], ..q("needle") }).unwrap()["items"][0]["class"], "history");
    // an item is the same item every other route returns, with a score
    let i = &core["items"][0];
    assert!(i["signed"].is_boolean() && i["verification"].is_string() && i["thread"].is_object() && i["score"].is_number(), "{i}");
}

#[test]
fn matches_page_newest_first_and_the_chain_reaches_the_oldest_with_nothing_twice() {
    let s = oracle();
    let mut all = vec![];
    for i in 0..37 {
        all.push(say(&s, "core", "rocky", &format!("marker number {i} in the log"), &[], i));
    }
    say(&s, "core", "rocky", "nothing relevant here", &[], 99);
    for limit in [5i64, 12, 100] {
        let (mut got, mut before, mut pages) = (vec![], None, 0);
        loop {
            let v = search(&s, &SearchQ { room: Some("core".into()), before, limit: Some(limit), ..q("marker") }).unwrap();
            assert_eq!(v["order"], "newest");
            got.extend(ids(&v));
            pages += 1;
            match v["next_before"].as_i64() {
                Some(n) => before = Some(n),
                None => break,
            }
            assert!(pages < 50);
        }
        assert_eq!(got, all.iter().rev().cloned().collect::<Vec<_>>(), "limit {limit}: every match, newest first, none twice, none missing");
    }
    // the bound is exclusive
    let first = search(&s, &SearchQ { room: Some("core".into()), limit: Some(1), ..q("marker") }).unwrap();
    let seq = first["items"][0]["seq"].as_i64().unwrap();
    let next = search(&s, &SearchQ { room: Some("core".into()), before: Some(seq), limit: Some(1), ..q("marker") }).unwrap();
    assert!(next["items"][0]["seq"].as_i64().unwrap() < seq);
}

#[test]
fn ranked_returns_the_best_matches_and_says_when_there_were_more() {
    let s = oracle();
    let strong = say(&s, "core", "rocky", "kernel kernel kernel ledger", &[], 0);
    say(&s, "core", "rocky", "a long ramble that mentions the kernel once among many other unrelated words about weather and tea", &[], 1);
    say(&s, "core", "rocky", "kernel", &[], 2);
    let v = search(&s, &SearchQ { room: Some("core".into()), order: Some("rank".into()), limit: Some(2), ..q("kernel") }).unwrap();
    assert_eq!((v["order"].as_str(), v["count"].as_i64(), v["truncated"].as_bool()), (Some("rank"), Some(2), Some(true)));
    assert!(v["next_before"].is_null(), "a ranked list is the best n, not a chain: it says truncated instead");
    let scores: Vec<f64> = v["items"].as_array().unwrap().iter().map(|i| i["score"].as_f64().unwrap()).collect();
    assert!(scores[0] >= scores[1], "best first: {scores:?}");
    assert_eq!(v["items"][0]["id"], strong.as_str(), "the message that is all about it ranks first");
    let all = search(&s, &SearchQ { room: Some("core".into()), order: Some("rank".into()), limit: Some(10), ..q("kernel") }).unwrap();
    assert_eq!((all["count"].as_i64(), all["truncated"].as_bool()), (Some(3), Some(false)));
}

#[test]
fn every_word_must_match_unless_any_is_asked_for_and_punctuation_is_harmless() {
    let s = oracle();
    let both = say(&s, "core", "rocky", "ledger and kernel together", &[], 0);
    let one = say(&s, "core", "rocky", "only the ledger here", &[], 1);
    let r = |query: &str, any: bool| ids(&search(&s, &SearchQ { room: Some("core".into()), any, ..q(query) }).unwrap());
    assert_eq!(r("ledger kernel", false), vec![both.clone()]);
    let mut either = r("ledger kernel", true);
    either.sort();
    let mut want = vec![both.clone(), one.clone()];
    want.sort();
    assert_eq!(either, want);
    assert_eq!(r("\"ledger\" AND (kernel) OR NEAR(", false), Vec::<String>::new(), "operators are words to look for, not syntax: nothing crashes");
    assert_eq!(r("ledger* kernel:", false), vec![both]);
    assert_eq!(search(&s, &q("   ")).unwrap()["count"], 0, "no words, no matches");
}

#[test]
fn bad_parameters_are_refused_and_say_why() {
    let s = oracle();
    assert!(refused(search(&s, &SearchQ { limit: Some(0), ..q("x") })).contains("limit"));
    assert!(refused(search(&s, &SearchQ { limit: Some(201), ..q("x") })).contains("1 to 200"));
    assert!(refused(search(&s, &SearchQ { class: vec!["gossip".into()], ..q("x") })).contains("unknown class"));
    assert!(refused(search(&s, &SearchQ { room: Some("Not A Room".into()), ..q("x") })).contains("not a room name"));
    assert!(refused(search(&s, &SearchQ { order: Some("best".into()), ..q("x") })).contains("order"));
    assert!(refused(search(&s, &SearchQ { order: Some("rank".into()), before: Some(5), ..q("x") })).contains("newest"));
}

#[test]
fn a_ledger_written_before_the_room_tokens_is_searchable_only_after_reindex_and_says_so() {
    let s = oracle();
    say(&s, "core", "rocky", "an old message about needles", &[], 0);
    s.with_conn(|c| Ok(c.execute("UPDATE obs_fts SET tags='old'", []).unwrap())).unwrap();
    s.with_conn(|c| Ok(c.execute("UPDATE index_meta SET v='0' WHERE k='ready'", []).unwrap())).unwrap();
    assert!(matches!(search(&s, &SearchQ { room: Some("core".into()), ..q("needles") }), Err(StoreError::Unavailable(_))), "until reindex has run a room's search cannot be trusted");
    s.reindex().unwrap();
    assert_eq!(search(&s, &SearchQ { room: Some("core".into()), ..q("needles") }).unwrap()["count"], 1);
}

#[test]
fn text_only_skips_tag_matches_date_orders_by_when_written_and_every_match_is_counted_and_pageable() {
    // Blaze, 2026-09-30: "openfab" listed records whose only link to the word was a tag naming an #openfab message, and
    // imported history sorted by import time. in=text, order=date, offset= and total answer both.
    let s = oracle();
    let said = say(&s, "core", "rocky", "openfab board ships", &[], 0);
    let tagged = say(&s, "core", "rocky", "an entity with links", &["rel:MENTIONS:OBS-openfab-20260523-011"], 1);
    s.import_legacy(&[LegacyRow {
        id: "OBS-core-openfab-old".into(), channel: "core".into(), author: "kepler".into(), kind: "say".into(), content: "an old openfab note".into(), cargo: None,
        tags: vec![], created_ms: NOW - 9000 * MIN, pubkey: String::new(), sig: String::new(), news: false, source: None, repair_legacy: false,
    }]).unwrap();
    let all = search(&s, &q("openfab")).unwrap();
    assert_eq!(all["total"], 3, "text and tags: every match counted");
    assert!(ids(&all).contains(&tagged));
    let text = search(&s, &SearchQ { text_only: true, ..q("openfab") }).unwrap();
    assert_eq!(text["total"], 2);
    assert!(!ids(&text).contains(&tagged), "a tag alone is not a match in=text");
    assert_eq!(ids(&search(&s, &SearchQ { text_only: true, ..q("openfab") }).unwrap())[0], "OBS-core-openfab-old", "newest = received last: the import");
    let first = search(&s, &SearchQ { text_only: true, order: Some("date".into()), limit: Some(1), ..q("openfab") }).unwrap();
    assert_eq!(ids(&first), vec![said.clone()], "date = when written: the import is older");
    assert_eq!((first["total"].as_i64(), first["next_offset"].as_i64()), (Some(2), Some(1)));
    let second = search(&s, &SearchQ { text_only: true, order: Some("date".into()), limit: Some(1), offset: Some(1), ..q("openfab") }).unwrap();
    assert_eq!(ids(&second), vec!["OBS-core-openfab-old".to_string()]);
    assert!(second["next_offset"].is_null(), "nothing after the last page");
    let ranked = search(&s, &SearchQ { order: Some("rank".into()), limit: Some(2), offset: Some(2), ..q("openfab") }).unwrap();
    assert_eq!((ranked["count"].as_i64(), ranked["total"].as_i64()), (Some(1), Some(3)), "a ranked search pages too");
    assert!(refused(search(&s, &SearchQ { order: Some("date".into()), before: Some(5), ..q("openfab") })).contains("offset"));
    assert!(refused(search(&s, &SearchQ { offset: Some(1), ..q("openfab") })).contains("before"));
    assert!(refused(search(&s, &SearchQ { order: Some("rank".into()), offset: Some(-1), ..q("openfab") })).contains("offset"));
}

#[test]
fn a_ranked_page_puts_graph_synthesis_after_other_rooms() {
    let s = oracle();
    // Without the weight the echo wins: both words three times in a short message, against once each in a long one.
    let echo = say(&s, "graph-synthesis", "rocky", "observer protocol observer protocol observer protocol", &[], 1);
    let room = say(&s, "axxis", "rocky", "today the team met, read the notes, argued for an hour about the observer protocol and then went home tired", &[], 2);
    let v = search(&s, &SearchQ { order: Some("rank".into()), ..q("observer protocol") }).unwrap();
    assert_eq!(ids(&v), [room, echo]);
}
