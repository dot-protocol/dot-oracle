//! Following `next_before` from /search reaches every listed row exactly once (Emmy, 2026-10-03: a filled page
//! handed back the cursor of the last row SCANNED, so the rows between it and the last row RETURNED were lost).

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::http::recent_rows;
use oracle_server::projection::View;
use oracle_server::store::{Principal, Scope, Store};
use std::collections::HashSet;

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
fn put(s: &Store, kind: Kind, text: &str, tags: &[String], mins: i64) -> String {
    let at = NOW + mins * MIN;
    let env = Envelope::sign(&sk(2), "core", "rocky", kind, text, None, tags.to_vec(), at);
    s.write(&Principal { handle: "rocky".into(), scope: Scope::Write }, &env, at).unwrap().id
}

#[test]
fn following_next_before_returns_every_listed_row_once_even_when_updates_are_interleaved() {
    let s = oracle();
    let mut said = Vec::new();
    for i in 0..40 {
        let id = put(&s, Kind::Say, &format!("message {i}"), &[], i * 2);
        // every other message gets a state update: judged, never listed, so pages fill mid-batch
        if i % 2 == 0 {
            put(&s, Kind::Annotate, "noted", &[format!("annotates:{id}"), "set:intent=result".into()], i * 2 + 1);
        }
        said.push(id);
    }
    let view = View::parse(None, None);
    let (mut seen, mut before, mut pages) = (Vec::new(), None, 0);
    loop {
        let (rows, _scanned, next) = recent_rows(&s, Some("core"), 7, before, None, None, &view, NOW + 1000 * MIN).unwrap();
        seen.extend(rows.iter().map(|r| r["id"].as_str().unwrap().to_string()));
        pages += 1;
        assert!(pages < 50, "the chain ends");
        match next {
            Some(b) if !rows.is_empty() => before = Some(b),
            _ => break,
        }
    }
    let unique: HashSet<_> = seen.iter().collect();
    assert_eq!(unique.len(), seen.len(), "nothing twice");
    let mut want = said.clone();
    want.reverse();
    assert_eq!(seen, want, "every message, newest first, nothing missing");
}
