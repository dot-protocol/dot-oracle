//! The recall scoreboard proves itself on a store it can see through (docs/RECALL-SCOREBOARD.md): the A/A arm agrees
//! with the control, the shuffled control scores nothing, and every refusal refuses.
use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::scoreboard::{paired_bootstrap, read_gold, read_pin, rotated_truth, run, Question, Setup, Snapshot};
use oracle_server::store::{Principal, Scope, Store};

const NOW: i64 = 1_800_000_000_000;
fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}
fn oracle() -> Store {
    let s = Store::memory().unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 100_000).unwrap();
    for (h, n) in [("rocky", 3u8), ("emmy", 4u8), ("stranger", 5u8)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(n)), NOW - 100_000), NOW - 100_000).unwrap();
    }
    s
}
fn put(s: &Store, seat: (&str, u8), room: &str, text: &str, tags: &[&str], cargo: Option<serde_json::Value>, at: i64) -> (String, i64) {
    let (who, key) = seat;
    let env = Envelope::sign(&sk(key), room, who, Kind::Say, text, cargo.map(|c| c.to_string()), tags.iter().map(|t| t.to_string()).collect(), at);
    let r = s.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap();
    (r.id, r.seq)
}
/// 22 questions, each one message with a word only it says; three framings each that share no word with any other
/// message (so the rotated truth can only be found by luck, which is what the control measures); plus noise that
/// shares the messages' filler words, so the top 10 for a wrong question fills with noise, not with other questions.
fn corpus(s: &Store) -> (String, Vec<Question>, i64) {
    let words = ["zorbital", "quenchline", "varnishwing", "plumbgate", "kettlemoor", "fiskwater", "oxbowline", "tarnhelm", "glassfold", "mirecastle", "hobnail", "sundrift", "copperwell", "lanternfish", "ashgrove", "brinefold", "cindertrail", "duskmantle", "emberwick", "frostline", "gildhollow", "harrowbend"];
    let mut gold = Vec::new();
    let mut last = 0;
    for (i, w) in words.iter().enumerate() {
        let (id, seq) = put(s, ("rocky", 3), "axxis", &format!("the {w} design was decided on tuesday, after the second review"), &[], None, NOW - 90_000 + i as i64 * 10);
        last = seq;
        for j in 0..3 {
            put(s, ("emmy", 4), "axxis", &format!("noise {i}-{j}: a design was decided on tuesday after the second review, status only"), &[], None, NOW - 90_000 + i as i64 * 10 + 1 + j);
        }
        gold.push(Question { code: w.to_string(), framings: vec![w.to_string(), format!("{w} please"), format!("find {w}")], relevant: vec![id], ..Default::default() });
    }
    let text: String = gold.iter().map(|q| serde_json::json!({"code": q.code, "framings": q.framings, "seed_ids": q.relevant}).to_string() + "\n").collect();
    (text, gold, last)
}
fn go(s: &Store, gold: &[Question], text: &str, pin: &oracle_server::scoreboard::Pin, id: &str, only: &[String], terms: Option<&str>) -> anyhow::Result<serde_json::Value> {
    run(s, &Setup { gold, gold_text: text, pin, pin_id: id, only, terms_sha: terms, build: "test", sem: None, snapshot: &Snapshot::default(), split: None, reranker: None })
}
fn pin(s: &Store, who: &str, key: u8, text: &str, cutoff: i64, arms: &[&str]) -> String {
    let cargo = serde_json::json!({"gold_sha256": oracle_proto::sha_hex(text.as_bytes()), "cutoff_seq": cutoff, "arms": arms, "k": [1, 5, 10], "positives": 20, "seed": 7});
    put(s, (who, key), "oracle", "scoreboard pin", &["scoreboard:pin"], Some(cargo), NOW - 1000).0
}

#[test]
fn the_instrument_proves_itself_aa_agrees_and_shuffled_scores_nothing() {
    let s = oracle();
    let (text, _, cutoff) = corpus(&s);
    let gold = read_gold(&text).unwrap();
    assert_eq!(gold.len(), 22);
    let id = pin(&s, "rocky", 3, &text, cutoff, &["bm25", "aa", "shuffled"]);
    let (p, _) = read_pin(&s, &id, &["blaze", "rocky", "emmy"]).unwrap();
    let r = go(&s, &gold, &text, &p, &id, &[], None).unwrap();
    assert_eq!(r["positives"], 22);
    assert_eq!(r["tables"]["bm25"]["hit@1"].as_f64().unwrap(), 1.0, "a word only one message says is found first: {}", r["tables"]);
    let quality = |t: &serde_json::Value| { let mut q = t.as_object().cloned().unwrap(); q.remove("p50_ms"); q.remove("p95_ms"); q };
    assert_eq!(quality(&r["tables"]["aa"]), quality(&r["tables"]["bm25"]), "the A/A arm agrees to the decimal on every quality number");
    assert_eq!(r["verdicts"]["aa_agrees"], true);
    assert!(r["tables"]["shuffled"]["hit@10"].as_f64().unwrap() < 0.05, "the rotated truth is not found: {}", r["tables"]["shuffled"]);
    assert_eq!(r["verdicts"]["shuffled_under_0_05"], true);
    assert_eq!(r["deltas"]["aa"]["mean"], 0.0, "no delta against itself");
    assert!(r["deltas"]["shuffled"]["hi"].as_f64().unwrap() < 0.0, "the control's delta is below zero across the interval");
    assert_eq!(r["rows"], 22 * 3 * 3);
    assert_eq!(r["errors"].as_array().unwrap().len(), 0);
    assert!(r["evidence"].as_str().unwrap().starts_with("pilot: 22 questions, 66 framings"));
}

#[test]
fn every_refusal_refuses() {
    let s = oracle();
    let (text, _, cutoff) = corpus(&s);
    let gold = read_gold(&text).unwrap();
    let trusted = ["blaze", "rocky", "emmy"];
    // an untrusted signer
    let id = pin(&s, "stranger", 5, &text, cutoff, &["bm25"]);
    assert!(read_pin(&s, &id, &trusted).unwrap_err().to_string().contains("not trusted"));
    // a message that is not a pin
    let (say, _) = put(&s, ("rocky", 3), "oracle", "just a say", &[], None, NOW - 900);
    assert!(read_pin(&s, &say, &trusted).unwrap_err().to_string().contains("not tagged"));
    // an arm this build does not have
    let id = pin(&s, "rocky", 3, &text, cutoff, &["bm25", "vector"]);
    assert!(read_pin(&s, &id, &trusted).unwrap_err().to_string().contains("does not have: vector"));
    // the gold changed after the pin
    let id = pin(&s, "rocky", 3, &text, cutoff, &["bm25"]);
    let (p, _) = read_pin(&s, &id, &trusted).unwrap();
    let changed = text.replace("zorbital", "zorbital2");
    assert!(go(&s, &read_gold(&changed).unwrap(), &changed, &p, &id, &[], None).unwrap_err().to_string().contains("sha256"));
    // an arm the pin did not name
    assert!(go(&s, &gold, &text, &p, &id, &["aa".into()], None).unwrap_err().to_string().contains("not in the pin"));
    // a cutoff that leaves too few positives
    let thin = pin(&s, "rocky", 3, &text, 3, &["bm25"]);
    let (tp, _) = read_pin(&s, &thin, &trusted).unwrap();
    assert!(go(&s, &gold, &text, &tp, &thin, &[], None).unwrap_err().to_string().contains("questions have a relevant id"));
    // the terms file changed
    let mut with_terms = p.clone();
    with_terms.terms_sha256 = Some("abc".into());
    assert!(go(&s, &gold, &text, &with_terms, &id, &[], Some("def")).unwrap_err().to_string().contains("terms file"));
}

#[test]
fn the_rotated_truth_drops_shared_ids_and_the_bootstrap_is_seeded() {
    let gold = vec![
        Question { code: "a".into(), framings: vec!["a".into()], relevant: vec!["x".into(), "shared".into()], ..Default::default() },
        Question { code: "b".into(), framings: vec!["b".into()], relevant: vec!["y".into(), "shared".into()], ..Default::default() },
        Question { code: "c".into(), framings: vec!["c".into()], relevant: vec!["z".into()], ..Default::default() },
    ];
    let (t, removed) = rotated_truth(&gold);
    assert_eq!(removed, 1, "a shared id is removed once, from the rotation into the question that also has it");
    assert!(t[0].contains("y") && !t[0].contains("shared"));
    assert!(t[1].contains("z"));
    assert!(t[2].contains("x") && t[2].contains("shared"));
    let d = [0.1, -0.2, 0.3, 0.0, 0.05];
    assert_eq!(paired_bootstrap(&d, 7, 500), paired_bootstrap(&d, 7, 500), "the same seed, the same interval");
    assert_ne!(paired_bootstrap(&d, 7, 500), paired_bootstrap(&d, 8, 500));
    let (mean, lo, hi) = paired_bootstrap(&d, 7, 500);
    assert!(lo <= mean && mean <= hi);
}

/// Stage 2 (docs/RECALL-SCOREBOARD.md 2c). A pin naming a meaning arm without a frozen snapshot is refused; a run whose
/// files are not the pin's snapshot is refused; a meaning arm is refused, with the reason, when meaning search is off;
/// and `bm25-messages` is `bm25` with State revisions left out, so on a store with none it scores exactly as `bm25`.
#[test]
fn stage_two_refusals_and_bm25_messages_matches_bm25_without_state() {
    let s = oracle();
    let (text, _, cutoff) = corpus(&s);
    let gold = read_gold(&text).unwrap();
    let trust = ["blaze", "rocky", "emmy"];
    let loose = pin(&s, "rocky", 3, &text, cutoff, &["bm25", "live"]);
    let e = read_pin(&s, &loose, &trust).unwrap_err().to_string();
    assert!(e.starts_with("refused:") && e.contains("frozen snapshot"), "{e}");
    // Hashes but no embedder probe: refused at the pin too.
    let cargo = serde_json::json!({"gold_sha256": oracle_proto::sha_hex(text.as_bytes()), "cutoff_seq": cutoff, "arms": ["bm25", "live"], "k": [1, 5, 10], "positives": 20, "seed": 7, "store_sha256": "aa11", "vectors_sha256": "bb22"});
    let unprobed = put(&s, ("rocky", 3), "oracle", "scoreboard pin", &["scoreboard:pin"], Some(cargo), NOW - 1000).0;
    let e = read_pin(&s, &unprobed, &trust).unwrap_err().to_string();
    assert!(e.starts_with("refused:") && e.contains("embedder must be pinned"), "{e}");
    let cargo = serde_json::json!({"gold_sha256": oracle_proto::sha_hex(text.as_bytes()), "cutoff_seq": cutoff, "arms": ["bm25", "bm25-messages", "live", "live-aa", "hybrid:and3"], "k": [1, 5, 10], "positives": 20, "seed": 7, "store_sha256": "aa11", "vectors_sha256": "bb22", "embed_probe_sha256": "cc33", "embed_dim": 1024});
    let id = put(&s, ("rocky", 3), "oracle", "scoreboard pin", &["scoreboard:pin"], Some(cargo), NOW - 1000).0;
    let (p, _) = read_pin(&s, &id, &trust).unwrap();
    let only = |a: &str| vec![a.to_string()];
    let snap = |st: &str, v: &str| Snapshot { store_sha256: Some(st.into()), vectors_sha256: Some(v.into()) };
    let with = |snapshot: &Snapshot, arms: &[String]| run(&s, &Setup { gold: &gold, gold_text: &text, pin: &p, pin_id: &id, only: arms, terms_sha: None, build: "test", sem: None, snapshot, split: None, reranker: None });
    for (sn, want) in [(snap("ff", "bb22"), "store file"), (snap("aa11", "ff"), "vector index file"), (Snapshot::default(), "could not hash")] {
        let e = with(&sn, &only("bm25")).unwrap_err().to_string();
        assert!(e.starts_with("refused:") && e.contains(want), "{want}: {e}");
    }
    let good = snap("aa11", "bb22");
    for arm in ["live", "live-aa", "hybrid:and3"] {
        let e = with(&good, &only(arm)).unwrap_err().to_string();
        assert!(e.starts_with("refused:") && e.contains("needs meaning search"), "{arm}: {e}");
    }
    let r = with(&good, &["bm25".into(), "bm25-messages".into()]).unwrap();
    let quality = |t: &serde_json::Value| {
        let mut q = t.as_object().unwrap().clone();
        q.remove("p50_ms");
        q.remove("p95_ms");
        q
    };
    assert_eq!(quality(&r["tables"]["bm25"]), quality(&r["tables"]["bm25-messages"]));
    assert_eq!(r["tables"]["bm25"]["hit@1"], 1.0);
    assert_eq!(r["snapshot"]["store_sha256"], "aa11");
    assert!(r["cutoff_role"].as_str().unwrap().starts_with("truth eligibility only"));
}

/// Words to 16 buckets, with doorbell/chime sharing one: a deterministic stand-in for meaning (as tests/hybrid_search.rs).
struct Toy;
impl oracle_server::semantic::Embedder for Toy {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; 16];
                for w in t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 3 && *w != "query") {
                    let w = if matches!(w, "doorbell" | "chime") { "doorbell" } else { w };
                    v[w.bytes().map(|b| b as usize).sum::<usize>() % 16] += 1.0;
                }
                oracle_server::semantic::normalise(v)
            })
            .collect())
    }
    fn model(&self) -> &str {
        "toy"
    }
}

/// Emmy: on a real store with a real vector index, `live` and `live-aa` agree, each
/// `hybrid:<r>` answers exactly as `search_ranked` with ranker r, the routing is observable (`hybrid:keyword` cannot find
/// a match only meaning finds, `live` does), and the prewarm runs before the arms.
#[test]
fn meaning_arms_route_to_their_rankers_and_live_aa_agrees() {
    use oracle_server::scoreboard::{answer, probe};
    use oracle_server::store::Ranker;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oracle.db").to_string_lossy().into_owned();
    let s = Store::open(&path).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 100_000).unwrap();
    for (h, n) in [("rocky", 3u8), ("emmy", 4u8)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(n)), NOW - 100_000), NOW - 100_000).unwrap();
    }
    let (text, _, cutoff) = corpus(&s);
    let (bell, _) = put(&s, ("rocky", 3), "axxis", "the doorbell rings twice at the gate", &[], None, NOW - 1);
    let sem = oracle_server::semantic::Semantic::open(dir.path().join("vec.db").to_str().unwrap(), Box::new(Toy)).unwrap();
    assert!(sem.index_pass(&path, 1000).unwrap() > 0);
    let (probe_sha, dim) = probe(&sem).unwrap();
    let gold = read_gold(&text).unwrap();
    let arms = ["bm25", "live", "live-aa", "hybrid:and-first", "hybrid:and3", "hybrid:keyword", "hybrid:and-top", "hybrid:wrrf", "hybrid:k10"];
    let cargo = serde_json::json!({"gold_sha256": oracle_proto::sha_hex(text.as_bytes()), "cutoff_seq": cutoff, "arms": arms, "k": [1, 5, 10], "positives": 20, "seed": 7,
        "store_sha256": "aa11", "vectors_sha256": "bb22", "embed_probe_sha256": probe_sha, "embed_dim": dim});
    let id = put(&s, ("rocky", 3), "oracle", "scoreboard pin", &["scoreboard:pin"], Some(cargo), NOW - 1000).0;
    let (p, _) = read_pin(&s, &id, &["blaze", "rocky", "emmy"]).unwrap();
    let snap = Snapshot { store_sha256: Some("aa11".into()), vectors_sha256: Some("bb22".into()) };
    let r = run(&s, &Setup { gold: &gold, gold_text: &text, pin: &p, pin_id: &id, only: &[], terms_sha: None, build: "test", sem: Some(&sem), snapshot: &snap, split: None, reranker: None }).unwrap();
    assert_eq!(r["verdicts"]["live_aa_agrees"], true, "{}", r["tables"]);
    assert_eq!(r["prewarm"]["framings"], 66);
    assert_eq!(r["prewarm"]["failed"], 0);
    // Each hybrid arm is search_ranked with its named ranker, framing by framing.
    for (arm, ranker) in [("live", None), ("hybrid:and-first", Some("and-first")), ("hybrid:and3", Some("and3")), ("hybrid:keyword", Some("keyword")),
        ("hybrid:and-top", Some("and-top")), ("hybrid:wrrf", Some("wrrf")), ("hybrid:k10", Some("k10"))] {
        for q in &gold {
            for f in &q.framings {
                let got = answer(&s, Some(&sem), arm, f, i64::MAX, 10).unwrap();
                let want: Vec<String> = s.search_ranked(f, None, None, 40, Some(&sem), Ranker::parse(ranker)).unwrap().0.into_iter().map(|h| h.obs.id).take(10).collect();
                assert_eq!(got, want, "{arm} on {f}");
            }
        }
    }
    // The routing is observable: only meaning finds the doorbell for "chime"; the keyword ranker cannot.
    assert!(answer(&s, Some(&sem), "live", "chime", i64::MAX, 10).unwrap().contains(&bell));
    assert!(!answer(&s, Some(&sem), "hybrid:keyword", "chime", i64::MAX, 10).unwrap().contains(&bell));
}

/// Gold 100 (docs/RECALL-SCOREBOARD.md 2d): `source` and `split` are read; a run reports a table per source; a split
/// run scores only that half while the hash and the pin's positive count cover the whole file; a split that is not dev
/// or test, a half under 20 positives, and a split on a file whose questions carry none are refused.
#[test]
fn gold_100_scores_one_half_and_reports_each_source() {
    let s = oracle();
    let (plain_text, plain, cutoff) = corpus(&s);
    let text: String = plain
        .iter()
        .enumerate()
        .map(|(i, q)| serde_json::json!({"code": q.code, "framings": q.framings, "seed_ids": q.relevant, "source": if i % 2 == 0 { "canon" } else { "memory" }, "split": if i < 20 { "dev" } else { "test" }}).to_string() + "\n")
        .collect();
    let gold = read_gold(&text).unwrap();
    assert_eq!((gold[1].source.as_deref(), gold[21].split.as_deref()), (Some("memory"), Some("test")));
    let id = pin(&s, "rocky", 3, &text, cutoff, &["bm25", "aa", "shuffled"]);
    let (p, _) = read_pin(&s, &id, &["blaze", "rocky", "emmy"]).unwrap();
    let with = |split: Option<&str>| run(&s, &Setup { gold: &gold, gold_text: &text, pin: &p, pin_id: &id, only: &[], terms_sha: None, build: "test", sem: None, snapshot: &Snapshot::default(), split, reranker: None });
    let all = with(None).unwrap();
    assert_eq!((all["split"].as_str(), all["positives"].as_u64()), (Some("all"), Some(22)));
    assert_eq!(all["tables_by_source"]["canon"]["bm25"]["framings"], 33, "11 canon questions x 3 framings: {}", all["tables_by_source"]);
    assert_eq!(all["tables_by_source"]["memory"]["bm25"]["hit@5"], 1.0);
    assert!(all["tables_by_source"]["memory"]["shuffled"]["hit@5"].as_f64().unwrap() < 0.05);
    let dev = with(Some("dev")).unwrap();
    assert_eq!((dev["split"].as_str(), dev["positives"].as_u64()), (Some("dev"), Some(20)));
    assert_eq!(dev["tables"]["bm25"]["framings"], 60, "only the 20 dev questions are scored");
    assert_eq!((dev["verdicts"]["aa_agrees"].as_bool(), dev["verdicts"]["shuffled_under_0_05"].as_bool()), (Some(true), Some(true)));
    for (split, want) in [("train", "is not dev or test"), ("test", "the run wants 20")] {
        let e = with(Some(split)).unwrap_err().to_string();
        assert!(e.starts_with("refused:") && e.contains(want), "{split}: {e}");
    }
    let pid = pin(&s, "rocky", 3, &plain_text, cutoff, &["bm25"]);
    let (pp, _) = read_pin(&s, &pid, &["blaze", "rocky", "emmy"]).unwrap();
    let plain_run = |split: Option<&str>| run(&s, &Setup { gold: &plain, gold_text: &plain_text, pin: &pp, pin_id: &pid, only: &[], terms_sha: None, build: "test", sem: None, snapshot: &Snapshot::default(), split, reranker: None });
    let e = plain_run(Some("dev")).unwrap_err().to_string();
    assert!(e.starts_with("refused:") && e.contains("no question in the gold file is marked split dev"), "{e}");
    assert!(plain_run(None).unwrap()["tables_by_source"].as_object().unwrap().is_empty(), "one source: no per-source tables");
    // The gates: no framing here is a phrase of four words or more; no dev question shares truth with a test one, but
    // every dev/test pair shares "please" and "find" (half their words), and the gate lists exactly those 20 x 2.
    assert_eq!(all["leakage"]["framings_checked"], 66);
    assert_eq!(all["leakage"]["found_verbatim"].as_array().unwrap().len(), 0);
    let twins = all["near_duplicates_across_splits"].as_array().unwrap();
    assert_eq!(twins.len(), 40);
    assert!(twins.iter().all(|t| t["shared_truth"].as_array().unwrap().is_empty() && t["word_jaccard"] == 0.5), "{twins:?}");
}

/// Emmy's gates find what they look for: a framing that a record already says, word for word, is listed with the
/// record; a test question that shares a truth id with a dev question, or most of its words, is listed as a twin.
#[test]
fn the_leak_and_twin_gates_find_planted_cases() {
    use oracle_server::scoreboard::{leaks, near_duplicates};
    let s = oracle();
    let (said, _) = put(&s, ("rocky", 3), "axxis", "Notes: where does the quiet harbour keep its lanterns at night, asked twice.", &[], None, NOW - 50);
    let q = |code: &str, f: &str, rel: &str, split: &str| Question { code: code.into(), framings: vec![f.into()], relevant: vec![rel.into()], source: None, split: Some(split.into()) };
    let gold = vec![
        q("d1", "where does the quiet harbour keep its lanterns", "x1", "dev"),
        q("d2", "copper kettles boiling over in the galley", "x2", "dev"),
        q("t1", "unrelated mountain survey of granite ridges", "x2", "test"),
        q("t2", "copper kettles boiling over in a galley today", "x9", "test"),
        q("t3", "openfab", "x3", "test"),
    ];
    let l = leaks(&s, &gold).unwrap();
    let found = l["found_verbatim"].as_array().unwrap();
    assert_eq!(found.len(), 1, "{l}");
    assert_eq!((found[0]["question"].as_str(), found[0]["ids"][0].as_str()), (Some("d1"), Some(said.as_str())));
    let twins = near_duplicates(&gold);
    let pairs: Vec<(String, String)> = twins.iter().map(|t| (t["dev"].as_str().unwrap().to_string(), t["test"].as_str().unwrap().to_string())).collect();
    assert_eq!(pairs, [("d2".to_string(), "t1".to_string()), ("d2".to_string(), "t2".to_string())], "shared truth x2, and most words: {twins:?}");
}

/// A reranker that reverses `live`'s order and counts its calls: the order it returns must be the arm's answer.
struct Reverse(std::cell::Cell<usize>);
impl oracle_server::scoreboard::Rerank for Reverse {
    fn order(&self, _: &str, cands: &[oracle_server::scoreboard::Cand]) -> Result<(Vec<String>, oracle_server::scoreboard::Usage), String> {
        self.0.set(self.0.get() + 1);
        Ok((cands.iter().rev().map(|c| c.id.clone()).collect(), oracle_server::scoreboard::Usage { input: 100, output: 5 }))
    }
}

/// Section 2e: `rerank:llm` answers the reranker's order of `live`'s top n (here reversed, so the unique answer that
/// `live` puts first falls to fifth: hit@1 drops, the order is applied); `rerank:llm-aa` calls it again and agrees; the
/// tokens and dollars are in the table; a pin without the reranker, with another prompt, and a run without a reranker
/// are refused.
#[test]
fn rerank_arms_apply_the_rerankers_order_and_count_its_cost() {
    use oracle_server::scoreboard::{answer_rerank, rerank_prompt_sha};
    use oracle_server::store::Ranker;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oracle.db").to_string_lossy().into_owned();
    let s = Store::open(&path).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 100_000).unwrap();
    for (h, n) in [("rocky", 3u8), ("emmy", 4u8)] {
        s.apply_key_event(&KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(n)), NOW - 100_000), NOW - 100_000).unwrap();
    }
    let (text, _, cutoff) = corpus(&s);
    let sem = oracle_server::semantic::Semantic::open(dir.path().join("vec.db").to_str().unwrap(), Box::new(Toy)).unwrap();
    assert!(sem.index_pass(&path, 1000).unwrap() > 0);
    let (probe_sha, dim) = oracle_server::scoreboard::probe(&sem).unwrap();
    let gold = read_gold(&text).unwrap();
    let trust = ["blaze", "rocky", "emmy"];
    let base = serde_json::json!({"gold_sha256": oracle_proto::sha_hex(text.as_bytes()), "cutoff_seq": cutoff, "arms": ["live", "rerank:llm", "rerank:llm-aa"], "k": [1, 5, 10], "positives": 20, "seed": 7,
        "store_sha256": "aa11", "vectors_sha256": "bb22", "embed_probe_sha256": probe_sha, "embed_dim": dim});
    let pin_with = |rerank: Option<serde_json::Value>| {
        let mut c = base.clone();
        if let Some(r) = rerank {
            c["rerank"] = r;
        }
        put(&s, ("rocky", 3), "oracle", "scoreboard pin", &["scoreboard:pin"], Some(c), NOW - 1000).0
    };
    let e = read_pin(&s, &pin_with(None), &trust).unwrap_err().to_string();
    assert!(e.starts_with("refused:") && e.contains("names a rerank arm but no rerank"), "{e}");
    let rr = |sha: &str| serde_json::json!({"model": "fake", "candidates": 5, "prompt_sha256": sha, "usd_per_mtok_in": 1.0, "usd_per_mtok_out": 5.0});
    let e = read_pin(&s, &pin_with(Some(rr("00ff"))), &trust).unwrap_err().to_string();
    assert!(e.starts_with("refused:") && e.contains("the prompt changed"), "{e}");
    let id = pin_with(Some(rr(&rerank_prompt_sha())));
    let (p, _) = read_pin(&s, &id, &trust).unwrap();
    let snap = Snapshot { store_sha256: Some("aa11".into()), vectors_sha256: Some("bb22".into()) };
    let e = run(&s, &Setup { gold: &gold, gold_text: &text, pin: &p, pin_id: &id, only: &[], terms_sha: None, build: "test", sem: Some(&sem), snapshot: &snap, split: None, reranker: None }).unwrap_err().to_string();
    assert!(e.starts_with("refused:") && e.contains("needs a reranker"), "{e}");
    let rev = Reverse(std::cell::Cell::new(0));
    let r = run(&s, &Setup { gold: &gold, gold_text: &text, pin: &p, pin_id: &id, only: &[], terms_sha: None, build: "test", sem: Some(&sem), snapshot: &snap, split: None, reranker: Some(&rev) }).unwrap();
    assert_eq!(rev.0.get(), 2 * 66, "one call per framing per rerank arm, none in the warm-up");
    // The egress inventory names exactly what those calls carried, and calls nothing.
    let inv = oracle_server::scoreboard::egress_inventory(&s, Some(&sem), &gold, &p, None, 2).unwrap();
    assert_eq!(rev.0.get(), 2 * 66, "the inventory made no reranker call");
    assert_eq!(inv["calls"], 2 * 66);
    let sends: u64 = inv["records"].as_array().unwrap().iter().map(|r| r["sends"].as_u64().unwrap()).sum();
    assert_eq!(inv["request_document_pairs"].as_u64(), Some(sends), "pairs = the sum of every record's sends");
    assert!(inv["records"].as_array().unwrap().iter().all(|r| r.get("text").is_none() && r["room"].is_string()));
    let t = &r["tables"]["rerank:llm"];
    assert_eq!((t["input_tokens"].as_u64(), t["output_tokens"].as_u64(), t["max_input_tokens"].as_u64()), (Some(6600), Some(330), Some(100)));
    assert_eq!(t["usd_per_framing"], 0.000125, "100 in at $1/M and 5 out at $5/M");
    assert_eq!(r["verdicts"]["rerank_aa_agrees"], true);
    assert_eq!(r["rerank"]["candidates"], 5);
    assert_eq!(r["tables"]["live"]["hit@1"], 1.0);
    assert!(t["hit@1"].as_f64().unwrap() < 0.5, "reversed, the first answer moves down: {t}");
    for q in gold.iter().take(3) {
        let f = &q.framings[0];
        let (got, _) = answer_rerank(&s, Some(&sem), &rev, 5, f, i64::MAX, 10).unwrap();
        let mut want: Vec<String> = s.search_ranked(f, None, None, 5, Some(&sem), Ranker::parse(None)).unwrap().0.into_iter().map(|h| h.obs.id).collect();
        want.reverse();
        assert_eq!(got, want, "{f}");
    }
}

#[test]
fn rerank_replies_and_vault_responses_parse() {
    use oracle_server::scoreboard::{http_body, merge_order, parse_order, Cand};
    let c = |id: &str| Cand { id: id.into(), room: "r".into(), author: "a".into(), date: "2026-10-20".into(), text: "t".into() };
    let cands = [c("a"), c("b"), c("c")];
    assert_eq!(parse_order("3, 1, 3, 99, x2", &cands), ["c", "a", "b"], "in order, each once, out-of-range dropped");
    assert_eq!(merge_order(&["c".into()], &cands, 10), ["c", "a", "b"], "the left-out keep live's order");
    assert_eq!(merge_order(&["zz".into(), "b".into()], &cands, 2), ["b", "a"], "an id that was not a candidate is dropped");
    assert_eq!(http_body(b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\n{\"a\":1}").unwrap(), "{\"a\":1}");
    assert_eq!(http_body(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n").unwrap(), "{\"a\":1}");
    let e = http_body(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 9\r\n\r\nno token.").unwrap_err();
    assert!(e.starts_with("vault 401") && e.contains("no token"), "{e}");
}
