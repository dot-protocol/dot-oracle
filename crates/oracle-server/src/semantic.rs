//! Meaning search: every observation embedded IN FULL, fused with keyword search.
//!
//! Vectors live in their own SQLite file (`ORACLE_VEC_DB`): they are derived from the observations
//! and can be rebuilt, so the main database and its backups stay lean. An observation longer than
//! one window is split into overlapping windows that together cover every character, and it
//! scores as its best window. Nothing is ever cut: the embedder is called with `truncate: false`,
//! so a window it cannot take whole is refused and split again.
//!
//! The index is in memory (one normalised f32 row per window). New observations are embedded by a
//! background thread a few seconds after they land; until then they are found by keyword alone.
//!
//! Chosen by measurement on the frozen canon gold set (evals/canon-gold, 2026-09-30):
//! keyword alone 55.0% hit@10 · snowflake-arctic-embed-l-v2.0 alone 61.7% · fused 61.7% hit@10,
//! 53.3% hit@5, MRR .377 (the graph Oracle scored 50.0% / 46.7% / .360 on the same set).

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Characters per window for text embedded here. Arctic takes 8192 tokens; even dense code at
/// ~2 characters a token stays far below that, and an oversize window is split, never cut.
pub const WINDOW: usize = 6000;
pub const OVERLAP: usize = 400;
/// Query prefix the model was trained with (documents take none).
const QUERY_PREFIX: &str = "query: ";

pub type Result<T> = std::result::Result<T, String>;
/// A window's (start, end) byte span and its vector.
pub type Windowed = Vec<((usize, usize), Vec<f32>)>;

/// Turns text into unit vectors. `Ollama` in production; tests inject their own.
pub trait Embedder: Send + Sync {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn model(&self) -> &str;
    /// One search query's vector, given up after `budget` (the indexer's batches may wait; a person may not).
    fn embed_query(&self, text: &str, budget: Duration) -> Result<Vec<f32>> {
        let _ = budget;
        self.embed(&[text.to_string()])?.pop().ok_or_else(|| "no query vector".to_string())
    }
    /// Whether [`Embedder::rerank`] is configured (`ORACLE_RERANK_URL`).
    fn reranks(&self) -> bool {
        false
    }
    /// One relevance score per document for `query`, higher is better: oracle-embed's cross-encoder (`/api/rerank`).
    fn rerank(&self, query: &str, docs: &[String], budget: Duration) -> Result<Vec<f32>> {
        let _ = (query, docs, budget);
        Err("no reranker configured".into())
    }
}

/// How long a search waits for the reranker before keeping the fused order (measured 3 Nov 2026 on core: 30 passages of
/// about 50 words in 2.2 s on 8 threads).
pub const RERANK_BUDGET: Duration = Duration::from_millis(6000);

/// How long a search waits for its query's vector before answering by words alone (2026-10-10: the CPU embedder took
/// 1.2 s per query, 10-20 s behind indexing backlogs, and seats' tools timed out with nothing).
pub const QUERY_BUDGET: Duration = Duration::from_millis(2500);
/// Query vectors kept, newest last: a search embeds its words once, and a repeated question is not embedded again.
pub const QUERY_CACHE: usize = 256;

pub struct Ollama {
    pub url: String,
    pub model: String,
    /// Where a search's query goes, when it is not `url` (`ORACLE_EMBED_QUERY_URL`): oracle-embed, the same model held
    /// loaded (2026-10-10: through Ollama a query paid ~0.5-1 s of setup per request and waited behind the indexer's
    /// batches; the same vectors, cosine 0.99999, come from oracle-embed in ~0.3-0.5 s).
    pub query_url: Option<String>,
    /// oracle-embed's `/api/rerank` (`ORACLE_RERANK_URL`), when a cross-encoder is loaded there.
    pub rerank_url: Option<String>,
}

// keep_alive: Ollama unloads an idle model after 5 minutes, which would make the first search after a quiet
// spell wait seconds for it to load again.
impl Embedder for Ollama {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let body = serde_json::json!({"model": self.model, "input": texts, "truncate": false,
                                      "options": {"num_ctx": 8192}, "keep_alive": "24h"});
        self.post(body, Duration::from_secs(600), texts.len())
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn embed_query(&self, text: &str, budget: Duration) -> Result<Vec<f32>> {
        let body = serde_json::json!({"model": self.model, "input": [text], "truncate": false,
                                      "options": {"num_ctx": 8192}, "keep_alive": "24h"});
        self.post_to(self.query_url.as_deref().unwrap_or(&self.url), body, budget, 1)?.pop().ok_or_else(|| "no query vector".to_string())
    }
    fn reranks(&self) -> bool {
        self.rerank_url.is_some()
    }
    fn rerank(&self, query: &str, docs: &[String], budget: Duration) -> Result<Vec<f32>> {
        let url = self.rerank_url.as_deref().ok_or("no reranker configured")?;
        let resp = ureq::post(url).timeout(budget).send_json(serde_json::json!({"query": query, "documents": docs}))
            .map_err(|e| match e {
                ureq::Error::Status(code, r) => format!("reranker {code}: {}", r.into_string().unwrap_or_default()),
                other => format!("reranker unreachable: {other}"),
            })?;
        let v: serde_json::Value = resp.into_json().map_err(|e| format!("reranker reply: {e}"))?;
        let scores: Vec<f32> = v["scores"].as_array().ok_or("reranker reply has no scores")?.iter().map(|x| x.as_f64().unwrap_or(f64::MIN) as f32).collect();
        if scores.len() != docs.len() {
            return Err(format!("reranker returned {} scores for {} documents", scores.len(), docs.len()));
        }
        Ok(scores)
    }
}

impl Ollama {
    fn post(&self, body: serde_json::Value, timeout: Duration, n: usize) -> Result<Vec<Vec<f32>>> {
        self.post_to(&self.url, body, timeout, n)
    }

    fn post_to(&self, url: &str, body: serde_json::Value, timeout: Duration, n: usize) -> Result<Vec<Vec<f32>>> {
        let resp = ureq::post(url)
            .timeout(timeout)
            .send_json(body)
            .map_err(|e| match e {
                ureq::Error::Status(code, r) => {
                    format!("embedder {code}: {}", r.into_string().unwrap_or_default())
                }
                other => format!("embedder unreachable: {other}"),
            })?;
        let v: serde_json::Value = resp
            .into_json()
            .map_err(|e| format!("embedder reply: {e}"))?;
        let rows = v["embeddings"]
            .as_array()
            .ok_or("embedder reply has no embeddings")?;
        if rows.len() != n {
            return Err(format!(
                "embedder returned {} vectors for {} texts",
                rows.len(),
                n
            ));
        }
        Ok(rows
            .iter()
            .map(|r| {
                normalise(
                    r.as_array()
                        .map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect())
                        .unwrap_or_default(),
                )
            })
            .collect())
    }
}

pub fn normalise(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

/// Char spans (byte offsets on char boundaries) that together cover ALL of `text`, each at most
/// `win` characters, consecutive spans overlapping by about `overlap`, cut at whitespace when there
/// is some in the back half of the window.
pub fn windows(text: &str, win: usize, overlap: usize) -> Vec<(usize, usize)> {
    let idx: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    let n = idx.len() - 1; // characters
    if n == 0 {
        return vec![(0, 0)];
    }
    let (mut out, mut s) = (Vec::new(), 0usize);
    loop {
        let mut e = (s + win).min(n);
        if e < n {
            if let Some(k) = (s + win / 2..e)
                .rev()
                .find(|&k| text[idx[k]..].starts_with(char::is_whitespace))
            {
                e = k;
            }
        }
        out.push((idx[s], idx[e]));
        if e >= n {
            return out;
        }
        s = (e.saturating_sub(overlap)).max(s + 1);
    }
}

/// Windows are stored keyed by (observation, start). Splitting a refused window can yield two windows
/// with the same start; the longer contains the shorter, so keep it: coverage is unchanged, no row is overwritten.
pub fn dedupe_starts(mut rows: Windowed) -> Windowed {
    rows.sort_by(|x, y| x.0 .0.cmp(&y.0 .0).then(y.0 .1.cmp(&x.0 .1)));
    rows.dedup_by(|later, kept| later.0 .0 == kept.0 .0);
    rows
}

/// f16 bits → f32 (the backfill stores float16; no extra crate for one conversion).
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let frac = (h & 0x3ff) as f32;
    match exp {
        0 => sign * frac * 2f32.powi(-24),
        31 => {
            if frac == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + frac / 1024.0) * 2f32.powi(exp - 15),
    }
}

pub fn f32_to_f16(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32 - 127 + 15;
    let man = b & 0x7f_ffff;
    if exp <= 0 {
        if exp < -10 {
            return sign;
        }
        let m = (man | 0x80_0000) >> (1 - exp);
        return sign | ((m + 0x1000) >> 13) as u16;
    }
    if exp >= 31 {
        return sign | 0x7c00;
    }
    let r = sign as u32 | ((exp as u32) << 10) | (man >> 13);
    (r + ((man >> 12) & 1)) as u16 // round half up on the dropped bit
}

fn decode(blob: &[u8]) -> Vec<f32> {
    blob.as_chunks::<2>().0.iter()
        .map(|c| f16_to_f32(u16::from_le_bytes(*c)))
        .collect()
}

fn encode(v: &[f32]) -> Vec<u8> {
    v.iter()
        .flat_map(|x| f32_to_f16(*x).to_le_bytes())
        .collect()
}

/// One row per window: which observation (seq) and its unit vector.
#[derive(Default)]
pub struct Index {
    pub dim: usize,
    pub seqs: Vec<i64>,
    pub data: Vec<f32>,
}

impl Index {
    fn push(&mut self, seq: i64, v: &[f32]) {
        if self.dim == 0 {
            self.dim = v.len();
        }
        if v.len() == self.dim {
            self.seqs.push(seq);
            self.data.extend_from_slice(v);
        }
    }

    /// The `k` observations whose best window is most similar to `q`, best first.
    pub fn top(&self, q: &[f32], k: usize) -> Vec<(i64, f32)> {
        if self.dim == 0 || q.len() != self.dim {
            return vec![];
        }
        let mut best: HashMap<i64, f32> = HashMap::new();
        for (row, seq) in self.data.chunks_exact(self.dim).zip(&self.seqs) {
            let s: f32 = row.iter().zip(q).map(|(a, b)| a * b).sum();
            let e = best.entry(*seq).or_insert(f32::MIN);
            if s > *e {
                *e = s;
            }
        }
        let mut all: Vec<(i64, f32)> = best.into_iter().collect();
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
        all.truncate(k);
        all
    }
}

/// Reciprocal-rank fusion: each list votes 1/(k+rank) for its items; best first with its fused score.
/// Ties keep the order in which items were first seen.
pub fn rrf(lists: &[Vec<i64>], k: f64) -> Vec<(i64, f64)> {
    let mut score: HashMap<i64, f64> = HashMap::new();
    let mut first: Vec<i64> = Vec::new();
    for list in lists {
        for (r, seq) in list.iter().enumerate() {
            let e = score.entry(*seq).or_insert_with(|| {
                first.push(*seq);
                0.0
            });
            *e += 1.0 / (k + r as f64 + 1.0);
        }
    }
    let pos: HashMap<i64, usize> = first.iter().enumerate().map(|(i, s)| (*s, i)).collect();
    first.sort_by(|a, b| score[b].total_cmp(&score[a]).then(pos[a].cmp(&pos[b])));
    first.into_iter().map(|s| (s, score[&s])).collect()
}

/// RRF's k, and how many candidates each side contributes (the measured configuration).
pub const RRF_K: f64 = 60.0;
pub const SIDE: usize = 50;

/// Why meaning search is off in this process, set once by `Semantic::from_env`; `None` when it is on (or
/// never configured through the environment, as in tests). Search reports it on its engine line.
pub static OFF_REASON: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub struct Semantic {
    pub path: String,
    pub embedder: Box<dyn Embedder>,
    pub index: RwLock<Index>,
    /// Last indexer outcome, for /health: (at_ms, message).
    pub status: RwLock<(i64, String)>,
    /// Recent query vectors, oldest first (`QUERY_CACHE`).
    pub queries: std::sync::Mutex<std::collections::VecDeque<(String, Vec<f32>)>>,
}

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT);
CREATE TABLE IF NOT EXISTS vec(id TEXT, seq INTEGER, a INTEGER, b INTEGER, v BLOB, PRIMARY KEY(id, a));
CREATE INDEX IF NOT EXISTS vec_seq ON vec(seq);
CREATE TABLE IF NOT EXISTS done(id TEXT PRIMARY KEY, windows INTEGER);
";

impl Semantic {
    /// `ORACLE_VEC_DB` turns meaning search on; `ORACLE_EMBED_URL` / `ORACLE_EMBED_MODEL` pick the embedder.
    /// `ORACLE_EMBED=off` turns it off by policy even when a vector store is configured (the embedder service
    /// was disabled on purpose, 2026-10-03), so searches say "off by policy" instead of failing a connection
    /// on every query and calling that "unavailable".
    pub fn from_env() -> Option<Arc<Semantic>> {
        if std::env::var("ORACLE_EMBED").is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
            let _ = OFF_REASON.set("disabled by policy (ORACLE_EMBED=off)".into());
            return None;
        }
        let Some(path) = std::env::var("ORACLE_VEC_DB").ok().filter(|p| !p.is_empty()) else {
            let _ = OFF_REASON.set("not configured (ORACLE_VEC_DB unset)".into());
            return None;
        };
        let embedder = Ollama {
            url: std::env::var("ORACLE_EMBED_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:11434/api/embed".into()),
            model: std::env::var("ORACLE_EMBED_MODEL")
                .unwrap_or_else(|_| "snowflake-arctic-embed2".into()),
            query_url: std::env::var("ORACLE_EMBED_QUERY_URL").ok().filter(|u| !u.trim().is_empty()),
            rerank_url: std::env::var("ORACLE_RERANK_URL").ok().filter(|u| !u.trim().is_empty()),
        };
        match Semantic::open(&path, Box::new(embedder)) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                eprintln!("meaning search off: {e}");
                let _ = OFF_REASON.set(format!("vector store failed to open: {e}"));
                None
            }
        }
    }

    pub fn open(path: &str, embedder: Box<dyn Embedder>) -> Result<Semantic> {
        let c = Connection::open(path).map_err(|e| format!("{path}: {e}"))?;
        c.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        let mut idx = Index::default();
        {
            let mut st = c
                .prepare("SELECT seq, v FROM vec ORDER BY seq, a")
                .map_err(|e| e.to_string())?;
            let rows = st
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (seq, blob) = row.map_err(|e| e.to_string())?;
                idx.push(seq, &decode(&blob));
            }
        }
        eprintln!(
            "meaning search: {} windows loaded from {path}",
            idx.seqs.len()
        );
        Ok(Semantic {
            path: path.into(),
            embedder,
            index: RwLock::new(idx),
            status: RwLock::new((0, "not run yet".into())),
            queries: Default::default(),
        })
    }

    /// Best `k` observations by meaning for a search query.
    /// Whether search can rerank its candidates (`ORACLE_RERANK_URL`).
    pub fn reranks(&self) -> bool {
        self.embedder.reranks()
    }

    /// One relevance score per document for `q` from the cross-encoder, within [`RERANK_BUDGET`].
    pub fn rerank(&self, q: &str, docs: &[String]) -> Result<Vec<f32>> {
        self.embedder.rerank(q, docs, RERANK_BUDGET)
    }

    pub fn query(&self, q: &str, k: usize) -> Result<Vec<(i64, f32)>> {
        let v = self.query_vector(q)?;
        Ok(self.index.read().map_err(|_| "index poisoned")?.top(&v, k))
    }

    /// Best `k` by meaning only when this query's vector is already at hand (never waits on the embedder): for
    /// readouts beside a search, which must not wait a second time when the search's own embedding missed.
    pub fn query_cached(&self, q: &str, k: usize) -> Option<Vec<(i64, f32)>> {
        let text = format!("{QUERY_PREFIX}{q}");
        let v = self.queries.lock().ok()?.iter().find(|(t, _)| *t == text)?.1.clone();
        Some(self.index.read().ok()?.top(&v, k))
    }

    /// A query's vector: from the recent ones, else embedded within `QUERY_BUDGET` (past it, an error, and the search
    /// answers by words alone and says so).
    pub fn query_vector(&self, q: &str) -> Result<Vec<f32>> {
        let text = format!("{QUERY_PREFIX}{q}");
        if let Ok(cache) = self.queries.lock() {
            if let Some((_, v)) = cache.iter().find(|(t, _)| *t == text) {
                return Ok(v.clone());
            }
        }
        let v = self.embedder.embed_query(&text, QUERY_BUDGET).map_err(|e| format!("query vector not ready in {} ms: {e}", QUERY_BUDGET.as_millis()))?;
        if let Ok(mut cache) = self.queries.lock() {
            if cache.len() >= QUERY_CACHE {
                cache.pop_front();
            }
            cache.push_back((text, v.clone()));
        }
        Ok(v)
    }

    /// The index's dimension, its windows and the distinct observations they cover (the scoreboard's coverage check).
    pub fn coverage(&self) -> (usize, usize, usize) {
        self.index
            .read()
            .map(|i| (i.dim, i.seqs.len(), i.seqs.iter().collect::<std::collections::HashSet<_>>().len()))
            .unwrap_or((0, 0, 0))
    }

    pub fn windows_loaded(&self) -> usize {
        self.index.read().map(|i| i.seqs.len()).unwrap_or(0)
    }

    /// The highest record seq the index holds: what a room page compares its newest record against ("N not yet in
    /// meaning search"). None while nothing is loaded.
    pub fn indexed_through(&self) -> Option<i64> {
        self.index.read().ok().and_then(|i| i.seqs.iter().copied().max())
    }

    /// Embed every window of one observation; a window the embedder refuses is split and retried.
    fn embed_full(&self, text: &str) -> Result<Windowed> {
        let spans = windows(text, WINDOW, OVERLAP);
        let texts: Vec<String> = spans.iter().map(|&(a, b)| text[a..b].to_string()).collect();
        match self.embedder.embed(&texts) {
            Ok(vs) => Ok(spans.into_iter().zip(vs).collect()),
            Err(e) if spans.len() == 1 && text.chars().count() > 1 => {
                let (a, b) = spans[0];
                let half = text[a..b].chars().count() / 2;
                let sub = windows(&text[a..b], half.max(1), OVERLAP.min(half / 2));
                if sub.len() < 2 {
                    return Err(e);
                }
                let mut out = Vec::new();
                for (sa, sb) in sub {
                    for ((x, y), v) in self.embed_full(&text[a + sa..a + sb])? {
                        out.push(((a + sa + x, a + sa + y), v));
                    }
                }
                Ok(out)
            }
            Err(_) if spans.len() > 1 => {
                let mut out = Vec::new();
                for (a, b) in spans {
                    for ((x, y), v) in self.embed_full(&text[a..b])? {
                        out.push(((a + x, a + y), v));
                    }
                }
                Ok(out)
            }
            Err(e) => Err(e),
        }
    }

    /// One indexer pass: embed observations not yet indexed (oldest first, at most `max`), and drop
    /// the vectors of observations that no longer exist. Returns how many were embedded.
    pub fn index_pass(&self, main_db: &str, max: usize) -> Result<usize> {
        let main = Connection::open_with_flags(
            main_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| e.to_string())?;
        let mut vc = Connection::open(&self.path).map_err(|e| e.to_string())?;
        vc.busy_timeout(Duration::from_secs(10))
            .map_err(|e| e.to_string())?;
        let high: i64 = vc
            .query_row("SELECT coalesce(max(seq),0) FROM vec", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        // New observations first (above the high-water mark), then any older one never indexed.
        let mut todo: Vec<(String, i64, String)> = {
            let mut st = main
                .prepare("SELECT id, seq, coalesce(content,'') FROM observations WHERE seq>?1 ORDER BY seq LIMIT ?2")
                .map_err(|e| e.to_string())?;
            let rows = st
                .query_map(params![high, max as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .map_err(|e| e.to_string())?;
            rows.collect::<std::result::Result<_, _>>()
                .map_err(|e| e.to_string())?
        };
        todo.retain(|(_, _, c)| !c.trim().is_empty());
        let mut n = 0;
        for (id, seq, content) in todo {
            let known: Option<i64> = vc
                .query_row("SELECT windows FROM done WHERE id=?1", [&id], |r| r.get(0))
                .optional()
                .map_err(|e| e.to_string())?;
            if known.is_some() {
                continue;
            }
            let rows = dedupe_starts(self.embed_full(&content)?);
            let tx = vc.transaction().map_err(|e| e.to_string())?;
            for ((a, b), v) in &rows {
                tx.execute(
                    "INSERT OR REPLACE INTO vec VALUES(?1,?2,?3,?4,?5)",
                    params![id, seq, *a as i64, *b as i64, encode(v)],
                )
                .map_err(|e| e.to_string())?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO done VALUES(?1,?2)",
                params![id, rows.len() as i64],
            )
            .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            let mut idx = self.index.write().map_err(|_| "index poisoned")?;
            for (_, v) in &rows {
                idx.push(seq, v);
            }
            n += 1;
        }
        Ok(n)
    }

    /// Remove vectors whose observation was deleted (live-state compaction deletes superseded rows).
    pub fn prune(&self, main_db: &str) -> Result<usize> {
        let vc = Connection::open(&self.path).map_err(|e| e.to_string())?;
        vc.execute(
            "ATTACH DATABASE ?1 AS m",
            [format!("file:{main_db}?mode=ro")],
        )
        .map_err(|e| e.to_string())?;
        let gone: Vec<i64> = {
            let mut st = vc
                .prepare("SELECT DISTINCT seq FROM vec WHERE seq NOT IN (SELECT seq FROM m.observations)")
                .map_err(|e| e.to_string())?;
            let r = st.query_map([], |r| r.get(0)).map_err(|e| e.to_string())?;
            r.collect::<std::result::Result<_, _>>()
                .map_err(|e| e.to_string())?
        };
        if gone.is_empty() {
            return Ok(0);
        }
        for seq in &gone {
            vc.execute(
                "DELETE FROM done WHERE id IN (SELECT id FROM vec WHERE seq=?1)",
                [seq],
            )
            .map_err(|e| e.to_string())?;
            vc.execute("DELETE FROM vec WHERE seq=?1", [seq])
                .map_err(|e| e.to_string())?;
        }
        let dead: std::collections::HashSet<i64> = gone.iter().copied().collect();
        let mut idx = self.index.write().map_err(|_| "index poisoned")?;
        let dim = idx.dim;
        let (mut seqs, mut data) = (
            Vec::with_capacity(idx.seqs.len()),
            Vec::with_capacity(idx.data.len()),
        );
        for (row, seq) in idx.data.chunks_exact(dim.max(1)).zip(&idx.seqs) {
            if !dead.contains(seq) {
                seqs.push(*seq);
                data.extend_from_slice(row);
            }
        }
        idx.seqs = seqs;
        idx.data = data;
        Ok(gone.len())
    }

    /// Background indexer: every few seconds embed what is new; every ten minutes prune deletions.
    pub fn spawn_indexer(self: &Arc<Self>, main_db: String) {
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name("oracle-indexer".into())
            .spawn(move || {
                let mut last_prune = 0i64;
                loop {
                    let now = oracle_proto::now_ms();
                    let msg = match me.index_pass(&main_db, 64) {
                        Ok(0) => None,
                        Ok(n) => Some(format!("embedded {n}")),
                        Err(e) => Some(format!("error: {e}")),
                    };
                    if now - last_prune > 600_000 {
                        last_prune = now;
                        if let Ok(n) = me.prune(&main_db) {
                            if n > 0 {
                                eprintln!("meaning search: pruned {n} deleted observations");
                            }
                        }
                    }
                    let busy = matches!(&msg, Some(m) if m.starts_with("embedded"));
                    if let Some(m) = msg {
                        if let Ok(mut s) = me.status.write() {
                            *s = (now, m);
                        }
                    }
                    std::thread::sleep(Duration::from_secs(if busy { 1 } else { 5 }));
                }
            })
            .expect("spawn indexer");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic toy embedder: a bag of hashed words, so similar words land close.
    struct Bag;
    impl Embedder for Bag {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0f32; 64];
                    for w in t
                        .to_lowercase()
                        .split(|c: char| !c.is_alphanumeric())
                        .filter(|w| !w.is_empty() && *w != "query")
                    {
                        let h = w
                            .bytes()
                            .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
                        v[(h % 64) as usize] += 1.0;
                    }
                    normalise(v)
                })
                .collect())
        }
        fn model(&self) -> &str {
            "bag"
        }
    }

    /// Refuses any text longer than 50 characters, like an embedder with truncate:false.
    struct Strict;
    impl Embedder for Strict {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            if texts.iter().any(|t| t.chars().count() > 50) {
                return Err("input exceeds context".into());
            }
            Bag.embed(texts)
        }
        fn model(&self) -> &str {
            "strict"
        }
    }

    /// Counts its calls; the second answers after the budget, like a CPU embedder behind an indexing backlog.
    struct Counting(std::sync::atomic::AtomicUsize);
    impl Embedder for Counting {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Bag.embed(texts)
        }
        fn model(&self) -> &str {
            "counting"
        }
        fn embed_query(&self, text: &str, budget: Duration) -> Result<Vec<f32>> {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                return Err(format!("timed out after {} ms", budget.as_millis()));
            }
            Ok(Bag.embed(&[text.to_string()])?.remove(0))
        }
    }

    #[test]
    fn a_query_is_embedded_once_and_a_slow_embedder_fails_fast_instead_of_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let s = Semantic::open(dir.path().join("vec.db").to_str().unwrap(), Box::new(Counting(Default::default()))).unwrap();
        let first = s.query_vector("dot mascot").unwrap();
        assert_eq!(s.query_vector("dot mascot").unwrap(), first, "the second ask of the same words is not embedded again");
        // A new question reaches the (now slow) embedder and comes back as an error the search can fall back from.
        let e = s.query_vector("something new").unwrap_err();
        assert!(e.contains("not ready in 2500 ms"), "{e}");
        assert!(s.query_cached("something new", 1).is_none(), "a readout never waits on the embedder");
        assert!(s.query_cached("dot mascot", 1).is_some());
    }

    fn covered(text: &str, spans: &[(usize, usize)]) -> bool {
        let mut hit = vec![false; text.len()];
        for &(a, b) in spans {
            hit[a..b].iter_mut().for_each(|h| *h = true);
        }
        text.char_indices().all(|(i, _)| hit[i])
    }

    #[test]
    fn windows_cover_every_character_and_respect_the_limit() {
        let text = "héllo wörld ".repeat(900) + "tail-without-spaces-".repeat(80).as_str();
        let spans = windows(&text, 500, 40);
        assert!(spans.len() > 10);
        assert!(
            covered(&text, &spans),
            "a character fell outside every window"
        );
        for &(a, b) in &spans {
            assert!(text[a..b].chars().count() <= 500);
        }
        assert_eq!(windows("short", 500, 40), vec![(0, 5)]);
        assert_eq!(windows("", 500, 40), vec![(0, 0)]);
    }

    #[test]
    fn f16_round_trips_unit_vector_values() {
        for x in [0.0f32, 1.0, -1.0, 0.5, 0.0312, -0.2718, 1e-5, 0.999] {
            let y = f16_to_f32(f32_to_f16(x));
            assert!((x - y).abs() <= x.abs() * 1e-3 + 1e-6, "{x} -> {y}");
        }
    }

    #[test]
    fn rrf_rewards_agreement_and_keeps_first_seen_order_on_ties() {
        let fused: Vec<i64> = rrf(&[vec![1, 2, 3], vec![3, 1, 9]], 60.0)
            .into_iter()
            .map(|x| x.0)
            .collect();
        assert_eq!(fused[0], 1); // rank 1 + rank 2
        assert_eq!(fused[1], 3); // rank 3 + rank 1
        assert!(fused.contains(&9) && fused.contains(&2));
        let one: Vec<i64> = rrf(&[vec![5, 6]], 60.0).into_iter().map(|x| x.0).collect();
        assert_eq!(one, vec![5, 6]);
    }

    #[test]
    fn an_observation_scores_as_its_best_window() {
        let mut idx = Index::default();
        idx.push(1, &normalise(vec![1.0, 0.0]));
        idx.push(1, &normalise(vec![0.0, 1.0]));
        idx.push(2, &normalise(vec![0.7, 0.7]));
        let top = idx.top(&normalise(vec![0.0, 1.0]), 5);
        assert_eq!(top[0].0, 1);
        assert!((top[0].1 - 1.0).abs() < 1e-6);
        assert_eq!(top.len(), 2, "one entry per observation, not per window");
    }

    fn main_db(dir: &std::path::Path, rows: &[(i64, &str, &str)]) -> String {
        let p = dir.join("main.db");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE observations(seq INTEGER PRIMARY KEY, id TEXT, content TEXT);",
        )
        .unwrap();
        for (seq, id, content) in rows {
            c.execute(
                "INSERT INTO observations VALUES(?1,?2,?3)",
                params![seq, id, content],
            )
            .unwrap();
        }
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn the_indexer_embeds_new_rows_in_full_and_prunes_deleted_ones() {
        let dir = tempfile::tempdir().unwrap();
        let long = "alpha beta gamma delta epsilon zeta ".repeat(20); // 720 chars, far over Strict's 50
        let db = main_db(
            dir.path(),
            &[
                (1, "OBS-1", "the doorbell rings for astra"),
                (2, "OBS-2", &long),
                (3, "OBS-3", "   "),
            ],
        );
        let s = Semantic::open(
            dir.path().join("vec.db").to_str().unwrap(),
            Box::new(Strict),
        )
        .unwrap();
        assert_eq!(
            s.index_pass(&db, 10).unwrap(),
            2,
            "blank content is skipped"
        );
        let c = Connection::open(dir.path().join("vec.db")).unwrap();
        let spans: Vec<(i64, i64)> = c
            .prepare("SELECT a, b FROM vec WHERE id='OBS-2' ORDER BY a")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let spans: Vec<(usize, usize)> = spans
            .into_iter()
            .map(|(a, b)| (a as usize, b as usize))
            .collect();
        assert!(spans.len() > 10, "the long row was split, not cut");
        assert!(
            covered(&long, &spans),
            "every character of the long row is in some window"
        );
        assert_eq!(
            s.index_pass(&db, 10).unwrap(),
            0,
            "a second pass has nothing new"
        );
        assert_eq!(s.query("astra doorbell", 1).unwrap()[0].0, 1);

        Connection::open(&db)
            .unwrap()
            .execute("DELETE FROM observations WHERE seq=1", [])
            .unwrap();
        assert_eq!(s.prune(&db).unwrap(), 1);
        assert!(s
            .query("astra doorbell", 5)
            .unwrap()
            .iter()
            .all(|(seq, _)| *seq != 1));
        let reopened =
            Semantic::open(dir.path().join("vec.db").to_str().unwrap(), Box::new(Bag)).unwrap();
        assert_eq!(
            reopened.windows_loaded(),
            s.windows_loaded(),
            "the file holds what memory holds"
        );
    }
}
