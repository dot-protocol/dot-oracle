//! Room delete and move (src/rooms.rs) on the real store: what a dry run reports and leaves alone, that a delete leaves
//! nothing behind in any derived table, that a move re-homes search, threads and heads, and that a move never carries a
//! signed message into a room its signature does not name.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::roomread::{search, SearchQ};
use oracle_server::store::{LegacyRow, Principal, Scope, Store, StoreError};

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 1000 * MIN), NOW - 1000 * MIN).unwrap();
    s
}

fn imported(id: &str, room: &str, content: &str, tags: &[&str], mins: i64) -> LegacyRow {
    LegacyRow {
        id: id.into(), channel: room.into(), author: "rocky".into(), kind: "say".into(), content: content.into(), cargo: None,
        tags: tags.iter().map(|t| t.to_string()).collect(), created_ms: NOW + mins * MIN, pubkey: String::new(), sig: String::new(),
        news: true, source: None, repair_legacy: false,
    }
}

fn signed(s: &Store, room: &str, text: &str, tags: &[&str], mins: i64) -> String {
    let at = NOW + mins * MIN;
    let env = Envelope::sign(&sk(2), room, "rocky", Kind::Say, text, None, tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap().id
}

fn count(s: &Store, sql: &str) -> i64 {
    s.with_conn(|c| Ok(c.query_row(sql, [], |r| r.get(0))?)).unwrap()
}

fn ids(v: &serde_json::Value) -> Vec<String> {
    v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_owned()).collect()
}

fn in_room(room: &str, q: &str) -> SearchQ {
    SearchQ { q: q.into(), room: Some(room.into()), ..Default::default() }
}

#[test]
fn a_dry_run_reports_and_changes_nothing() {
    let s = oracle();
    s.import_legacy(&[imported("OBS-t-1", "scratch", "probe one", &[], 0), imported("OBS-t-2", "scratch", "probe two", &[], 1)]).unwrap();
    let before = count(&s, "SELECT count(*) FROM observations");
    let r = s.delete_room("scratch", false, true, "blaze", NOW).unwrap();
    assert_eq!((r.rows, r.dry_run, r.outcome.as_str()), (2, true, "dry run: nothing changed"));
    assert_eq!(count(&s, "SELECT count(*) FROM observations"), before);
    assert_eq!(count(&s, "SELECT count(*) FROM room_ops"), 0, "a dry run is not recorded as a change");
    let m = s.move_room("scratch", "kept", true, "blaze", NOW).unwrap();
    assert_eq!((m.rows, m.outcome.as_str()), (2, "dry run: nothing changed"));
    assert_eq!(count(&s, "SELECT count(*) FROM observations WHERE channel='scratch'"), 2);
}

#[test]
fn a_deleted_room_leaves_nothing_behind() {
    let s = oracle();
    s.import_legacy(&[
        imported("OBS-t-1", "scratch", "needle probe", &["intent:question"], 0),
        imported("OBS-t-2", "scratch", "needle reply", &["re:OBS-t-1"], 1),
        imported("OBS-k-1", "kept", "needle that stays", &[], 2),
    ])
    .unwrap();
    s.ack("blaze", &[("scratch".to_string(), 1_i64)].into_iter().collect(), false).unwrap();
    assert_eq!(ids(&search(&s, &in_room("scratch", "needle")).unwrap()).len(), 2);

    let r = s.delete_room("scratch", false, false, "blaze", NOW).unwrap();
    assert_eq!((r.rows, r.outcome.as_str()), (2, "deleted"));
    for (table, sql) in [
        ("observations", "SELECT count(*) FROM observations WHERE channel='scratch'"),
        ("tags", "SELECT count(*) FROM tags WHERE obs_seq NOT IN (SELECT seq FROM observations)"),
        ("search rows", "SELECT count(*) FROM obs_fts WHERE rowid NOT IN (SELECT seq FROM observations)"),
        ("imported sources", "SELECT count(*) FROM legacy_sources WHERE obs_id LIKE 'OBS-t-%'"),
        ("index messages", "SELECT count(*) FROM msg WHERE channel='scratch'"),
        ("index threads", "SELECT count(*) FROM thread WHERE root_seq NOT IN (SELECT seq FROM observations)"),
        ("room head", "SELECT count(*) FROM channel_heads WHERE channel='scratch'"),
        ("read marks", "SELECT count(*) FROM cursors WHERE channel='scratch'"),
    ] {
        assert_eq!(count(&s, sql), 0, "{table} still holds the deleted room");
    }
    assert_eq!(ids(&search(&s, &in_room("kept", "needle")).unwrap()), vec!["OBS-k-1"], "another room is untouched");
    assert_eq!(count(&s, "SELECT total FROM channel_heads WHERE channel='kept'"), 1);
    assert_eq!(count(&s, "SELECT count(*) FROM room_ops WHERE op='delete' AND room='scratch' AND rows=2"), 1);
    assert!(matches!(s.delete_room("scratch", false, false, "blaze", NOW), Err(StoreError::NotFound)), "a room that is gone is not found");
}

#[test]
fn a_room_other_rooms_point_into_is_kept_unless_forced() {
    let s = oracle();
    s.import_legacy(&[
        imported("OBS-t-1", "scratch", "the question", &["intent:question"], 0),
        imported("OBS-k-1", "kept", "an answer from elsewhere", &["re:OBS-t-1"], 1),
    ])
    .unwrap();
    let r = s.delete_room("scratch", false, false, "blaze", NOW).unwrap();
    assert_eq!(r.referenced_from_outside, 1);
    assert!(r.outcome.starts_with("refused"), "{}", r.outcome);
    assert_eq!(count(&s, "SELECT count(*) FROM observations WHERE channel='scratch'"), 1, "a refusal changes nothing");

    let forced = s.delete_room("scratch", true, false, "blaze", NOW).unwrap();
    assert_eq!(forced.outcome, "deleted");
    assert_eq!(count(&s, "SELECT count(*) FROM observations WHERE id='OBS-k-1'"), 1, "the message that pointed in stays");
    assert_eq!(
        count(&s, "SELECT count(*) FROM msg m JOIN observations o ON o.seq=m.seq WHERE o.id='OBS-k-1' AND m.root_seq=m.seq"),
        1,
        "and now starts its own thread"
    );
}

#[test]
fn a_state_head_on_a_deleted_row_falls_back_to_the_last_one_left() {
    let s = oracle();
    let state = |id: &str, room: &str, word: &str, mins: i64| LegacyRow {
        kind: "state".into(),
        ..imported(id, room, &format!("energy {word}"), &["live-state", "subject:energy", &format!("state:{word}"), "owner:watt"], mins)
    };
    s.import_legacy(&[state("OBS-e-1", "estate", "open", 0), state("OBS-e-2", "scratch", "blocked", 1)]).unwrap();
    assert_eq!(s.with_conn(|c| Ok(c.query_row("SELECT state FROM state_heads WHERE subject='energy'", [], |r| r.get::<_, String>(0))?)).unwrap(), "blocked");
    let r = s.delete_room("scratch", false, false, "blaze", NOW).unwrap();
    assert_eq!(r.state_heads_changed, 1);
    assert_eq!(s.with_conn(|c| Ok(c.query_row("SELECT state FROM state_heads WHERE subject='energy'", [], |r| r.get::<_, String>(0))?)).unwrap(), "open");
    s.delete_room("estate", false, false, "blaze", NOW).unwrap();
    assert_eq!(count(&s, "SELECT count(*) FROM state_heads WHERE subject='energy'"), 0, "no state row left: no head");
}

#[test]
fn a_moved_room_is_searched_threaded_and_counted_in_its_new_home() {
    let s = oracle();
    s.import_legacy(&[
        imported("OBS-l-1", "loom", "moltbook research verdict", &["intent:result"], 0),
        imported("OBS-l-2", "loom", "a follow up on moltbook", &["re:OBS-l-1"], 1),
        imported("OBS-k-1", "knowledge", "skills marketplace research", &[], 2),
    ])
    .unwrap();
    let r = s.move_room("loom", "knowledge", false, "blaze", NOW).unwrap();
    assert_eq!((r.rows, r.outcome.as_str()), (2, "moved"));
    assert_eq!(count(&s, "SELECT count(*) FROM observations WHERE channel='loom'"), 0);
    assert_eq!(count(&s, "SELECT count(*) FROM channel_heads WHERE channel='loom'"), 0);
    assert_eq!(count(&s, "SELECT total FROM channel_heads WHERE channel='knowledge'"), 3);
    let found = ids(&search(&s, &in_room("knowledge", "moltbook")).unwrap());
    assert_eq!(found.len(), 2, "the room's search finds the moved messages: {found:?}");
    assert!(ids(&search(&s, &in_room("loom", "moltbook")).unwrap()).is_empty());
    assert_eq!(count(&s, "SELECT count(*) FROM msg WHERE channel='knowledge'"), 3, "the index places them in the new room");
    assert_eq!(
        count(&s, "SELECT count(*) FROM msg m JOIN observations o ON o.seq=m.seq WHERE o.id='OBS-l-2' AND m.root_seq=(SELECT seq FROM observations WHERE id='OBS-l-1')"),
        1,
        "the reply still hangs under its question"
    );
    assert_eq!(count(&s, "SELECT count(*) FROM room_ops WHERE op='move' AND room='loom' AND target='knowledge'"), 1);
}

#[test]
fn a_move_never_carries_a_signed_message() {
    let s = oracle();
    signed(&s, "decisions-draft", "a signed decision", &[], 0);
    s.import_legacy(&[imported("OBS-d-1", "decisions-draft", "imported note", &[], 1)]).unwrap();
    let r = s.move_room("decisions-draft", "decision", false, "blaze", NOW).unwrap();
    assert_eq!(r.signed, 1);
    assert!(r.outcome.starts_with("refused"), "{}", r.outcome);
    assert_eq!(count(&s, "SELECT count(*) FROM observations WHERE channel='decisions-draft'"), 2, "refused as a whole");
    assert_eq!(count(&s, "SELECT count(*) FROM room_ops"), 0);
    // Deleting a room with a signed message is the owner's call, and allowed.
    assert_eq!(s.delete_room("decisions-draft", false, false, "blaze", NOW).unwrap().outcome, "deleted");
}

#[test]
fn bad_names_and_self_moves_are_refused() {
    let s = oracle();
    s.import_legacy(&[imported("OBS-t-1", "scratch", "x", &[], 0)]).unwrap();
    for bad in ["", "Has Caps", "a/b", "x'; DROP TABLE observations;--"] {
        assert!(matches!(s.delete_room(bad, false, true, "blaze", NOW), Err(StoreError::Rejected(_))), "{bad:?}");
        assert!(matches!(s.move_room("scratch", bad, true, "blaze", NOW), Err(StoreError::Rejected(_))), "{bad:?}");
    }
    assert!(matches!(s.move_room("scratch", "scratch", true, "blaze", NOW), Err(StoreError::Rejected(_))));
    assert!(matches!(s.move_room("nowhere", "kept", true, "blaze", NOW), Err(StoreError::NotFound)));
}

#[test]
fn state_rows_with_plain_text_cargo_do_not_stop_a_delete() {
    // An earlier Oracle's state rows carry free text as cargo; reading every candidate's cargo as JSON made SQLite refuse
    // the whole delete ("malformed JSON", 2026-10-01). A dry run now does every step and rolls back, so it fails the same way.
    let s = oracle();
    let row = |id: &str, room: &str, mins: i64| LegacyRow {
        kind: "state".into(),
        cargo: Some("A1 plain words, not JSON".into()),
        ..imported(id, room, "A1", &["live-state", "subject:probe-a", "state:open"], mins)
    };
    s.import_legacy(&[row("OBS-v-1", "elsewhere", 0), row("OBS-v-2", "vca", 1)]).unwrap();
    let dry = s.delete_room("vca", false, true, "blaze", NOW).unwrap();
    assert_eq!((dry.state_heads_changed, dry.outcome.as_str()), (1, "dry run: nothing changed"));
    assert_eq!(count(&s, "SELECT count(*) FROM observations WHERE channel='vca'"), 1, "the dry run rolled back");
    let r = s.delete_room("vca", false, false, "blaze", NOW).unwrap();
    assert_eq!(r.outcome, "deleted");
    assert_eq!(
        count(&s, "SELECT count(*) FROM state_heads h JOIN observations o ON o.seq=h.obs_seq WHERE h.subject='probe-a' AND o.id='OBS-v-1'"),
        1,
        "the head falls back to the row that is left"
    );
}
