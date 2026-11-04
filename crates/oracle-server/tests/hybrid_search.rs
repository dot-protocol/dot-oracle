//! Meaning search fused with keyword search, on the real store with real signed writes. The toy embedder
//! knows that "chime" means "doorbell", so a query only meaning can answer proves the meaning side is in
//! the ranking, and the room/author scoping proves it is held to the same filters as the words.

use ed25519_dalek::SigningKey;
use oracle_proto::{pubkey_hex, Envelope, KeyAction, KeyEvent, Kind};
use oracle_server::roomread::{search, search_with, SearchQ};
use oracle_server::semantic::{normalise, Embedder, Semantic};
use oracle_server::store::{Principal, Scope, Store};
use serde_json::Value;

const NOW: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn sk(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

/// Words to 16 buckets, with doorbell/chime/bell sharing one: a stand-in for "similar meaning".
struct Synonyms;
impl Embedder for Synonyms {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; 16];
                for w in t
                    .to_lowercase()
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| w.len() > 3 && *w != "query")
                {
                    let w = if matches!(w, "doorbell" | "chime" | "bell") {
                        "doorbell"
                    } else {
                        w
                    };
                    v[(w.bytes().map(|b| b as usize).sum::<usize>()) % 16] += 1.0;
                }
                normalise(v)
            })
            .collect())
    }
    fn model(&self) -> &str {
        "synonyms"
    }
}

struct Down;
impl Embedder for Down {
    fn embed(&self, _: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Err("embedder unreachable: connection refused".into())
    }
    fn model(&self) -> &str {
        "down"
    }
}

fn oracle(dir: &std::path::Path) -> (Store, String) {
    let path = dir.join("oracle.db").to_string_lossy().into_owned();
    let s = Store::open(&path).unwrap();
    s.init_owner("blaze", &pubkey_hex(&sk(1)), NOW - 1000 * MIN)
        .unwrap();
    for (h, k) in [("rocky", 2), ("dinesh", 3)] {
        s.apply_key_event(
            &KeyEvent::sign(
                &sk(1),
                "blaze",
                KeyAction::Add,
                h,
                &pubkey_hex(&sk(k)),
                NOW - 1000 * MIN,
            ),
            NOW - 1000 * MIN,
        )
        .unwrap();
    }
    (s, path)
}

fn say(s: &Store, room: &str, who: &str, key: u8, text: &str, mins: i64) -> String {
    let at = NOW + mins * MIN;
    let env = Envelope::sign(&sk(key), room, who, Kind::Say, text, None, vec![], at);
    s.write(
        &Principal {
            handle: who.into(),
            scope: Scope::Write,
        },
        &env,
        at,
    )
    .unwrap()
    .id
}

fn ids(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_owned())
        .collect()
}

fn rank(text: &str) -> SearchQ {
    SearchQ {
        q: text.into(),
        order: Some("rank".into()),
        ..Default::default()
    }
}

#[test]
fn meaning_finds_what_the_words_cannot_and_keeps_to_the_same_room_and_author() {
    let dir = tempfile::tempdir().unwrap();
    let (s, path) = oracle(dir.path());
    let astra = say(
        &s,
        "astra",
        "rocky",
        2,
        "the doorbell rings twice for every new message",
        0,
    );
    let other_room = say(
        &s,
        "ops",
        "rocky",
        2,
        "the doorbell rings in ops as well",
        1,
    );
    let other_author = say(
        &s,
        "astra",
        "dinesh",
        3,
        "my doorbell is configured differently",
        2,
    );
    say(&s, "astra", "rocky", 2, "deploys go out on thursday", 3);
    let sem = Semantic::open(
        dir.path().join("vec.db").to_str().unwrap(),
        Box::new(Synonyms),
    )
    .unwrap();
    assert_eq!(sem.index_pass(&path, 100).unwrap(), 4);

    // No message contains "chime": keyword search finds nothing, the fused search finds the doorbells.
    assert!(ids(&search(&s, &rank("chime")).unwrap()).is_empty());
    let fused = search_with(&s, &rank("chime"), Some(&sem)).unwrap();
    assert_eq!(fused["engine"], "hybrid");
    let got = ids(&fused);
    assert!(
        got.contains(&astra) && got.contains(&other_room) && got.contains(&other_author),
        "{got:?}"
    );

    // A room-scoped, author-scoped search holds the meaning side to the same room and author. Meaning search ranks
    // everything in scope (unrelated rows come last), so the doorbell leads and the other room and author never appear.
    let scoped = ids(&search_with(
        &s,
        &SearchQ {
            room: Some("astra".into()),
            author: Some("rocky".into()),
            ..rank("chime")
        },
        Some(&sem),
    )
    .unwrap());
    assert_eq!(scoped.first(), Some(&astra));
    assert!(
        !scoped.contains(&other_room) && !scoped.contains(&other_author),
        "{scoped:?}"
    );

    // Newest-first listings stay keyword-exact: meaning search only ranks.
    let newest = search_with(
        &s,
        &SearchQ {
            q: "chime".into(),
            ..Default::default()
        },
        Some(&sem),
    )
    .unwrap();
    assert!(ids(&newest).is_empty());
    assert_eq!(newest["engine"], "keyword");

    // The agents' ranked recall fuses too, and says so.
    let (hits, engine) = s
        .search_with("chime", Some("astra"), None, 10, Some(&sem))
        .unwrap();
    assert_eq!(engine, "hybrid");
    let recall: Vec<String> = hits.iter().map(|h| h.obs.id.clone()).collect();
    assert!(
        recall.contains(&astra) && !recall.contains(&other_room),
        "{recall:?}"
    );
}

#[test]
fn when_the_embedder_is_down_search_falls_back_to_words_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let (s, path) = oracle(dir.path());
    let hit = say(&s, "astra", "rocky", 2, "the doorbell rings twice", 0);
    let up = Semantic::open(
        dir.path().join("vec.db").to_str().unwrap(),
        Box::new(Synonyms),
    )
    .unwrap();
    up.index_pass(&path, 100).unwrap();
    let down = Semantic::open(dir.path().join("vec.db").to_str().unwrap(), Box::new(Down)).unwrap();
    let v = search_with(&s, &rank("doorbell"), Some(&down)).unwrap();
    assert_eq!(ids(&v), vec![hit.clone()]);
    assert!(
        v["engine"]
            .as_str()
            .unwrap()
            .starts_with("keyword (meaning search unavailable"),
        "{}",
        v["engine"]
    );
    let (hits, engine) = s
        .search_with("doorbell", None, None, 10, Some(&down))
        .unwrap();
    assert_eq!(hits[0].obs.id, hit);
    assert!(engine.contains("connection refused"));
}

/// Every page of one ranked query is a slice of one order (Emmy's review of #59: ranking must survive pagination).
fn pages(s: &Store, text: &str, sem: Option<&Semantic>, size: i64) -> Vec<String> {
    let mut out = vec![];
    let mut offset = Some(0);
    while let Some(o) = offset {
        let page = search_with(s, &SearchQ { limit: Some(size), offset: Some(o), ..rank(text) }, sem).unwrap();
        out.extend(ids(&page));
        offset = page["next_offset"].as_i64();
    }
    out
}

#[test]
fn ranked_pages_never_repeat_or_skip_a_match_with_or_without_meaning() {
    let dir = tempfile::tempdir().unwrap();
    let (s, path) = oracle(dir.path());
    // More matches than the fused window (SIDE = 50), many tied on bm25 (identical texts), some found only by meaning.
    let mut all = vec![];
    for i in 0..70 {
        let text = if i % 3 == 0 { "the doorbell rings".to_string() } else { format!("the doorbell rings and rings again {}", i % 2) };
        all.push(say(&s, "astra", "rocky", 2, &text, i));
    }
    for i in 0..5 {
        all.push(say(&s, "astra", "rocky", 2, "a chime sounded", 100 + i));
    }
    let sem = Semantic::open(dir.path().join("vec.db").to_str().unwrap(), Box::new(Synonyms)).unwrap();
    sem.index_pass(&path, 1000).unwrap();
    for sem in [None, Some(&sem)] {
        let whole = ids(&search_with(&s, &SearchQ { limit: Some(200), ..rank("doorbell") }, sem).unwrap());
        for size in [1, 7, 50] {
            let paged = pages(&s, "doorbell", sem, size);
            assert_eq!(paged, whole, "pages of {size} (meaning: {}) are slices of the one order", sem.is_some());
        }
        let mut unique = whole.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), whole.len(), "no match twice");
        let total = search_with(&s, &SearchQ { limit: Some(1), ..rank("doorbell") }, sem).unwrap()["total"].as_i64().unwrap();
        assert!(whole.len() >= 70 && whole.len() as i64 == total, "every keyword match, and total counts what the pages hold: {} of {total}", whole.len());
    }
}

#[test]
fn hybrid_total_does_not_count_keyword_hits_outside_the_first_window_twice() {
    struct TailFirst;
    impl Embedder for TailFirst {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts.iter().map(|t| if t.contains("lowrank") || t.contains("query") {
                vec![1.0, 0.0]
            } else { vec![0.0, 1.0] }).collect())
        }
        fn model(&self) -> &str { "tail-first" }
    }
    let dir = tempfile::tempdir().unwrap();
    let (s, path) = oracle(dir.path());
    for i in 0..70 {
        let text = if i < 60 { "doorbell" } else { "doorbell lowrank padding words for a much longer document with a weaker keyword score" };
        say(&s, "astra", "rocky", 2, text, i);
    }
    let sem = Semantic::open(dir.path().join("vec.db").to_str().unwrap(), Box::new(TailFirst)).unwrap();
    sem.index_pass(&path, 1000).unwrap();
    for offset in [0, 30, 60] {
        let page = search_with(&s, &SearchQ { limit: Some(1), offset: Some(offset), ..rank("doorbell") }, Some(&sem)).unwrap();
        assert_eq!(page["total"], 70, "semantic matches in the keyword tail are not extra matches, offset={offset}");
    }
}

/// Synonyms for meaning, and a reranker that scores a passage by how often it says "answer" (or fails, for the control).
struct Ranks(bool);
impl Embedder for Ranks {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Synonyms.embed(texts)
    }
    fn model(&self) -> &str {
        "ranks"
    }
    fn reranks(&self) -> bool {
        true
    }
    fn rerank(&self, _: &str, docs: &[String], _: std::time::Duration) -> Result<Vec<f32>, String> {
        if !self.0 {
            return Err("reranker unreachable: connection refused".into());
        }
        Ok(docs.iter().map(|d| d.matches("answer").count() as f32).collect())
    }
}

/// The cross-encoder orders the first candidates (3 Nov 2026, the 20 real moments: on the same 100 records Oracle went from 19
/// to 20 of 20 in the top 5, 13 to 18 first); `rerank=off` keeps the fused order; a reranker that fails keeps it and says so; a
/// mind the query names joins the fusion with its own words.
#[test]
fn the_reranker_orders_the_first_candidates_and_a_named_mind_adds_its_own_words() {
    use oracle_server::store::{Ranker, Tuning, SEARCH_TUNING};
    let dir = tempfile::tempdir().unwrap();
    let (s, path) = oracle(dir.path());
    let strong = say(&s, "astra", "rocky", 2, "lattice lattice lattice bootstrap lattice", 0);
    let quiet = say(&s, "astra", "rocky", 2, "a long note; somewhere in it the lattice, and the answer answer answer", 1);
    for i in 0..20 {
        say(&s, "ops", "dinesh", 3, &format!("dinesh writes about deploys, note {i}"), 2 + i);
    }
    let theirs = say(&s, "ops", "dinesh", 3, "the lattice deploy goes out on thursday", 30);
    let sem = Semantic::open(dir.path().join("vec.db").to_str().unwrap(), Box::new(Ranks(true))).unwrap();
    sem.index_pass(&path, 100).unwrap();
    let order = |q: &str, sem: &Semantic, rerank: Option<&str>| {
        SEARCH_TUNING.with(|t| *t.borrow_mut() = Tuning::parse(None, None).with_rerank(rerank));
        let (hits, engine) = s.search_in(q, None, None, 10, Some(sem), Ranker::Rrf, false).unwrap();
        (hits.into_iter().map(|h| h.obs.id).collect::<Vec<_>>(), engine)
    };
    let (fused, engine) = order("lattice", &sem, Some("off"));
    assert_eq!(fused.first(), Some(&strong), "control: the words put the strong match first ({engine})");
    assert!(!engine.contains("reranked"));
    let (reranked, engine) = order("lattice", &sem, None);
    assert_eq!(reranked.first(), Some(&quiet), "the reranker puts the answer first: {engine}");
    assert!(engine.contains("reranked"), "{engine}");
    let down = Semantic::open(dir.path().join("vec2.db").to_str().unwrap(), Box::new(Ranks(false))).unwrap();
    down.index_pass(&path, 100).unwrap();
    let (kept, engine) = order("lattice", &down, None);
    assert_eq!(kept.first(), Some(&strong), "a failed reranker keeps the fused order");
    assert!(engine.contains("rerank unavailable: reranker unreachable"), "{engine}");
    let (_, engine) = order("what did dinesh say about the lattice", &sem, Some("off"));
    assert!(engine.contains("their own words (dinesh)"), "dinesh has 21 signed records: {engine}");
    let (found, _) = order("what did dinesh say about the lattice", &sem, Some("off"));
    assert!(found.iter().take(3).any(|i| i == &theirs), "{found:?}");
    let (_, engine) = order("what did rocky say about the lattice", &sem, Some("off"));
    assert!(!engine.contains("their own words"), "rocky has 2 records, under the 20 a mind needs: {engine}");
}
