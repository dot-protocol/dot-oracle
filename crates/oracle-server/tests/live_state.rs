//! Live state keeps its head, not every revision (src/livestate.rs). A state write tagged `live-state`, or owned by a machine, replaces the previous revision of
//! its subject when both name the same state word; a change of word stays as history; a revision something points at stays. `compact` gives the rows already
//! there the same shape, and the index and the search stay right.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn key_of(who: &str) -> u8 {
    match who {
        "blaze" => 1,
        "cron" => 2,
        "estate" => 3,
        _ => 4,
    }
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    for (h, k) in [("cron", 2), ("estate", 3), ("kepler", 4)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    }
    s
}
static CLOCK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
fn tick() -> i64 {
    NOW + CLOCK.fetch_add(1, std::sync::atomic::Ordering::SeqCst) * MIN
}
fn put(s: &Store, room: &str, who: &str, kind: Kind, text: &str, cargo: Option<String>, tags: &[&str]) -> String {
    let at = tick();
    let env = Envelope::sign(&sk(key_of(who)), room, who, kind, text, cargo, tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
/// A state write for `subject` by `who`, owned by `owner`.
#[allow(clippy::too_many_arguments)]
fn head(s: &Store, room: &str, who: &str, subject: &str, state: &str, owner: &str, text: &str, tags: &[&str]) -> String {
    let cargo = serde_json::json!({"subject": subject, "state": state, "owner": owner, "next_touch": "2026-10-01"}).to_string();
    put(s, room, who, Kind::State, text, Some(cargo), tags)
}
fn count(s: &Store, sql: &str) -> i64 {
    s.with_conn(|c| Ok(c.query_row(sql, [], |r| r.get(0)).unwrap())).unwrap()
}
fn exists(s: &Store, id: &str) -> bool {
    s.get(id).is_ok()
}
fn subject_rows(s: &Store, subject: &str) -> i64 {
    count(s, &format!("SELECT count(*) FROM observations WHERE kind='state' AND cargo LIKE '%\"subject\":\"{subject}\"%'"))
}
const LIVE: &[&str] = &["live-state"];

#[test]
fn a_live_state_write_replaces_the_previous_revision_of_the_same_word() {
    let s = oracle();
    let a = head(&s, "ops", "cron", "job:census", "running", "cron", "census reading alpha", LIVE);
    let b = head(&s, "ops", "cron", "job:census", "running", "cron", "census reading bravo", LIVE);
    assert!(!exists(&s, &a) && exists(&s, &b), "the older revision is gone, the newer is the head");
    assert_eq!(subject_rows(&s, "job:census"), 1);
    let heads = s.state_heads_in(None, Some("ops")).unwrap();
    assert_eq!((heads.len(), heads[0].id.clone()), (1, b.clone()));
    // everything derived from the old row went with it, and the room's count is right
    assert_eq!(count(&s, "SELECT count(*) FROM obs_fts"), 1);
    assert_eq!(count(&s, "SELECT count(*) FROM msg"), 1);
    assert_eq!(count(&s, "SELECT count(*) FROM thread"), 1);
    assert_eq!(count(&s, "SELECT total FROM channel_heads WHERE channel='ops'"), 1);
    assert_eq!(count(&s, "SELECT count(*) FROM tags WHERE obs_seq NOT IN (SELECT seq FROM observations)"), 0, "no tag row dangles");
    assert!(s.search("alpha", Some("ops"), None, 10).map(|h| h.is_empty()).unwrap_or(true), "the old text is not searchable");
    assert!(!s.search("bravo", Some("ops"), None, 10).unwrap().is_empty());
}

#[test]
fn a_change_of_state_word_stays_as_history() {
    let s = oracle();
    let r1 = head(&s, "ops", "cron", "job:x", "running", "cron", "one", LIVE);
    let r2 = head(&s, "ops", "cron", "job:x", "running", "cron", "two", LIVE);
    let r3 = head(&s, "ops", "cron", "job:x", "blocked", "cron", "three", LIVE);
    let r4 = head(&s, "ops", "cron", "job:x", "running", "cron", "four", LIVE);
    let r5 = head(&s, "ops", "cron", "job:x", "running", "cron", "five", LIVE);
    assert!(!exists(&s, &r1) && !exists(&s, &r4), "a stretch of one word keeps its newest row");
    assert!(exists(&s, &r2) && exists(&s, &r3) && exists(&s, &r5), "running, then blocked, then running: the changes stay");
    assert_eq!(subject_rows(&s, "job:x"), 3);
    assert_eq!(s.state_heads_in(None, None).unwrap()[0].id, r5);
}

#[test]
fn what_is_not_live_is_never_replaced_and_a_machine_owner_is_live_without_the_tag() {
    let s = oracle();
    // a person's or a seat's state, no tag, not a machine: every revision stays
    let a = head(&s, "ops", "kepler", "task:build", "running", "kepler", "a", &[]);
    let b = head(&s, "ops", "kepler", "task:build", "running", "kepler", "b", &[]);
    assert!(exists(&s, &a) && exists(&s, &b));
    // a machine owner replaces with no tag at all
    let e1 = head(&s, "feeds", "estate", "estate:astra", "noted", "estate", "Astra estate 1", &[]);
    let e2 = head(&s, "feeds", "estate", "estate:astra", "noted", "estate", "Astra estate 2", &[]);
    assert!(!exists(&s, &e1) && exists(&s, &e2));
    // a live write never eats a not-live predecessor
    let p = head(&s, "ops", "kepler", "job:mixed", "running", "kepler", "by a person", &[]);
    let l = head(&s, "ops", "cron", "job:mixed", "running", "cron", "by a machine", LIVE);
    assert!(exists(&s, &p) && exists(&s, &l));
}

#[test]
fn a_revision_something_points_at_is_kept_and_other_rooms_are_untouched() {
    let s = oracle();
    let a = head(&s, "ops", "cron", "job:y", "running", "cron", "watched", LIVE);
    put(&s, "ops", "kepler", Kind::Say, "I am looking at this one", None, &[&format!("re:{a}")]);
    let b = head(&s, "ops", "cron", "job:y", "running", "cron", "newer", LIVE);
    assert!(exists(&s, &a) && exists(&s, &b), "a reply pins the revision it answers");
    // the same subject in another room is another row
    let other = head(&s, "elsewhere", "cron", "job:y", "running", "cron", "other room", LIVE);
    assert!(exists(&s, &b) && exists(&s, &other), "replacement is per room");
    // replies and ordinary messages elsewhere are unaffected
    let said = put(&s, "ops", "kepler", Kind::Say, "unrelated", None, &[]);
    head(&s, "ops", "cron", "job:z", "running", "cron", "z1", LIVE);
    head(&s, "ops", "cron", "job:z", "running", "cron", "z2", LIVE);
    assert!(exists(&s, &said));
}

#[test]
fn compaction_gives_old_rows_the_shape_the_rule_would_have_left_and_the_index_stays_equal() {
    let s = oracle();
    // rows as they were before the rule: not replaced at write time (no tag, owner not a machine), then tagged as the old mirror wrote them
    let words = ["running", "running", "running", "blocked", "running"];
    let ids: Vec<String> = words.iter().enumerate().map(|(i, w)| head(&s, "mirror", "cron", "machine:ns", w, "cron", &format!("census {i}"), &[])).collect();
    let cold: Vec<String> = (0..3).map(|i| head(&s, "mirror", "kepler", "task:plain", "running", "kepler", &format!("plain {i}"), &[])).collect();
    let pinned_a = head(&s, "mirror", "cron", "machine:pin", "running", "cron", "pinned", &[]);
    put(&s, "mirror", "kepler", Kind::Say, "see this", None, &[&format!("re:{pinned_a}")]);
    let pinned_b = head(&s, "mirror", "cron", "machine:pin", "running", "cron", "pinned newer", &[]);
    let untouched_room = head(&s, "keep", "cron", "machine:keep", "running", "cron", "elsewhere 1", &[]);
    let untouched_room2 = head(&s, "keep", "cron", "machine:keep", "running", "cron", "elsewhere 2", &[]);
    s.with_conn(|c| Ok(c.execute("UPDATE observations SET tags_json='[\"live-state\"]' WHERE author='cron'", []).unwrap())).unwrap();
    let before_rows = count(&s, "SELECT count(*) FROM observations");
    // dry run: counts, changes nothing
    let dry = s.compact_live(&["mirror".to_string()], true).unwrap();
    assert_eq!((dry[0].states, dry[0].deleted), (10, 2), "machine:ns loses its 1st and 2nd; the pinned one is not counted");
    assert_eq!(count(&s, "SELECT count(*) FROM observations"), before_rows);
    let done = s.compact_live(&["mirror".to_string()], false).unwrap();
    assert_eq!(done[0].deleted, 2);
    // running,running,running,blocked,running -> running(3rd), blocked, running(last)
    let alive: Vec<bool> = ids.iter().map(|i| exists(&s, i)).collect();
    assert_eq!(alive, vec![false, false, true, true, true]);
    assert!(cold.iter().all(|i| exists(&s, i)), "not live: untouched");
    assert!(exists(&s, &pinned_a) && exists(&s, &pinned_b), "a pinned revision stays");
    assert!(exists(&s, &untouched_room) && exists(&s, &untouched_room2), "rooms not named are untouched");
    assert_eq!(count(&s, "SELECT total FROM channel_heads WHERE channel='mirror'"), count(&s, "SELECT count(*) FROM observations WHERE channel='mirror'"));
    // heads and search right, and a second run has nothing left to do
    assert_eq!(s.state_heads_in(None, Some("mirror")).unwrap().iter().find(|h| h.subject == "machine:ns").unwrap().id, ids[4]);
    assert!(s.search("census", Some("mirror"), None, 10).unwrap().len() == 3);
    assert_eq!(s.compact_live(&["mirror".to_string()], false).unwrap()[0].deleted, 0);
    // the index built by the writes equals the index built from scratch
    let built = s.index_dump().unwrap();
    s.reindex().unwrap();
    assert_eq!(s.index_dump().unwrap(), built, "reindex leaves exactly what compaction left");
}

#[test]
fn the_mirror_path_replaces_too() {
    use oracle_server::store::LegacyRow;
    let s = oracle();
    let row = |id: &str, ms: i64, state: &str, text: &str| LegacyRow {
        id: id.into(), channel: "mirror".into(), author: "unknown".into(), kind: "state".into(), content: text.into(), cargo: None,
        tags: ["subject:machine:ns", &format!("state:{state}"), "owner:census", "clock:2026-09-30", "live-state"].iter().map(|t| t.to_string()).collect(),
        created_ms: ms, pubkey: String::new(), sig: String::new(), news: true, source: None, repair_legacy: false,
    };
    s.import_legacy(&[row("OBS-mirror-1", NOW, "running", "census one")]).unwrap();
    s.import_legacy(&[row("OBS-mirror-2", NOW + MIN, "running", "census two")]).unwrap();
    assert!(!exists(&s, "OBS-mirror-1") && exists(&s, "OBS-mirror-2"), "the mirrored head replaces the one before it");
    s.import_legacy(&[row("OBS-mirror-3", NOW + 2 * MIN, "blocked", "census three")]).unwrap();
    assert!(exists(&s, "OBS-mirror-2") && exists(&s, "OBS-mirror-3"), "a change of word stays");
    assert_eq!(count(&s, "SELECT total FROM channel_heads WHERE channel='mirror'"), 2);
    assert_eq!(s.state_heads_in(None, Some("mirror")).unwrap()[0].id, "OBS-mirror-3");
    let built = s.index_dump().unwrap();
    s.reindex().unwrap();
    assert_eq!(s.index_dump().unwrap(), built);
}

#[test]
fn replacing_a_live_state_keeps_search_valid_so_a_restart_does_not_rebuild_it() {
    let s = oracle();
    let meta = |k: &str| count(&s, &format!("SELECT CAST(v AS INTEGER) FROM store_meta WHERE k='{k}'"));
    head(&s, "ops", "cron", "job:census", "running", "cron", "census reading alpha", LIVE);
    head(&s, "ops", "cron", "job:census", "running", "cron", "census reading bravo", LIVE);
    head(&s, "ops", "cron", "job:census", "running", "cron", "census reading charlie", LIVE);
    assert_eq!(meta("fts_cargo_verified_generation"), meta("fts_cargo_source_generation"), "inserts and the deletes they caused are all certified");
    // Negative control: a change made outside the projector (an older binary) still leaves validity behind.
    s.with_conn(|c| {
        c.execute("UPDATE observations SET content=content", []).unwrap();
        Ok(())
    })
    .unwrap();
    assert!(meta("fts_cargo_verified_generation") < meta("fts_cargo_source_generation"));
}
