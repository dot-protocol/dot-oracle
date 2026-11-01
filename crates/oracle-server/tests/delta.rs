//! `GET /delta` (delta.rs; Blaze, 1 Nov 2026: food for an agent is the diff between the world's last known state and its
//! current state): a seeded store with records before and after a seat's RESUME, on the real store with real signed
//! writes; the page shows what came after and nothing from before.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::delta::{delta, resolve_since};
use oracle_server::store::{Principal, Scope, Store, StoreError};
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
        _ => 3,
    }
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    }
    s
}
/// One signed write `ago` minutes before NOW.
fn write(s: &Store, room: &str, who: &str, text: &str, cargo: Option<String>, tags: &[&str], ago: i64) -> String {
    let at = NOW - ago * MIN;
    let env = Envelope::sign(&sk(key_of(who)), room, who, Kind::Say, text, cargo, tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn say(s: &Store, room: &str, who: &str, text: &str, tags: &[&str], ago: i64) -> String {
    write(s, room, who, text, None, tags, ago)
}
/// A first State revision of `room` (no parents yet) with `fields`.
fn state(s: &Store, room: &str, who: &str, fields: Value, ago: i64) -> String {
    let cargo = serde_json::json!({"schema": "room-context/1", "channel": room, "parents": [], "fields": fields}).to_string();
    write(s, room, who, "State", Some(cargo), &["room-context:v1"], ago)
}
fn rocky() -> Principal {
    Principal { handle: "rocky".into(), scope: Scope::Write }
}
fn room<'a>(page: &'a Value, name: &str) -> Option<&'a Value> {
    page["rooms"].as_array().unwrap().iter().find(|r| r["room"] == name)
}
fn ids(list: &Value) -> Vec<&str> {
    list.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect()
}

#[test]
fn the_page_shows_what_came_after_the_resume_and_nothing_from_before() {
    let s = oracle();
    // BEFORE the point: rocky's State follows #core and #ops; news in both; Blaze's words; an ask rocky opened.
    state(&s, "rocky", "rocky", serde_json::json!({"rooms": "#core and #ops"}), 100);
    let old_news = say(&s, "core", "blaze", "old news in core", &[], 90);
    let old_to_me = say(&s, "ops", "dinesh", "old, for rocky", &["to:rocky"], 85);
    let old_words = say(&s, "blaze", "blaze", "old words", &[], 80);
    let ask = say(&s, "core", "rocky", "ship R-1", &["to:dinesh", "ask:act"], 75);
    let resume = say(&s, "rocky", "rocky", "RESUME: shipping R-1", &["resume"], 60);
    // AFTER it: #core moves (news, one addressed, one reply to rocky, a State revision); #ops stays quiet; a room rocky
    // does not follow speaks; Blaze writes; dinesh asks rocky; the R-1 ask is completed; rocky writes once.
    let new_news = say(&s, "core", "dinesh", "new in core", &[], 50);
    let for_me = say(&s, "core", "blaze", "@rocky look at this", &["to:rocky"], 45);
    let receipt = say(&s, "core", "dinesh", "shipped abc123", &[&format!("reply:{ask}"), "intent:receipt"], 40);
    let core_state = state(&s, "core", "blaze", serde_json::json!({"current": "R-1 shipped"}), 35);
    let elsewhere = say(&s, "elsewhere", "dinesh", "a room rocky never follows", &[], 30);
    let new_words = say(&s, "blaze", "blaze", "new words\nsecond line", &[], 25);
    let question = say(&s, "ops2", "dinesh", "which port?", &["to:rocky", "intent:question"], 20);
    let mine = say(&s, "rocky", "rocky", "a note after the resume", &[], 10);

    let since = resolve_since(&s, "rocky", None, NOW).unwrap();
    assert_eq!((since.kind, since.id.as_deref()), ("resume", Some(resume.as_str())), "the default point is the last RESUME");
    let page = delta(&s, &rocky(), &[], &since, NOW - 120 * MIN, NOW).unwrap();
    assert_eq!(page["schema"], "delta/1");

    // ROOMS: #core changed, with its counts and its State's new head; #ops is quiet; #elsewhere is not followed at all.
    let core = room(&page, "core").expect("core changed");
    assert_eq!(core["new"], 4, "four records after the point: {core}");
    assert_eq!(core["to_me"], 2, "the addressed one and the reply to rocky's ask: {core}");
    let mut to_me = ids(&core["to_me_lines"]);
    to_me.sort();
    let mut want = vec![for_me.as_str(), receipt.as_str()];
    want.sort();
    assert_eq!(to_me, want);
    assert_eq!(core["state"]["changed"], true);
    assert_eq!(core["state"]["head"], core_state.as_str());
    assert!(core["why"].as_array().unwrap().iter().any(|w| w == "state"), "followed because rocky's State names it: {core}");
    assert!(room(&page, "ops").is_none() && page["quiet"].as_array().unwrap().iter().any(|r| r == "ops"), "ops is quiet: {page}");
    assert!(room(&page, "elsewhere").is_none() && !page["quiet"].as_array().unwrap().iter().any(|r| r == "elsewhere"));

    // ASKS: the question to rocky opened since; R-1 (opened before) closed since, and is not counted as opened.
    assert_eq!(ids(&page["asks"]["opened"]), vec![question.as_str()]);
    assert_eq!(ids(&page["asks"]["closed"]), vec![ask.as_str()]);

    // WORDS: Blaze's new record, head line only.
    let words = page["words"].as_array().unwrap().iter().find(|w| w["room"] == "blaze").unwrap();
    assert_eq!(words["new"], 1);
    assert_eq!(ids(&words["items"]), vec![new_words.as_str()]);
    assert_eq!(words["items"][0]["line"], "new words", "the head line, not the record");

    // MACHINE: the server started before the point, so no restart since; rocky's own record after the point.
    assert_eq!(page["machine"]["restarted_since"], false);
    assert_eq!(ids(&page["machine"]["mine"]), vec![mine.as_str()]);
    assert_eq!(page["machine"]["mine_since"], 1, "the RESUME is the point, not after it");

    // Nothing from before is on the page (R-1 appears only as the ask closed since; the header names the point itself).
    let mut body = page.clone();
    body.as_object_mut().unwrap().remove("since");
    let text = body.to_string();
    for (what, id) in [("old news", &old_news), ("old to me", &old_to_me), ("old words", &old_words), ("the resume", &resume), ("elsewhere", &elsewhere)] {
        assert!(!text.contains(id.as_str()), "{what} ({id}) is not after the point: {text}");
    }
    assert!(text.contains(new_news.as_str()) || core["new"] == 4, "the after is counted");

    // NEGATIVE CONTROL: from the newest record, nothing changed and every followed room is quiet.
    let last = resolve_since(&s, "rocky", Some(&mine), NOW).unwrap();
    let still = delta(&s, &rocky(), &[], &last, NOW - 120 * MIN, NOW).unwrap();
    assert!(still["rooms"].as_array().unwrap().is_empty(), "{still}");
    assert!(still["asks"]["opened"].as_array().unwrap().is_empty() && still["asks"]["closed"].as_array().unwrap().is_empty(), "{still}");
    assert_eq!(still["machine"]["mine_since"], 0);
    // A restart after the point is said.
    assert_eq!(delta(&s, &rocky(), &[], &since, NOW - 5 * MIN, NOW).unwrap()["machine"]["restarted_since"], true);
}

#[test]
fn since_takes_an_id_a_time_or_a_span_and_a_seat_without_a_resume_must_give_one() {
    let s = oracle();
    let a = say(&s, "core", "blaze", "first", &[], 120);
    let b = say(&s, "core", "blaze", "second", &[], 30);
    let by_id = resolve_since(&s, "rocky", Some(&a), NOW).unwrap();
    assert_eq!((by_id.kind, by_id.seq), ("record", s.get(&a).unwrap().seq));
    // a span: an hour back puts the cursor between the two records
    let span = resolve_since(&s, "rocky", Some("1h"), NOW).unwrap();
    assert_eq!(span.kind, "time");
    assert!(span.seq >= s.get(&a).unwrap().seq && span.seq < s.get(&b).unwrap().seq, "{span:?}");
    // a time in UTC, to the minute: the same cursor
    let at = oracle_proto::iso_ms(NOW - 60 * MIN);
    let time = resolve_since(&s, "rocky", Some(&at[..16]), NOW).unwrap();
    assert_eq!(time.seq, span.seq, "{at}");
    // NEGATIVE CONTROLS: no RESUME and no since is refused with what to give; a future time is refused.
    match resolve_since(&s, "dinesh", None, NOW) {
        Err(StoreError::Rejected(m)) => assert!(m.contains("no RESUME") && m.contains("6h"), "{m}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(resolve_since(&s, "rocky", Some(&oracle_proto::iso_ms(NOW + 600 * MIN)[..16]), NOW).is_err());
}
