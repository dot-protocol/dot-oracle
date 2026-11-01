//! The global card board. Parity first: tests/fixtures/cards-parity.json is edge-case rows plus what the AXXIS web board
//! (src/lib/project-board.ts, via ops/cards_golden.mjs) makes of them; this reader must fold the same cards. Then the same
//! reader over a real store with real signed writes, across rooms, with the filters an agent uses.

use ed25519_dalek::SigningKey;
use oracle_proto::{iso_ms, pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::cards::{accept, cards_cached, cards_for, BoardCache, fold_room, moves_of, names_seat, parse_task, task_id, CardFilter, Rev};
use oracle_server::store::{Observation, Principal, Scope, Store};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../../tests/fixtures/cards-parity.json")).unwrap()
}
fn observation(v: &Value) -> Observation {
    Observation {
        seq: v["seq"].as_i64().unwrap(), id: v["id"].as_str().unwrap().into(), channel: v["channel"].as_str().unwrap().into(), author: v["author"].as_str().unwrap().into(),
        kind: v["kind"].as_str().unwrap().into(), content: v["content"].as_str().unwrap().into(), cargo: v["cargo"].as_str().map(str::to_owned),
        tags: v["tags"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_owned()).collect(), created_ms: v["created_ms"].as_i64().unwrap(),
        authored_ms: None, pubkey: String::new(), sig: String::new(), envelope: v["envelope"].as_i64().unwrap(),
    }
}
fn strs(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_owned()).collect()
}
fn opt(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

/// The web board's card, compared field by field against ours.
fn same_card(room: &str, want: &Value, got: &oracle_server::cards::Card) {
    let id = want["id"].as_str().unwrap();
    let ctx = format!("{room}/{id}");
    assert_eq!(got.title, want["title"].as_str().unwrap(), "{ctx} title");
    assert_eq!(got.status, want["status"].as_str().unwrap(), "{ctx} status");
    if let Some(e) = want["effective_status"].as_str() {
        assert_eq!(got.effective_status, e, "{ctx} effective status"); // the web board derives none for a superseded card
    }
    assert_eq!(got.owner, want["owner"].as_str().unwrap(), "{ctx} owner");
    assert_eq!(got.group, want["group"].as_str().unwrap(), "{ctx} group");
    assert_eq!(got.impact, want["impact"].as_str().unwrap(), "{ctx} impact");
    assert_eq!(got.priority, opt(&want["priority"]), "{ctx} priority");
    assert_eq!(got.delivery, want["delivery"].as_str().unwrap(), "{ctx} delivery");
    assert_eq!(got.summary, want["summary"].as_str().unwrap(), "{ctx} summary");
    assert_eq!(got.acceptance, want["acceptance"].as_str().unwrap(), "{ctx} acceptance");
    assert_eq!(got.evidence, strs(&want["evidence"]), "{ctx} evidence");
    assert_eq!(got.depends_on, strs(&want["depends_on"]), "{ctx} depends_on");
    assert_eq!(got.unblock, opt(&want["unblock"]), "{ctx} unblock");
    assert_eq!(got.handoff, opt(&want["handoff"]), "{ctx} handoff");
    assert_eq!(got.reviewer, opt(&want["reviewer"]), "{ctx} reviewer");
    assert_eq!(got.superseded_by, opt(&want["superseded_by"]), "{ctx} superseded_by");
    assert_eq!(got.conflict, want["conflict"].as_bool().unwrap(), "{ctx} conflict");
    assert_eq!(got.heads, strs(&want["heads"]), "{ctx} heads");
    assert_eq!(got.blocking, strs(&want["blocking"]), "{ctx} blocking");
    assert_eq!(got.issues, strs(&want["issues"]), "{ctx} issues");
    assert_eq!(got.event, want["event"].as_str().unwrap(), "{ctx} event");
    assert_eq!(iso_ms(got.at_ms), want["at"].as_str().unwrap(), "{ctx} at");
}

fn board_of(room: &str, rows: &[Observation]) -> oracle_server::cards::RoomBoard {
    let (mut revs, mut refused) = (Vec::new(), Vec::new());
    for o in rows.iter().filter(|o| o.channel == room) {
        let a = accept(o, room);
        revs.extend(a.revisions);
        refused.extend(a.refused);
    }
    fold_room(room, revs, refused)
}

/// One room's golden (what the web board made of it) against what this reader made; the number of cards compared.
fn compare_room(room: &str, want: &Value, board: oracle_server::cards::RoomBoard) -> usize {
    let mut checked = 0;
    {
        let ours: BTreeMap<&str, &oracle_server::cards::Card> = board.cards.iter().map(|c| (c.id.as_str(), c)).collect();
        let theirs: BTreeMap<&str, &Value> = want["cards"].as_array().unwrap().iter().map(|c| (c["id"].as_str().unwrap(), c)).collect();
        assert_eq!(ours.keys().collect::<Vec<_>>(), theirs.keys().collect::<Vec<_>>(), "{room}: the same active cards");
        for (id, w) in &theirs {
            same_card(room, w, ours[id]);
            checked += 1;
        }
        let sup: BTreeMap<&str, &oracle_server::cards::Card> = board.superseded.iter().map(|c| (c.id.as_str(), c)).collect();
        let want_sup: Vec<&str> = want["superseded"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(sup.keys().copied().collect::<Vec<_>>(), { let mut w = want_sup.clone(); w.sort(); w }, "{room}: the same superseded cards");
        for w in want["superseded"].as_array().unwrap() {
            same_card(room, w, sup[w["id"].as_str().unwrap()]);
        }
        let mut inv: Vec<(String, String)> = board.invalid.iter().map(|u| (u.event.clone(), u.task.clone())).collect();
        inv.sort();
        let want_inv: Vec<(String, String)> = want["invalid"].as_array().unwrap().iter().map(|u| (u["event"].as_str().unwrap().to_owned(), u["task"].as_str().unwrap().to_owned())).collect();
        // The web board throws for a whole room where a face disagrees with its cargo or a lineage cycles; this reader refuses just the
        // revision, so it may list a refusal the web never reaches, but never fewer.
        for w in &want_inv {
            assert!(inv.contains(w), "{room}: the web board refused {w:?}; so must this reader ({inv:?})");
        }
    }
    checked
}

#[test]
fn every_card_folds_exactly_as_the_web_board_folds_it() {
    let f = fixture();
    let rows: Vec<Observation> = f["observations"].as_array().unwrap().iter().map(observation).collect();
    let mut checked = 0;
    for (room, want) in f["golden"].as_object().unwrap() {
        checked += compare_room(room, want, board_of(room, &rows));
    }
    assert!(checked >= 14, "the fixture exercises the board: {checked} cards compared");
}

#[test]
fn the_rules_the_web_contract_enforces_are_enforced_here() {
    let base = |over: Value| {
        let mut v = json!({"schema":"room-task/1","channel":"core","id":"c1","title":"T","group":"Run reliably","impact":"high","owner":"rocky","status":"queue","delivery":"not_live","summary":"s","acceptance":"a","evidence":[],"dependsOn":[],"parents":[]});
        for (k, x) in over.as_object().unwrap() {
            if x.is_null() { v.as_object_mut().unwrap().remove(k); } else { v[k] = x.clone(); }
        }
        v
    };
    let ok = |o: Value| parse_task(&base(o), "core");
    assert!(ok(json!({})).is_ok());
    assert_eq!(ok(json!({"impact":"med"})).unwrap().impact, "medium", "the historical abbreviation");
    for (what, over) in [
        ("a room that is not this one", json!({"channel":"other"})), ("an unknown group", json!({"group":"Room UX"})), ("an unknown status", json!({"status":"maybe"})),
        ("an unknown impact", json!({"impact":"huge"})), ("an unknown delivery", json!({"delivery":"soon"})), ("a title over 160", json!({"title":"x".repeat(161)})),
        ("an unblock over 80", json!({"unblock":"x".repeat(81)})), ("a handoff of nothing", json!({"handoff":"   "})), ("a priority that is not P0-P3", json!({"priority":"P9"})),
        ("a null priority", json!({"priority":Value::Null.clone()})), ("done with no evidence", json!({"status":"done"})), ("live with no evidence", json!({"delivery":"live"})),
        ("13 pieces of evidence", json!({"evidence":(0..13).map(|i| format!("e{i}")).collect::<Vec<_>>()})), ("a prerequisite that is itself", json!({"dependsOn":["c1"]})),
        ("a repeated prerequisite", json!({"dependsOn":["d1","d1"]})), ("a supersession of itself", json!({"supersededBy":"c1"})), ("a bad id", json!({"id":"Not Ok"})),
    ] {
        let r = parse_task(&{ let mut b = base(json!({})); for (k, x) in over.as_object().unwrap() { b[k] = x.clone(); } b }, "core");
        assert!(r.is_err(), "refused: {what}");
    }
    assert!(ok(json!({"status":"done","evidence":["abc"]})).is_ok());
    assert!(ok(json!({"handoff":"x".repeat(1600)})).is_ok(), "a handoff may be 1600");
    assert!(parse_task(&base(json!({"schema":"astra-task/1"})), "core").is_err() && parse_task(&{ let mut v = base(json!({})); v["schema"] = "astra-task/1".into(); v["channel"] = "astra".into(); v }, "astra").is_ok(), "astra-task/1 is astra's alone");
    // A length is counted the way JavaScript counts it (UTF-16 units): an astral character is two.
    assert!(ok(json!({"title":"𝛼".repeat(80)})).is_ok() && ok(json!({"title":"𝛼".repeat(81)})).is_err());
}

#[test]
fn a_face_names_its_card_only_in_its_own_room() {
    assert_eq!(task_id("[Rocky task:ci-one-list] review: x", Some("rocky")).as_deref(), Some("ci-one-list"));
    assert_eq!(task_id("[Rocky task:ci-one-list] review: x", Some("hustler")), None, "another room's face");
    assert_eq!(task_id("[Rocky task:  spaced] queue: x", Some("rocky")).as_deref(), Some("spaced"));
    assert_eq!(task_id("[Rocky task:Upper] queue: x", Some("rocky")).as_deref(), Some("Upper"), "the contract's id class is case-insensitive; parse_task then refuses it");
    assert_eq!(task_id("[Rocky task:] queue", Some("rocky")), None);
    assert_eq!(task_id("Rocky task:x] queue", Some("rocky")), None);
    assert_eq!(task_id("[Rocky Task:x] queue", Some("rocky")), None);
    assert_eq!(task_id("[Message state: x]", None), None);
    assert_eq!(task_id("[a task:b]", None).as_deref(), Some("b"));
}

#[test]
fn a_batch_is_all_or_nothing() {
    let rev = |id: &str| json!({"schema":"room-task/1","channel":"core","id":id,"title":"T","group":"Run reliably","impact":"high","owner":"rocky","status":"queue","delivery":"not_live","summary":"s","acceptance":"a","evidence":[],"dependsOn":[],"parents":[]});
    let batch = |moves: Vec<Value>| json!({"schema":"room-task-event/1","channel":"core","moves":moves});
    let mv = |id: &str| json!({"task":id,"to":"queue","revision":rev(id)});
    assert_eq!(moves_of(&batch(vec![mv("a"), mv("b")]), "core").unwrap().len(), 2);
    assert!(moves_of(&batch(vec![]), "core").is_err(), "an empty batch");
    assert!(moves_of(&batch((0..26).map(|i| mv(&format!("c{i}"))).collect()), "core").is_err(), "more than 25 moves");
    assert!(moves_of(&batch(vec![mv("a"), mv("a")]), "core").unwrap_err().contains("twice"));
    assert!(moves_of(&batch(vec![json!({"task":"zzz","to":"queue","revision":rev("a")})]), "core").unwrap_err().contains("names zzz but revises a"));
    assert!(moves_of(&batch(vec![json!({"task":"a","to":"done","revision":rev("a")})]), "core").unwrap_err().contains("says done"));
    assert!(moves_of(&batch(vec![mv("a"), json!({"task":"b"})]), "core").unwrap_err().contains("has no revision"), "one bad move refuses them all");
    assert!(moves_of(&json!({"schema":"room-task-event/1","channel":"other","moves":[mv("a")]}), "core").unwrap_err().contains("channel"));
}

#[test]
fn an_imported_row_counts_only_with_its_own_proof() {
    let rev = json!({"schema":"room-task/1","channel":"core","id":"c1","title":"T","group":"Run reliably","impact":"high","owner":"emmy","status":"queue","delivery":"not_live","summary":"s","acceptance":"a","evidence":[],"dependsOn":[],"parents":[]});
    let wrapped = json!({"source":"graph","cargo":rev.to_string()}).to_string();
    let row = |tags: &[&str], env: i64, cargo: Option<String>| Observation { seq: 1, id: "OBS-core-1".into(), channel: "core".into(), author: "emmy".into(), kind: "say".into(), content: "[Core task:c1] queue: T".into(), cargo, tags: tags.iter().map(|t| t.to_string()).collect(), created_ms: 1000, authored_ms: None, pubkey: String::new(), sig: String::new(), envelope: env };
    assert!(accept(&row(&["from:emmy", "signed:ok", "cargo-signed:ok"], 1, Some(wrapped.clone())), "core").revisions.len() == 1, "an imported row with both proofs");
    assert_eq!(accept(&row(&["from:emmy", "signed:ok", "cargo-signed:ok"], 1, Some(wrapped.clone())), "core").revisions[0].author, "emmy");
    assert!(accept(&row(&["from:emmy"], 1, Some(wrapped.clone())), "core").skipped, "no proof: not a revision");
    assert!(accept(&row(&["signed:ok"], 1, Some(wrapped.clone())), "core").skipped, "a signature without a cargo signature");
    assert!(accept(&row(&[], 2, Some(rev.to_string())), "core").revisions.len() == 1, "a native row was verified when written");
    assert!(accept(&row(&[], 2, None), "core").skipped, "a native row with no cargo carries no revision");
    let bad = accept(&row(&[], 2, Some("{\"schema\":\"room-task/1\"}".into())), "core");
    assert!(bad.revisions.is_empty() && bad.refused.len() == 1 && bad.refused[0].task == "c1", "a task face with an incomplete cargo is refused, by name");
    let mismatch = accept(&Observation { content: "[Core task:other] queue: T".into(), ..row(&[], 2, Some(rev.to_string())) }, "core");
    assert!(mismatch.revisions.is_empty() && mismatch.refused[0].reason.contains("disagree"), "a face that disagrees with its cargo refuses that revision, not the room");
}

#[test]
fn an_unblock_line_names_the_seats_it_waits_on() {
    assert!(names_seat("dinesh: push the mention fix; rocky: merge when green", "dinesh"));
    assert!(names_seat("dinesh: push the mention fix; rocky: merge when green", "rocky"));
    assert!(names_seat("Blaze: your call", "blaze"), "case does not matter");
    assert!(names_seat("@dinesh look", "dinesh") && names_seat("@dinesh", "dinesh"));
    assert!(!names_seat("rocky: merge dinesh's fix", "dinesh"), "a seat mentioned in passing is not the one it waits on");
    assert!(!names_seat("dineshk: x", "dinesh") && !names_seat("dinesh review", "dinesh"));
    assert!(!names_seat("", "dinesh"));
}

// ---- over a real store
const NOW: i64 = 1_800_000_000_000;
fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 1000), NOW - 1000).unwrap();
    }
    s
}
fn key_of(who: &str) -> u8 {
    match who { "blaze" => 1, "rocky" => 2, _ => 3 }
}
/// What `ox card` writes: a signed say whose face names the card and whose cargo is the room-task/1 revision.
fn card(s: &Store, room: &str, who: &str, ago_min: i64, over: Value) -> String {
    let mut rev = json!({"schema":"room-task/1","channel":room,"id":"c1","title":"A card","group":"Run reliably","impact":"high","owner":who,"status":"queue","delivery":"not_live","summary":"s","acceptance":"a","evidence":[],"dependsOn":[],"parents":[]});
    for (k, v) in over.as_object().unwrap() { rev[k] = v.clone(); }
    let at = NOW - ago_min * 60_000;
    let face = format!("[{}{} task:{}] {}: {}", room[..1].to_uppercase(), &room[1..], rev["id"].as_str().unwrap(), rev["status"].as_str().unwrap(), rev["title"].as_str().unwrap());
    let env = Envelope::sign(&sk(key_of(who)), room, who, Kind::Say, &face, Some(rev.to_string()), vec!["schema:room-task/1".into(), "intent:status".into(), "ask:none".into()], at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn ids(c: &oracle_server::cards::Cards) -> Vec<String> {
    c.cards.iter().map(|x| format!("{}/{}", x.room, x.id)).collect()
}

#[test]
fn cards_from_every_room_fold_into_one_board_with_the_filters_an_agent_uses() {
    let s = oracle();
    let a1 = card(&s, "core", "rocky", 60, json!({"id":"ship-it","title":"Ship it"}));
    card(&s, "core", "rocky", 30, json!({"id":"ship-it","title":"Ship it","status":"review","parents":[a1],"handoff":"changed X · test: Y · gotcha: Z","unblock":"dinesh: review it"}));
    card(&s, "core", "dinesh", 50, json!({"id":"fix-it","title":"Fix it","status":"in_progress","owner":"dinesh"}));
    card(&s, "ops", "dinesh", 40, json!({"id":"ship-it","title":"Same id, another room","status":"blocked","owner":"dinesh","unblock":"rocky: pick a date; blaze: sign off","summary":"waiting"}));
    card(&s, "ops", "rocky", 20, json!({"id":"old-news","title":"Done","status":"done","evidence":["abc1234"],"delivery":"live"}));
    // chatter that starts like a card but carries nothing usable, and ordinary speech
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &Envelope::sign(&sk(2), "core", "rocky", Kind::Say, "[Core task:broken] queue: nothing behind it", Some("{}".into()), vec![], NOW), NOW).unwrap();
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &Envelope::sign(&sk(2), "core", "rocky", Kind::Say, "just talking", None, vec![], NOW), NOW).unwrap();

    let all = cards_for(&s, &CardFilter::default()).unwrap();
    let mut got = ids(&all);
    got.sort();
    assert_eq!(got, ["core/fix-it", "core/ship-it", "ops/old-news", "ops/ship-it"], "one board, two rooms, the same card id kept apart");
    let ship = all.cards.iter().find(|c| c.room == "core" && c.id == "ship-it").unwrap();
    assert_eq!((ship.status.as_str(), ship.owner.as_str(), ship.unblock.as_deref()), ("review", "rocky", Some("dinesh: review it")));
    assert_eq!(ship.since_ms, NOW - 30 * 60_000, "since: when it entered review, not when it was made");
    assert_eq!(ship.handoff.as_deref(), Some("changed X · test: Y · gotcha: Z"));
    assert!(all.invalid.iter().any(|u| u.room == "core" && u.task == "broken"), "a refused card is reported, not hidden");
    assert_eq!(all.rooms.iter().map(|r| (r.room.as_str(), r.cards)).collect::<Vec<_>>(), [("core", 2), ("ops", 2)]);

    let mine = cards_for(&s, &CardFilter { owner: Some("dinesh".into()), ..Default::default() }).unwrap();
    let mut got = ids(&mine);
    got.sort();
    assert_eq!(got, ["core/fix-it", "ops/ship-it"]);
    let open = cards_for(&s, &CardFilter { status: Some(vec!["open".into()]), ..Default::default() }).unwrap();
    assert!(!ids(&open).contains(&"ops/old-news".to_string()) && open.cards.len() == 3, "open is everything but done");
    let two = cards_for(&s, &CardFilter { status: Some(vec!["review".into(), "done".into()]), ..Default::default() }).unwrap();
    assert_eq!(two.cards.len(), 2);
    let room = cards_for(&s, &CardFilter { room: Some("ops".into()), ..Default::default() }).unwrap();
    assert!(room.cards.iter().all(|c| c.room == "ops") && room.cards.len() == 2);
    let waiting = cards_for(&s, &CardFilter { waiting_on: Some("dinesh".into()), ..Default::default() }).unwrap();
    assert_eq!(ids(&waiting), ["core/ship-it"], "review or blocked, with dinesh named in the unblock line");
    let waiting_rocky = cards_for(&s, &CardFilter { waiting_on: Some("rocky".into()), ..Default::default() }).unwrap();
    assert_eq!(ids(&waiting_rocky), ["ops/ship-it"]);
    let nobody = cards_for(&s, &CardFilter { waiting_on: Some("kepler".into()), ..Default::default() }).unwrap();
    assert!(nobody.cards.is_empty());
    let none = cards_for(&s, &CardFilter { room: Some("Not A Room".into()), ..Default::default() }).unwrap();
    assert!(none.cards.is_empty(), "an invalid room name reads nothing");
}

#[test]
fn a_card_written_by_ox_card_is_read_back_field_for_field() {
    let s = oracle();
    card(&s, "core", "rocky", 5, json!({"id":"full","title":"Every field","status":"review","priority":"P1","reviewer":"dinesh","handoff":"h","unblock":"dinesh: look","evidence":["abc","OBS-core-1"],"dependsOn":[],"delivery":"preview","impact":"critical","group":"Release safely","summary":"the summary","acceptance":"the test"}));
    let c = &cards_for(&s, &CardFilter::default()).unwrap().cards[0];
    assert_eq!((c.priority.as_deref(), c.reviewer.as_deref(), c.impact.as_str(), c.group.as_str(), c.delivery.as_str()), (Some("P1"), Some("dinesh"), "critical", "Release safely", "preview"));
    assert_eq!((c.summary.as_str(), c.acceptance.as_str(), c.evidence.clone()), ("the summary", "the test", vec!["abc".to_string(), "OBS-core-1".to_string()]));
    assert_eq!(c.author, "rocky");
    let _: &Rev = &parse_task(&json!({"schema":"room-task/1","channel":"core","id":"x","title":"t","group":"Run reliably","impact":"high","owner":"o","status":"queue","delivery":"not_live","summary":"s","acceptance":"a","evidence":[],"dependsOn":[],"parents":[]}), "core").unwrap();
}

/// The proof on real boards, run by hand against a backup of the live ledger (never the live file):
///   ORACLE_CARDS_DB=/cache/live.db ORACLE_CARDS_GOLDEN=/cache/live-golden.json cargo test --test cards -- --ignored --nocapture
/// where the golden is what ops/cards_golden.mjs (CARDS_OBS/CARDS_OUT) made of the same rows.
#[test]
#[ignore]
fn live_boards_match_the_web_board() {
    let (Ok(db), Ok(g)) = (std::env::var("ORACLE_CARDS_DB"), std::env::var("ORACLE_CARDS_GOLDEN")) else { return };
    let golden: Value = serde_json::from_str(&std::fs::read_to_string(g).unwrap()).unwrap();
    let store = Store::open(&db).unwrap();
    for (room, want) in golden["golden"].as_object().unwrap() {
        let t = std::time::Instant::now();
        let (board, capped) = oracle_server::cards::read_room(&store, room).unwrap();
        let took = t.elapsed();
        assert!(!capped, "{room}: a read hit its cap");
        let n = compare_room(room, want, board);
        println!("{room}: {n} cards match the web board field for field; read_room took {took:?}");
        assert!(n > 0, "{room} has cards");
    }
    let mut per: Vec<(std::time::Duration, String, i64)> = store.channels().unwrap().into_iter().map(|(room, n, _, _)| {
        let t = std::time::Instant::now();
        let _ = oracle_server::cards::read_room(&store, &room);
        (t.elapsed(), room, n)
    }).collect();
    per.sort();
    per.reverse();
    println!("{} rooms scanned; slowest: {:?}", per.len(), per.iter().take(6).collect::<Vec<_>>());
    let cache = BoardCache::default();
    for pass in ["cold (first request after a start)", "warm (nothing new)"] {
        let t = std::time::Instant::now();
        let all = cards_cached(&store, &CardFilter::default(), &cache).unwrap();
        println!("every room, {pass}: {} cards from {} rooms, {} refused updates, in {:?}", all.cards.len(), all.rooms.len(), all.invalid.len(), t.elapsed());
    }
    let t = std::time::Instant::now();
    let mine = cards_for(&store, &CardFilter { room: Some("rocky".into()), ..Default::default() }).unwrap();
    println!("one room (rocky): {} cards in {:?}", mine.cards.len(), t.elapsed());
}

#[test]
fn a_cached_board_is_the_board_a_fresh_read_makes_as_the_rooms_grow() {
    let s = oracle();
    let cache = BoardCache::default();
    let f = CardFilter::default();
    let same = |what: &str| {
        let (a, b) = (cards_cached(&s, &f, &cache).unwrap(), cards_for(&s, &f).unwrap());
        assert_eq!(serde_json::to_value(&a).unwrap(), serde_json::to_value(&b).unwrap(), "{what}");
        a
    };
    assert!(same("an empty ledger").cards.is_empty());
    let first = card(&s, "core", "rocky", 60, json!({"id":"one","title":"One"}));
    assert_eq!(same("the first card").cards.len(), 1);
    assert_eq!(same("nothing changed: served from the cache").cards.len(), 1);
    // a new revision of the same card, a new card, and a card in another room: each is seen at once
    card(&s, "core", "rocky", 30, json!({"id":"one","title":"One","status":"review","parents":[first],"handoff":"a · test: b · gotcha: c","unblock":"dinesh: look"}));
    assert_eq!(same("a revision").cards[0].status, "review");
    card(&s, "core", "dinesh", 20, json!({"id":"two","title":"Two","owner":"dinesh"}));
    card(&s, "ops", "dinesh", 10, json!({"id":"three","title":"Three","owner":"dinesh"}));
    assert_eq!(same("new cards, one in a new room").cards.len(), 3);
    // chatter and a broken card in a room that has cards move the room's newest seq without adding a card
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &Envelope::sign(&sk(2), "core", "rocky", Kind::Say, "just talking", None, vec![], NOW), NOW).unwrap();
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &Envelope::sign(&sk(2), "core", "rocky", Kind::Say, "[Core task:broken] queue: x", Some("{}".into()), vec![], NOW), NOW).unwrap();
    let after = same("a broken card and chatter");
    assert_eq!(after.cards.len(), 3);
    assert!(after.invalid.iter().any(|u| u.task == "broken"));
}
