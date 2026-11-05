#![forbid(unsafe_code)]

//! asks-phases — where the asks view's time goes, phase by phase, per walk, on a REAL store (a disposable copy,
//! never the live store), read only, at one fixed now_ms. Cases: the named seats × {all, open, done} without a
//! room, and the named seats × the named rooms × {all, open, done}. Per case both walks run once to warm the page
//! cache, then once measured (reference, batched); the measured ledgers are printed per case and summed
//! (Emmy: candidate reads, parent walk, member/resolver walk, judgement,
//! serialization; indexing and row cloning apart from decoding).
//! `cargo run --release --bin asks-phases -- <store.db> [seat,seat,...] [room,room,...]` (an empty rooms argument:
//! no rooms). With `ASKS_PHASES_FIRST=1` each case runs reference then batched once and that first pass is what is
//! printed: the process is fresh (an empty SQLite page cache) and the OS page cache is whatever the driver left.

use oracle_server::asks::{asks_for_timed, Phases, StateFilter, Walk};
use oracle_server::store::Store;
use std::collections::BTreeSet;
use std::time::Instant;

fn table(label: &str, r: &Phases, b: &Phases) {
    let names: BTreeSet<&&str> = r.ms.keys().chain(b.ms.keys()).collect();
    println!("{label}");
    println!("  {:<22} {:>10} {:>10}   {:>9} {:>9}   {:>10} {:>10}", "phase", "ref ms", "bat ms", "ref loads", "bat loads", "ref rows", "bat rows");
    for n in names {
        println!(
            "  {:<22} {:>10.1} {:>10.1}   {:>9} {:>9}   {:>10} {:>10}",
            n,
            r.ms.get(*n).copied().unwrap_or(0.0),
            b.ms.get(*n).copied().unwrap_or(0.0),
            r.loads.get(*n).copied().unwrap_or(0),
            b.loads.get(*n).copied().unwrap_or(0),
            r.rows.get(*n).copied().unwrap_or(0),
            b.rows.get(*n).copied().unwrap_or(0)
        );
    }
    println!("  {:<22} {:>10.1} {:>10.1}   {:>9} {:>9}   {:>10} {:>10}", "total", r.total_ms(), b.total_ms(), r.loads.values().sum::<u64>(), b.loads.values().sum::<u64>(), r.rows.values().sum::<u64>(), b.rows.values().sum::<u64>());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("store path");
    let list = |i: usize, default: &[&str]| -> Vec<String> { args.get(i).map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_else(|| default.iter().map(|s| s.to_string()).collect()) };
    let seats = list(2, &["blaze", "rocky", "emmy"]);
    let rooms: Vec<String> = list(3, &["illuminati", "axxis", "dot-mail", "hustler", "dot-protocol", "oracle", "room"]).into_iter().filter(|r| !r.is_empty()).collect();
    let first = std::env::var("ASKS_PHASES_FIRST").is_ok_and(|v| v == "1");
    let rounds: &[Walk] = if first { &[Walk::PerRoot, Walk::Batched] } else { &[Walk::PerRoot, Walk::Batched, Walk::PerRoot, Walk::Batched] };
    let s = Store::open(path).expect("open");
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let filters = [StateFilter::All, StateFilter::Open, StateFilter::Done];
    let mut cases: Vec<(String, Option<String>)> = seats.iter().map(|s| (s.clone(), None)).collect();
    for seat in &seats {
        for room in &rooms {
            cases.push((seat.clone(), Some(room.clone())));
        }
    }
    let (mut sum_r, mut sum_b) = (Phases::default(), Phases::default());
    let (mut equal, mut differ) = (0usize, 0usize);
    for (seat, room) in &cases {
        for f in filters {
            let mut out: Vec<String> = Vec::new();
            let mut measured: Vec<Phases> = Vec::new();
            for (round, walk) in rounds.iter().copied().enumerate() {
                let mut ph = Phases::default();
                let v = asks_for_timed(&s, seat, room.as_deref(), f, now_ms, walk, &mut ph).expect("view");
                let t = Instant::now();
                let json = serde_json::to_string(&v).unwrap();
                *ph.ms.entry("serialize").or_default() += t.elapsed().as_secs_f64() * 1000.0;
                out.push(json);
                if round + 2 >= rounds.len() {
                    measured.push(ph);
                }
            }
            if out.iter().all(|o| o == &out[0]) {
                equal += 1;
            } else {
                differ += 1;
                println!("DIFFER seat={seat} room={room:?} filter={f:?}");
            }
            let (r, b) = (&measured[0], &measured[1]);
            table(&format!("seat={seat} room={} filter={f:?} ({})", room.as_deref().unwrap_or("-"), if first { "first pass, process fresh" } else { "warm, measured once each" }), r, b);
            sum_r.add(r);
            sum_b.add(b);
        }
    }
    println!();
    table(&format!("SUM over {} cases ({} equal, {differ} differ)", cases.len() * filters.len(), equal), &sum_r, &sum_b);
    if differ > 0 {
        std::process::exit(1);
    }
}
