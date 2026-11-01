//! /tasks read through the store: signed messages in, derived rows out (docs/ASKS-AND-EVENTS.md, "Tasks").
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};
use oracle_server::tasks::tasks_in;

const NOW: i64 = 1_790_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn oracle(dir: &std::path::Path) -> Store {
    let s = Store::open(dir.join("oracle.db").to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    for (h, k) in [("rocky", 2), ("emmy", 3), ("core-ci", 4)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    }
    s
}

fn say(s: &Store, who: &str, key: u8, text: &str, tags: &[&str], mins: i64) -> String {
    let at = NOW + mins * MIN;
    let env = Envelope::sign(&sk(key), "oracle", who, Kind::Say, text, None, tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}

#[test]
fn a_room_reads_as_tasks_filtered_by_state_and_by_who_is_involved() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let review = say(&s, "rocky", 2, "review oracle-core#61", &["subject:oracle-core#61", "ask:review", "to:emmy", "intent:proposal", "ref:https://github.com/example/oracle/pull/61"], 0);
    say(&s, "rocky", 2, "evidence", &["subject:oracle-core#61", "rev:c46c51a", "intent:result"], 1);
    say(&s, "rocky", 2, "ci", &["subject:oracle-core#61", "rev:c46c51a", "ci:passed"], 2);
    say(&s, "core-ci", 4, "ci", &["subject:oracle-core#61", "rev:c46c51a", "ci:passed"], 3);
    say(&s, "rocky", 2, "an idea nobody was asked about", &["intent:proposal"], 4);
    say(&s, "rocky", 2, "chatter", &[], 5);

    let open = tasks_in(&s, "oracle", None, "open", None).unwrap();
    assert!(open.shadow && !open.capped);
    assert_eq!(open.scanned, 6);
    assert_eq!(open.tasks.len(), 1, "{:#?}", open.tasks);
    let t = &open.tasks[0];
    assert_eq!((t.subject.as_str(), t.summary.as_str()), ("oracle-core#61", "waiting on emmy (review)"));
    assert_eq!(t.obligations[0].id, review);
    assert_eq!(t.revisions[0].checks.len(), 1, "only core-ci's check counts");
    assert_eq!(t.revisions[0].claims.len(), 1);

    assert_eq!(tasks_in(&s, "oracle", None, "unrouted", None).unwrap().tasks[0].summary, "needs routing");
    assert_eq!(tasks_in(&s, "oracle", None, "all", None).unwrap().tasks.len(), 2);
    assert_eq!(tasks_in(&s, "oracle", Some("emmy"), "all", None).unwrap().tasks.len(), 1, "emmy holds the review");
    assert!(tasks_in(&s, "oracle", Some("blaze"), "all", None).unwrap().tasks.is_empty());

    say(&s, "emmy", 3, "approve", &["subject:oracle-core#61", "rev:c46c51a", "verdict:approve"], 6);
    assert!(tasks_in(&s, "oracle", None, "open", None).unwrap().tasks.is_empty());
    assert_eq!(tasks_in(&s, "oracle", None, "done", None).unwrap().tasks[0].obligations[0].state, "done");
}

#[test]
fn an_at_word_that_names_no_seat_addresses_nobody() {
    // The review of 2026-10-01: "(fyi, from the @mention)" in prose made a holder called "mention".
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    let ask = say(&s, "rocky", 2, "@emmy please act on the deploy; the @mention parser and @everyone are not seats", &["ask:act"], 0);
    let t = tasks_in(&s, "oracle", None, "all", None).unwrap();
    let holders: Vec<&str> = t.tasks.iter().flat_map(|x| x.obligations.iter()).map(|o| o.holder.as_str()).collect();
    assert_eq!(holders, ["emmy"], "{:#?}", t.tasks);
    let asks = oracle_server::asks::asks_for(&s, "emmy", None, oracle_server::asks::StateFilter::All, NOW + 10 * MIN).unwrap();
    let v = serde_json::to_value(&asks).unwrap();
    let row = v["waiting_on_me"].as_array().unwrap().iter().find(|r| r["id"] == ask.as_str()).expect("emmy holds it");
    assert_eq!(row["held_by"], serde_json::json!(["emmy"]), "{row}");
    let none = serde_json::to_value(oracle_server::asks::asks_for(&s, "mention", None, oracle_server::asks::StateFilter::All, NOW + 10 * MIN).unwrap()).unwrap();
    assert!(none["waiting_on_me"].as_array().unwrap().is_empty(), "no seat called mention: {none}");
}

#[test]
fn a_registry_read_failure_cannot_silently_drop_task_holders() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    say(&s, "rocky", 2, "@emmy please act", &["ask:act"], 0);
    s.with_conn(|c| { c.execute_batch("DROP TABLE keys")?; Ok(()) }).unwrap();
    assert!(tasks_in(&s, "oracle", None, "all", None).is_err());
    assert!(oracle_server::asks::asks_for(&s, "emmy", None, oracle_server::asks::StateFilter::All, NOW + 10 * MIN).is_err());
}

#[test]
fn a_fixed_cursor_answers_the_same_after_appends_and_the_default_cursor_is_now() {
    let dir = tempfile::tempdir().unwrap();
    let s = oracle(dir.path());
    say(&s, "rocky", 2, "review oracle-core#70", &["subject:oracle-core#70", "ask:review", "to:emmy", "intent:proposal", "ref:https://github.com/example/oracle/pull/70"], 0);
    let first = tasks_in(&s, "oracle", None, "all", None).unwrap();
    assert_eq!(first.tasks.len(), 1);
    assert!(first.complete && !first.capped && first.shadow);
    assert_eq!(first.as_of_seq, s.newest_seq().unwrap() + 1, "the default cursor is the newest seq + 1");
    let cursor = first.as_of_seq;
    // Appended after the cursor: evidence on the first task and a second task. Neither enters an answer at the old cursor.
    say(&s, "rocky", 2, "evidence", &["subject:oracle-core#70", "rev:abc1234", "intent:result"], 1);
    say(&s, "rocky", 2, "review oracle-core#71", &["subject:oracle-core#71", "ask:review", "to:emmy", "intent:proposal", "ref:https://github.com/example/oracle/pull/71"], 2);
    let again = tasks_in(&s, "oracle", None, "all", Some(cursor)).unwrap();
    assert_eq!(again.as_of_seq, cursor);
    assert_eq!(again.tasks.len(), 1);
    assert_eq!(again.tasks[0].current_rev, first.tasks[0].current_rev, "the old cursor sees no new evidence");
    assert_eq!(again.scanned, first.scanned);
    let now = tasks_in(&s, "oracle", None, "all", None).unwrap();
    assert_eq!(now.tasks.len(), 2);
    assert_eq!(now.tasks.iter().find(|t| t.subject == "oracle-core#70").unwrap().current_rev.as_deref(), Some("abc1234"));
    assert!(tasks_in(&s, "oracle", None, "all", Some(0)).is_err(), "a cursor below 1 is refused");
}
