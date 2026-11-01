//! The asks view's large controls (Emmy): two roots of 2,000
//! replies each (a shared LIMIT 2000 would lose one) and a thread of 2,050 direct replies (over THREAD_SCAN). Thousands
//! of signed writes into a memory store: minutes in a debug build, so they are not in the bounded CI run; they are the
//! REQUIRED release-mode verification, run by scripts/verify-asks-heavy.sh, which keeps the output as the artifact:
//!     cargo test --release -p oracle-server --test asks_heavy -- --ignored
//! The helpers are those of tests/asks.rs.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::asks::{asks_for_with, thread_members_batched, StateFilter, Walk};
use oracle_server::store::{Principal, Scope, Store, StoreError};

const NOW: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn key_of(who: &str) -> u8 {
    match who {
        "blaze" => 1,
        "rocky" => 2,
        "dinesh" => 3,
        _ => 4,
    }
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 10 * DAY).unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3), ("bobby-core", 4)] {
        let ev = KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), NOW - 10 * DAY);
        s.apply_key_event(&ev, NOW - 10 * DAY).unwrap();
    }
    s
}
fn tags(t: &[&str]) -> Vec<String> {
    t.iter().map(|x| x.to_string()).collect()
}
fn try_write(s: &Store, (room, who): (&str, &str), kind: Kind, text: &str, cargo: Option<&str>, t: &[&str], ago: i64) -> Result<String, StoreError> {
    let at = NOW - ago;
    let env = Envelope::sign(&sk(key_of(who)), room, who, kind, text, cargo.map(str::to_owned), tags(t), at);
    s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).map(|r| r.id)
}
fn say(s: &Store, room: &str, who: &str, text: &str, t: &[&str], ago: i64) -> String {
    try_write(s, (room, who), Kind::Say, text, None, t, ago).unwrap()
}

fn members_equal(s: &Store, roots: &[String]) {
    let batched = thread_members_batched(s, roots, 300).unwrap();
    for r in roots {
        let per_root: Vec<String> = s.thread_members(r, 300).unwrap().iter().map(|o| o.id.clone()).collect();
        let b: Vec<String> = batched[r].iter().map(|o| o.id.clone()).collect();
        assert_eq!(b, per_root, "root {r}: same members, same order");
    }
}

fn views_equal(s: &Store, seat: &str) -> (u64, u64) {
    let l0 = Store::loads();
    let a = asks_for_with(s, seat, None, StateFilter::All, NOW, Walk::PerRoot).unwrap();
    let l1 = Store::loads();
    let b = asks_for_with(s, seat, None, StateFilter::All, NOW, Walk::Batched).unwrap();
    let l2 = Store::loads();
    assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap(), "the whole view, serialized");
    (l1 - l0, l2 - l1)
}

#[test]
#[ignore = "release-mode verification: scripts/verify-asks-heavy.sh (thousands of signed writes)"]
fn two_roots_of_two_thousand_replies_each_keep_their_own_members_under_the_batched_walk() {
    let s = oracle();
    let a = say(&s, "room", "blaze", "root A, an ask", &["ask:reply", "to:rocky"], 10 * DAY);
    let b = say(&s, "room", "blaze", "root B, an ask", &["ask:reply", "to:rocky"], 10 * DAY - 1);
    for i in 0..2000 {
        say(&s, "room", "blaze", &format!("reply {i} to A"), &[&format!("reply:{a}")], 9 * DAY - i);
        say(&s, "room", "blaze", &format!("reply {i} to B"), &[&format!("reply:{b}")], 9 * DAY - i - 2000);
    }
    members_equal(&s, &[a.clone(), b.clone()]);
    let batched = thread_members_batched(&s, &[a.clone(), b.clone()], 300).unwrap();
    assert_eq!(batched[&a].len(), 300, "A keeps its newest 300 of 2,000");
    assert_eq!(batched[&b].len(), 300, "B keeps its newest 300 of 2,000; a shared LIMIT 2000 would have lost A");
    let (per_root, batched_loads) = views_equal(&s, "rocky");
    assert!(batched_loads < per_root, "fewer row loads: per-root {per_root}, batched {batched_loads}");
    println!("two roots of 2,000: equal; row loads per-root {per_root}, batched {batched_loads}");
}

#[test]
#[ignore = "release-mode verification: scripts/verify-asks-heavy.sh (thousands of signed writes)"]
fn an_over_cap_thread_of_2050_replies_walks_the_same_and_keeps_the_newest_300() {
    let s = oracle();
    let big = say(&s, "room", "blaze", "big root", &["ask:reply", "to:rocky"], 8 * DAY);
    for i in 0..2050 {
        say(&s, "room", "blaze", &format!("big reply {i}"), &[&format!("reply:{big}")], 7 * DAY - i);
    }
    members_equal(&s, std::slice::from_ref(&big));
    let batched = thread_members_batched(&s, std::slice::from_ref(&big), 300).unwrap();
    assert_eq!(batched[&big].len(), 300, "2,000 scanned, newest 300 kept");
    let (per_root, batched_loads) = views_equal(&s, "rocky");
    assert!(batched_loads < per_root, "fewer row loads: per-root {per_root}, batched {batched_loads}");
    println!("over cap 2,050: equal; row loads per-root {per_root}, batched {batched_loads}");
}
