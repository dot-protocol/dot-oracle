//! Step 3b of docs/ROOM-INDEX.md: the room, its threads and one thread, as pages out of the index. Real signed writes on the real store; the same
//! functions the routes call. The fixtures Emmy asked for are here: an empty room, an unsigned import, a superseded item, more than 200 messages,
//! a body over 64 KiB, and a page chain that must reach the room's first message with nothing twice and nothing missing.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::roomread::{body, mark_stale, messages, thread, threads, PageQ, BODY_MAX};
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
fn say(s: &Store, room: &str, who: &str, text: &str, cargo: Option<&str>, tags: &[&str], mins: i64) -> String {
    let at = NOW + mins * MIN;
    let env = Envelope::sign(&sk(key_of(who)), room, who, Kind::Say, text, cargo.map(str::to_owned), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn legacy(id: &str, channel: &str, content: &str, tags: &[&str], ago_min: i64) -> LegacyRow {
    LegacyRow {
        id: id.into(), channel: channel.into(), author: "rocky".into(), kind: "say".into(), content: content.into(), cargo: None,
        tags: tags.iter().map(|t| t.to_string()).collect(), created_ms: NOW - ago_min * MIN, pubkey: String::new(), sig: String::new(), news: false,
        source: None, repair_legacy: false,
    }
}
fn seqs(v: &Value, key: &str) -> Vec<i64> {
    v[key].as_array().unwrap().iter().map(|i| i["seq"].as_i64().unwrap()).collect()
}
fn ids(v: &Value) -> Vec<&str> {
    v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect()
}
fn q() -> PageQ {
    PageQ::default()
}
fn rejected(r: Result<Value, StoreError>) -> String {
    match r {
        Err(StoreError::Rejected(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn an_empty_room_is_an_empty_page_that_says_it_is_exhausted() {
    let s = oracle();
    let v = messages(&s, "nowhere", &q()).unwrap();
    assert_eq!((v["count"].as_i64(), v["limit"].as_i64(), v["items"].as_array().unwrap().len()), (Some(0), Some(50), 0));
    assert!(v["next_before"].is_null() && v["next_after"].is_null());
    let t = threads(&s, "nowhere", &q()).unwrap();
    assert!(t["threads"].as_array().unwrap().is_empty() && t["next_before"].is_null());
}

#[test]
fn a_page_chain_reaches_the_first_message_with_nothing_twice_and_nothing_missing() {
    let s = oracle();
    let mut all = vec![];
    for i in 0..230 {
        all.push(say(&s, "core", if i % 2 == 0 { "rocky" } else { "dinesh" }, &format!("message {i}"), None, &[], i));
    }
    say(&s, "ops", "rocky", "another room, never in core's pages", None, &[], 5);
    let want: Vec<i64> = { let mut w: Vec<i64> = all.iter().map(|id| s.get(id).unwrap().seq).collect(); w.sort_by(|a, b| b.cmp(a)); w };
    for limit in [50i64, 200, 7] {
        let (mut got, mut before, mut pages) = (vec![], None, 0);
        loop {
            let v = messages(&s, "core", &PageQ { before, limit: Some(limit), ..q() }).unwrap();
            assert_eq!(v["limit"].as_i64(), Some(limit));
            assert_eq!(v["count"].as_u64(), Some(v["items"].as_array().unwrap().len() as u64));
            got.extend(seqs(&v, "items"));
            pages += 1;
            match v["next_before"].as_i64() {
                Some(n) => {
                    assert_eq!(Some(n), got.last().copied(), "next_before is the seq of the last item, to pass as before=");
                    before = Some(n);
                }
                None => break,
            }
            assert!(pages < 100, "the chain must end");
        }
        assert_eq!(got, want, "limit {limit}: the whole room, newest first, no gap and no repeat, in {pages} pages");
    }
    // a page that ends exactly at the start says so: 230 messages, limit 230 is refused (max 200), so check the boundary at 200 and 30
    let first = messages(&s, "core", &PageQ { limit: Some(200), ..q() }).unwrap();
    let rest = messages(&s, "core", &PageQ { before: first["next_before"].as_i64(), limit: Some(200), ..q() }).unwrap();
    assert_eq!(rest["count"].as_i64(), Some(30));
    assert!(rest["next_before"].is_null(), "exhausted is null, never the start");
}

#[test]
fn after_pages_forward_from_a_seq_oldest_first_and_its_cursor_never_goes_null() {
    let s = oracle();
    let all: Vec<i64> = (0..25).map(|i| { let id = say(&s, "core", "rocky", &format!("m{i}"), None, &[], i); s.get(&id).unwrap().seq }).collect();
    let (mut got, mut after): (Vec<i64>, i64) = (vec![], all[4]);
    loop {
        let v = messages(&s, "core", &PageQ { after: Some(after), limit: Some(8), ..q() }).unwrap();
        assert!(v["next_before"].is_null(), "going forward has no next_before");
        let page = seqs(&v, "items");
        assert!(page.windows(2).all(|w| w[0] < w[1]) && page.iter().all(|s| *s > after), "ascending, and the bound is exclusive");
        got.extend(&page);
        after = v["next_after"].as_i64().expect("the forward cursor is never null");
        assert_eq!(after, page.last().copied().unwrap_or(after), "the cursor is the last seq returned");
        if v["more"] == false {
            break;
        }
    }
    assert_eq!(got, all[5..].to_vec(), "everything after the seq, oldest first, nothing twice");
    assert_eq!(after, *all.last().unwrap());
    // caught up: nothing new, and the cursor stays where the client is, so a reconnect resumes from it
    let idle = messages(&s, "core", &PageQ { after: Some(after), ..q() }).unwrap();
    assert!(idle["items"].as_array().unwrap().is_empty() && idle["more"] == false && idle["next_after"].as_i64() == Some(after));
    // the bound is exclusive: asking after the very first seq does not return it
    let v = messages(&s, "core", &PageQ { after: Some(all[0]), limit: Some(1), ..q() }).unwrap();
    assert_eq!(seqs(&v, "items"), vec![all[1]]);
    // and a message that arrives later is the next page, never a repeat
    let late = s.get(&say(&s, "core", "rocky", "late", None, &[], 99)).unwrap().seq;
    let v = messages(&s, "core", &PageQ { after: Some(after), ..q() }).unwrap();
    assert_eq!((seqs(&v, "items"), v["next_after"].as_i64()), (vec![late], Some(late)));
}

#[test]
fn the_room_is_every_class_unless_it_is_narrowed() {
    let s = oracle();
    s.import_legacy(&[legacy("OBS-core-old", "core", "from before", &[], 9000)]).unwrap();
    let m = say(&s, "core", "rocky", "hello", None, &[], 0);
    say(&s, "core", "dinesh", "✓", None, &[&format!("re:{m}"), "react:go"], 1);
    say(&s, "core", "kepler", "[Core task:x] queue: X", None, &["schema:room-task/1", "intent:status"], 2);
    let classes = |v: &Value| -> Vec<String> { v["items"].as_array().unwrap().iter().map(|i| i["class"].as_str().unwrap().to_owned()).collect() };
    let all = messages(&s, "core", &q()).unwrap();
    let mut got = classes(&all);
    got.sort();
    assert_eq!(got, ["card", "history", "mark", "message"], "nothing is filtered by default, history included");
    assert_eq!(classes(&messages(&s, "core", &PageQ { class: vec!["history".into()], ..q() }).unwrap()), ["history"]);
    let two = messages(&s, "core", &PageQ { class: vec!["message".into(), "mark".into()], ..q() }).unwrap();
    assert_eq!(two["count"].as_i64(), Some(2));
    assert_eq!(messages(&s, "core", &PageQ { author: Some("kepler".into()), ..q() }).unwrap()["count"].as_i64(), Some(1));
}

#[test]
fn bad_parameters_are_refused_and_say_why() {
    let s = oracle();
    assert!(rejected(messages(&s, "core", &PageQ { limit: Some(0), ..q() })).contains("limit"));
    assert!(rejected(messages(&s, "core", &PageQ { limit: Some(201), ..q() })).contains("1 to 200"));
    assert!(rejected(messages(&s, "core", &PageQ { before: Some(5), after: Some(1), ..q() })).contains("together"));
    assert!(rejected(messages(&s, "core", &PageQ { class: vec!["gossip".into()], ..q() })).contains("unknown class"));
    assert!(rejected(messages(&s, "Not A Room", &q())).contains("not a room name"));
    assert!(rejected(threads(&s, "core", &PageQ { state: vec!["maybe".into()], ..q() })).contains("unknown state"));
    assert!(rejected(threads(&s, "core", &PageQ { after: Some(1), ..q() })).contains("before="));
    assert!(matches!(thread(&s, "OBS-core-nope", &q()), Err(StoreError::NotFound)));
    assert!(matches!(thread(&s, "99999", &q()), Err(StoreError::NotFound)));
}

#[test]
fn signed_means_this_server_verified_it_and_an_import_never_vouches_for_itself() {
    let s = oracle();
    let native = say(&s, "core", "rocky", "signed when written", None, &[], 0);
    // an importer's own tags claim proof; nothing records a trusted importer's provenance, so the claim is not a verification
    let mut proven = legacy("OBS-core-proven", "core", "imported, tags claim both proofs", &["signed:ok", "cargo-signed:ok"], 100);
    proven.sig = "x".into();
    s.import_legacy(&[proven, legacy("OBS-core-bare", "core", "imported with no tags", &[], 80)]).unwrap();
    let v = messages(&s, "core", &q()).unwrap();
    let item = |id: &str| v["items"].as_array().unwrap().iter().find(|i| i["id"] == id).unwrap().clone();
    assert_eq!((item(&native)["signed"].clone(), item(&native)["verification"].clone()), (true.into(), "verified".into()));
    for id in ["OBS-core-proven", "OBS-core-bare"] {
        assert_eq!((item(id)["signed"].clone(), item(id)["verification"].clone()), (false.into(), "imported_unverified".into()), "{id}: its own tags do not make it signed");
    }
}

#[test]
fn a_body_is_never_cut_and_an_omitted_body_is_typed_and_fetched_whole_from_an_item_endpoint() {
    use sha2::{Digest, Sha256};
    let s = oracle();
    let big = "x".repeat(BODY_MAX as usize + 1);
    let edge = "y".repeat(BODY_MAX as usize);
    let small_cargo = r#"{"options":["a","b"]}"#;
    let huge_cargo = format!("{{\"blob\":\"{}\"}}", "z".repeat(BODY_MAX as usize));
    // a native message tops out at exactly the cap (the envelope refuses more), so a body over it can only be an imported one
    let mut old = legacy("OBS-core-bigold", "core", &big, &[], 9000);
    old.content = big.clone();
    s.import_legacy(&[old]).unwrap();
    let a = "OBS-core-bigold".to_string();
    let b = say(&s, "core", "rocky", &edge, Some(small_cargo), &[], 1);
    let c = say(&s, "core", "rocky", "small words", Some(&huge_cargo), &[], 2);
    let v = messages(&s, "core", &PageQ { include_cargo: true, ..q() }).unwrap();
    let item = |id: &str| v["items"].as_array().unwrap().iter().find(|i| i["id"] == id).unwrap().clone();
    let (ia, ib, ic) = (item(&a), item(&b), item(&c));
    assert!(ia["content"].is_null() && ia.get("too_large").is_none(), "no ambiguous flag: the reason is typed");
    assert_eq!(ia["body_omitted"], serde_json::json!({"reason": "too_large", "bytes": BODY_MAX + 1, "limit": BODY_MAX, "fetch": format!("/observations/{a}/content")}));
    assert_eq!(ib["content"].as_str().unwrap().len() as i64, BODY_MAX, "exactly the cap is sent whole");
    assert!(ib.get("body_omitted").is_none());
    assert_eq!(ib["cargo"], small_cargo);
    assert!(ic["cargo"].is_null() && ic["cargo_omitted"]["reason"] == "too_large" && ic["cargo_omitted"]["fetch"] == format!("/observations/{c}/cargo").as_str());
    assert!(ic["cargo_omitted"]["bytes"].as_i64().unwrap() > BODY_MAX);
    assert_eq!(ic["content"], "small words");
    // cargo only when asked for
    let plain = messages(&s, "core", &q()).unwrap();
    assert!(plain["items"].as_array().unwrap().iter().all(|i| i.get("cargo").is_none() && i.get("cargo_omitted").is_none()), "no cargo unless include_cargo=true");
    // the item endpoint: the exact bytes, and a hash to check them by
    let (text, sha) = body(&s, &a, "content").unwrap();
    assert_eq!(text, big);
    assert_eq!(sha, hex::encode(Sha256::digest(big.as_bytes())));
    let (cargo, cargo_sha) = body(&s, &c, "cargo").unwrap();
    assert_eq!((cargo.as_str(), cargo_sha), (huge_cargo.as_str(), hex::encode(Sha256::digest(huge_cargo.as_bytes()))));
    assert!(matches!(body(&s, &a, "cargo"), Err(StoreError::NotFound)), "no cargo, no body");
    assert!(matches!(body(&s, "OBS-core-nope", "content"), Err(StoreError::NotFound)));
    assert!(matches!(body(&s, &a, "tags"), Err(StoreError::NotFound)), "only content and cargo");
    assert!(oracle_proto::MAX_CONTENT_BYTES as i64 == BODY_MAX, "the cap on a body is the cap on a write");
}

#[test]
fn a_superseded_message_names_its_successor_and_stays_readable() {
    let s = oracle();
    let old = say(&s, "core", "rocky", "plan A", None, &["intent:proposal"], 0);
    let new = say(&s, "core", "rocky", "plan B", None, &["intent:proposal", &format!("supersedes:{old}")], 1);
    let mark = say(&s, "core", "dinesh", "✓", None, &[&format!("re:{new}"), "react:like"], 2);
    let withdrawn = say(&s, "core", "dinesh", "✓", None, &[&format!("archives:{mark}")], 3);
    let v = messages(&s, "core", &q()).unwrap();
    let item = |id: &str| v["items"].as_array().unwrap().iter().find(|i| i["id"] == id).unwrap().clone();
    assert_eq!(item(&old)["superseded_by"], new.as_str());
    assert_eq!(item(&old)["content"], "plan A", "the original is kept");
    assert!(item(&new)["superseded_by"].is_null());
    assert_eq!(item(&new)["supersedes"], serde_json::json!([old]));
    assert_eq!(item(&mark)["archived_by"], withdrawn.as_str());
    assert!(ids(&v).contains(&old.as_str()), "a read never silently hides a superseded item");
}

#[test]
fn items_marks_and_votes_ride_on_the_message_they_belong_to() {
    let s = oracle();
    let m = say(&s, "core", "blaze", "R1 and R2", None, &["item:R1=requirement", "item:R2=requirement", "to:dinesh"], 0);
    say(&s, "core", "dinesh", "on it", None, &[&format!("re:{m}"), "item:R1=building", "owner:R1=dinesh", "vote:R2=yes"], 1);
    say(&s, "core", "kepler", "✓", None, &[&format!("re:{m}"), "react:like"], 2);
    let v = messages(&s, "core", &PageQ { class: vec!["message".into()], author: Some("blaze".into()), ..q() }).unwrap();
    let i = &v["items"][0];
    assert_eq!(i["items"][0], serde_json::json!({"k": "R1", "kind": "requirement", "state": "building", "owner": "dinesh"}));
    assert_eq!(i["items"][1]["state"], "open");
    assert_eq!(i["marks"], serde_json::json!({"like": ["kepler"]}));
    assert_eq!(i["votes"], serde_json::json!([{"k": "R2", "seat": "dinesh", "stance": "yes"}]));
    assert_eq!((i["intent"].as_str(), i["ask"].as_str()), (Some("note"), Some("none")));
}

#[test]
fn threads_page_by_their_newest_message_and_carry_their_root() {
    let s = oracle();
    s.import_legacy(&[legacy("OBS-core-h", "core", "history alone", &[], 9000)]).unwrap();
    let mut roots = vec![];
    for i in 0..5 {
        let r = say(&s, "core", "rocky", &format!("root {i}"), None, &["intent:question", "ask:reply", "to:dinesh"], i * 10);
        if i % 2 == 0 {
            say(&s, "core", "dinesh", "an answer", None, &[&format!("re:{r}")], i * 10 + 1);
        }
        roots.push(r);
    }
    let v = threads(&s, "core", &q()).unwrap();
    let rows = v["threads"].as_array().unwrap();
    assert_eq!(rows.len(), 5, "history alone has no thread row");
    let order: Vec<&str> = rows.iter().map(|t| t["root"]["id"].as_str().unwrap()).collect();
    assert_eq!(order, roots.iter().rev().map(String::as_str).collect::<Vec<_>>(), "newest activity first");
    assert_eq!(rows[0]["state"], "answered");
    assert_eq!((rows[0]["replies"].as_i64(), rows[1]["replies"].as_i64()), (Some(1), Some(0)));
    let open = threads(&s, "core", &PageQ { state: vec!["open".into()], ..q() }).unwrap();
    assert_eq!(open["threads"].as_array().unwrap().len(), 2);
    // keyset over last_seq: two at a time, all five, none twice
    let (mut got, mut before) = (vec![], None);
    loop {
        let p = threads(&s, "core", &PageQ { before, limit: Some(2), ..q() }).unwrap();
        got.extend(p["threads"].as_array().unwrap().iter().map(|t| t["root"]["id"].as_str().unwrap().to_owned()));
        match p["next_before"].as_i64() {
            Some(n) => before = Some(n),
            None => break,
        }
    }
    assert_eq!(got, roots.iter().rev().cloned().collect::<Vec<_>>());
}

#[test]
fn a_thread_is_found_by_its_root_a_reply_or_a_seq_and_pages_newest_first() {
    let s = oracle();
    let root = say(&s, "core", "rocky", "the question", None, &["intent:question", "ask:reply", "to:dinesh"], 0);
    let mut replies = vec![];
    for i in 0..9 {
        replies.push(say(&s, "core", "dinesh", &format!("reply {i}"), None, &[&format!("re:{root}")], 1 + i));
    }
    say(&s, "core", "rocky", "another thread", None, &[], 20);
    let v = thread(&s, &root, &PageQ { limit: Some(4), ..q() }).unwrap();
    assert_eq!(v["root"]["id"], root.as_str());
    assert_eq!(v["thread"]["replies"], 9);
    assert_eq!(v["thread"]["state"], "answered");
    assert_eq!(ids(&v), replies.iter().rev().take(4).map(String::as_str).collect::<Vec<_>>(), "the root is not repeated among the items; newest first");
    let (mut got, mut before) = (vec![], None);
    loop {
        let p = thread(&s, &root, &PageQ { before, limit: Some(4), ..q() }).unwrap();
        got.extend(p["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_owned()));
        match p["next_before"].as_i64() {
            Some(n) => before = Some(n),
            None => break,
        }
    }
    assert_eq!(got, replies.iter().rev().cloned().collect::<Vec<_>>(), "the chain reaches the first reply, nothing twice");
    let by_reply = thread(&s, &replies[3], &q()).unwrap();
    assert_eq!(by_reply["root"]["id"], root.as_str(), "any message names its thread");
    let by_seq = thread(&s, &s.get(&root).unwrap().seq.to_string(), &q()).unwrap();
    assert_eq!(by_seq["root"]["id"], root.as_str());
}

#[test]
fn until_the_index_is_built_a_read_is_refused_not_answered_short() {
    let s = oracle();
    say(&s, "core", "rocky", "hello", None, &[], 0);
    s.with_conn(|c| Ok(c.execute("UPDATE index_meta SET v='0' WHERE k='ready'", []).unwrap())).unwrap();
    for r in [messages(&s, "core", &q()), threads(&s, "core", &q()), thread(&s, "1", &q())] {
        assert!(matches!(r, Err(StoreError::Unavailable(m)) if m.contains("oracle reindex")), "a half-built index is never the room");
    }
    s.reindex().unwrap();
    assert_eq!(messages(&s, "core", &q()).unwrap()["count"], 1);
}

/// A live page follows a room with `after=` while writers are writing. Whatever the interleaving, and however often the reader drops the connection and
/// resumes from its last cursor, it sees every message once and none twice.
#[test]
fn a_reader_following_with_after_misses_and_repeats_nothing_while_writers_write() {
    use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
    let dir = std::env::temp_dir().join(format!("oracle-follow-{}-{}", std::process::id(), NOW));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("oracle.db");
    let s = Arc::new(Store::open(path.to_str().unwrap()).unwrap());
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3), ("kepler", 4)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    }
    const WRITERS: usize = 4;
    const EACH: usize = 60;
    let done = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];
    for (w, who) in ["rocky", "dinesh", "kepler", "rocky"].iter().enumerate() {
        let (s, done) = (s.clone(), done.clone());
        let who = who.to_string();
        handles.push(std::thread::spawn(move || {
            for i in 0..EACH {
                let at = NOW + (w * EACH + i) as i64 * 1000;
                let env = Envelope::sign(&sk(key_of(&who)), "core", &who, Kind::Say, &format!("writer {w} message {i}"), None, vec![], at);
                // a busy database is retried, as a client would
                loop {
                    match s.write(&Principal { handle: who.clone(), scope: Scope::Write }, &env, at) {
                        Ok(_) => break,
                        Err(StoreError::Internal(m)) if m.contains("locked") || m.contains("busy") || m.contains("timed out") => std::thread::sleep(std::time::Duration::from_millis(2)),
                        Err(e) => panic!("{e:?}"),
                    }
                }
            }
            done.fetch_add(1, Ordering::SeqCst);
        }));
    }
    // the follower: pages forward, and every few pages "reconnects", forgetting everything but its last cursor
    let (mut cursor, mut seen, mut rounds) = (0i64, Vec::<i64>::new(), 0usize);
    let started = std::time::Instant::now();
    loop {
        let finished = done.load(Ordering::SeqCst) == WRITERS;
        let v = messages(&s, "core", &PageQ { after: Some(cursor), limit: Some(1 + (rounds % 7) as i64 * 5), ..q() }).unwrap();
        let page = seqs(&v, "items");
        assert!(page.iter().all(|x| *x > cursor), "exclusive bound: {page:?} after {cursor}");
        seen.extend(&page);
        cursor = v["next_after"].as_i64().unwrap();
        rounds += 1;
        if finished && v["more"] == false && page.is_empty() {
            break;
        }
        // Bounded by time, not by polls: on a loaded box the writers slow down and an empty poll is not a failure to
        // catch up (it tripped a 20 000-poll cap on core-ci, 2026-09-30). An empty page waits a moment, as a client would.
        if page.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(120), "the follower must catch up");
    }
    for h in handles {
        h.join().unwrap();
    }
    let mut want = seqs(&messages(&s, "core", &PageQ { limit: Some(200), ..q() }).unwrap(), "items");
    let second = messages(&s, "core", &PageQ { before: messages(&s, "core", &PageQ { limit: Some(200), ..q() }).unwrap()["next_before"].as_i64(), limit: Some(200), ..q() }).unwrap();
    want.extend(seqs(&second, "items"));
    want.sort();
    assert_eq!(want.len(), WRITERS * EACH, "the room holds every message written");
    assert_eq!(seen, want, "the follower saw every message exactly once, in order, across {rounds} reads");
    let _ = std::fs::remove_dir_all(&dir);
}


/// Blaze, 2026-09-30: a ledger said "policy.p1 waits on Blaze" a day after Kepler retired it. A message that names a
/// subject is read with that subject's state as it is now, when the state moved after the message.
#[test]
fn a_message_naming_a_subject_carries_its_state_when_the_state_moved_after_it() {
    let s = oracle();
    let state = |who: &str, st: &str, text: &str, mins: i64| {
        let cargo = format!(r#"{{"subject":"policy.p1","state":"{st}","owner":"{who}"}}"#);
        let at = NOW + mins * MIN;
        let env = Envelope::sign(&sk(key_of(who)), "hustler", who, Kind::State, text, Some(cargo), vec!["subject:policy.p1".into()], at);
        s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
    };
    state("kepler", "noted", "Leverage at most 10x", 0);
    let before = say(&s, "hustler", "rocky", "policy.p1 is under review", None, &[], 1);
    let tagged = say(&s, "hustler", "rocky", "the leverage rule", None, &["ref:policy.p1"], 1);
    let plain = say(&s, "hustler", "rocky", "nothing named here, not even policy", None, &[], 1);
    let retired = state("kepler", "done", "Retired: Blaze decided V1", 2);
    let after = say(&s, "hustler", "dinesh", "saw that policy.p1 closed", None, &[], 3);
    let v = messages(&s, "hustler", &q()).unwrap();
    let stale_of = |id: &str| v["items"].as_array().unwrap().iter().find(|i| i["id"] == id).unwrap()["stale"].clone();
    let st = stale_of(&before);
    assert_eq!(st[0]["subject"], "policy.p1");
    assert_eq!((st[0]["state"].as_str(), st[0]["owner"].as_str(), st[0]["id"].as_str()), (Some("done"), Some("kepler"), Some(retired.as_str())));
    assert_eq!(stale_of(&tagged)[0]["state"], "done", "a ref: tag names the subject too");
    // negative controls: written after the change, naming nothing, and the state record itself
    assert!(stale_of(&after).as_array().unwrap().is_empty(), "a message newer than the state is not stale");
    assert!(stale_of(&plain).as_array().unwrap().is_empty());
    assert!(stale_of(&retired).as_array().unwrap().is_empty(), "the record that set the state never flags itself");
}


/// The /search listing (ox read, the MCP tools) carries the same marks: items shaped {seq, content, tags, kind}.
#[test]
fn the_listing_shape_is_marked_the_same_way() {
    let s = oracle();
    let at = NOW + 2 * MIN;
    let before = say(&s, "hustler", "rocky", "policy.p1 waits on Blaze", None, &[], 1);
    let env = Envelope::sign(&sk(4), "hustler", "kepler", Kind::State, "Retired", Some(r#"{"subject":"policy.p1","state":"done","owner":"kepler"}"#.into()), vec!["subject:policy.p1".into()], at);
    let retired = s.write(&Principal { handle: "kepler".into(), scope: Scope::Write }, &env, at).unwrap().id;
    let item = |id: &str, kind: &str| {
        let o = s.get(id).unwrap();
        serde_json::json!({"seq": o.seq, "content": o.content, "tags": o.tags, "kind": kind, "id": id})
    };
    let mut items = vec![item(&before, "say"), item(&retired, "state")];
    mark_stale(&s, &mut items).unwrap();
    assert_eq!(items[0]["stale"][0]["state"], "done");
    assert!(items[1]["stale"].as_array().unwrap().is_empty(), "the state record does not flag itself (kind=state)");
}
