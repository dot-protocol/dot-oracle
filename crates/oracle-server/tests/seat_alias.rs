//! Card seat-alias-migration: a request addressed to a retired label (emmy-core) is answered by a signed reply from its seat (emmy), through one signed
//! record and no second signer. The record is `seat-alias/1`, a whole table (`label → seat`) posted in #oracle under the face `[Oracle aliases]` by an
//! owner-key seat, like `[Room members]`; the newest valid one is the table. The `/asks` fold and the room index honour it; the original addressee stays visible.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::alias;
use oracle_server::asks::{asks_for, StateFilter};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::Value;

const NOW: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn key_of(who: &str) -> u8 {
    match who {
        "blaze" => 1,
        "rocky" => 2,
        "emmy" => 3,
        "emmy-core" => 4,
        _ => 5,
    }
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 10 * HOUR).unwrap();
    for (h, k) in [("rocky", 2), ("emmy", 3), ("emmy-core", 4), ("kepler", 5)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 10 * HOUR), NOW - 10 * HOUR).unwrap();
    }
    s
}
fn put(s: &Store, room: &str, who: &str, text: &str, cargo: Option<&str>, tags: &[&str], ago: i64) -> String {
    let at = NOW - ago;
    let env = Envelope::sign(&sk(key_of(who)), room, who, Kind::Say, text, cargo.map(str::to_owned), tags.iter().map(|t| t.to_string()).collect(), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id
}
fn say(s: &Store, room: &str, who: &str, text: &str, tags: &[&str], ago: i64) -> String {
    put(s, room, who, text, None, tags, ago)
}
/// The alias record, as `oracle alias set` writes it.
fn record(s: &Store, who: &str, table: &[(&str, &str)], ago: i64) -> String {
    let t: alias::Aliases = table.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    put(s, alias::ROOM, who, &format!("{} {}", alias::FACE, table.iter().map(|(a, b)| format!("{a} → {b}")).collect::<Vec<_>>().join(", ")), Some(&alias::cargo(&t)), &["schema:seat-alias/1", "intent:decision"], ago)
}
fn ids(rows: &[oracle_server::asks::AskRow]) -> Vec<&str> {
    rows.iter().map(|r| r.id.as_str()).collect()
}

#[test]
fn without_an_alias_a_label_stays_a_label_and_its_asks_stay_open() {
    let s = oracle();
    let ask = say(&s, "core", "rocky", "review PR 12", &["ask:act", "to:emmy-core"], 5 * HOUR);
    say(&s, "core", "emmy", "reviewed: green", &[&format!("re:{ask}"), "intent:receipt"], 4 * HOUR);
    let old = asks_for(&s, "emmy-core", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&old.waiting_on_me), vec![ask.as_str()], "emmy is not emmy-core: her receipt does not close an ask to the other label");
    assert!(asks_for(&s, "emmy", None, StateFilter::All, NOW).unwrap().waiting_on_me.is_empty(), "and it is not emmy's ask either");
}

#[test]
fn an_alias_lets_her_signed_reply_close_the_old_ask_and_the_addressee_stays_visible() {
    let s = oracle();
    let acted = say(&s, "core", "rocky", "review PR 12", &["ask:act", "to:emmy-core"], 5 * HOUR);
    let reply = say(&s, "core", "rocky", "what is your view?", &["ask:reply", "to:emmy-core"], 5 * HOUR);
    let unanswered = say(&s, "core", "rocky", "and PR 13?", &["ask:act", "to:emmy-core"], 3 * HOUR);
    say(&s, "core", "emmy", "reviewed: green", &[&format!("re:{acted}"), "intent:receipt"], 4 * HOUR);
    say(&s, "core", "emmy", "my view: ship it", &[&format!("re:{reply}")], 4 * HOUR);
    record(&s, "blaze", &[("emmy-core", "emmy")], 2 * HOUR);
    // asked by the old label: the two she answered are done; the third is still open, and still says who it was addressed to
    let old = asks_for(&s, "emmy-core", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&old.waiting_on_me), vec![unanswered.as_str()], "only what she has not answered is open");
    let row = &old.waiting_on_me[0];
    assert_eq!(row.held_by, vec!["emmy-core".to_string()], "the original addressee is kept, not rewritten");
    assert_eq!(row.addressee_aliases.get("emmy-core").map(String::as_str), Some("emmy"), "and the alias that applied is shown beside it");
    assert_eq!(old.canonical.as_deref(), Some("emmy"), "the answer says which seat the label meant");
    // …and it is the same list for her real seat
    let now = asks_for(&s, "emmy", None, StateFilter::All, NOW).unwrap();
    assert_eq!(ids(&now.waiting_on_me), vec![unanswered.as_str()], "an ask to the retired label is waiting on her");
    assert!(now.canonical.is_none());
}

#[test]
fn a_record_that_is_not_the_owners_or_is_not_valid_changes_nothing() {
    let s = oracle();
    let ask = say(&s, "core", "rocky", "review PR 12", &["ask:act", "to:emmy-core"], 5 * HOUR);
    say(&s, "core", "emmy", "reviewed", &[&format!("re:{ask}"), "intent:receipt"], 4 * HOUR);
    let open = |s: &Store| asks_for(s, "emmy-core", None, StateFilter::All, NOW).unwrap().waiting_on_me.len();
    record(&s, "rocky", &[("emmy-core", "emmy")], 3 * HOUR);
    assert_eq!(open(&s), 1, "a record from a seat that is not an owner-key seat is ignored");
    // owner-signed but not a table anyone can honour: a chain, a self-alias, a label that is a target, a bad schema, junk
    for bad in [&[("a", "b"), ("b", "c")][..], &[("emmy", "emmy")][..], &[("Emmy Core", "emmy")][..], &[("emmy-core", "")][..]] {
        record(&s, "blaze", bad, 2 * HOUR);
        assert_eq!(open(&s), 1, "{bad:?}");
    }
    put(&s, alias::ROOM, "blaze", &format!("{} junk", alias::FACE), Some("not json"), &[], 2 * HOUR);
    put(&s, alias::ROOM, "blaze", &format!("{} other schema", alias::FACE), Some(r#"{"schema":"room-members/1","aliases":{"emmy-core":"emmy"}}"#), &[], 2 * HOUR);
    assert_eq!(open(&s), 1);
    // the owner's valid record after all that is honoured
    record(&s, "blaze", &[("emmy-core", "emmy")], HOUR);
    assert_eq!(open(&s), 0);
}

#[test]
fn the_newest_valid_record_is_the_whole_table_so_removing_an_alias_reopens_the_ask() {
    let s = oracle();
    let ask = say(&s, "core", "rocky", "review PR 12", &["ask:act", "to:emmy-core"], 5 * HOUR);
    say(&s, "core", "emmy", "reviewed", &[&format!("re:{ask}"), "intent:receipt"], 4 * HOUR);
    let open = || asks_for(&s, "emmy-core", None, StateFilter::All, NOW).unwrap().waiting_on_me.len();
    record(&s, "blaze", &[("emmy-core", "emmy")], 3 * HOUR);
    assert_eq!(open(), 0);
    record(&s, "blaze", &[("kepler-old", "kepler")], 2 * HOUR);
    assert_eq!(open(), 1, "a newer record without it: the label is a label again, and nothing was rewritten");
    assert_eq!(s.seat_aliases().get("kepler-old").map(String::as_str), Some("kepler"));
    assert!(!s.seat_aliases().contains_key("emmy-core"));
}

#[test]
fn a_decider_named_by_the_old_label_settles_through_her_new_one_in_the_room_index() {
    let s = oracle();
    // a proposal to emmy-core; emmy (the seat) says go: an opinion until the alias says she is emmy-core
    let p = say(&s, "core", "rocky", "let us ship friday", &["intent:proposal", "ask:reply", "to:emmy-core"], 5 * HOUR);
    say(&s, "core", "emmy", "✓", &[&format!("re:{p}"), "react:go"], 4 * HOUR);
    let word = |s: &Store, id: &str| -> String { let n = s.get(id).unwrap().seq; s.with_conn(|c| Ok(c.query_row("SELECT state FROM thread WHERE root_seq=?1", [n], |r| r.get::<_, String>(0)).unwrap())).unwrap() };
    assert_eq!(word(&s, &p), "open", "before the alias her go is an opinion: a mark is not a reply, and she is not the seat it was addressed to");
    record(&s, "blaze", &[("emmy-core", "emmy")], 3 * HOUR);
    assert_eq!(word(&s, &p), "settled", "writing the alias re-judges the threads that name the label, without a reindex");
    let before = s.index_dump().unwrap();
    s.reindex().unwrap();
    assert_eq!(s.index_dump().unwrap(), before, "and reindex leaves exactly that");
    assert_eq!(s.seat_aliases().len(), 1);
}

#[test]
fn a_table_is_validated_the_same_way_by_the_cli_and_the_reader() {
    let ok = |pairs: &[(&str, &str)]| alias::parse(&alias::cargo(&pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect())).map(|t| t.len());
    assert_eq!(ok(&[("emmy-core", "emmy"), ("kepler-old", "kepler")]), Ok(2));
    assert!(ok(&[("a", "b"), ("b", "c")]).unwrap_err().contains("chain"));
    assert!(ok(&[("emmy", "emmy")]).unwrap_err().contains("itself"));
    assert!(ok(&[("EMMY", "emmy")]).unwrap_err().contains("handle"));
    assert!(alias::parse("not json").is_err() && alias::parse(r#"{"schema":"x/1","aliases":{}}"#).is_err());
    let t: alias::Aliases = [("emmy-core".to_string(), "emmy".to_string())].into();
    assert_eq!((alias::canon(&t, "emmy-core"), alias::canon(&t, "emmy"), alias::canon(&t, "kepler")), ("emmy", "emmy", "kepler"));
    assert!(alias::same(&t, "emmy-core", "emmy") && !alias::same(&t, "emmy-core", "kepler"));
    let _: Value = serde_json::from_str(&alias::cargo(&t)).unwrap();
}

#[test]
fn a_seat_reading_the_lists_of_its_own_old_label_is_reading_its_own() {
    let s = oracle();
    let read = |caller: &str, target: &str| s.cross_seat(caller, target, "asks", true, NOW);
    assert!(read("emmy", "emmy-core").is_err(), "before the alias they are two seats");
    record(&s, "blaze", &[("emmy-core", "emmy")], HOUR);
    read("emmy", "emmy-core").unwrap();
    read("emmy-core", "emmy").unwrap();
    assert!(read("kepler", "emmy-core").is_err() && read("rocky", "emmy").is_err(), "and nobody else gets in by it");
}
