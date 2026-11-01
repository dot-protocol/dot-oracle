//! The server refuses a stale or forked Room State revision, and a false adoption, in the transaction that would store it (Emmy, 2026-10-03: the client's
//! `head` is a preflight, not a compare-and-swap). A `room-context:v1` say whose cargo is `room-context/1` of its own room must name exactly the room's
//! current head set as `parents`; an `adopts` it carries must be a proposal of that room that a decider accepted, by the settling mark that is the thread's
//! settlement right now. Anything else tagged room-context:v1 is stored as before (the reader reports it as bad); this guard never rewrites history.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store, StoreError};
use serde_json::{json, Value};

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;
const ROOM: &str = "core";

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 5000 * MIN).unwrap();
    for (n, who) in [(2u8, "rocky"), (3, "kepler")] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, who, &pubkey_hex(&sk(n)), NOW - 5000 * MIN), NOW - 5000 * MIN).unwrap();
    }
    // The room's deciders: its roots (without this message nobody decides a proposal there).
    for (room, face) in [(ROOM, "[Core members] roots"), ("other", "[Other members] roots")] {
        let roots = envelope("blaze", room, face, Some(json!({"schema": "room-members/1", "roots": ["blaze"]}).to_string()), &[], -10);
        put(&s, &roots, -10).unwrap();
    }
    s
}
fn key_of(who: &str) -> u8 {
    match who {
        "blaze" => 1,
        "rocky" => 2,
        _ => 3,
    }
}
fn envelope(who: &str, room: &str, text: &str, cargo: Option<String>, tags: &[&str], mins: i64) -> Envelope {
    Envelope::sign(&sk(key_of(who)), room, who, Kind::Say, text, cargo, tags.iter().map(|t| t.to_string()).collect(), NOW + mins * MIN)
}
fn put(s: &Store, e: &Envelope, mins: i64) -> Result<String, StoreError> {
    let who = Principal { handle: e.author.clone(), scope: Scope::Write };
    s.write(&who, e, NOW + mins * MIN).map(|r| r.id)
}
fn say(s: &Store, who: &str, room: &str, text: &str, tags: &[&str], mins: i64) -> String {
    put(s, &envelope(who, room, text, None, tags, mins), mins).unwrap()
}
fn cargo(parents: &[&str], adopts: Option<Value>) -> String {
    let mut c = json!({"schema": "room-context/1", "channel": ROOM, "parents": parents, "fields": {"goal": "g"}});
    if let Some(a) = adopts {
        c["adopts"] = a;
    }
    c.to_string()
}
fn revision(s: &Store, parents: &[&str], adopts: Option<Value>, mins: i64) -> Result<String, StoreError> {
    put(s, &envelope("rocky", ROOM, "State updated", Some(cargo(parents, adopts)), &["room-context:v1"], mins), mins)
}
/// Today's fold: who settled this proposal (the id of the mark), if anyone.
fn settled_by_now(s: &Store, id: &str) -> Option<String> {
    let root = seq(s, id);
    s.with_conn(|c| {
        let by: Option<i64> = c.query_row("SELECT settled_by FROM thread WHERE root_seq=?1", rusqlite::params![root], |r| r.get(0)).unwrap();
        Ok(by.map(|b| c.query_row("SELECT id FROM observations WHERE seq=?1", rusqlite::params![b], |r| r.get::<_, String>(0)).unwrap()))
    })
    .unwrap()
}
fn seq(s: &Store, id: &str) -> i64 {
    s.get(id).unwrap().seq
}
fn conflict(r: Result<String, StoreError>) -> String {
    match r {
        Err(StoreError::Conflict(m)) => m,
        other => panic!("expected a conflict, got {other:?}"),
    }
}
fn rejected(r: Result<String, StoreError>) -> String {
    match r {
        Err(StoreError::Rejected(m)) => m,
        other => panic!("expected a rejection, got {other:?}"),
    }
}
/// The adopts record the client would build for `proposal` settled by `mark`.
fn adopts(s: &Store, proposal: &str, mark: &str, by: &str) -> Value {
    json!({"room": ROOM, "proposal": proposal, "word": "accepted", "mark": mark, "cursor": seq(s, mark), "decided_by": by, "basis": "membership"})
}
fn adopts_addressed(s: &Store, proposal: &str, mark: &str, by: &str) -> Value {
    let mut a = adopts(s, proposal, mark, by);
    a["basis"] = json!("addressed");
    a
}

#[test]
fn a_revision_must_name_exactly_the_current_heads() {
    let s = oracle();
    let r1 = revision(&s, &[], None, 1).unwrap();
    // The room already has a head: a second root is stale.
    assert!(conflict(revision(&s, &[], None, 2)).contains(&r1));
    let r2 = revision(&s, &[&r1], None, 3).unwrap();
    // r1 is no longer a head.
    assert!(conflict(revision(&s, &[&r1], None, 4)).contains(&r2));
    // A parent nobody stored is not a head either.
    conflict(revision(&s, &["OBS-core-20000101-000000000000"], None, 5));
    // Naming a head and a stale revision is not the head set.
    conflict(revision(&s, &[&r1, &r2], None, 6));
    revision(&s, &[&r2], None, 7).unwrap();
}

#[test]
fn two_writers_from_one_head_cannot_both_land() {
    let s = oracle();
    let r1 = revision(&s, &[], None, 1).unwrap();
    // Both read head r1; the first wins, the second is refused rather than forking the State.
    let a = envelope("rocky", ROOM, "A", Some(cargo(&[&r1], None)), &["room-context:v1"], 2);
    let b = envelope("rocky", ROOM, "B", Some(cargo(&[&r1], None)), &["room-context:v1"], 2);
    let id_a = put(&s, &a, 2).unwrap();
    assert!(conflict(put(&s, &b, 2)).contains(&id_a));
    // The loser rebases onto the new head and lands.
    revision(&s, &[&id_a], None, 3).unwrap();
}

#[test]
fn a_delayed_correction_is_refused_and_a_retry_of_a_landed_write_is_not() {
    let s = oracle();
    let r1 = revision(&s, &[], None, 1).unwrap();
    let late = envelope("rocky", ROOM, "late", Some(cargo(&[&r1], None)), &["room-context:v1"], 2);
    let landed = envelope("rocky", ROOM, "landed", Some(cargo(&[&r1], None)), &["room-context:v1"], 3);
    let id = put(&s, &landed, 3).unwrap();
    // The delayed writer arrives after: stale.
    conflict(put(&s, &late, 4));
    // The delivered write retried (a queue replays it) is a success, not a conflict: idempotency answers first.
    let again = s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &landed, NOW + 5 * MIN).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.id, id);
}

#[test]
fn an_archived_revision_is_not_a_head_and_other_rooms_do_not_count() {
    let s = oracle();
    let r1 = revision(&s, &[], None, 1).unwrap();
    let r2 = revision(&s, &[&r1], None, 2).unwrap();
    // Another room's State is its own.
    let elsewhere = cargo(&[], None).replace("\"core\"", "\"other\"");
    put(&s, &envelope("rocky", "other", "State", Some(elsewhere), &["room-context:v1"], 3), 3).unwrap();
    // r2 archived: the reader then sees r1 as the head again, and so does the guard.
    let tag = format!("archives:{r2}");
    say(&s, "rocky", ROOM, "archive r2", &[&tag], 4);
    conflict(revision(&s, &[&r2], None, 5));
    revision(&s, &[&r1], None, 6).unwrap();
}

#[test]
fn other_tagged_lines_are_stored_as_before() {
    let s = oracle();
    let r1 = revision(&s, &[], None, 1).unwrap();
    // No cargo, another schema, another room: the reader reports them; the guard does not take them over.
    say(&s, "rocky", ROOM, "State updated (no cargo)", &["room-context:v1"], 2);
    let other = json!({"schema": "room-context/1", "channel": "elsewhere"}).to_string();
    put(&s, &envelope("rocky", ROOM, "bad", Some(other), &["room-context:v1"], 3), 3).unwrap();
    // Untagged cargo of the same shape is not a revision.
    put(&s, &envelope("rocky", ROOM, "plain", Some(cargo(&[], None)), &[], 4), 4).unwrap();
    // And the head is still r1.
    revision(&s, &[&r1], None, 5).unwrap();
}

#[test]
fn adopting_needs_a_proposal_a_decider_accepted_and_the_settling_mark_named() {
    let s = oracle();
    let r1 = revision(&s, &[], None, 1).unwrap();
    let p = say(&s, "rocky", ROOM, "ship friday", &["intent:proposal"], 2);
    // Unsettled: nothing to adopt.
    let fake = json!({"room": ROOM, "proposal": p, "word": "accepted", "mark": p, "cursor": seq(&s, &p), "decided_by": "blaze"});
    rejected(revision(&s, &[&r1], Some(fake), 3));
    // A go from a seat that does not decide is an opinion.
    let opinion = say(&s, "kepler", ROOM, "✓", &[&format!("re:{p}"), "react:go"], 4);
    rejected(revision(&s, &[&r1], Some(adopts(&s, &p, &opinion, "kepler")), 5));
    // The root decides.
    let go = say(&s, "blaze", ROOM, "✓", &[&format!("re:{p}"), "react:go"], 6);
    // Wrong settling event, wrong cursor, wrong decider, wrong room, a message that is no proposal: each refused.
    rejected(revision(&s, &[&r1], Some(adopts(&s, &p, &opinion, "kepler")), 7));
    let mut bad = adopts(&s, &p, &go, "blaze");
    bad["cursor"] = json!(seq(&s, &go) + 1);
    rejected(revision(&s, &[&r1], Some(bad), 8));
    rejected(revision(&s, &[&r1], Some(adopts(&s, &p, &go, "rocky")), 9));
    let mut bad = adopts(&s, &p, &go, "blaze");
    bad["room"] = json!("other");
    rejected(revision(&s, &[&r1], Some(bad), 10));
    let plain = say(&s, "rocky", ROOM, "just talking", &[], 11);
    rejected(revision(&s, &[&r1], Some(adopts(&s, &plain, &go, "blaze")), 12));
    rejected(revision(&s, &[&r1], Some(json!("accepted")), 13));
    // The real settlement lands, and is on the stored revision as signed.
    let ok = revision(&s, &[&r1], Some(adopts(&s, &p, &go, "blaze")), 14).unwrap();
    let stored = s.get(&ok).unwrap();
    let c: Value = serde_json::from_str(stored.cargo.as_deref().unwrap()).unwrap();
    assert_eq!(c["adopts"]["mark"], json!(go));
    // A revision without adopts stays valid (it is simply not claimed as adopted).
    revision(&s, &[&ok], None, 15).unwrap();
}

#[test]
fn a_declined_or_other_rooms_proposal_cannot_be_adopted() {
    let s = oracle();
    let r1 = revision(&s, &[], None, 1).unwrap();
    let p = say(&s, "rocky", ROOM, "ship friday", &["intent:proposal"], 2);
    let no = say(&s, "blaze", ROOM, "✕", &[&format!("re:{p}"), "react:no"], 3);
    let mut a = adopts(&s, &p, &no, "blaze");
    a["word"] = json!("accepted");
    rejected(revision(&s, &[&r1], Some(a), 4));
    // A proposal of another room, accepted there, is not this room's.
    let q = say(&s, "rocky", "other", "ship", &["intent:proposal"], 5);
    let go = say(&s, "blaze", "other", "✓", &[&format!("re:{q}"), "react:go"], 6);
    rejected(revision(&s, &[&r1], Some(adopts(&s, &q, &go, "blaze")), 7));
}

/// Racing writers on a real file database (WAL, a pool of connections): eight seats read the same head and write at once. Exactly one lands; every other
/// writer gets an explicit 409 naming the head to rebase on (the write lock is taken first, so none is turned away with a lock error) and none forks the State.
#[test]
fn racing_writers_from_one_head_never_fork_the_state() {
    let path = std::env::temp_dir().join(format!("oracle-guard-race-{}.db", std::process::id()));
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
    let s = std::sync::Arc::new(Store::open(path.to_str().unwrap()).unwrap());
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 5000 * MIN).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 5000 * MIN), NOW - 5000 * MIN).unwrap();
    for round in 0..5 {
        let head = {
            // the current head: the newest revision (each round builds on the previous winner)
            let rows = s.recent(Some(ROOM), 50, None).unwrap();
            rows.iter().find(|o| o.tags.iter().any(|t| t == "room-context:v1")).map(|o| o.id.clone())
        };
        let parents: Vec<String> = head.into_iter().collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let (s, barrier, parents) = (s.clone(), barrier.clone(), parents.clone());
                std::thread::spawn(move || {
                    let refs: Vec<&str> = parents.iter().map(String::as_str).collect();
                    let mins = 10 * round + 1;
                    let e = envelope("rocky", ROOM, &format!("writer {i}"), Some(cargo(&refs, None)), &["room-context:v1"], mins);
                    barrier.wait();
                    put(&s, &e, mins)
                })
            })
            .collect();
        let results: Vec<Result<String, StoreError>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let landed = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(landed, 1, "round {round}: exactly one racer lands, got {results:?}");
        for r in results.iter().filter(|r| r.is_err()) {
            // an explicit conflict that names the head the loser must rebase on; never a lock error, never a fork
            let Err(StoreError::Conflict(m)) = r else { panic!("round {round}: a loser must get an explicit conflict, got {r:?}") };
            assert!(m.contains("stale State") && m.contains("OBS-core-"), "{m}");
        }
        // one head: the revisions of the room that no other names
        let all = s.recent(Some(ROOM), 500, None).unwrap();
        let revs: Vec<(String, Vec<String>)> = all
            .iter()
            .filter(|o| o.tags.iter().any(|t| t == "room-context:v1"))
            .map(|o| (o.id.clone(), serde_json::from_str::<Value>(o.cargo.as_deref().unwrap()).unwrap()["parents"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect()))
            .collect();
        let named: std::collections::HashSet<&String> = revs.iter().flat_map(|r| r.1.iter()).collect();
        assert_eq!(revs.iter().filter(|r| !named.contains(&r.0)).count(), 1, "round {round}: the State has one head");
        assert_eq!(revs.len(), round as usize + 1);
    }
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
}

/// Emmy, 2026-10-03 (cd30dce51279, c2ef12176517): authority is judged at the mark, from facts that cannot change afterwards. A grant made AFTER the mark must not
/// authorize it; a proposal's own `to:` is immutable; and the record names which basis it claims.
#[test]
fn the_deciding_authority_is_the_one_that_held_at_the_mark() {
    let s = oracle();
    let r1 = revision(&s, &[], None, 1).unwrap();
    // ADDRESSED: the proposal names its decider in its own signed tags; that seat's go settles it, whatever the room's roots say.
    let p = say(&s, "blaze", ROOM, "kepler, may I ship?", &["intent:proposal", "to:kepler"], 2);
    let go = say(&s, "kepler", ROOM, "✓", &[&format!("re:{p}"), "react:go"], 3);
    // The claimed basis must be the true one: kepler is not a root, so "membership" is false.
    rejected(revision(&s, &[&r1], Some(adopts(&s, &p, &go, "kepler")), 4));
    let r2 = revision(&s, &[&r1], Some(adopts_addressed(&s, &p, &go, "kepler")), 5).unwrap();
    // MEMBERSHIP: the roots at the mark. rocky is made a root only AFTER her go; today's roots must not authorize the earlier mark.
    let q = say(&s, "kepler", ROOM, "ship saturday?", &["intent:proposal"], 6);
    let early = say(&s, "rocky", ROOM, "✓", &[&format!("re:{q}"), "react:go"], 7);
    put(&s, &envelope("blaze", ROOM, "[Core members] roots", Some(json!({"schema": "room-members/1", "roots": ["blaze", "rocky"]}).to_string()), &[], 8), 8).unwrap();
    // The fold now reads rocky as a root and calls q settled, but she was not a root when she marked: the guard does not take today's roots for then.
    assert_eq!(settled_by_now(&s, &q).as_deref(), Some(early.as_str()), "precondition: today's fold calls it settled, so only the mark-time check can refuse");
    rejected(revision(&s, &[&r2], Some(adopts(&s, &q, &early, "rocky")), 9));
    // A mark made after the grant is authorized by it.
    let r = say(&s, "kepler", ROOM, "ship sunday?", &["intent:proposal"], 10);
    let late = say(&s, "rocky", ROOM, "✓", &[&format!("re:{r}"), "react:go"], 11);
    revision(&s, &[&r2], Some(adopts(&s, &r, &late, "rocky")), 12).unwrap();
}

#[test]
fn an_annotation_does_not_make_a_decider_and_the_guard_would_not_take_it_either() {
    let s = oracle();
    let r1 = revision(&s, &[], None, 1).unwrap();
    let p = say(&s, "blaze", ROOM, "ship?", &["intent:proposal"], 2);
    // blaze (a person) annotates `to=kepler` onto his own proposal. Today's fold does not make her a decider for it (checked below), and the guard reads only the
    // proposal's own signed to: tags besides, so neither basis can be reached by an annotation.
    let note = Envelope::sign(&sk(1), ROOM, "blaze", Kind::Annotate, "addressed", None, vec![format!("annotates:{p}"), "set:to=kepler".into()], NOW + 3 * MIN);
    put(&s, &note, 3).unwrap();
    let go = say(&s, "kepler", ROOM, "✓", &[&format!("re:{p}"), "react:go"], 4);
    assert_eq!(settled_by_now(&s, &p), None, "an annotated to: does not settle in today's fold");
    rejected(revision(&s, &[&r1], Some(adopts_addressed(&s, &p, &go, "kepler")), 5));
    rejected(revision(&s, &[&r1], Some(adopts(&s, &p, &go, "kepler")), 6));
}

/// Parity with the reader: a row the reader would call bad (its stored envelope does not verify, or it is archived) is not a head to the guard either, so a
/// valid client write is never refused for a revision the reader does not see.
#[test]
fn the_guard_counts_the_heads_the_reader_counts() {
    let s = oracle();
    let r1 = revision(&s, &[], None, 1).unwrap();
    // A revision whose stored signature was tampered with in the database: the reader reports it bad; the guard must not demand it as a parent.
    let r2 = revision(&s, &[&r1], None, 2).unwrap();
    s.with_conn(|c| Ok(c.execute("UPDATE observations SET cargo=replace(cargo,'\"g\"','\"tampered\"') WHERE id=?1", rusqlite::params![r2]).unwrap())).unwrap();
    // The reader (verify_stored fails) drops r2: the head is r1 again. The guard agrees.
    revision(&s, &[&r1], None, 3).unwrap();
}

/// The read side is the guard's own fold: whatever `room_state_at` calls the head is exactly what the guard demands as parents, at any cursor.
#[test]
fn the_resolver_and_the_guard_share_one_fold() {
    let s = oracle();
    assert_eq!(s.room_state_at(ROOM, None).unwrap()["state"], "empty");
    let r1 = revision(&s, &[], None, 1).unwrap();
    let p = say(&s, "rocky", ROOM, "ship?", &["intent:proposal"], 2);
    let go = say(&s, "blaze", ROOM, "✓", &[&format!("re:{p}"), "react:go"], 3);
    let r2 = revision(&s, &[&r1], Some(adopts(&s, &p, &go, "blaze")), 4).unwrap();
    let r3 = revision(&s, &[&r2], None, 5).unwrap();
    let now = s.room_state_at(ROOM, None).unwrap();
    // the cursor it read through is named; sql coverage and trust are separate fields; the signed evidence for the client is present and verifies
    assert_eq!((now["coverage"]["sql_complete"].as_bool(), now["trustworthy"].as_bool(), now["as_of_requested"].is_null()), (Some(true), Some(true), true));
    assert_eq!(now["as_of_seq"], json!(now["read_through_seq"].as_i64().unwrap() + 1));
    let ev: Vec<Envelope> = now["evidence"].as_array().unwrap().iter().map(|e| serde_json::from_value(e["envelope"].clone()).unwrap()).collect();
    assert_eq!(ev.len(), 3);
    // the same-snapshot stored row rides with each: the verifier can compare it to the signed envelope byte for byte
    for e in now["evidence"].as_array().unwrap() {
        let (row, env): (&Value, Envelope) = (&e["row"], serde_json::from_value(e["envelope"].clone()).unwrap());
        assert_eq!((row["id"].as_str(), row["author"].as_str(), row["channel"].as_str(), row["envelope"].as_i64()), (e["id"].as_str(), Some("rocky"), Some(ROOM), Some(2)));
        assert_eq!(row["cargo"].as_str(), env.cargo.as_deref());
        assert_eq!(row["pubkey"], e["pubkey"]);
        assert!(e["parents"].is_array() && e["fields"].is_object());
    }
    assert!(now["evidence"].as_array().unwrap().iter().all(|e| e["registered"] == json!(true) && e["pubkey"].is_string()));
    assert!(ev.iter().zip(now["evidence"].as_array().unwrap()).all(|(e, row)| e.verify_stored(row["pubkey"].as_str().unwrap()).is_ok()));
    let h = &now["head_seqs"][0];
    assert_eq!((h["author"].as_str(), h["signed_ms"].as_i64()), (Some("rocky"), Some(NOW + 5 * MIN)), "the head says who signed it and when");
    assert!(h["received_ms"].as_i64().unwrap() > 0);
    assert_eq!((now["state"].as_str(), now["heads"][0].as_str(), now["revisions"].as_i64(), now["complete"].as_bool()), (Some("alive"), Some(r3.as_str()), Some(3), Some(true)));
    // at a cursor: below r2 the head was r1; below r3 it was r2 and it said why it existed; below r1 there is no State
    let below = |id: &str| Some(seq(&s, id));
    assert_eq!(s.room_state_at(ROOM, below(&r3)).unwrap()["heads"][0], json!(r2));
    assert_eq!(s.room_state_at(ROOM, below(&r2)).unwrap()["heads"][0], json!(r1));
    assert_eq!(s.room_state_at(ROOM, below(&r1)).unwrap()["state"], "empty");
    let at_r2 = s.room_state_at(ROOM, Some(seq(&s, &r3))).unwrap();
    assert_eq!(at_r2["adoptions"][&r2]["mark"], json!(go));
    // the guard demands exactly the resolver's head, now
    conflict(revision(&s, &[&r2], None, 6));
    revision(&s, &[now["heads"][0].as_str().unwrap()], None, 7).unwrap();
    // a tampered revision is a bad row to the resolver and never a head
    let r5 = s.room_state_at(ROOM, None).unwrap()["heads"][0].as_str().unwrap().to_string();
    s.with_conn(|c| Ok(c.execute("UPDATE observations SET cargo=replace(cargo,'\"g\"','\"x\"') WHERE id=?1", rusqlite::params![r5]).unwrap())).unwrap();
    let v = s.room_state_at(ROOM, None).unwrap();
    assert_eq!(v["state"], "degraded");
    assert_eq!((v["trustworthy"].as_bool(), v["complete"].as_bool(), v["coverage"]["sql_complete"].as_bool()), (Some(false), Some(false), Some(true)), "SQL coverage is not trust");
    assert_eq!(v["bad"][0]["id"], json!(r5));
    assert!(!v["heads"].as_array().unwrap().contains(&json!(r5)));
}

/// Bobby's controls on the membership basis (14:09Z): the record that grants roots must itself be trustworthy AT THE TIME it was written.
mod membership_controls {
    use super::*;

    fn members(s: &Store, key: u8, who: &str, roots: &[&str], mins: i64) -> String {
        let e = Envelope::sign(&sk(key), ROOM, who, Kind::Say, "[Core members] roots", Some(json!({"schema": "room-members/1", "roots": roots}).to_string()), vec![], NOW + mins * MIN);
        let who = Principal { handle: who.into(), scope: Scope::Write };
        s.write(&who, &e, NOW + mins * MIN).unwrap().id
    }
    /// A fresh room whose only roots record is the one under test (the fixture's own is replaced by being older and by naming nobody useful).
    fn world() -> (Store, String) {
        let s = Store::memory().unwrap();
        s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 5000 * MIN).unwrap();
        for (n, who) in [(2u8, "rocky"), (3, "kepler")] {
            s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, who, &pubkey_hex(&sk(n)), NOW - 5000 * MIN), NOW - 5000 * MIN).unwrap();
        }
        let r1 = revision(&s, &[], None, 1).unwrap();
        (s, r1)
    }
    fn proposal_and_go(s: &Store, go_by: &str, mins: i64) -> (String, String) {
        let p = say(s, "kepler", ROOM, "ship?", &["intent:proposal"], mins);
        let go = say(s, go_by, ROOM, "✓", &[&format!("re:{p}"), "react:go"], mins + 1);
        (p, go)
    }

    #[test]
    fn a_roots_record_signed_by_a_non_owner_key_of_an_owner_handle_authorizes_nothing() {
        let (s, r1) = world();
        // blaze registers a second, ordinary key for himself; a roots record signed with it is not owner-signed
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "blaze", &pubkey_hex(&sk(9)), NOW - 4000 * MIN), NOW - 4000 * MIN).unwrap();
        members(&s, 9, "blaze", &["rocky"], 2);
        let (p, go) = proposal_and_go(&s, "rocky", 3);
        assert_eq!(settled_by_now(&s, &p).as_deref(), Some(go.as_str()), "precondition: today's fold calls it settled, so only the guard's own check can refuse");
        rejected(revision(&s, &[&r1], Some(adopts(&s, &p, &go, "rocky")), 5));
        // the same record signed by the owner key does authorize
        members(&s, 1, "blaze", &["rocky"], 6);
        let (p, go) = proposal_and_go(&s, "rocky", 7);
        revision(&s, &[&r1], Some(adopts(&s, &p, &go, "rocky")), 9).unwrap();
    }

    #[test]
    fn a_tampered_roots_record_authorizes_nothing() {
        let (s, r1) = world();
        let rec = members(&s, 1, "blaze", &["rocky"], 2);
        let (p, go) = proposal_and_go(&s, "rocky", 3);
        assert_eq!(settled_by_now(&s, &p).as_deref(), Some(go.as_str()), "precondition: the fold settled it from the record as signed");
        // the record is then rewritten in the database (it now also names someone else): its signature no longer covers what it says
        s.with_conn(|c| Ok(c.execute("UPDATE observations SET cargo=replace(cargo,'[\"rocky\"]','[\"rocky\",\"x\"]') WHERE id=?1", rusqlite::params![rec]).unwrap())).unwrap();
        rejected(revision(&s, &[&r1], Some(adopts(&s, &p, &go, "rocky")), 5));
    }

    #[test]
    fn a_key_made_owner_after_the_record_does_not_validate_it() {
        let (s, r1) = world();
        // kepler (a seat) writes a roots record naming rocky: nothing then
        members(&s, 3, "kepler", &["rocky"], 2);
        let (p, go) = proposal_and_go(&s, "rocky", 3);
        assert_eq!(settled_by_now(&s, &p), None, "a seat's roots record settles nothing when written");
        // CONTROL: promoted to owner with her key's role dated BEFORE the record, today's fold, and the guard, both accept (so only the date can be the reason below)
        s.with_conn(|c| Ok(c.execute("UPDATE keys SET role='owner', created_ms=?1 WHERE handle='kepler'", rusqlite::params![NOW - 1000 * MIN]).unwrap())).unwrap();
        s.reindex().unwrap();
        assert_eq!(settled_by_now(&s, &p).as_deref(), Some(go.as_str()), "precondition: today's fold now reads her old record as owner-signed");
        revision(&s, &[&r1], Some(adopts(&s, &p, &go, "rocky")), 4).unwrap();
        // ...and dated AFTER the record: the fold still says settled (it reads today's role), the guard refuses: the key was not an owner key when it wrote the record.
        let (_, r1b) = (0, s.room_state_at(ROOM, None).unwrap()["heads"][0].as_str().unwrap().to_string());
        s.with_conn(|c| Ok(c.execute("UPDATE keys SET created_ms=?1 WHERE handle='kepler'", rusqlite::params![NOW + 50 * MIN]).unwrap())).unwrap();
        s.reindex().unwrap();
        assert_eq!(settled_by_now(&s, &p).as_deref(), Some(go.as_str()), "precondition: the fold is unchanged by the date");
        rejected(revision(&s, &[&r1b], Some(adopts(&s, &p, &go, "rocky")), 60));
    }

    #[test]
    fn an_imported_roots_record_never_counts() {
        let (s, r1) = world();
        use oracle_server::store::LegacyRow;
        s.import_legacy(&[LegacyRow {
            id: "OBS-core-20260101-members0001".into(), channel: ROOM.into(), author: "blaze".into(), kind: "say".into(), content: "[Core members] roots".into(),
            cargo: Some(json!({"schema": "room-members/1", "roots": ["rocky"]}).to_string()), tags: vec!["signed:ok".into(), "cargo-signed:ok".into()],
            created_ms: NOW - 100 * MIN, pubkey: String::new(), sig: String::new(), news: true, source: None, repair_legacy: false,
        }])
        .unwrap();
        let (p, go) = proposal_and_go(&s, "rocky", 3);
        // (today's fold may or may not read an imported roots record; the guard never does)
        rejected(revision(&s, &[&r1], Some(adopts(&s, &p, &go, "rocky")), 5));
    }

    #[test]
    fn an_addressed_proposal_never_reports_a_membership_basis() {
        let (s, r1) = world();
        let p = say(&s, "blaze", ROOM, "kepler, ship?", &["intent:proposal", "to:kepler"], 2);
        let go = say(&s, "kepler", ROOM, "✓", &[&format!("re:{p}"), "react:go"], 3);
        // kepler is not a root, so a membership claim is false; only the addressed basis is true
        rejected(revision(&s, &[&r1], Some(adopts(&s, &p, &go, "kepler")), 4));
        let ok = revision(&s, &[&r1], Some(adopts_addressed(&s, &p, &go, "kepler")), 5).unwrap();
        let c: Value = serde_json::from_str(s.get(&ok).unwrap().cargo.as_deref().unwrap()).unwrap();
        assert_eq!(c["adopts"]["basis"], "addressed");
    }
}


#[test]
fn state_refuses_future_and_nonpositive_cursors_and_replays_the_available_boundary() {
    let s = oracle();
    let a = revision(&s, &[], None, 1).unwrap();
    let first = s.room_state_at(ROOM, None).unwrap();
    let cursor = first["as_of_seq"].as_i64().unwrap();
    for bad in [0, -1, cursor + 1, i64::MAX] {
        assert!(matches!(s.room_state_at(ROOM, Some(bad)), Err(StoreError::Rejected(_))), "cursor {bad} cannot certify a replay boundary");
    }
    let b = revision(&s, &[&a], None, 2).unwrap();
    let replay = s.room_state_at(ROOM, Some(cursor)).unwrap();
    assert_eq!(replay["heads"], first["heads"]);
    assert_eq!(replay["fields"], first["fields"]);
    assert_eq!(replay["as_of_seq"], first["as_of_seq"]);
    assert_eq!(s.room_state_at(ROOM, Some(cursor + 1)).unwrap()["heads"], json!([b]));
}

#[test]
fn malformed_revision_shapes_are_bad_in_imported_history_and_refused_on_new_writes() {
    use oracle_server::store::LegacyRow;
    for (name, parents, fields, reason) in [
        ("string-parents", json!("x"), json!({"goal": "g"}), "parents"),
        ("numeric-parent", json!([1]), json!({"goal": "g"}), "parents"),
        ("array-fields", Value::Null, json!(["g"]), "fields"),
    ] {
        let s = oracle();
        let a = revision(&s, &[], None, 1).unwrap();
        let parents = if parents.is_null() { json!([a]) } else { parents };
        let raw = json!({"schema": "room-context/1", "channel": ROOM, "parents": parents, "fields": fields}).to_string();
        let before = s.room_state_at(ROOM, None).unwrap()["read_through_seq"].clone();
        let error = rejected(put(&s, &envelope("rocky", ROOM, "bad shape", Some(raw.clone()), &["room-context:v1"], 2), 2));
        assert!(error.contains(reason), "{name}: {error}");
        assert_eq!(s.room_state_at(ROOM, None).unwrap()["read_through_seq"], before, "refusal appends nothing");
        let id = format!("OBS-core-20260101-{name}");
        s.import_legacy(&[LegacyRow {
            id: id.clone(), channel: ROOM.into(), author: "rocky".into(), kind: "say".into(), content: "historical bad shape".into(),
            cargo: Some(raw), tags: vec!["room-context:v1".into(), "signed:ok".into(), "cargo-signed:ok".into()],
            created_ms: NOW, pubkey: String::new(), sig: String::new(), news: true, source: None, repair_legacy: false,
        }]).unwrap();
        let v = s.room_state_at(ROOM, None).unwrap();
        assert_eq!(v["state"], "degraded", "{name}");
        assert_eq!(v["complete"], false);
        assert_eq!(v["trustworthy"], false);
        assert_eq!(v["coverage"]["sql_complete"], true);
        assert!(v["fields"].is_null());
        assert_eq!(v["heads"], json!([a]));
        assert!(v["bad"].as_array().unwrap().iter().any(|b| b["id"] == id && b["why"].as_str().unwrap().contains(reason)), "{name}");
        assert!(v["evidence"].as_array().unwrap().iter().all(|e| e["id"] != id));
    }
}
