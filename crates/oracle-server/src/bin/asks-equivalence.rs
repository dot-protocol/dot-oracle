//! asks-equivalence — the batched asks view against the per-root reference on a REAL store, read only, at one
//! fixed now_ms: every seat × {all, open, done} without a room, and the named seats × every room × {all, open,
//! done}. Per case the two walks run alternately, warm (reference, batched, reference, batched); the complete
//! serialized outputs are compared; per-view wall time (median, p95, max), row loads, rows fetched and the
//! process's peak resident memory are reported per walk (Emmy).
//! `cargo run --release --bin asks-equivalence -- <store.db> [seat,seat,...]` (a disposable copy, never the live store).

use oracle_server::asks::{asks_for_with, StateFilter, Walk};
use oracle_server::store::Store;
use std::time::Instant;

fn peak_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok())))
        .unwrap_or(0)
}

fn pct(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * q).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("store path");
    let deep: Vec<String> = args.get(2).map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_else(|| vec!["blaze".into(), "rocky".into(), "emmy".into()]);
    let s = Store::open(path).expect("open");
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let seats = s.seat_names().expect("seats");
    let rooms: Vec<String> = s.channels().expect("channels").into_iter().map(|(c, _, _, _)| c).collect();
    let filters = [StateFilter::All, StateFilter::Open, StateFilter::Done];
    let mut cases: Vec<(String, Option<String>, StateFilter)> = Vec::new();
    for seat in &seats {
        for f in filters {
            cases.push((seat.clone(), None, f));
        }
    }
    for seat in &deep {
        for room in &rooms {
            for f in filters {
                cases.push((seat.clone(), Some(room.clone()), f));
            }
        }
    }
    let (mut equal, mut differ) = (0usize, 0usize);
    let (mut t_ref, mut t_new): (Vec<f64>, Vec<f64>) = (Vec::new(), Vec::new());
    let (mut loads_ref, mut loads_new, mut rows_ref, mut rows_new) = (0u64, 0u64, 0u64, 0u64);
    let (mut peak_ref, mut peak_new) = (0u64, 0u64);
    for (seat, room, f) in &cases {
        let mut out: Vec<String> = Vec::new();
        for (round, walk) in [Walk::PerRoot, Walk::Batched, Walk::PerRoot, Walk::Batched].into_iter().enumerate() {
            let (l0, r0) = (Store::loads(), Store::rows_loaded());
            let t = Instant::now();
            let v = asks_for_with(&s, seat, room.as_deref(), *f, now_ms, walk).expect("view");
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let (l1, r1) = (Store::loads(), Store::rows_loaded());
            let peak = peak_rss_kb();
            if round >= 2 {
                // the warm pair counts; the first pair primes the page cache for both
                match walk {
                    Walk::PerRoot => { t_ref.push(ms); loads_ref += l1 - l0; rows_ref += r1 - r0; peak_ref = peak_ref.max(peak); }
                    Walk::Batched => { t_new.push(ms); loads_new += l1 - l0; rows_new += r1 - r0; peak_new = peak_new.max(peak); }
                }
            }
            out.push(serde_json::to_string(&v).unwrap());
        }
        if out[0] == out[1] && out[1] == out[2] && out[2] == out[3] {
            equal += 1;
        } else {
            differ += 1;
            println!("DIFFER seat={seat} room={room:?} filter={f:?} sizes {:?}", out.iter().map(String::len).collect::<Vec<_>>());
        }
    }
    println!("cases {} equal {equal} differ {differ} (each case: reference, batched, reference, batched; the warm pair measured)", cases.len());
    println!("per-view ms  reference: median {:.1} p95 {:.1} max {:.1}   batched: median {:.1} p95 {:.1} max {:.1}", pct(&mut t_ref, 0.5), pct(&mut t_ref, 0.95), pct(&mut t_ref, 1.0), pct(&mut t_new, 0.5), pct(&mut t_new, 0.95), pct(&mut t_new, 1.0));
    println!("total ms     reference {:.0}   batched {:.0}", t_ref.iter().sum::<f64>(), t_new.iter().sum::<f64>());
    println!("row loads    reference {loads_ref}   batched {loads_new}");
    println!("rows fetched reference {rows_ref}   batched {rows_new}");
    println!("peak RSS kB  after reference views {peak_ref}   after batched views {peak_new} (process-wide high-water mark, order: reference first)");
    if differ > 0 {
        std::process::exit(1);
    }
}
