//! Cost of Room State per revision count, measured, not assumed (Bobby's 15:26 numbers: writes ~3x slower and a State read ~240 KB at 113 revisions).
//! `cargo test -p oracle-server --test room_context_cost -- --ignored --nocapture`
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::json;
use std::time::Instant;

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

#[test]
#[ignore]
fn what_a_revision_costs_as_the_room_grows() {
    let sk = |n: u8| SigningKey::from_bytes(&[n; 32]);
    let path = std::env::temp_dir().join(format!("oracle-cost-{}.db", std::process::id()));
    let s = Store::open(path.to_str().unwrap()).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 5000 * MIN).unwrap();
    s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, "rocky", &pubkey_hex(&sk(2)), NOW - 5000 * MIN), NOW - 5000 * MIN).unwrap();
    let who = Principal { handle: "rocky".into(), scope: Scope::Write };
    let mut head: Option<String> = None;
    let mut other_ms = 0u128;
    for n in 1..=300usize {
        let parents: Vec<String> = head.iter().cloned().collect();
        let cargo = json!({"schema": "room-context/1", "channel": "r", "parents": parents, "fields": {"purpose": "p".repeat(200), "current": format!("revision {n} {}", "x".repeat(300)), "goal": "g".repeat(200)}}).to_string();
        let at = NOW + n as i64 * MIN;
        let e = Envelope::sign(&sk(2), "r", "rocky", Kind::Say, "State updated", Some(cargo), vec!["room-context:v1".into()], at);
        let t = Instant::now();
        head = Some(s.write(&who, &e, at).unwrap().id);
        let write_ms = t.elapsed().as_micros();
        // an unguarded write of the same size for comparison (a plain say)
        let plain = Envelope::sign(&sk(2), "other", "rocky", Kind::Say, "plain", Some("{}".into()), vec![], at);
        let t = Instant::now();
        s.write(&who, &plain, at).unwrap();
        other_ms = t.elapsed().as_micros();
        if [1, 10, 50, 100, 150, 200, 300].contains(&n) {
            let t = Instant::now();
            let v = s.room_state_at("r", None).unwrap();
            let read_us = t.elapsed().as_micros();
            let bytes = v.to_string().len();
            println!("n={n:3}  guarded write {write_ms:6} us  plain write {other_ms:5} us  room_state_at {read_us:7} us  answer {bytes:7} bytes ({} per revision)", bytes / n);
        }
    }
    let _ = other_ms;
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
}
