//! By hand, on a COPY of the live ledger (never the live file):
//!   ORACLE_ASKS_DB=/cache/live2/oracle.db cargo test -p oracle-server --test asks_live -- --ignored --nocapture
use oracle_server::asks::{asks_for, StateFilter};
use oracle_server::store::Store;

#[test]
#[ignore]
fn who_is_waiting_on_whom_on_the_live_ledger() {
    let Ok(db) = std::env::var("ORACLE_ASKS_DB") else { return };
    let s = Store::open(&db).unwrap();
    let seat = std::env::var("ORACLE_ASKS_SEAT").unwrap_or_else(|_| "blaze".into());
    let a = asks_for(&s, &seat, None, StateFilter::All, oracle_server::http::now_ms()).unwrap();
    println!("{} waiting on {seat}, {} waiting on by {seat}", a.waiting_on_me.len(), a.im_waiting_on.len());
    for r in a.waiting_on_me.iter().take(30) {
        println!("  {} #{} by {} to {:?} {} {}", &r.id[r.id.len().saturating_sub(12)..], r.room, r.author, r.held_by, r.ask, r.card.chars().take(60).collect::<String>());
    }
}
