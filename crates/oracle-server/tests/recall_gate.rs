//! The recall gate (plan.oracle.search CI-S9; tests/data/recall-gate/README.md). A fixed synthetic team memory is
//! written as real signed records, a fixed set of questions is asked of the ranked recall agents use
//! (`Store::search_in`, the path behind `GET /search?q=` and `oracle_recall`), and the counts are held to a committed
//! baseline: a change to ranking that loses an answer fails here, in CI, before it ships. Two arms run: words only
//! (`Ranker::Keyword`, no embedder) and the default fusion with a toy embedder, so the meaning side is measured
//! without a model. The second test is the negative control: the same scoring, fed each ranking reversed, must fail.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::semantic::{normalise, Embedder, Semantic};
use oracle_server::store::{Principal, Ranker, Scope, Store};
use serde_json::Value;

const MIN: i64 = 60_000;
/// The seats of the made-up team and their key seeds; seed 1 is the owner.
const SEATS: [(&str, u8); 3] = [("ana", 2), ("ben", 3), ("cleo", 4)];

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/recall-gate").join(name)
}

fn jsonl(name: &str) -> Vec<Value> {
    std::fs::read_to_string(data(name))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{name}: {e}: {l}")))
        .collect()
}

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

/// Words hashed into 64 buckets after a synonym table: a deterministic stand-in for "similar meaning", so a paraphrase
/// that shares no word with its answer can still find it, and the words-only arm cannot.
struct Toy(BTreeMap<String, String>);
impl Embedder for Toy {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; 64];
                for w in t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 2) {
                    let w = self.0.get(w).map(String::as_str).unwrap_or(w);
                    // FNV-1a: stable across platforms and Rust versions, unlike the std hasher.
                    let h = w.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
                    v[(h % 64) as usize] += 1.0;
                }
                normalise(v)
            })
            .collect())
    }
    fn model(&self) -> &str {
        "recall-gate-toy"
    }
}

/// The corpus written into a fresh Oracle as signed records, in file order. Its clock is 30 days before now, so every
/// `valid_until_mins` has passed when the gate asks and an expired fact ranks as the store ranks one.
struct Memory {
    store: Store,
    sem: Semantic,
    key_of: BTreeMap<String, String>,
    _dir: tempfile::TempDir,
}

fn memory() -> Memory {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oracle.db").to_string_lossy().into_owned();
    let store = Store::open(&path).unwrap();
    let t0 = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64 - 30 * 24 * 60 * MIN;
    store.init_owner("owner", &pubkey_hex(&sk(1)), t0 - MIN).unwrap();
    for (h, k) in SEATS {
        let ev = KeyEvent::sign(&sk(1), "owner", KeyAction::Add, h, &pubkey_hex(&sk(k)), t0 - MIN);
        store.apply_key_event(&ev, t0 - MIN).unwrap();
    }
    let mut id_of = BTreeMap::<String, String>::new();
    let mut key_of = BTreeMap::new();
    for r in jsonl("corpus.jsonl") {
        let (key, who) = (r["key"].as_str().unwrap(), r["who"].as_str().unwrap());
        let seed = SEATS.iter().find(|(h, _)| *h == who).unwrap_or_else(|| panic!("{key}: unknown seat {who}")).1;
        let at = t0 + r["mins"].as_i64().unwrap() * MIN;
        let mut tags: Vec<String> = r["tags"]
            .as_array()
            .map(|a| a.iter().map(|t| t.as_str().unwrap().to_owned()).collect())
            .unwrap_or_default();
        for t in &mut tags {
            if let Some(k) = t.strip_prefix("supersedes:@") {
                *t = format!("supersedes:{}", id_of.get(k).unwrap_or_else(|| panic!("{key}: supersedes a later or unknown key {k}")));
            }
        }
        if let Some(m) = r["valid_until_mins"].as_i64() {
            tags.push(format!("valid-until:{}", t0 + m * MIN));
        }
        let env = Envelope::sign(&sk(seed), r["room"].as_str().unwrap(), who, Kind::Say, r["text"].as_str().unwrap(), None, tags, at);
        let id = store.write(&Principal { handle: who.into(), scope: Scope::Write }, &env, at).unwrap().id;
        id_of.insert(key.to_owned(), id.clone());
        key_of.insert(id, key.to_owned());
    }
    let synonyms: BTreeMap<String, String> = serde_json::from_str(&std::fs::read_to_string(data("synonyms.json")).unwrap()).unwrap();
    let sem = Semantic::open(dir.path().join("vec.db").to_str().unwrap(), Box::new(Toy(synonyms))).unwrap();
    let n = sem.index_pass(&path, 1000).unwrap();
    assert_eq!(n, key_of.len(), "the toy embedder indexed every record");
    Memory { store, sem, key_of, _dir: dir }
}

/// One arm's answers: for each question, the keys it returned, best first.
type Rankings = Vec<Vec<String>>;

fn ask(m: &Memory, fused: bool, limit: i64) -> (Vec<Value>, Rankings) {
    let qs: Vec<Value> = jsonl("queries.jsonl").into_iter().filter(|q| fused || q["arm"] == "both").collect();
    let ranked = qs
        .iter()
        .map(|q| {
            let (sem, ranker) = if fused { (Some(&m.sem), Ranker::Rrf) } else { (None, Ranker::Keyword) };
            let (hits, _) = m.store.search_in(q["q"].as_str().unwrap(), q["room"].as_str(), None, limit, sem, ranker, false).unwrap();
            hits.iter().map(|h| m.key_of.get(&h.obs.id).cloned().unwrap_or_else(|| h.obs.id.clone())).collect()
        })
        .collect();
    (qs, ranked)
}

/// Counts per category and in total: an answer first (top1) and among the first five (top5). A question with an empty
/// `expect` asks that nothing in scope be found, and passes both only when nothing is returned.
fn score(qs: &[Value], ranked: &Rankings) -> (BTreeMap<String, (u32, u32)>, Vec<String>) {
    let mut by = BTreeMap::<String, (u32, u32)>::new();
    let mut misses = vec![];
    for (q, got) in qs.iter().zip(ranked) {
        let want: Vec<&str> = q["expect"].as_array().unwrap().iter().map(|e| e.as_str().unwrap()).collect();
        let (top1, top5) = if want.is_empty() {
            (got.is_empty(), got.is_empty())
        } else {
            (got.first().is_some_and(|k| want.contains(&k.as_str())), got.iter().take(5).any(|k| want.contains(&k.as_str())))
        };
        for cat in [q["cat"].as_str().unwrap(), "total"] {
            let e = by.entry(cat.to_owned()).or_default();
            e.0 += top1 as u32;
            e.1 += top5 as u32;
        }
        if !top1 {
            misses.push(format!("  {:<11} {:?} want {:?}, got {:?}", q["cat"].as_str().unwrap(), q["q"].as_str().unwrap(), want, &got[..got.len().min(5)]));
        }
    }
    (by, misses)
}

/// Every count of `by` held to the baseline's; the shortfalls, named. Missing categories in the baseline count as 0.
fn shortfalls(arm: &str, by: &BTreeMap<String, (u32, u32)>) -> Vec<String> {
    let base: Value = serde_json::from_str(&std::fs::read_to_string(data("BASELINE.json")).unwrap()).unwrap();
    let mut out = vec![];
    for (cat, b) in base[arm].as_object().unwrap_or_else(|| panic!("BASELINE.json has no arm {arm}")) {
        // `measured` and `known_misses` sit beside the arms for the reader; only the arms hold counts.
        let (t1, t5) = by.get(cat).copied().unwrap_or_default();
        let (b1, b5) = (b["top1"].as_u64().unwrap() as u32, b["top5"].as_u64().unwrap() as u32);
        if t1 < b1 || t5 < b5 {
            out.push(format!("{arm}/{cat}: top1 {t1} (baseline {b1}), top5 {t5} (baseline {b5})"));
        }
    }
    out
}

fn table(arm: &str, by: &BTreeMap<String, (u32, u32)>, n: usize) -> String {
    let rows: Vec<String> = by.iter().map(|(c, (a, b))| format!("{c} {a}/{b}")).collect();
    format!("{arm} ({n} questions; top1/top5): {}", rows.join(", "))
}

#[test]
fn recall_holds_the_baseline_on_the_fixed_team_memory() {
    let m = memory();
    let mut failed = vec![];
    for (arm, fused) in [("words", false), ("fused", true)] {
        let (qs, ranked) = ask(&m, fused, 10);
        let (by, misses) = score(&qs, &ranked);
        // The receipt: what each arm scored and what it missed first place on, printed on every run (`--nocapture`).
        eprintln!("{}", table(arm, &by, qs.len()));
        for l in &misses {
            eprintln!("{l}");
        }
        failed.extend(shortfalls(arm, &by));
    }
    assert!(failed.is_empty(), "recall fell below tests/data/recall-gate/BASELINE.json:\n{}", failed.join("\n"));
}

#[test]
fn the_gate_fails_a_ranking_that_got_worse() {
    // The negative control: every arm's ranking reversed (the best answers last, from a longer list) must fall below
    // the baseline. If this passes, the gate cannot see a regression and its green means nothing.
    let m = memory();
    for (arm, fused) in [("words", false), ("fused", true)] {
        let (qs, mut ranked) = ask(&m, fused, 50);
        ranked.iter_mut().for_each(|r| r.reverse());
        let (by, _) = score(&qs, &ranked);
        assert!(!shortfalls(arm, &by).is_empty(), "{arm}: a reversed ranking still met the baseline: {}", table(arm, &by, qs.len()));
    }
}
