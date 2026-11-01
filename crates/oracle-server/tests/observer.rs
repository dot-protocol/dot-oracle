//! The thread observer's two changes to the fold (Blaze, Rocky, 2026-09-30): the observer seat's model annotation may say whom a message is for (any other
//! model's `set:to=` still addresses nobody), and `set:done=` (settled, stale, superseded, acked) is what became of a thread, in the author, then person, then
//! model order `ask` has. `answered` stays derived from real replies; a real link or a decider's mark still decides first.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::asks::{asks_for, StateFilter};
use oracle_server::projection::OBSERVER_SEATS;
use oracle_server::roomread::{threads, PageQ};
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
        "dinesh" => 3,
        "kepler" => 4,
        _ => 5,
    }
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 100 * HOUR).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3), ("kepler", 4), ("emmy", 5)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 100 * HOUR), NOW - 100 * HOUR).unwrap();
    }
    s
}
fn put(s: &Store, who: &str, kind: Kind, text: &str, tags: &[&str], ago: i64) -> String {
    let at = NOW - ago;
    let env = Envelope::sign(&sk(key_of(who)), "core", who, kind, text, None, tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn say(s: &Store, who: &str, text: &str, tags: &[&str], ago: i64) -> String {
    put(s, who, Kind::Say, text, tags, ago)
}
/// A signed annotation: `who` says `sets` about `id`. `model` adds `set:by=model`.
fn note(s: &Store, who: &str, id: &str, sets: &[&str], model: bool, ago: i64) {
    let mut tags = vec![format!("annotates:{id}")];
    tags.extend(sets.iter().map(|x| format!("set:{x}")));
    if model {
        tags.push("set:by=model".into());
    }
    let t: Vec<&str> = tags.iter().map(String::as_str).collect();
    put(s, who, Kind::Annotate, "why", &t, ago);
}
fn state(s: &Store, id: &str) -> String {
    let n = s.get(id).unwrap().seq;
    s.with_conn(|c| Ok(c.query_row("SELECT state FROM thread WHERE root_seq=?1", [n], |r| r.get::<_, String>(0)).unwrap())).unwrap()
}
fn waiting(s: &Store, seat: &str) -> Vec<String> {
    asks_for(s, seat, None, StateFilter::All, NOW).unwrap().waiting_on_me.into_iter().map(|r| r.id).collect()
}

#[test]
fn the_observer_seats_model_annotation_addresses_a_message_and_no_other_models_does() {
    assert_eq!(OBSERVER_SEATS, ["dinesh"], "the observer seat is code: every reader and every reindex agrees");
    let s = oracle();
    let ask = say(&s, "blaze", "someone please check the ledger", &["ask:act"], 5 * HOUR);
    // another model says it is for rocky: nobody is addressed
    note(&s, "kepler", &ask, &["to=rocky"], true, 4 * HOUR);
    assert!(waiting(&s, "rocky").is_empty(), "any other model's set:to addresses nobody");
    // the observer seat's model annotation does
    note(&s, "dinesh", &ask, &["to=emmy"], true, 3 * HOUR);
    assert_eq!(waiting(&s, "emmy"), vec![ask.clone()], "the observer seat's set:to addresses emmy");
    assert!(waiting(&s, "rocky").is_empty(), "and only who it names");
    // a person's annotation still outranks the observer's
    note(&s, "kepler", &ask, &["to=rocky"], false, 2 * HOUR);
    assert_eq!(waiting(&s, "rocky"), vec![ask.clone()]);
    assert!(waiting(&s, "emmy").is_empty(), "a person beats a model, the observer included");
    // and the author's own to: outranks every annotation
    let own = say(&s, "blaze", "for kepler only", &["ask:act", "to:kepler"], 5 * HOUR);
    note(&s, "dinesh", &own, &["to=emmy"], true, HOUR);
    assert_eq!(waiting(&s, "kepler"), vec![own.clone()]);
    assert!(!waiting(&s, "emmy").contains(&own));
}

#[test]
fn a_verdict_of_done_becomes_the_threads_state_and_answered_stays_derived() {
    let s = oracle();
    let mk = |t: &str| say(&s, "rocky", t, &["intent:question", "ask:reply", "to:emmy"], 9 * HOUR);
    let (stale, settled, superseded, plain) = (mk("q stale"), mk("q settled"), mk("q superseded"), mk("q plain"));
    let status = say(&s, "rocky", "shipped it, blaze should see", &["intent:status"], 9 * HOUR);
    note(&s, "dinesh", &stale, &["done=stale"], true, 8 * HOUR);
    note(&s, "dinesh", &settled, &["done=settled"], true, 8 * HOUR);
    note(&s, "dinesh", &superseded, &["done=superseded"], true, 8 * HOUR);
    note(&s, "dinesh", &status, &["done=acked"], true, 8 * HOUR);
    assert_eq!((state(&s, &stale), state(&s, &settled), state(&s, &superseded), state(&s, &status), state(&s, &plain)), ("stale".into(), "settled".into(), "superseded".into(), "acked".into(), "open".into()));
    // the words that are not verdicts change nothing: answered is derived from replies, open is what it was, an unknown word is nothing
    for w in ["answered", "open", "finished"] {
        note(&s, "dinesh", &plain, &[&format!("done={w}")], true, 7 * HOUR);
        assert_eq!(state(&s, &plain), "open", "done={w}");
    }
    say(&s, "emmy", "here is the answer", &[&format!("re:{plain}")], 6 * HOUR);
    assert_eq!(state(&s, &plain), "answered", "a real reply answers it");
    note(&s, "dinesh", &plain, &["done=stale"], true, 5 * HOUR);
    assert_eq!(state(&s, &plain), "stale", "and a verdict of stale then says what became of it");
    // the thread routes take the new states
    let v = threads(&s, "core", &PageQ { state: vec!["stale".into(), "acked".into()], ..Default::default() }).unwrap();
    let mut got: Vec<&str> = v["threads"].as_array().unwrap().iter().map(|t| t["state"].as_str().unwrap()).collect();
    got.sort();
    assert_eq!(got, ["acked", "stale", "stale"]);
}

#[test]
fn the_author_then_a_person_then_a_model_decide_what_became_of_it() {
    let s = oracle();
    let m = say(&s, "rocky", "ship the release?", &["intent:question", "ask:reply", "to:blaze"], 9 * HOUR);
    note(&s, "dinesh", &m, &["done=stale"], true, 8 * HOUR);
    assert_eq!(state(&s, &m), "stale");
    note(&s, "kepler", &m, &["done=settled"], false, 7 * HOUR);
    assert_eq!(state(&s, &m), "settled", "a person beats a model");
    note(&s, "dinesh", &m, &["done=superseded"], true, 6 * HOUR);
    assert_eq!(state(&s, &m), "settled", "a newer model does not undo a person");
    // the author's own tag beats every annotation, and the author's own later annotation beats the tag
    let own = say(&s, "rocky", "done and dusted", &["intent:status", "done:settled"], 9 * HOUR);
    note(&s, "kepler", &own, &["done=stale"], false, 8 * HOUR);
    assert_eq!(state(&s, &own), "settled", "the author's own tag stands against another person");
    note(&s, "rocky", &own, &["done=stale"], false, 7 * HOUR);
    assert_eq!(state(&s, &own), "stale", "the author may change their own mind");
}

#[test]
fn a_real_link_or_a_deciders_mark_still_decides_first() {
    let s = oracle();
    put(&s, "blaze", Kind::Say, "[Core members] roots", &[], 90 * HOUR); // no record: nobody but a named seat decides
    let closed = say(&s, "rocky", "a question", &["intent:question", "ask:reply", "to:emmy"], 9 * HOUR);
    say(&s, "emmy", "closing this", &[&format!("closes:{closed}")], 8 * HOUR);
    note(&s, "dinesh", &closed, &["done=stale"], true, 7 * HOUR);
    assert_eq!(state(&s, &closed), "closed", "an explicit close is not overruled by a verdict");
    let replaced = say(&s, "rocky", "plan A", &["intent:proposal"], 9 * HOUR);
    say(&s, "rocky", "plan B", &["intent:proposal", &format!("supersedes:{replaced}")], 8 * HOUR);
    note(&s, "dinesh", &replaced, &["done=stale"], true, 7 * HOUR);
    assert_eq!(state(&s, &replaced), "superseded");
    let decided = say(&s, "rocky", "ship friday?", &["intent:proposal", "to:blaze"], 9 * HOUR);
    say(&s, "blaze", "✓", &[&format!("re:{decided}"), "react:go"], 8 * HOUR);
    note(&s, "dinesh", &decided, &["done=stale"], true, 7 * HOUR);
    assert_eq!(state(&s, &decided), "settled", "a named decider's go is not overruled by a model");
}

#[test]
fn an_ask_that_is_stale_or_superseded_waits_on_nobody_and_a_settled_or_acked_one_is_done() {
    let s = oracle();
    let mk = |t: &str| say(&s, "rocky", t, &["ask:act", "to:emmy"], 9 * HOUR);
    let (stale, sup, settled, acked, open) = (mk("a"), mk("b"), mk("c"), mk("d"), mk("e"));
    note(&s, "dinesh", &stale, &["done=stale"], true, 8 * HOUR);
    note(&s, "dinesh", &sup, &["done=superseded"], true, 8 * HOUR);
    note(&s, "dinesh", &settled, &["done=settled"], true, 2 * HOUR);
    note(&s, "dinesh", &acked, &["done=acked"], true, HOUR);
    assert_eq!(waiting(&s, "emmy"), vec![open.clone()], "only the one nobody has said anything about is waiting");
    let a = asks_for(&s, "emmy", None, StateFilter::All, NOW).unwrap();
    let done: Vec<(&str, &str)> = a.done_today.iter().map(|r| (r.id.as_str(), r.state)).collect();
    assert!(done.contains(&(settled.as_str(), "done")) && done.contains(&(acked.as_str(), "done")), "{done:?}");
    assert!(!a.done_today.iter().any(|r| r.id == stale || r.id == sup), "stale and superseded are gone, not done");
}

#[test]
fn reindex_leaves_exactly_what_the_annotations_left() {
    let s = oracle();
    let m = say(&s, "rocky", "q", &["intent:question", "ask:reply", "to:emmy"], 9 * HOUR);
    let st = say(&s, "rocky", "status", &["intent:status"], 9 * HOUR);
    note(&s, "dinesh", &m, &["done=stale", "to=emmy"], true, 8 * HOUR);
    note(&s, "dinesh", &st, &["done=acked"], true, 8 * HOUR);
    let before = s.index_dump().unwrap();
    s.reindex().unwrap();
    assert_eq!(s.index_dump().unwrap(), before);
    assert_eq!((state(&s, &m), state(&s, &st)), ("stale".to_string(), "acked".to_string()));
}

#[test]
fn the_observer_seat_outranks_every_other_model_whatever_the_age_and_a_person_outranks_it() {
    let s = oracle();
    let m = say(&s, "rocky", "ship the release?", &["ask:none"], 9 * HOUR);
    let bare = say(&s, "rocky", "fyi, the old way", &[], 9 * HOUR);
    let ask_of = |id: &str| -> String {
        let n = s.get(id).unwrap().seq;
        s.with_conn(|c| Ok(c.query_row("SELECT ask FROM msg WHERE seq=?1", [n], |r| r.get::<_, String>(0)).unwrap())).unwrap()
    };
    // the observer says act; a newer classifier says read: the observer's verdict stands
    note(&s, "dinesh", &bare, &["ask=act", "done=stale"], true, 8 * HOUR);
    note(&s, "kepler", &bare, &["ask=read", "done=settled"], true, HOUR);
    assert_eq!(ask_of(&bare), "act", "the newest model does not overwrite the observer seat's verdict");
    assert_eq!(state(&s, &bare), "stale");
    // and the other way round: an older model's verdict does not beat a newer observer one
    note(&s, "kepler", &bare, &["ask=review"], true, 7 * HOUR);
    assert_eq!(ask_of(&bare), "act");
    // a person still outranks the observer seat
    note(&s, "rocky", &bare, &["ask=reply", "done=settled"], false, 30 * 60_000);
    assert_eq!(ask_of(&bare), "reply");
    assert_eq!(state(&s, &bare), "settled");
    let _ = m;
}

#[test]
fn a_done_of_none_is_recorded_and_changes_no_state_and_closes_no_ask() {
    let s = oracle();
    let notice = say(&s, "rocky", "deployed build 1099", &[], 5 * HOUR);
    let before = state(&s, &notice);
    note(&s, "dinesh", &notice, &["ask=none", "done=none"], true, 4 * HOUR);
    assert_eq!(state(&s, &notice), before, "none is not a state: the thread keeps the one its replies and marks give it");
    let ask = say(&s, "rocky", "review PR 12", &["ask:act", "to:dinesh"], 3 * HOUR);
    note(&s, "dinesh", &ask, &["done=none"], true, 2 * HOUR);
    assert_eq!(asks_for(&s, "dinesh", None, StateFilter::All, NOW).unwrap().waiting_on_me.len(), 1, "and it does not close a live ask");
    s.reindex().unwrap();
    assert_eq!(state(&s, &notice), before);
}
