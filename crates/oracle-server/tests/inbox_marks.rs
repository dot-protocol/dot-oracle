//! Inbox marks (Blaze, 2026-10-10): "whatever you get in your inbox once, you can mark it read, mark it acknowledged, archive
//! it, mark done … every time you pull inbox, you should not get the same ask." An open ask to a seat stays on its list on
//! every read until that seat marks it: seen keeps it asking, ack and stuck move it to their own lists, done or no takes it
//! off. Each mark is one seat's; the ask settles for its author when every seat it names marked done (or no).

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::asks::{asks_for, Asks, StateFilter};
use oracle_server::store::{Principal, Scope, Store};

const NOW: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

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
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 100 * HOUR).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 100 * HOUR), NOW - 100 * HOUR).unwrap();
    }
    s
}
fn say(s: &Store, who: &str, text: &str, tags: &[&str], ago: i64) -> String {
    let at = NOW - ago;
    let env = Envelope::sign(&sk(key_of(who)), "core", who, Kind::Say, text, None, tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn mark(s: &Store, who: &str, id: &str, m: &str, ago: i64) {
    say(s, who, m, &[&format!("reply:{id}"), &format!("react:{m}")], ago);
}
fn asks(s: &Store, seat: &str) -> Asks {
    asks_for(s, seat, None, StateFilter::All, NOW).unwrap()
}
fn ids(l: &[oracle_server::asks::AskRow]) -> Vec<String> {
    l.iter().map(|r| r.id.clone()).collect()
}

#[test]
fn an_ask_keeps_asking_each_seat_until_that_seat_marks_it() {
    let s = oracle();
    let q = say(&s, "blaze", "ship the inbox and tell me when", &["to:rocky", "to:dinesh", "ask:act"], 10 * HOUR);
    assert_eq!(ids(&asks(&s, "rocky").waiting_on_me), vec![q.clone()]);
    assert_eq!(ids(&asks(&s, "dinesh").waiting_on_me), vec![q.clone()]);

    // seen is not an answer: it still asks, on every read.
    mark(&s, "rocky", &q, "seen", 9 * HOUR);
    assert_eq!(ids(&asks(&s, "rocky").waiting_on_me), vec![q.clone()], "seen keeps asking");

    // ack moves it to rocky's acked list; dinesh is still asked (a mark is one seat's).
    mark(&s, "rocky", &q, "ack", 8 * HOUR);
    let r = asks(&s, "rocky");
    assert!(r.waiting_on_me.is_empty(), "acked: it stops asking rocky");
    assert_eq!(ids(&r.acked), vec![q.clone()]);
    assert_eq!(ids(&asks(&s, "dinesh").waiting_on_me), vec![q.clone()], "negative control: rocky's ack is not dinesh's");

    // stuck is listed apart.
    mark(&s, "dinesh", &q, "stuck", 7 * HOUR);
    let d = asks(&s, "dinesh");
    assert!(d.waiting_on_me.is_empty());
    assert_eq!(ids(&d.stuck), vec![q.clone()]);

    // rocky done: off rocky's lists; blaze still waits, and sees what each did.
    mark(&s, "rocky", &q, "done", 6 * HOUR);
    let r = asks(&s, "rocky");
    assert!(r.waiting_on_me.is_empty() && r.acked.is_empty() && r.stuck.is_empty());
    let b = asks(&s, "blaze");
    assert_eq!(ids(&b.im_waiting_on), vec![q.clone()], "one of two named seats done: still open for its author");
    let row = &b.im_waiting_on[0];
    assert_eq!((row.marks.get("rocky").copied(), row.marks.get("dinesh").copied()), (Some("done"), Some("stuck")));

    // every named seat done: settled for its author too.
    mark(&s, "dinesh", &q, "done", 5 * HOUR);
    assert!(asks(&s, "blaze").im_waiting_on.is_empty(), "both named seats marked done: settled");
    assert!(asks(&s, "dinesh").stuck.is_empty());
}

#[test]
fn a_mark_is_not_news() {
    let s = oracle();
    let q = say(&s, "blaze", "which seats are live?", &["to:rocky", "ask:reply"], 2 * HOUR);
    mark(&s, "dinesh", &q, "seen", HOUR);
    let plain = say(&s, "dinesh", "a real message", &[], HOUR / 2);
    let f = s.fold("rocky", 50, NOW).unwrap();
    let got: Vec<String> = f.items.iter().map(|i| i.obs.id.clone()).collect();
    assert!(got.contains(&plain), "negative control: speech is news: {got:?}");
    assert!(!got.iter().any(|id| s.get(id).unwrap().content == "seen"), "a seen mark is not a new message: {got:?}");
}

#[test]
fn a_declared_ask_is_never_ambient_and_never_ages_out() {
    let s = oracle();
    // Noor's rounds: a result that asks for a reply (Round 46, 2026-10-10, reached no list).
    let r = say(&s, "dinesh", "round 46: challenge the spec before anyone runs it", &["to:rocky", "intent:result", "ask:reply"], 2 * HOUR);
    // A question asked four days ago, past the old 72-hour fuse, never answered.
    let q = say(&s, "dinesh", "which seats are live?", &["to:rocky", "intent:question", "ask:reply"], 96 * HOUR);
    // Negative controls: a result that asks nothing stays ambient; an author's own ttl still ends an ask.
    let plain = say(&s, "dinesh", "round 47: the table has its shape", &["to:rocky", "intent:result"], HOUR);
    let fused = say(&s, "dinesh", "answer within a day or not at all", &["to:rocky", "intent:question", "ask:reply", "ttl:24"], 48 * HOUR);
    let w = ids(&asks(&s, "rocky").waiting_on_me);
    assert!(w.contains(&r), "a result that asks is an ask: {w:?}");
    assert!(w.contains(&q), "an open ask does not age out: {w:?}");
    assert!(!w.contains(&plain) && !w.contains(&fused), "{w:?}");
}

#[test]
fn an_authors_own_follow_up_does_not_answer_its_ask() {
    let s = oracle();
    // Round 46 asks rocky; Round 47, by the same author, replies to it.
    let q = say(&s, "dinesh", "round 46: challenge the spec", &["to:rocky", "intent:result", "ask:reply"], 3 * HOUR);
    say(&s, "dinesh", "round 47: the parts, defined", &[&format!("reply:{q}"), "intent:result"], 2 * HOUR);
    let r = asks(&s, "rocky");
    let row = r.waiting_on_me.iter().find(|x| x.id == q).expect("still waiting on rocky");
    assert_eq!((row.state, row.answered_by.as_deref()), ("open", None), "the author's follow-up is not an answer");
    // Negative control: rocky's reply does answer it.
    say(&s, "rocky", "the spec needs a basis", &[&format!("reply:{q}")], HOUR);
    assert!(asks(&s, "rocky").waiting_on_me.iter().all(|x| x.id != q), "answered by the seat asked");
}

#[test]
fn an_act_ask_to_two_seats_stays_open_for_the_one_that_has_not_delivered() {
    let s = oracle();
    let q = say(&s, "blaze", "both of you: post your read of the thread", &["to:rocky", "to:dinesh", "ask:act"], 3 * HOUR);
    say(&s, "dinesh", "my read: done", &[&format!("reply:{q}"), "intent:result"], 2 * HOUR);
    assert!(ids(&asks(&s, "rocky").waiting_on_me).contains(&q), "dinesh delivering does not deliver rocky's part");
    assert!(!ids(&asks(&s, "dinesh").waiting_on_me).contains(&q), "negative control: dinesh delivered");
    // a plain reply from rocky is not a delivery on an act ask
    say(&s, "rocky", "on it", &[&format!("reply:{q}")], 90 * 60_000);
    assert!(ids(&asks(&s, "rocky").waiting_on_me).contains(&q), "a note is not a delivery");
    say(&s, "rocky", "my read: posted", &[&format!("reply:{q}"), "intent:result"], HOUR);
    assert!(!ids(&asks(&s, "rocky").waiting_on_me).contains(&q), "rocky delivered");
}

#[test]
fn later_takes_an_ask_off_a_list_until_its_time_then_it_asks_again() {
    // Blaze, 23 Oct: "give me a option to defer things". later is one seat's mark with a time (until:<ms>).
    let s = oracle();
    let q = say(&s, "rocky", "decide: ship it now or hold?", &["to:blaze", "ask:act"], 10 * HOUR);
    let r = say(&s, "rocky", "decide: which room first?", &["to:blaze", "ask:act"], 9 * HOUR);
    let later = |id: &str, until: i64, ago: i64| say(&s, "blaze", "later", &[&format!("reply:{id}"), "react:later", &format!("until:{until}")], ago);

    // until in the future: off waiting, on later; the other ask still waits.
    later(&q, NOW + 2 * HOUR, 8 * HOUR);
    let b = asks(&s, "blaze");
    assert_eq!(ids(&b.later), vec![q.clone()]);
    assert_eq!(ids(&b.waiting_on_me), vec![r.clone()], "negative control: only the deferred ask leaves");
    assert_eq!(b.later[0].later_until.get("blaze").copied(), Some(NOW + 2 * HOUR));
    assert!(asks(&s, "rocky").im_waiting_on.iter().any(|x| x.id == q), "the author still waits: later is not an answer");

    // its time passed: back on waiting by itself, no new record needed.
    later(&r, NOW - HOUR, 7 * HOUR);
    let b = asks(&s, "blaze");
    assert!(ids(&b.waiting_on_me).contains(&r), "a later whose time passed asks again");
    assert!(!ids(&b.later).contains(&r));

    // a later without a time defers nothing.
    let t = say(&s, "rocky", "decide: tabs or rail?", &["to:blaze", "ask:act"], 6 * HOUR);
    say(&s, "blaze", "later", &[&format!("reply:{t}"), "react:later"], 5 * HOUR);
    assert!(ids(&asks(&s, "blaze").waiting_on_me).contains(&t), "a later with no until is ignored");

    // a newer mark replaces the later: ack moves it to acked.
    mark(&s, "blaze", &q, "ack", 4 * HOUR);
    let b = asks(&s, "blaze");
    assert!(!ids(&b.later).contains(&q) && ids(&b.acked).contains(&q));
}

#[test]
fn a_seat_with_more_asks_than_the_cap_still_lists_its_newest() {
    // Emmy's repro: with 200+ open asks, the cut kept the oldest and `ox close`
    // could not find a seat's own ask of minutes ago. The newest are kept; an intent:result ask:reply is one of them.
    let s = oracle();
    let n = oracle_server::asks::LIST_CAP + 2;
    for i in 0..n {
        say(&s, "rocky", &format!("question {i}?"), &["to:blaze", "ask:act"], 50 * HOUR - i as i64 * 1000);
    }
    let newest = say(&s, "rocky", "result, and does this hold?", &["to:blaze", "ask:reply", "intent:result"], HOUR);
    let r = asks(&s, "rocky");
    assert_eq!(r.im_waiting_on.len(), oracle_server::asks::LIST_CAP);
    assert!(r.totals.im_waiting_on > oracle_server::asks::LIST_CAP, "the total still says how many there are");
    assert!(ids(&r.im_waiting_on).contains(&newest), "the author's newest ask is listed");
    assert!(ids(&asks(&s, "blaze").waiting_on_me).contains(&newest), "and the asked seat's");
    let oldest_kept = r.im_waiting_on.iter().map(|x| x.created_ms).min().unwrap();
    assert!(oldest_kept > NOW - 50 * HOUR, "negative control: the oldest are the ones cut");
}
