//! LongMemEval-S retrieval on Oracle (evals/longmemeval/README.md): for every question, a fresh in-memory Oracle holds that question's
//! haystack as signed observations, the question is searched in it, and the hit counts when a session that holds the answer is among
//! the first k sessions returned: session-level retrieval, the unit ai-memory publishes as hit@5.
//!
//!   cargo run --release -p oracle-server --example longmemeval -- <dataset.json> [--arm turns|sessions] [--ranker rrf|keyword|andtop]
//!       [--limit N] [--type <question_type>] [--control] [--debug] [--per-question <out.jsonl>]
//!
//! `turns`: one observation per turn (user and assistant), sessions ranked by their best turn. `sessions`: one observation per whole
//! session. A body over Oracle's 64 KiB content limit is written as consecutive parts of the same session, never skipped; any write
//! the store refuses ends the run with an error, so a receipt always covers the whole haystack (Emmy's review of 754f4ba). Unknown
//! flags and bad values stop the run before it starts. Abstention questions (`_abs`) have no answer session and are left out (470 of
//! 500). The receipt names the input file and its sha256. No model is called and nothing leaves the machine: words only.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind, MAX_CONTENT_BYTES};
use oracle_server::store::{Principal, Ranker, Scope, Store};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::time::Instant;

const USAGE: &str = "usage: longmemeval <dataset.json> [--arm turns|sessions] [--ranker rrf|keyword|andtop] [--limit N] [--type T] [--control] [--debug] [--per-question out.jsonl]";

fn fail(msg: &str) -> ! {
    eprintln!("longmemeval: {msg}\n{USAGE}");
    std::process::exit(2);
}

/// `body` cut into parts under the content limit, each on a char boundary, together covering every byte.
fn parts(body: &str) -> Vec<String> {
    let max = MAX_CONTENT_BYTES - 1024;
    let mut out = Vec::new();
    let mut rest = body;
    while rest.len() > max {
        let mut cut = max;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    out.push(rest.to_string());
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().filter(|a| !a.starts_with("--")).cloned().unwrap_or_else(|| fail("the dataset path comes first"));
    // Every flag is known and every value valid, or the run does not start: a typo must never produce a receipt.
    const VALUED: [&str; 5] = ["--arm", "--ranker", "--limit", "--type", "--per-question"];
    const BARE: [&str; 2] = ["--control", "--debug"];
    let mut i = 1;
    let mut val: BTreeMap<&str, String> = BTreeMap::new();
    let mut flags: HashSet<&str> = HashSet::new();
    while i < args.len() {
        let a = args[i].as_str();
        if let Some(k) = VALUED.iter().find(|k| **k == a) {
            let v = args.get(i + 1).filter(|v| !v.starts_with("--")).unwrap_or_else(|| fail(&format!("{a} needs a value")));
            val.insert(k, v.clone());
            i += 2;
        } else if let Some(k) = BARE.iter().find(|k| **k == a) {
            flags.insert(k);
            i += 1;
        } else {
            fail(&format!("unknown argument {a:?}"));
        }
    }
    let arm = val.get("--arm").cloned().unwrap_or_else(|| "turns".into());
    if arm != "turns" && arm != "sessions" {
        fail(&format!("--arm is turns or sessions, not {arm:?}"));
    }
    let ranker_name = val.get("--ranker").cloned().unwrap_or_else(|| "rrf".into());
    if !["rrf", "keyword", "andtop"].contains(&ranker_name.as_str()) {
        fail(&format!("--ranker is rrf, keyword or andtop, not {ranker_name:?}"));
    }
    let limit: usize = match val.get("--limit") {
        Some(n) => n.parse().unwrap_or_else(|_| fail(&format!("--limit is a number, not {n:?}"))),
        None => usize::MAX,
    };
    let only_type = val.get("--type").cloned();
    let (control, debug) = (flags.contains("--control"), flags.contains("--debug"));

    let raw = std::fs::read(&path).unwrap_or_else(|e| fail(&format!("cannot read {path}: {e}")));
    let input_sha256 = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&raw));
    let data: Vec<Value> = serde_json::from_slice(&raw).unwrap_or_else(|e| fail(&format!("{path} is not a LongMemEval JSON list: {e}")));
    let mut per_q_out = val.get("--per-question").map(|p| std::fs::File::create(p).unwrap_or_else(|e| fail(&format!("cannot write {p}: {e}"))));

    let (owner, seat) = (SigningKey::from_bytes(&[1; 32]), SigningKey::from_bytes(&[2; 32]));
    let mut per_type: BTreeMap<String, [usize; 4]> = BTreeMap::new(); // type -> [n, hit1, hit5, hit10]
    let (mut n, mut h1, mut h5, mut h10, mut rr) = (0usize, 0usize, 0usize, 0usize, 0f64);
    // The accounting closes: every body is written (in one part or several) or counted empty, so nothing is dropped unseen.
    let (mut sessions_in, mut records_in, mut split_bodies, mut empty_bodies) = (0usize, 0usize, 0usize, 0usize);
    let mut lat: Vec<u128> = Vec::new();
    let started = Instant::now();
    let ranker = match ranker_name.as_str() { "keyword" => Ranker::Keyword, "andtop" => Ranker::AndTop, _ => Ranker::Rrf };
    for q in data
        .iter()
        .filter(|q| !q["question_id"].as_str().unwrap_or("").ends_with("_abs"))
        .filter(|q| only_type.as_deref().is_none_or(|t| q["question_type"].as_str() == Some(t)))
        .take(limit)
    {
        let qid = q["question_id"].as_str().unwrap_or("?").to_string();
        let store = Store::memory().unwrap();
        store.init_owner("blaze", &pubkey_hex(&owner), 1).unwrap();
        store.apply_key_event(&KeyEvent::sign(&owner, "blaze", KeyAction::Add, "bench", &pubkey_hex(&seat), 2), 2).unwrap();
        let who = Principal { handle: "bench".into(), scope: Scope::Write };
        let mut at = 10i64;
        let mut session_of: BTreeMap<String, String> = BTreeMap::new(); // observation id -> session id
        let ids = q["haystack_session_ids"].as_array().unwrap_or_else(|| fail(&format!("{qid}: no haystack_session_ids")));
        let sessions = q["haystack_sessions"].as_array().unwrap_or_else(|| fail(&format!("{qid}: no haystack_sessions")));
        if ids.len() != sessions.len() {
            fail(&format!("{qid}: {} session ids for {} sessions", ids.len(), sessions.len()));
        }
        for (si, sess) in sessions.iter().enumerate() {
            let sid = ids[si].as_str().unwrap().to_string();
            let turns: Vec<String> = sess.as_array().unwrap().iter()
                .map(|t| format!("{}: {}", t["role"].as_str().unwrap_or(""), t["content"].as_str().unwrap_or("")))
                .collect();
            let bodies: Vec<String> = if arm == "sessions" { vec![turns.join("\n")] } else { turns };
            sessions_in += 1;
            for body in bodies {
                if body.trim().is_empty() {
                    empty_bodies += 1;
                    continue;
                }
                let pieces = parts(&body);
                if pieces.len() > 1 {
                    split_bodies += 1;
                }
                for piece in pieces {
                    at += 1;
                    let env = Envelope::sign(&seat, "haystack", "bench", Kind::Say, &piece, None, vec![format!("session:{sid}")], at);
                    match store.write(&who, &env, at) {
                        Ok(r) => {
                            session_of.insert(r.id, sid.clone());
                            records_in += 1;
                        }
                        Err(e) => {
                            eprintln!("longmemeval: {qid}: the store refused a record of session {sid} ({e}); the receipt would not cover the haystack");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        let real: HashSet<&str> = q["answer_session_ids"].as_array().unwrap().iter().filter_map(|s| s.as_str()).collect();
        // --control: score against sessions that do NOT hold the answer (as many as the real ones, picked by a fixed stride), so the
        // same pipeline must fall to chance; a control that scores like the real run means the measure is broken, not the search.
        let answers: HashSet<&str> = if control {
            let others: Vec<&str> = ids.iter().filter_map(|s| s.as_str()).filter(|s| !real.contains(s)).collect();
            (0..real.len().min(others.len())).map(|i| others[(i * 7 + 3) % others.len()]).collect()
        } else {
            real
        };
        let t = Instant::now();
        let (hits, _) = store.search_ranked(q["question"].as_str().unwrap(), Some("haystack"), None, 100, None, ranker).unwrap();
        lat.push(t.elapsed().as_millis());
        // Sessions in the order their first record appears.
        let mut ranked: Vec<String> = Vec::new();
        for h in &hits {
            if let Some(s) = session_of.get(&h.obs.id) {
                if !ranked.contains(s) {
                    ranked.push(s.clone());
                }
            }
        }
        let first = ranked.iter().position(|s| answers.contains(s.as_str()));
        if debug && first.is_none_or(|p| p >= 5) {
            let snip = |s: &str| -> String {
                let i = ids.iter().position(|x| x.as_str() == Some(s)).unwrap();
                let body: String = sessions[i].as_array().unwrap().iter().filter(|t| t["role"] == "user")
                    .map(|t| t["content"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join(" | ");
                body.chars().take(220).collect()
            };
            eprintln!("MISS {qid} [{}] Q: {}", first.map_or("none".into(), |p| p.to_string()), q["question"]);
            for s in ranked.iter().take(2) { eprintln!("   top: {}", snip(s)); }
            for s in &answers { eprintln!("   ANS: {}", snip(s)); }
        }
        if let Some(f) = per_q_out.as_mut() {
            let mut answer_list: Vec<&&str> = answers.iter().collect();
            answer_list.sort();
            let line = json!({"question_id": qid, "question_type": q["question_type"], "answer_sessions": answer_list,
                              "first_answer_rank": first.map(|p| p + 1), "top10": ranked.iter().take(10).collect::<Vec<_>>(),
                              "haystack_sessions": ids.len(), "sessions_returned": ranked.len()});
            writeln!(f, "{line}").unwrap();
        }
        let qt = q["question_type"].as_str().unwrap_or("?").to_string();
        let e = per_type.entry(qt).or_insert([0; 4]);
        n += 1;
        e[0] += 1;
        if let Some(p) = first {
            rr += 1.0 / (p as f64 + 1.0);
            if p < 1 { h1 += 1; e[1] += 1; }
            if p < 5 { h5 += 1; e[2] += 1; }
            if p < 10 { h10 += 1; e[3] += 1; }
        }
        if n % 50 == 0 {
            eprintln!("{n} questions: hit@5 {:.3} ({:.0}s)", h5 as f64 / n as f64, started.elapsed().as_secs_f64());
        }
    }
    if n == 0 {
        fail("no questions matched");
    }
    lat.sort_unstable();
    let pct = |p: f64| lat.get(((lat.len() as f64 - 1.0) * p) as usize).copied().unwrap_or(0);
    let f = |x: usize, of: usize| if of == 0 { 0.0 } else { (x as f64 / of as f64 * 1000.0).round() / 1000.0 };
    let receipt = json!({
        "schema": "longmemeval-receipt/2", "input": path, "input_sha256": input_sha256, "arm": arm, "ranker": ranker_name, "control": control,
        "type_filter": only_type, "questions": n, "sessions_ingested": sessions_in, "records_written": records_in, "bodies_split": split_bodies, "bodies_empty": empty_bodies,
        "records_refused": 0,
        "hit_at_1": f(h1, n), "hit_at_5": f(h5, n), "hit_at_10": f(h10, n), "mrr": (rr / n as f64 * 1000.0).round() / 1000.0,
        "search_ms_p50": pct(0.5), "search_ms_p95": pct(0.95), "wall_s": started.elapsed().as_secs(),
        "per_type": per_type.iter().map(|(k, v)| (k.clone(), json!({"n": v[0], "hit_at_1": f(v[1], v[0]), "hit_at_5": f(v[2], v[0]), "hit_at_10": f(v[3], v[0])}))).collect::<BTreeMap<_, _>>(),
    });
    println!("{}", serde_json::to_string_pretty(&receipt).unwrap());
}
