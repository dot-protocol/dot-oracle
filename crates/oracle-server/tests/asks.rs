//! Layer 1 of docs/ASKS-AND-EVENTS.md: asks, options and the three lists, on the real store with real
//! signed writes. Nothing here is stored as status: every list is read out of the messages.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::asks::{asks_for, asks_for_with, counts, thread_members_batched, thread_roots_batched, StateFilter, Walk, LIST_CAP};
use oracle_server::store::{Principal, Scope, Store, StoreError};

const NOW: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

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
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 10 * DAY).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3), ("bobby-core", 4)] {
        let ev = KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 10 * DAY);
        s.apply_key_event(&ev, NOW - 10 * DAY).unwrap();
    }
    s
}
fn tags(t: &[&str]) -> Vec<String> {
    t.iter().map(|x| x.to_string()).collect()
}
/// Write one message; `at` is milliseconds before NOW.
fn try_write(s: &Store, (room, who): (&str, &str), kind: Kind, text: &str, cargo: Option<&str>, t: &[&str], ago: i64) -> Result<String, StoreError> {
    let at = NOW - ago;
    let env = Envelope::sign(&sk(key_of(who)), room, who, kind, text, cargo.map(str::to_owned), tags(t), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).map(|r| r.id)
}
fn say(s: &Store, room: &str, who: &str, text: &str, t: &[&str], ago: i64) -> String {
    try_write(s, (room, who), Kind::Say, text, None, t, ago).unwrap()
}
fn ids(rows: &[oracle_server::asks::AskRow]) -> Vec<&str> {
    rows.iter().map(|r| r.id.as_str()).collect()
}
const OPTS: &str = r#"{"options":["Ship now","Wait for CI"],"recommendation":{"option":2,"why":"CI is red"}}"#;

#[test]
fn an_undeclared_message_never_appears_but_a_question_asks_for_a_reply_by_itself() {
    let s = oracle();
    let plain = say(&s, "core", "rocky", "just so you know", &["to:dinesh"], HOUR);
    let note = say(&s, "core", "rocky", "fyi", &["to:dinesh", "intent:note", "ask:none"], HOUR);
    let read = say(&s, "core", "rocky", "please read", &["to:dinesh", "ask:read"], HOUR);
    let q = say(&s, "core", "rocky", "which port?", &["to:dinesh", "intent:question"], HOUR);
    let a = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&a.waiting_on_me), vec![q.as_str()], "only the question asks for anything");
    for id in [&plain, &note, &read] {
        assert!(!ids(&a.waiting_on_me).contains(&id.as_str()));
    }
    assert!(a.im_waiting_on.is_empty() && a.done_today.is_empty());
}

#[test]
fn an_act_ask_is_on_both_lists_with_who_holds_it_and_nobody_elses() {
    let s = oracle();
    let id = say(&s, "core", "rocky", "ship R-101", &["to:dinesh", "ask:act"], HOUR);
    let me = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&me.waiting_on_me), vec![id.as_str()]);
    assert!(me.im_waiting_on.is_empty());
    let them = asks_for(&s, "rocky", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&them.im_waiting_on), vec![id.as_str()]);
    assert_eq!(them.im_waiting_on[0].held_by, vec!["dinesh".to_string()]);
    assert_eq!(them.im_waiting_on[0].ask, "act");
    assert!(them.waiting_on_me.is_empty());
    let other = asks_for(&s, "bobby-core", None, StateFilter::All, NOW).unwrap();
    assert!(other.waiting_on_me.is_empty() && other.im_waiting_on.is_empty());
}

#[test]
fn a_chat_reply_answers_an_act_ask_but_only_a_receipt_from_the_addressee_completes_it() {
    let s = oracle();
    let id = say(&s, "core", "rocky", "ship R-101", &["to:dinesh", "ask:act"], 3 * HOUR);
    // Someone who was not asked says a receipt: not the addressee, so not done.
    say(&s, "core", "bobby-core", "done!", &[&format!("reply:{id}"), "intent:receipt"], 2 * HOUR);
    let a = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&a.waiting_on_me), vec![id.as_str()], "a receipt from the wrong seat completes nothing");
    // The addressee chats: answered, not done.
    say(&s, "core", "dinesh", "looking", &[&format!("reply:{id}")], HOUR);
    let a = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&a.waiting_on_me), vec![id.as_str()]);
    assert_eq!(a.waiting_on_me[0].state, "answered");
    assert!(a.done_today.is_empty());
    // The addressee's receipt completes it, for both sides.
    let rc = say(&s, "core", "dinesh", "shipped abc123", &[&format!("reply:{id}"), "intent:receipt"], 60_000);
    let a = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert!(a.waiting_on_me.is_empty());
    assert_eq!(ids(&a.done_today), vec![id.as_str()]);
    assert_eq!(a.done_today[0].state, "done");
    assert_eq!(a.done_today[0].done_by.as_deref(), Some(rc.as_str()), "the addressee's receipt is what completed it");
    let r = asks_for(&s, "rocky", None, StateFilter::All, NOW).unwrap();
    assert!(r.im_waiting_on.is_empty());
    assert_eq!(ids(&r.done_today), vec![id.as_str()]);
}

#[test]
fn a_decide_ask_needs_two_to_six_options_and_they_round_trip_signed() {
    let s = oracle();
    let refused = |cargo: Option<&str>| {
        match try_write(&s, ("core", "rocky"), Kind::Say, "ship?", cargo, &["ask:decide", "to:blaze"], HOUR) {
            Err(StoreError::Rejected(m)) => m,
            other => panic!("expected a refusal, got {other:?}"),
        }
    };
    assert!(refused(None).contains("options"));
    assert!(refused(Some(r#"{"options":["only one"]}"#)).contains("2 to 6"));
    assert!(refused(Some(r#"{"options":["a","b","c","d","e","f","g"]}"#)).contains("2 to 6"));
    assert!(refused(Some(r#"{"options":["a","a"]}"#)).contains("distinct"));
    assert!(refused(Some(r#"{"options":["a",""]}"#)).contains("empty"));
    assert!(refused(Some(&format!(r#"{{"options":["a","{}"]}}"#, "x".repeat(81)))).contains("80"));
    // Every decide carries the asker's pick: none, one out of range, or no why is refused (Blaze, 27 Oct).
    assert!(refused(Some(r#"{"options":["a","b"]}"#)).contains("recommendation.option"));
    assert!(refused(Some(r#"{"options":["a","b"],"recommendation":{"option":3,"why":"w"}}"#)).contains("recommendation.option"));
    assert!(refused(Some(r#"{"options":["a","b"],"recommendation":{"option":1}}"#)).contains("recommendation.why"));
    let id = try_write(&s, ("core", "rocky"), Kind::Say, "ship?", Some(OPTS), &["ask:decide", "to:blaze"], HOUR).unwrap();
    let env = s.envelope_of(&id).unwrap();
    assert_eq!(env.cargo.as_deref(), Some(OPTS), "the options are inside the signed envelope");
    assert!(env.verify(&pubkey_hex(&sk(2)), NOW - HOUR).is_ok());
    let a = asks_for(&s, "blaze", None, StateFilter::All, NOW).unwrap();
    assert_eq!(a.waiting_on_me[0].options, vec!["Ship now".to_string(), "Wait for CI".to_string()]);
}

#[test]
fn a_choice_closes_the_ask_and_is_checked_against_the_options() {
    let s = oracle();
    let id = try_write(&s, ("core", "rocky"), Kind::Say, "ship?", Some(OPTS), &["ask:decide", "to:blaze"], 2 * HOUR).unwrap();
    let choice = |who: &str, n: &str, parent: &str| try_write(&s, ("core", who), Kind::Say, "my pick", None, &[&format!("reply:{parent}"), &format!("choice:{n}")], HOUR);
    for bad in ["0", "3", "x", "-1", "7"] {
        assert!(matches!(choice("blaze", bad, &id), Err(StoreError::Rejected(_))), "choice:{bad}");
    }
    let orphan = try_write(&s, ("core", "blaze"), Kind::Say, "pick", None, &["choice:1"], HOUR);
    assert!(matches!(orphan, Err(StoreError::Rejected(m)) if m.contains("points at nothing")));
    let plain = say(&s, "core", "rocky", "no options here", &["ask:reply", "to:blaze"], HOUR);
    assert!(matches!(choice("blaze", "1", &plain), Err(StoreError::Rejected(m)) if m.contains("no options")));
    let before = asks_for(&s, "blaze", None, StateFilter::All, NOW).unwrap();
    assert!(ids(&before.waiting_on_me).contains(&id.as_str()));
    let pick = choice("blaze", "2", &id).unwrap();
    let after = asks_for(&s, "blaze", None, StateFilter::All, NOW).unwrap();
    assert!(!ids(&after.waiting_on_me).contains(&id.as_str()));
    let row = after.done_today.iter().find(|r| r.id == id).expect("answered today");
    assert_eq!((row.choice, row.answered_by.as_deref()), (Some(2), Some(pick.as_str())));
    assert_eq!(row.state, "answered");
    // A free reply also closes it, with no choice.
    let id2 = try_write(&s, ("core", "rocky"), Kind::Say, "and now?", Some(OPTS), &["ask:decide", "to:blaze"], 2 * HOUR).unwrap();
    say(&s, "core", "blaze", "neither, wait a day", &[&format!("reply:{id2}")], HOUR);
    let after = asks_for(&s, "blaze", None, StateFilter::All, NOW).unwrap();
    let row = after.done_today.iter().find(|r| r.id == id2).unwrap();
    assert_eq!(row.choice, None);
}

#[test]
fn a_review_ask_points_at_a_ref() {
    let s = oracle();
    let no_ref = try_write(&s, ("core", "rocky"), Kind::Say, "look at this", None, &["ask:review", "to:dinesh"], HOUR);
    assert!(matches!(no_ref, Err(StoreError::Rejected(m)) if m.contains("ref")));
    let id = say(&s, "core", "rocky", "review PR 24", &["ask:review", "to:dinesh", "ref:https://github.com/example/oracle/pull/24"], HOUR);
    let a = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!((a.waiting_on_me[0].id.as_str(), a.waiting_on_me[0].ask.as_str()), (id.as_str(), "review"));
}

#[test]
fn an_unanswered_decide_or_review_past_its_clock_is_overdue() {
    let s = oracle();
    let past = (NOW - HOUR).to_string();
    let future = (NOW + HOUR).to_string();
    let late = try_write(&s, ("core", "rocky"), Kind::Say, "ship?", Some(OPTS), &["ask:decide", "to:blaze", &format!("by:{past}")], 2 * HOUR).unwrap();
    let fine = try_write(&s, ("core", "rocky"), Kind::Say, "and this?", Some(OPTS), &["ask:decide", "to:blaze", &format!("by:{future}")], 2 * HOUR).unwrap();
    let a = asks_for(&s, "blaze", None, StateFilter::All, NOW).unwrap();
    let get = |id: &str| a.waiting_on_me.iter().find(|r| r.id == id).unwrap();
    assert!(get(&late).overdue);
    assert!(!get(&fine).overdue);
    assert_eq!(get(&late).by_ms, Some(NOW - HOUR));
}

#[test]
fn the_author_beats_a_person_who_beats_a_model_when_an_annotation_classifies_a_message() {
    let s = oracle();
    // A message the author left unclassified.
    let m = say(&s, "core", "blaze", "someone must fix the ledger", &[], 5 * HOUR);
    let note = |who: &str, by: &str, set: &[&str], ago: i64| {
        let mut t = vec![format!("annotates:{m}"), format!("set:by={by}")];
        t.extend(set.iter().map(|x| format!("set:{x}")));
        let tags: Vec<&str> = t.iter().map(String::as_str).collect();
        try_write(&s, ("core", who), Kind::Annotate, "why", None, &tags, ago).unwrap();
    };
    // Nothing yet: it asks nothing.
    assert!(asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap().waiting_on_me.is_empty());
    // A model reads it: an act. It may say what the message is; it cannot say whom it is for.
    note("bobby-core", "model", &["ask=act", "to=dinesh"], 4 * HOUR);
    let a = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert!(a.waiting_on_me.is_empty(), "a model's guess at an addressee addresses no one");
    let b = asks_for(&s, "blaze", None, StateFilter::All, NOW).unwrap();
    assert_eq!((ids(&b.im_waiting_on), b.im_waiting_on[0].ask.as_str(), b.im_waiting_on[0].held_by.is_empty()), (vec![m.as_str()], "act", true), "the ask is classified, and held by nobody");
    assert!(b.im_waiting_on[0].classified_by.is_some());
    // A person names the addressee: an act for dinesh.
    note("rocky", "person", &["ask=act", "to=dinesh"], 3 * HOUR + 30 * 60_000);
    let a = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&a.waiting_on_me), vec![m.as_str()]);
    // A person corrects it later: a reply for rocky. The person beats the model even though the model is not older.
    note("rocky", "person", &["ask=reply", "to=rocky"], 3 * HOUR);
    note("bobby-core", "model", &["ask=act", "to=dinesh"], 2 * HOUR);
    let a = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert!(a.waiting_on_me.is_empty(), "the model's newer reading does not undo the person's");
    let r = asks_for(&s, "rocky", None, StateFilter::All, NOW).unwrap();
    assert_eq!((ids(&r.waiting_on_me), r.waiting_on_me[0].ask.as_str()), (vec![m.as_str()], "reply"));
    // The author's own tags outrank every annotation.
    let own = say(&s, "core", "blaze", "own words", &["ask:act", "to:dinesh"], 5 * HOUR);
    let mut t = vec![format!("annotates:{own}"), "set:ask=none".to_string()];
    t.push("set:to=rocky".into());
    let tags: Vec<&str> = t.iter().map(String::as_str).collect();
    try_write(&s, ("core", "rocky"), Kind::Annotate, "person says none", None, &tags, HOUR).unwrap();
    let a = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&a.waiting_on_me), vec![own.as_str()], "the author's ask:act stands");
    // ...but the author may change their own mind (Bobby, 2026-09-30): the author's own annotation withdraws it.
    let tags = [format!("annotates:{own}"), "set:ask=none".to_string()];
    let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
    try_write(&s, ("core", "blaze"), Kind::Annotate, "done, no reply needed", None, &tags, HOUR / 2).unwrap();
    let a = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert!(a.waiting_on_me.is_empty(), "the author's own annotation withdraws the author's own ask");
}

/// Rocky, 2026-10-16: an ask that is the sixth reply of an exchange was on neither list, because its thread was read
/// only four links deep and the ask itself was never judged. However deep an ask sits, it is listed.
#[test]
fn an_ask_deep_in_a_long_exchange_is_on_both_lists() {
    let s = oracle();
    let mut parent = say(&s, "hustler", "dinesh", "scan: two setups moved", &[], 20 * HOUR);
    for i in 1..=5 {
        let who = if i % 2 == 1 { "rocky" } else { "dinesh" };
        parent = say(&s, "hustler", who, &format!("note {i}"), &[&format!("reply:{parent}")], (20 - i) * HOUR);
    }
    let deep = say(&s, "hustler", "rocky", "live now: create the sleeve", &[&format!("reply:{parent}"), "to:dinesh", "ask:act"], HOUR);
    let mine = asks_for(&s, "rocky", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&mine.im_waiting_on), vec![deep.as_str()], "the sixth reply is an ask I am waiting on");
    let theirs = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&theirs.waiting_on_me), vec![deep.as_str()], "and an ask waiting on its addressee");
}

#[test]
fn i_am_waiting_on_scans_every_room_and_a_room_narrows_it() {
    let s = oracle();
    let a = say(&s, "core", "rocky", "in core", &["to:dinesh", "ask:act"], HOUR);
    let b = say(&s, "hustler", "rocky", "in hustler", &["to:bobby-core", "ask:reply"], HOUR);
    let all = asks_for(&s, "rocky", None, StateFilter::All, NOW).unwrap();
    let mut got = ids(&all.im_waiting_on);
    got.sort();
    let mut want = vec![a.as_str(), b.as_str()];
    want.sort();
    assert_eq!(got, want);
    let core = asks_for(&s, "rocky", Some("core"), StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&core.im_waiting_on), vec![a.as_str()]);
    assert_eq!(core.im_waiting_on[0].room, "core");
}

#[test]
fn done_today_is_only_today_and_the_state_filter_picks_the_lists() {
    let s = oracle();
    // NOW is 2027-01-15 08:00 UTC; midnight was 8 hours ago.
    let old = say(&s, "core", "rocky", "old ask", &["to:dinesh", "ask:reply"], 2 * DAY);
    say(&s, "core", "dinesh", "answered long ago", &[&format!("reply:{old}")], DAY + HOUR);
    let fresh = say(&s, "core", "rocky", "fresh ask", &["to:dinesh", "ask:reply"], 3 * HOUR);
    say(&s, "core", "dinesh", "answered today", &[&format!("reply:{fresh}")], HOUR);
    let open = say(&s, "core", "rocky", "still open", &["to:dinesh", "ask:reply"], HOUR);
    let all = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&all.done_today), vec![fresh.as_str()], "yesterday's answer is not done today");
    let open_only = asks_for(&s, "dinesh", None, StateFilter::Open, NOW).unwrap();
    assert_eq!(ids(&open_only.waiting_on_me), vec![open.as_str()]);
    assert!(open_only.done_today.is_empty());
    let done_only = asks_for(&s, "dinesh", None, StateFilter::Done, NOW).unwrap();
    assert!(done_only.waiting_on_me.is_empty() && done_only.im_waiting_on.is_empty());
    assert_eq!(ids(&done_only.done_today), vec![fresh.as_str()]);
}

#[test]
fn an_archived_or_superseded_ask_leaves_the_lists() {
    let s = oracle();
    let a = say(&s, "core", "rocky", "cancelled ask", &["to:dinesh", "ask:act"], 3 * HOUR);
    let b = say(&s, "core", "rocky", "replaced ask", &["to:dinesh", "ask:act"], 3 * HOUR);
    say(&s, "core", "rocky", "never mind", &[&format!("archives:{a}")], 2 * HOUR);
    let b2 = say(&s, "core", "rocky", "better ask", &["to:dinesh", "ask:act", &format!("supersedes:{b}")], 2 * HOUR);
    let got = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&got.waiting_on_me), vec![b2.as_str()]);
}

#[test]
fn an_at_mention_addresses_an_ask_and_a_to_list_names_every_seat_in_it() {
    let s = oracle();
    // Addressed by @mention alone: no to: tag at all (four asks in ten are addressed this way).
    let m = say(&s, "core", "rocky", "@dinesh please rebuild the index", &["ask:act"], HOUR);
    // A seat mentioning itself addresses nobody.
    let own = say(&s, "core", "rocky", "note to @rocky: check the disk", &["ask:reply"], HOUR);
    // A to: list is several seats, not one seat named "rocky,bobby-core".
    let list = say(&s, "core", "blaze", "you two decide", &["ask:reply", "to:rocky,bobby-core"], HOUR);
    // to: and a mention together: each seat once.
    let both = say(&s, "core", "blaze", "@dinesh and rocky, look", &["ask:act", "to:rocky"], HOUR);
    let d = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    let mut got = ids(&d.waiting_on_me);
    got.sort();
    let mut want = vec![m.as_str(), both.as_str()];
    want.sort();
    assert_eq!(got, want, "dinesh is waiting on the two asks that mention him");
    assert_eq!(d.waiting_on_me.iter().find(|r| r.id == m).unwrap().held_by, vec!["dinesh".to_string()]);
    let r = asks_for(&s, "rocky", None, StateFilter::All, NOW).unwrap();
    let mut got = ids(&r.waiting_on_me);
    got.sort();
    let mut want = vec![list.as_str(), both.as_str()];
    want.sort();
    assert_eq!(got, want, "rocky: the list and the to: (his own self-mention is not an address)");
    let mut mine = ids(&r.im_waiting_on);
    mine.sort();
    let mut want = vec![m.as_str(), own.as_str()];
    want.sort();
    assert_eq!(mine, want);
    assert!(r.im_waiting_on.iter().find(|x| x.id == own).unwrap().held_by.is_empty(), "a self-mention holds nobody");
    let b = asks_for(&s, "bobby-core", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&b.waiting_on_me), vec![list.as_str()]);
    let row = b.waiting_on_me.iter().find(|x| x.id == list).unwrap();
    assert_eq!(row.held_by, vec!["rocky".to_string(), "bobby-core".to_string()]);
    let both_row = d.waiting_on_me.iter().find(|x| x.id == both).unwrap();
    assert_eq!(both_row.held_by, vec!["rocky".to_string(), "dinesh".to_string()], "to: first, then the mention, no duplicates");
    // The mentioned seat is a real addressee for done: their receipt completes an act-ask.
    say(&s, "core", "dinesh", "index rebuilt", &[&format!("reply:{m}"), "intent:receipt"], 60_000);
    let d = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(d.done_today.iter().find(|x| x.id == m).map(|x| x.state), Some("done"));
}

#[test]
fn a_note_to_rocky_is_never_on_blazes_list_however_a_classifier_guesses() {
    // 2026-09-29, live: dinesh's PR notes to rocky, tagged only `reply:`, were classified by a model as `ask=act, to=blaze` and appeared
    // in blaze's waiting_on_me. The room is his; that is no reason to hold everything for him.
    let s = oracle();
    let ask = say(&s, "core", "rocky", "please review PR 28", &["ask:act", "to:dinesh"], 3 * HOUR);
    let note = say(&s, "core", "dinesh", "PR 28 is up: yours to merge", &[&format!("reply:{ask}")], 2 * HOUR);
    for (id, extra) in [(&ask, "to=blaze"), (&note, "to=blaze")] {
        let tags = [format!("annotates:{id}"), "set:by=model".into(), "set:ask=act".into(), extra.replace("to=", "set:to=")];
        let t: Vec<&str> = tags.iter().map(String::as_str).collect();
        try_write(&s, ("core", "bobby-core"), Kind::Annotate, "model reading", None, &t, HOUR).unwrap();
    }
    let blaze = asks_for(&s, "blaze", None, StateFilter::All, NOW).unwrap();
    assert!(blaze.waiting_on_me.is_empty(), "nothing names blaze: {:?}", ids(&blaze.waiting_on_me));
    let dinesh = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&dinesh.waiting_on_me), vec![ask.as_str()], "the ask that names dinesh is still his");
    // a message that does name him is his, by `to:` or by @mention
    let by_to = say(&s, "core", "rocky", "your call", &["ask:reply", "to:blaze"], HOUR);
    let by_at = say(&s, "core", "rocky", "@blaze please look", &["ask:reply"], HOUR);
    let mine = ids(&asks_for(&s, "blaze", None, StateFilter::All, NOW).unwrap().waiting_on_me).iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert!(mine.contains(&by_to) && mine.contains(&by_at), "a to: or an @mention addresses: {mine:?}");
}

#[test]
fn every_row_says_which_room_it_is_in() {
    let s = oracle();
    say(&s, "core", "rocky", "in core", &["to:dinesh", "ask:act"], HOUR);
    say(&s, "hustler", "rocky", "in hustler", &["to:dinesh", "ask:reply"], HOUR);
    let a = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    let v = serde_json::to_value(&a).unwrap();
    let mut rooms: Vec<&str> = v["waiting_on_me"].as_array().unwrap().iter().map(|r| r["room"].as_str().expect("room is a string, never null")).collect();
    rooms.sort();
    assert_eq!(rooms, ["core", "hustler"]);
}


/// Bobby, 2026-09-30: an ask four replies deep was closed twice and annotated twice by its author and stayed listed,
/// because the thread walk stops at four levels and never loaded what pointed at it.
#[test]
fn an_ask_deep_in_a_thread_sees_its_own_close_and_its_authors_annotation() {
    let s = oracle();
    let mut parent = say(&s, "core", "dinesh", "root", &[], 10 * HOUR);
    for i in 0..3 {
        parent = say(&s, "core", "dinesh", &format!("level {}", i + 1), &[&format!("reply:{parent}")], 9 * HOUR - i * 60_000);
    }
    let deep = say(&s, "core", "rocky", "a question, four deep", &["ask:reply", "to:dinesh", &format!("reply:{parent}")], 8 * HOUR);
    let listed = |seat: &str| asks_for(&s, seat, None, StateFilter::Open, NOW).unwrap().waiting_on_me.iter().any(|r| r.id == deep);
    assert!(listed("dinesh"), "the deep ask is listed to begin with");
    let tags = [format!("annotates:{deep}"), "set:ask=none".to_string()];
    let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
    try_write(&s, ("core", "rocky"), Kind::Annotate, "withdrawn", None, &tags, 7 * HOUR).unwrap();
    assert!(!listed("dinesh"), "its author's annotation reaches it at depth four");
}

#[test]
fn a_question_to_two_seats_stays_on_the_list_of_the_one_who_has_not_answered() {
    // Blaze, 2026-10-08: Noor asked Emmy and Rocky; Emmy answered; Rocky's part left his list.
    let s = oracle();
    let q = say(&s, "math", "bobby-core", "what have you each built?", &["to:rocky", "to:dinesh", "intent:question"], 3 * HOUR);
    let _a = say(&s, "math", "dinesh", "the index, see PR #31", &[&format!("reply:{q}"), "intent:result"], 2 * HOUR);
    let rocky = asks_for(&s, "rocky", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&rocky.waiting_on_me), vec![q.as_str()], "Dinesh's answer is not Rocky's");
    assert_eq!(rocky.waiting_on_me[0].state, "open");
    assert!(rocky.waiting_on_me[0].answered_by.is_some(), "the other answer is shown beside it");
    assert!(!ids(&rocky.done_today).contains(&q.as_str()));
    let dinesh = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert!(dinesh.waiting_on_me.is_empty(), "the seat that answered is done");
    let author = asks_for(&s, "bobby-core", None, StateFilter::All, NOW).unwrap();
    assert!(author.im_waiting_on.is_empty(), "for its author the question was answered");
    // Rocky answers: it leaves his list.
    say(&s, "math", "rocky", "the experiment registry", &[&format!("reply:{q}")], HOUR);
    let rocky = asks_for(&s, "rocky", None, StateFilter::All, NOW).unwrap();
    assert!(rocky.waiting_on_me.is_empty());
    // A question to one seat, answered by someone else, is still answered (unchanged).
    let one = say(&s, "math", "bobby-core", "which port?", &["to:rocky", "intent:question"], HOUR);
    say(&s, "math", "dinesh", "8893", &[&format!("reply:{one}")], HOUR / 2);
    assert!(!ids(&asks_for(&s, "rocky", None, StateFilter::All, NOW).unwrap().waiting_on_me).contains(&one.as_str()));
}

#[test]
fn a_message_that_names_a_seat_and_asks_a_question_waits_on_it_without_an_ask_tag() {
    let s = oracle();
    let implied = say(&s, "math", "bobby-core", "Round 31.\nIs there a keyword-only version that works right now?", &["to:rocky", "intent:result"], HOUR);
    let mention = say(&s, "math", "bobby-core", "@rocky what is your threat model?", &[], HOUR);
    let url = say(&s, "math", "bobby-core", "see /search?q=dbit for the rows", &["to:rocky"], HOUR);
    let status = say(&s, "math", "bobby-core", "done; anything else?", &["to:rocky", "intent:status"], HOUR);
    let none = say(&s, "math", "bobby-core", "fyi, nothing needed?", &["to:rocky", "ask:none"], HOUR);
    let old = say(&s, "math", "bobby-core", "still there?", &["to:rocky"], 8 * DAY);
    let other = say(&s, "math", "bobby-core", "which build?", &["to:dinesh"], HOUR);
    let a = asks_for(&s, "rocky", None, StateFilter::All, NOW).unwrap();
    let w = ids(&a.waiting_on_me);
    assert!(w.contains(&implied.as_str()) && w.contains(&mention.as_str()), "{w:?} implied {implied} mention {mention}");
    for id in [&url, &status, &none, &old, &other] {
        assert!(!w.contains(&id.as_str()), "{id} is not an ask for rocky");
    }
    let row = a.waiting_on_me.iter().find(|r| r.id == implied).unwrap();
    assert_eq!(row.ask, "reply");
    assert!(row.classified_by.as_deref().unwrap_or("").starts_with("implied"), "it says why it is listed");
    // It is the addressee's list that must not lose it; the author declared nothing, so theirs stays as it was.
    assert!(!ids(&asks_for(&s, "bobby-core", None, StateFilter::All, NOW).unwrap().im_waiting_on).contains(&implied.as_str()));
    // A reply settles it.
    say(&s, "math", "rocky", "yes: oracle_recall with order=rank", &[&format!("reply:{implied}")], HOUR / 2);
    assert!(!ids(&asks_for(&s, "rocky", None, StateFilter::All, NOW).unwrap().waiting_on_me).contains(&implied.as_str()));
}

// GET /asks is answered once per change (AsksCache, Blaze's go 17 Oct): one walk per question while nothing new is written,
// a walk again after any write (read your own writes), never older than MAX_AGE_MS, and always exactly what asks_for says.
#[test]
fn the_asks_cache_walks_once_per_change_and_never_outlives_its_windows() {
    use oracle_server::asks::{AsksCache, MAX_AGE_MS};
    use std::sync::atomic::Ordering::Relaxed;
    let s = oracle();
    say(&s, "room", "rocky", "can you check the export list?", &["to:dinesh", "ask:reply"], HOUR);
    let c = AsksCache::default();
    let walks = |c: &AsksCache| (c.walks.load(Relaxed), c.hits.load(Relaxed));
    let direct = |at: i64| serde_json::to_value(asks_for(&s, "dinesh", None, StateFilter::All, at).unwrap()).unwrap();

    let first = c.get(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert_eq!(*first, direct(NOW), "the kept answer is exactly asks_for's");
    c.get(&s, "dinesh", None, StateFilter::All, NOW + 2_010).unwrap();
    assert_eq!(walks(&c), (1, 1), "nothing new written: the same question is one indexed lookup");
    c.get(&s, "rocky", None, StateFilter::All, NOW).unwrap();
    c.get(&s, "dinesh", Some("room"), StateFilter::All, NOW).unwrap();
    assert_eq!(walks(&c).0, 3, "another seat or another room is another question");

    let late = say(&s, "room", "rocky", "and the licence file?", &["to:dinesh", "ask:reply"], 0);
    let soon = c.get(&s, "dinesh", None, StateFilter::All, NOW + 100).unwrap();
    assert!(soon.to_string().contains(&late), "a write is seen by the very next question, however soon (e2e: say, then asks)");
    assert_eq!(*soon, direct(NOW + 100));

    let (w, _) = walks(&c);
    c.get(&s, "dinesh", None, StateFilter::All, NOW + 100 + MAX_AGE_MS).unwrap();
    assert_eq!(walks(&c).0, w + 1, "an answer is never older than MAX_AGE_MS, so ages and deadlines move on");
}

#[test]
fn the_same_question_asked_at_once_is_one_walk() {
    use oracle_server::asks::AsksCache;
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::Arc;
    let s = Arc::new(oracle());
    for i in 0..20 {
        say(&s, "room", "rocky", &format!("question {i}?"), &["to:dinesh", "ask:reply"], HOUR + i);
    }
    let c = Arc::new(AsksCache::default());
    let threads: Vec<_> = (0..8).map(|_| { let (s, c) = (s.clone(), c.clone()); std::thread::spawn(move || c.get(&s, "dinesh", None, StateFilter::All, NOW).unwrap()) }).collect();
    let answers: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(c.walks.load(Relaxed), 1, "eight concurrent identical questions share one walk");
    assert!(answers.iter().all(|a| a == &answers[0]));
}

/// A handle quoted or in code is a reference, not an address (Emmy, 2026-10-04):
/// Rocky's Inbox trace to Emmy and Jobs quoted "'@blaze' match" about a regex and landed on Blaze's list.
#[test]
fn a_quoted_or_coded_handle_does_not_address_that_seat_but_a_plain_one_still_does() {
    let s = oracle();
    let quoted = say(&s, "core", "rocky", "@dinesh the bug: '@blaze' match is case-sensitive; also `@bobby-core` in code", &["ask:reply"], HOUR);
    let curly = say(&s, "core", "rocky", "@dinesh he wrote \u{201c}@blaze ship it\u{201d} yesterday", &["ask:reply"], HOUR);
    let plain = say(&s, "core", "rocky", "@dinesh and @blaze, which port?", &["ask:reply"], HOUR);
    let apostrophe = say(&s, "core", "rocky", "Dinesh's fix, @blaze, isn't merged: go?", &["ask:reply"], HOUR);
    let b = asks_for(&s, "blaze", None, StateFilter::All, NOW).unwrap();
    let mine = ids(&b.waiting_on_me);
    assert!(mine.contains(&plain.as_str()), "a plain @blaze still addresses Blaze");
    assert!(mine.contains(&apostrophe.as_str()), "an apostrophe inside a word opens no quote: the @blaze between them counts");
    assert!(!mine.contains(&quoted.as_str()), "a quoted '@blaze' is a reference");
    assert!(!mine.contains(&curly.as_str()), "a curly-quoted @blaze is a reference");
    let bobby = asks_for(&s, "bobby-core", None, StateFilter::All, NOW).unwrap();
    assert!(!ids(&bobby.waiting_on_me).contains(&quoted.as_str()), "a handle in backticks is code");
    let d = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    for id in [&quoted, &curly, &plain] {
        assert!(ids(&d.waiting_on_me).contains(&id.as_str()), "the addressee outside the quote still holds it");
    }
}

/// No cut by count (Emmy's Room contract, 2026-10-04): an ask older than the newest 500 of its tag is still found,
/// and a list longer than LIST_CAP says its full length.
#[test]
fn an_ask_older_than_the_newest_page_of_its_tag_is_still_found_and_totals_are_whole() {
    let s = oracle();
    let old = say(&s, "core", "rocky", "@dinesh the oldest one: which port?", &["ask:act", "to:dinesh"], HOUR);
    for n in 0..620 {
        say(&s, "core", "rocky", &format!("@blaze filler {n}"), &["ask:act", "to:blaze"], HOUR / 2 - n * 1000);
    }
    let d = asks_for(&s, "dinesh", None, StateFilter::Open, NOW).unwrap();
    assert!(ids(&d.waiting_on_me).contains(&old.as_str()), "an ask behind 620 newer ones of its tag is found");
    assert!(!d.capped, "620 is well inside the paged bound");
    let b = asks_for(&s, "blaze", None, StateFilter::Open, NOW).unwrap();
    assert_eq!(b.totals.waiting_on_me, 620, "the total is the whole list");
    assert_eq!(b.waiting_on_me.len(), oracle_server::asks::LIST_CAP, "the rows are cut, the total is not");
}

/// An unanswered ask past its time-to-live is still owed (Blaze 2026-09-30; Emmy's Room contract 2026-10-04):
/// a question whose intent gives it a fuse is listed and marked `aged` once the fuse is spent. A declared ask has no
/// fuse at all (Blaze 2026-10-10, projection.rs): it stays open, never aged.
#[test]
fn an_unanswered_ask_past_its_clock_is_still_listed_and_marked_aged() {
    let s = oracle();
    let old = say(&s, "core", "rocky", "which port, still?", &["intent:question", "to:dinesh"], 20 * DAY);
    let fresh = say(&s, "core", "rocky", "and this one?", &["intent:question", "to:dinesh"], HOUR);
    let declared = say(&s, "core", "rocky", "@dinesh the licence file?", &["ask:act", "to:dinesh"], 20 * DAY);
    let d = asks_for(&s, "dinesh", None, StateFilter::Open, NOW).unwrap();
    let row = d.waiting_on_me.iter().find(|r| r.id == old).expect("a 20-day-old unanswered question is still waiting");
    assert!(row.aged, "and says it is old");
    assert!(!d.waiting_on_me.iter().find(|r| r.id == fresh).unwrap().aged);
    let act = d.waiting_on_me.iter().find(|r| r.id == declared).expect("a declared ask is waiting");
    assert!(!act.aged && act.state == "open", "a declared ask has no fuse: open, not aged");
}


// ── the batched thread walk equals the per-root walk, root by root (Emmy) ──

fn members_equal(s: &Store, roots: &[String]) {
    let batched = thread_members_batched(s, roots, 300).unwrap();
    for r in roots {
        let per_root: Vec<String> = s.thread_members(r, 300).unwrap().iter().map(|o| o.id.clone()).collect();
        let b: Vec<String> = batched[r].iter().map(|o| o.id.clone()).collect();
        assert_eq!(b, per_root, "root {r}: same members, same order");
    }
}

/// A link written under the door, for a fixture the door refuses: `from` gets `reply:<to>` in its tags table row and
/// its `tags_json` (what the walks read; the search row is not touched), so two records can answer each other. A
/// reply to a record that does not exist yet cannot be written, so a genuine cycle needs this (Emmy:
/// the earlier "cycle" fixtures were acyclic).
fn link_back(s: &Store, from: &str, to: &str) {
    s.with_conn(|c| {
        let (seq, tags_json): (i64, String) = c.query_row("SELECT seq, tags_json FROM observations WHERE id=?1", [from], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut tags: Vec<String> = serde_json::from_str(&tags_json).unwrap();
        tags.push(format!("reply:{to}"));
        c.execute("INSERT INTO tags(name, obs_seq) VALUES(?1, ?2)", rusqlite::params![format!("reply:{to}"), seq])?;
        c.execute("UPDATE observations SET tags_json=?1 WHERE seq=?2", rusqlite::params![serde_json::to_string(&tags).unwrap(), seq])?;
        Ok(())
    })
    .unwrap();
}

fn views_equal(s: &Store, seat: &str) -> (u64, u64) {
    let l0 = Store::loads();
    let a = asks_for_with(s, seat, None, StateFilter::All, NOW, Walk::PerRoot).unwrap();
    let l1 = Store::loads();
    let b = asks_for_with(s, seat, None, StateFilter::All, NOW, Walk::Batched).unwrap();
    let l2 = Store::loads();
    assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap(), "the whole view, serialized");
    (l1 - l0, l2 - l1)
}

/// The 2,000-reply and 2,050-reply controls are in tests/asks_heavy.rs, the release-mode verification
/// (scripts/verify-asks-heavy.sh); this file stays within CI's bound.
#[test]
fn a_seventy_deep_chain_a_genuine_cycle_and_a_shared_descendant_walk_the_same() {
    let s = oracle();
    // deep: a chain of 70 replies, beyond THREAD_HOPS
    let deep = say(&s, "room", "blaze", "deep root", &["ask:reply", "to:rocky"], 20 * DAY);
    let mut prev = deep.clone();
    for i in 0..70 {
        prev = say(&s, "room", "blaze", &format!("deep {i}"), &[&format!("reply:{prev}")], 20 * DAY - 1 - i);
    }
    // a genuine cycle: y replies to x, and x is made to reply to y (link_back); each walk's seen set ends it
    let x = say(&s, "room", "blaze", "cycle x", &["ask:reply", "to:rocky"], 15 * DAY);
    let y = say(&s, "room", "blaze", "cycle y", &[&format!("reply:{x}")], 15 * DAY - 1);
    link_back(&s, &x, &y);
    let _xy = say(&s, "room", "blaze", "answers both", &[&format!("reply:{y}"), &format!("re:{x}")], 15 * DAY - 2);
    assert_eq!(s.thread_root(&y).unwrap().id, x, "from y the walk goes to x, whose parent y is seen: the root is x");
    assert_eq!(s.thread_root(&x).unwrap().id, y, "from x the walk goes to y, whose parent x is seen: the root is y");
    // shared descendant: one record resolves a member of root P and a member of root Q
    let p = say(&s, "room", "blaze", "root P", &["ask:reply", "to:rocky"], 12 * DAY);
    let q = say(&s, "room", "blaze", "root Q", &["ask:reply", "to:rocky"], 12 * DAY - 1);
    let pm = say(&s, "room", "blaze", "P member", &[&format!("reply:{p}")], 12 * DAY - 2);
    let qm = say(&s, "room", "blaze", "Q member", &[&format!("reply:{q}")], 12 * DAY - 3);
    let _shared = say(&s, "room", "blaze", "closes both", &[&format!("closes:{pm}"), &format!("closes:{qm}")], 12 * DAY - 4);
    members_equal(&s, &[deep.clone(), x.clone(), y.clone(), p.clone(), q.clone()]);
    let batched = thread_members_batched(&s, std::slice::from_ref(&deep), 300).unwrap();
    assert_eq!(batched[&deep].len(), 64, "the chain stops at THREAD_HOPS levels, as the per-root walk does");
    let (per_root, batched_loads) = views_equal(&s, "rocky");
    assert!(batched_loads < per_root, "fewer row loads: per-root {per_root}, batched {batched_loads}");
}


#[test]
fn the_batched_walk_to_thread_roots_matches_thread_root_for_every_candidate() {
    let s = oracle();
    let root = say(&s, "room", "blaze", "the root", &["ask:reply", "to:rocky"], 30 * DAY);
    let mut prev = root.clone();
    let mut chain = vec![root.clone()];
    for i in 0..70 {
        prev = say(&s, "room", "blaze", &format!("hop {i}"), &[&format!("reply:{prev}")], 30 * DAY - 1 - i);
        chain.push(prev.clone());
    }
    let x = say(&s, "room", "blaze", "cycle x", &["ask:reply", "to:rocky"], 20 * DAY);
    let y = say(&s, "room", "blaze", "cycle y", &[&format!("reply:{x}")], 20 * DAY - 1);
    link_back(&s, &x, &y);
    let dangling = say(&s, "room", "blaze", "points nowhere", &["reply:OBS-room-20200101-000000000000", "ask:reply", "to:rocky"], 10 * DAY);
    let mut candidates = std::collections::BTreeMap::new();
    for id in chain.iter().chain([&x, &y, &dangling]) {
        candidates.insert(id.clone(), s.get(id).unwrap());
    }
    let batched = thread_roots_batched(&s, &candidates).unwrap();
    for id in candidates.keys() {
        assert_eq!(batched[id].id, s.thread_root(id).unwrap().id, "root of {id}");
    }
    assert_eq!(batched[&chain[70]].id, chain[70 - 64], "the 70-deep tail stops after THREAD_HOPS parents, as thread_root does");
    assert_eq!(batched[&dangling].id, dangling, "a link that points nowhere ends the walk where it stands");
    assert_eq!(batched[&x].id, y, "a genuine cycle: from x the walk reaches y and stops before x again");
    assert_eq!(batched[&y].id, x, "and from y it reaches x");
}


/// A recipient named by `to:` holds the ask with no '@' in the text at all, whatever quotes the text carries
/// (the view's mention scan is skipped for such a text; `addressed` keeps the explicit recipient).
#[test]
fn an_explicit_recipient_holds_an_ask_whose_text_names_nobody() {
    let s = oracle();
    let plain = say(&s, "core", "rocky", "the port question, no handle in the text: which?", &["ask:reply", "to:dinesh"], HOUR);
    let quoted = say(&s, "core", "rocky", "he wrote \u{201c}ship it\u{201d} and `main.rs` is merged: go?", &["ask:reply", "to:blaze"], HOUR);
    let d = asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap();
    assert!(ids(&d.waiting_on_me).contains(&plain.as_str()), "to:dinesh holds it without an @");
    let b = asks_for(&s, "blaze", None, StateFilter::All, NOW).unwrap();
    assert!(ids(&b.waiting_on_me).contains(&quoted.as_str()), "to:blaze holds it, quotes and code notwithstanding");
    assert!(!ids(&b.waiting_on_me).contains(&plain.as_str()), "an ask to dinesh alone is not blaze's");
}


/// `Store::room_roots` reads the room's members record through the search index (`starting_with`), and gets what
/// the content scan got: the newest row beginning with the face, by the owner, with the schema; a non-owner's record
/// with the same face, the face quoted mid-text, the same face in another room, an older record, do not change it;
/// a room without the record has no roots; the index rows equal the scan's rows, row for row, newest first.
#[test]
fn room_roots_through_the_index_are_the_scans() {
    let s = oracle();
    let cargo = |roots: &str| format!(r#"{{"schema":"room-members/1","roots":[{roots}]}}"#);
    try_write(&s, ("core", "blaze"), Kind::Say, "[Core members] the first", Some(&cargo(r#""blaze""#)), &["intent:status"], 5 * DAY).unwrap();
    try_write(&s, ("core", "blaze"), Kind::Say, "[Core members] the current", Some(&cargo(r#""blaze","rocky""#)), &["intent:status"], 3 * DAY).unwrap();
    try_write(&s, ("core", "rocky"), Kind::Say, "[Core members] not the owner's", Some(&cargo(r#""rocky""#)), &["intent:status"], 2 * DAY).unwrap();
    say(&s, "core", "rocky", "see the [Core members] record above", &[], DAY);
    try_write(&s, ("hustler", "blaze"), Kind::Say, "[Core members] in another room", Some(&cargo(r#""dinesh""#)), &["intent:status"], HOUR).unwrap();
    try_write(&s, ("hustler", "blaze"), Kind::Say, "[Hustler members]", Some(&cargo(r#""bobby-core""#)), &["intent:status"], HOUR).unwrap();
    assert_eq!(s.room_roots(Some("core")), vec!["blaze".to_owned(), "rocky".to_owned()], "the owner's newest record");
    assert_eq!(s.room_roots(Some("hustler")), vec!["bobby-core".to_owned()]);
    assert!(s.room_roots(Some("dot")).is_empty(), "a room without the record");
    for (room, face) in [("core", "[Core members]"), ("hustler", "[Core members]"), ("hustler", "[Hustler members]"), ("dot", "[Dot members]")] {
        let scan: Vec<String> = s.recent_filtered(Some(room), 20, None, None, Some(face)).unwrap().into_iter().map(|o| o.id).collect();
        let index: Vec<String> = s.starting_with(room, face, 20).unwrap().into_iter().map(|o| o.id).collect();
        assert_eq!(index, scan, "{room} {face}");
    }
    assert_eq!(s.starting_with("core", "[Core members]", 20).unwrap().len(), 3, "three rows begin with the face in core; the quoted one does not");
    // the contract's edge (Emmy): a face that ends inside a token takes the scan and matches as a prefix does
    say(&s, "core", "rocky", "alphabet soup", &[], HOUR);
    for face in ["alpha", "alphabet", "alphabet ", "alphabet s", "[Core members] the"] {
        let scan: Vec<String> = s.recent_filtered(Some("core"), 20, None, None, Some(face)).unwrap().into_iter().map(|o| o.id).collect();
        let index: Vec<String> = s.starting_with("core", face, 20).unwrap().into_iter().map(|o| o.id).collect();
        assert_eq!(index, scan, "face {face:?}");
        assert_eq!(scan.len(), if face == "[Core members] the" { 2 } else { 1 }, "face {face:?}");
    }
    assert_eq!(s.starting_with("core", "[Core members]", 2).unwrap().len(), 2, "the limit holds");
    assert_eq!(s.starting_with("core", "[]", 20).unwrap().len(), 0, "a face without a word takes the scan and finds nothing here");
}

/// Emmy's regression: Rust's `!is_alphanumeric` is not unicode61's token boundary;
/// a combining mark ends the face but not the token, so such a face takes the scan.
#[test]
fn emmy_combining_mark_is_not_a_token_boundary() {
    let s = oracle();
    let face = "alpha\u{301}";
    let content = format!("{face}bet soup");
    say(&s, "core", "blaze", &content, &[], HOUR);
    let scan: Vec<_> = s.recent_filtered(Some("core"),20,None,None,Some(face)).unwrap().into_iter().map(|o|o.id).collect();
    let indexed: Vec<_> = s.starting_with("core",face,20).unwrap().into_iter().map(|o|o.id).collect();
    assert_eq!(indexed,scan);
}

/// The footer's count answer (Rocky, 1 Nov 2026: every ox command fetched 168 KB of open asks to print one line, and the
/// footer timed out): the right numbers against a seeded store, counted before the lists are cut, and never a row.
#[test]
fn the_count_answer_counts_every_list_before_the_cut_and_never_carries_an_ask() {
    let s = oracle();
    // rocky waits on dinesh for LIST_CAP + 5 asks; dinesh chats on one, so it is answered and waits for rocky to close it.
    let mine: Vec<String> = (0..LIST_CAP + 5).map(|i| say(&s, "core", "rocky", &format!("ship R-{i}"), &["to:dinesh", "ask:act"], 10 * HOUR - i as i64 * 1000)).collect();
    say(&s, "core", "dinesh", "looking", &[&format!("reply:{}", mine[3])], HOUR);
    // two asks wait on rocky; the older one was written five hours ago.
    say(&s, "ops", "dinesh", "which port?", &["to:rocky", "intent:question"], 5 * HOUR);
    say(&s, "ops", "blaze", "ship it?", &["to:rocky", "ask:act"], 2 * HOUR);
    let full = serde_json::to_value(asks_for(&s, "rocky", None, StateFilter::Open, NOW).unwrap()).unwrap();
    assert_eq!(full["im_waiting_on"].as_array().unwrap().len(), LIST_CAP, "the list itself is cut");
    let c = counts(&full);
    assert_eq!(c["schema"], "asks-count/1");
    assert_eq!(c["waiting_on_me"], 2);
    assert_eq!(c["oldest_ms"], NOW - 5 * HOUR, "the oldest ask waiting on me");
    assert_eq!(c["im_waiting_on"], LIST_CAP + 5, "counted before the cut");
    assert_eq!(c["im_waiting_on_answered"], 1);
    assert_eq!(c["done_today"], 0);
    // never a row: no list and no object anywhere in the answer, and a small body
    for (k, v) in c.as_object().unwrap() {
        assert!(!v.is_array() && !v.is_object(), "{k} carries {v}");
    }
    assert!(c.to_string().len() < 400, "{c}");
    assert!(full.to_string().len() > 20 * c.to_string().len(), "the whole answer is what the footer no longer fetches");
    // NEGATIVE CONTROL: a seat with nothing open counts nothing and names no oldest ask
    let none = counts(&serde_json::to_value(asks_for(&s, "bobby-core", None, StateFilter::Open, NOW).unwrap()).unwrap());
    assert_eq!((none["waiting_on_me"].as_u64(), none["im_waiting_on"].as_u64(), none["im_waiting_on_answered"].as_u64()), (Some(0), Some(0), Some(0)));
    assert!(none["oldest_ms"].is_null(), "{none}");
}
