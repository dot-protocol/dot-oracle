// Retention (retention.rs): a room with a rule forgets its records after the ttl, records already there included; a record's own
// expires tag wins; a room without a rule keeps everything; the sweep counts first and removes only with apply; a non-owner's
// record changes nothing.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};

const H: i64 = 3_600_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

#[allow(clippy::too_many_arguments)] // the envelope's own fields, passed through as Envelope::sign takes them
fn say(store: &Store, key: &SigningKey, who: &str, room: &str, text: &str, cargo: Option<String>, tags: Vec<String>, at: i64) -> String {
    let env = Envelope::sign(key, room, who, Kind::Say, text, cargo, tags, at);
    store.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}

fn setup() -> (Store, SigningKey, SigningKey) {
    let store = Store::memory().unwrap();
    let owner = sk(1);
    let seat = sk(2);
    store.init_owner("blaze", &pubkey_hex(&owner), 1).unwrap();
    store.apply_key_event(&KeyEvent::sign(&owner, "blaze", KeyAction::Add, "estate", &pubkey_hex(&seat), 2), 2).unwrap();
    (store, owner, seat)
}

#[test]
fn a_rule_backfills_the_room_and_sets_every_new_record_s_expiry_and_the_sweep_removes_the_expired() {
    let (store, owner, seat) = setup();
    let t0 = 1_000_000;
    let old = say(&store, &seat, "estate", "feeds", "before the rule", None, vec![], t0);
    let kept = say(&store, &seat, "estate", "oracle", "a room without a rule", None, vec![], t0);
    let rule = r#"{"schema":"retention/1","rooms":{"feeds":{"ttl_hours":48}}}"#;
    say(&store, &owner, "blaze", "oracle", "[Retention] #feeds forgets after 48 h", Some(rule.into()), vec!["schema:retention/1".into()], t0 + 1);
    assert_eq!(store.retention_table().unwrap()["feeds"].ttl_hours, 48);
    let new = say(&store, &seat, "estate", "feeds", "after the rule", None, vec![], t0 + 2);
    let own = say(&store, &seat, "estate", "feeds", "my own expiry", None, vec![format!("expires:{}", t0 + 10 * H)], t0 + 3);
    let exp = |id: &str| store.expires_ms_of(id).unwrap();
    assert_eq!(exp(&old), Some(t0 + 48 * H), "a record from before the rule is backfilled");
    assert_eq!(exp(&new), Some(t0 + 2 + 48 * H), "a new record takes the room's ttl");
    assert_eq!(exp(&own), Some(t0 + 10 * H), "a record's own expires tag wins");
    assert_eq!(exp(&kept), None, "a room without a rule keeps everything");
    // Before the ttl nothing is expired; a dry run counts, an applied sweep removes.
    assert_eq!(store.sweep_expired(t0 + 9 * H, false).unwrap(), vec![("feeds".to_string(), 0)]);
    assert_eq!(store.sweep_expired(t0 + 11 * H, false).unwrap(), vec![("feeds".to_string(), 1)], "the own-expiry record, counted");
    assert!(store.get(&own).is_ok(), "a dry run removes nothing");
    assert_eq!(store.sweep_expired(t0 + 49 * H, true).unwrap(), vec![("feeds".to_string(), 3)]);
    assert!(store.get(&old).is_err() && store.get(&new).is_err() && store.get(&own).is_err(), "removed from the store");
    assert!(store.get(&kept).is_ok(), "the other room is untouched");
    assert_eq!(store.sweep_expired(t0 + 49 * H, true).unwrap(), vec![("feeds".to_string(), 0)], "nothing left to sweep");
}

#[test]
fn only_the_owner_sets_a_rule_and_a_bad_record_changes_nothing() {
    let (store, _owner, seat) = setup();
    let rule = r#"{"schema":"retention/1","rooms":{"feeds":{"ttl_hours":48}}}"#;
    say(&store, &seat, "estate", "oracle", "[Retention] not mine to set", Some(rule.into()), vec!["schema:retention/1".into()], 10);
    assert!(store.retention_table().unwrap().is_empty(), "a seat's record is not the table");
    let id = say(&store, &seat, "estate", "feeds", "stays", None, vec![], 11);
    assert_eq!(store.expires_ms_of(&id).unwrap(), None);
    assert!(store.sweep_expired(1_000 * H, true).unwrap().is_empty(), "no rule, no sweep");
    assert!(store.get(&id).is_ok());
}

#[test]
fn the_sweep_runs_on_a_live_store_opened_for_control_without_rebuilding_its_search_index() {
    // 4 Nov 2026: the hourly sweep opened the serving database with Store::open, whose FTS backfill restarted a whole shadow
    // rebuild (every live write moves the source generation) and held the write lock; live writes answered 503 meanwhile.
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("o.db").to_str().unwrap().to_string();
    let t0 = 1_000_000;
    {
        let store = Store::open(&path).unwrap();
        let owner = sk(1);
        let seat = sk(2);
        store.init_owner("blaze", &pubkey_hex(&owner), 1).unwrap();
        store.apply_key_event(&KeyEvent::sign(&owner, "blaze", KeyAction::Add, "estate", &pubkey_hex(&seat), 2), 2).unwrap();
        let rule = r#"{"schema":"retention/1","rooms":{"feeds":{"ttl_hours":48}}}"#;
        say(&store, &owner, "blaze", "oracle", "[Retention] #feeds forgets after 48 h", Some(rule.into()), vec!["schema:retention/1".into()], t0);
        say(&store, &seat, "estate", "feeds", "a feed record", None, vec![], t0 + 1);
        // A writer the index did not certify: the search index is now behind, as on the serving store.
        store.with_conn(|c| Ok(c.execute_batch("UPDATE store_meta SET v=CAST(v AS INTEGER)+5 WHERE k='fts_cargo_source_generation'")?)).unwrap();
    }
    let control = Store::open_control(&path).unwrap();
    assert_eq!(control.sweep_expired(t0 + 49 * H, true).unwrap(), vec![("feeds".to_string(), 1)]);
    let shadow: bool = control
        .with_conn(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='obs_fts_v1c')", [], |r| r.get(0))?))
        .unwrap();
    assert!(!shadow, "a control open never starts the search index rebuild");
}
