//! oracle-bench — load a realistic corpus with REAL signed writes, then measure every
//! read path. Reproducible: `cargo run --release --bin oracle-bench -- --n 200000`.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::store::{Principal, Scope, Store};
use std::time::Instant;

const WORDS: &[&str] = &[
    "oracle",
    "fold",
    "signed",
    "memory",
    "agent",
    "room",
    "notifier",
    "latency",
    "index",
    "sqlite",
    "deploy",
    "release",
    "cursor",
    "registry",
    "token",
    "search",
    "vector",
    "graph",
    "decision",
    "lesson",
    "rollback",
    "migration",
    "benchmark",
    "envelope",
    "nonce",
    "replay",
    "stream",
    "health",
    "queue",
    "outbox",
];

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

fn measure(name: &str, iters: usize, mut f: impl FnMut(usize)) {
    let mut t: Vec<f64> = (0..iters)
        .map(|i| {
            let s = Instant::now();
            f(i);
            s.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "| {name:<34} | {:>8.2} | {:>8.2} | {:>8.2} |",
        percentile(&t, 0.5),
        percentile(&t, 0.99),
        t[t.len() - 1]
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args
        .iter()
        .position(|a| a == "--n")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let path = std::env::temp_dir().join(format!("oracle-bench-{}.db", std::process::id()));
    let store = Store::open(path.to_str().unwrap()).unwrap();

    let now = 1_800_000_000_000i64;
    let owner = SigningKey::from_bytes(&[1; 32]);
    store.init_owner("owner", &pubkey_hex(&owner), now).unwrap();
    let seats: Vec<(String, SigningKey)> = (0..8u8)
        .map(|i| (format!("agent{i}"), SigningKey::from_bytes(&[i + 10; 32])))
        .collect();
    for (h, k) in &seats {
        store
            .apply_key_event(
                &KeyEvent::sign(&owner, "owner", KeyAction::Add, h, &pubkey_hex(k), now),
                now,
            )
            .unwrap();
    }

    let channels = 40usize;
    let start = Instant::now();
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for i in 0..n {
        let (h, k) = &seats[(next() % 8) as usize];
        // Zipf-ish: a few loud rooms, many quiet ones.
        let ch = format!(
            "room{}",
            ((next() % 1000) as f64 / 1000.0).powi(3) * channels as f64
        );
        let ch = ch.split('.').next().unwrap().to_string();
        let len = 8 + (next() % 40) as usize;
        let mut text: Vec<String> = (0..len)
            .map(|_| WORDS[(next() % WORDS.len() as u64) as usize].to_string())
            .collect();
        if next() % 10 == 0 {
            text.push(format!("@agent{}", next() % 8));
        }
        let at = now + i as i64 * 50;
        let env = Envelope::sign(
            k,
            &ch,
            h,
            Kind::Say,
            &text.join(" "),
            None,
            vec![format!("topic:{}", next() % 200)],
            at,
        );
        store
            .write(
                &Principal {
                    handle: h.clone(),
                    scope: Scope::Write,
                },
                &env,
                at,
            )
            .unwrap();
    }
    let load = start.elapsed().as_secs_f64();
    let end = now + n as i64 * 50;
    let size_mb = std::fs::metadata(&path)
        .map(|m| m.len() as f64 / 1e6)
        .unwrap_or(0.0);
    println!("corpus: {n} signed observations, {channels} rooms, 8 seats — loaded in {load:.1}s ({:.0} verified writes/s), {size_mb:.0} MB on disk\n", n as f64 / load);
    println!("| operation                          |  p50 ms  |  p99 ms  |  max ms  |");
    println!("|------------------------------------|----------|----------|----------|");

    let me = Principal {
        handle: "agent0".into(),
        scope: Scope::Write,
    };
    let k0 = &seats[0].1;
    measure("say (verify + store + index)", 2000, |i| {
        let env = Envelope::sign(
            k0,
            "room1",
            "agent0",
            Kind::Say,
            "benchmark write with a few words",
            None,
            vec![],
            end + i as i64,
        );
        store.write(&me, &env, end + i as i64).unwrap();
    });
    measure("recent, one room (20)", 2000, |i| {
        store
            .recent(Some(&format!("room{}", i % channels)), 20, None)
            .unwrap();
    });
    measure("recent, all rooms (20)", 2000, |_| {
        store.recent(None, 20, None).unwrap();
    });
    measure("recent, deep page (keyset)", 2000, |i| {
        store
            .recent(None, 20, Some((n as i64 / 2) - i as i64))
            .unwrap();
    });
    // A caught-up agent: acked to the head of every room, then one new post per fold.
    let heads: std::collections::BTreeMap<String, i64> = (0..channels)
        .map(|c| (format!("room{c}"), i64::MAX / 2))
        .collect();
    store.ack("agent1", &heads, true).unwrap();
    let caught_up: std::collections::BTreeMap<String, i64> = store
        .recent(None, 1, None)
        .unwrap()
        .first()
        .map(|o| (0..channels).map(|c| (format!("room{c}"), o.seq)).collect())
        .unwrap_or_default();
    store.ack("agent1", &caught_up, true).unwrap();
    let k2 = &seats[2].1;
    let me2 = Principal {
        handle: "agent2".into(),
        scope: Scope::Write,
    };
    measure("fold + ack, caught-up agent", 2000, |i| {
        let at = end + 5_000 + i as i64;
        let env = Envelope::sign(
            k2,
            "room2",
            "agent2",
            Kind::Say,
            "one new message @agent1",
            None,
            vec![],
            at,
        );
        store.write(&me2, &env, at).unwrap();
        let f = store.fold("agent1", 50, at).unwrap();
        assert_eq!(f.returned, 1);
        store.ack("agent1", &f.next_marks, false).unwrap();
    });
    measure("fold, agent 175k msgs behind (50)", 500, |_| {
        store.fold("agent7", 50, end + 10_000).unwrap();
    });
    measure("tag lookup (50)", 2000, |i| {
        store.by_tag(&format!("topic:{}", i % 200), 50).unwrap();
    });
    measure("mentions of me (50)", 2000, |_| {
        store.by_tag("mention:agent3", 50).unwrap();
    });
    measure("search, 2 terms, all rooms (10)", 500, |i| {
        store
            .search(
                &format!("{} {}", WORDS[i % 30], WORDS[(i * 7) % 30]),
                None,
                None,
                10,
            )
            .unwrap();
    });
    measure("search, scoped to a room (10)", 500, |i| {
        store
            .search(WORDS[i % 30], Some("room3"), None, 10)
            .unwrap();
    });
    measure("get by id", 2000, |i| {
        let r = store
            .recent(None, 1, Some(1 + (i as i64 * 37) % n as i64 + 1))
            .unwrap();
        if let Some(o) = r.first() {
            store.get(&o.id).unwrap();
        }
    });
    // PUSH-COST: exercise the existing replay implementation, including JSON
    // serialization. This synthetic sample is not a production day forecast.
    measure("feed + JSON, all rooms (1000)", 100, |_| {
        let rows = store.after_seq(None, 0, 1000).unwrap();
        serde_json::to_vec(&rows).unwrap();
    });
    let rooms = vec!["room3".to_string(), "room7".to_string()];
    measure("feed + JSON, two rooms (1000)", 100, |_| {
        let rows = store.after_seq(Some(&rooms), 0, 1000).unwrap();
        serde_json::to_vec(&rows).unwrap();
    });
    let sample = store.after_seq(None, 0, 1000).unwrap();
    let mut sizes: Vec<usize> = sample.iter().map(|r| serde_json::to_vec(r).unwrap().len()).collect();
    sizes.sort_unstable();
    let c = rusqlite::Connection::open(&path).unwrap();
    let plans: Vec<String> = [
        "EXPLAIN QUERY PLAN SELECT * FROM observations WHERE seq>100 ORDER BY seq LIMIT 1000",
        "EXPLAIN QUERY PLAN SELECT * FROM observations WHERE channel='room3' AND seq>100 ORDER BY seq LIMIT 1000",
    ].iter().flat_map(|sql| {
        c.prepare(sql).unwrap().query_map([], |r| r.get::<_, String>(3)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }).collect();
    if !sizes.is_empty() {
        println!("{}", serde_json::json!({"schema":"oracle-feed-cost/1","corpus":n,"sample":sizes.len(),
            "sample_json_bytes":serde_json::to_vec(&sample).unwrap().len(),"record_bytes_p50":sizes[sizes.len()/2],
            "record_bytes_p95":sizes[(sizes.len()-1)*95/100],"record_bytes_max":sizes.last(),"query_plans":plans,
            "scope":"synthetic Store replay and JSON only; excludes HTTP, ACL filtering, TLS and production traffic"}));
    }
    drop(c);
    drop(store);
    let _ = std::fs::remove_file(&path);
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
}
