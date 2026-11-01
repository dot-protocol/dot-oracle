//! The experiment registry (`Store::experiments`, GET /experiments): every exp.* head in every room with its data.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};

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
fn head(s: &Store, room: &str, subject: &str, data: &str, mins: i64) {
    let at = NOW + mins * MIN;
    let cargo = format!(r#"{{"subject":"{subject}","state":"open","owner":"rocky","next_touch":"2026-12-01","data":{data}}}"#);
    let env = Envelope::sign(&sk(2), room, "rocky", Kind::State, "an experiment", Some(cargo), vec![format!("subject:{subject}")], at);
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap();
}

#[test]
fn the_registry_holds_every_exp_head_in_every_room_with_its_data_newest_first_and_nothing_else() {
    let s = oracle();
    head(&s, "mathematics-lab", "exp.qsb.a", r#"{"title":"QSB A","stage":"measured","result":"0 mismatches"}"#, 1);
    head(&s, "hustler", "exp.hustler.fill", r#"{"title":"Fill audit","stage":"proposed"}"#, 2);
    head(&s, "core", "core.ship", r#"{"title":"not an experiment"}"#, 3);
    let all = s.experiments().unwrap();
    let subjects: Vec<&str> = all.iter().map(|e| e["subject"].as_str().unwrap()).collect();
    assert_eq!(subjects, ["exp.hustler.fill", "exp.qsb.a"]);
    assert_eq!(all[1]["room"], "mathematics-lab");
    assert_eq!(all[1]["data"]["result"], "0 mismatches");
    // a newer revision replaces the head: still one entry, the newest data
    head(&s, "mathematics-lab", "exp.qsb.a", r#"{"title":"QSB A","stage":"killed","result":"assumption failed"}"#, 4);
    let all = s.experiments().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["data"]["stage"], "killed");
}
