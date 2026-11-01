//! The recall scoreboard (docs/RECALL-SCOREBOARD.md): the instrument that measures recall, proven on itself before any
//! change to `/search` counts as a fix. A signed PIN in #oracle freezes the gold set's hash, the store's sequence cutoff,
//! the arms, the metrics and the seed; `oracle scoreboard --pin <id>` refuses to run unless everything matches and the
//! pin's signer is trusted (Grok's BUGS learnings). Stage 1 arms are the instrument:
//! `bm25` (today's `/search`), `aa` (the same run twice: its two tables must agree) and `shuffled` (the gold's truth
//! rotated to the next question: the negative control). Every framing is a row; an error is a row with its reason.
use crate::semantic::Semantic;
use crate::store::{Observation, Result as StoreResult, Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::Instant;

pub const ARMS: [&str; 14] = [
    "bm25", "aa", "shuffled", "bm25-messages", "live", "live-aa",
    "hybrid:and-first", "hybrid:and3", "hybrid:keyword", "hybrid:and-top", "hybrid:wrrf", "hybrid:k10",
    "rerank:llm", "rerank:llm-aa",
];

/// Whether an arm reorders `live`'s candidates with a reranker (docs/RECALL-SCOREBOARD.md 2e).
pub fn is_rerank(arm: &str) -> bool {
    arm.starts_with("rerank:")
}

/// The reranker's instructions. Their sha256, with the candidate format's, is pinned: a changed prompt is a new arm.
pub const RERANK_PROMPT: &str = "You order search results from a shared memory of signed messages. You get a question and numbered candidate records, each with its room, author, date and id, then its whole text. Order the candidates from the record that best answers the question to the least useful. Judge by whether the record answers the question, not by how many words it shares with it. Reply with the candidate numbers only, comma-separated, most useful first. You may leave out records that do not answer the question.";
/// How each candidate is shown to the reranker (hashed with the prompt).
pub const RERANK_FORMAT: &str = "v1: 'Question: <framing>' then for each candidate '[<n>] #<room> · <author> · <YYYY-MM-DD> · <id>' and its whole content";

/// The sha256 a pin's `rerank.prompt_sha256` must equal.
pub fn rerank_prompt_sha() -> String {
    oracle_proto::sha_hex(format!("{RERANK_PROMPT}\n{RERANK_FORMAT}").as_bytes())
}

/// One candidate as the reranker sees it: never a prefix of the text.
#[derive(Debug, Clone)]
pub struct Cand {
    pub id: String,
    pub room: String,
    pub author: String,
    pub date: String,
    pub text: String,
}

/// Tokens one reranker call used.
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
}

/// A reranker: the candidate ids in its order (it may leave some out), and what the call used.
pub trait Rerank {
    fn order(&self, framing: &str, cands: &[Cand]) -> Result<(Vec<String>, Usage), String>;
}

/// The message the reranker reads for one framing.
pub fn rerank_message(framing: &str, cands: &[Cand]) -> String {
    let mut m = format!("Question: {framing}\n\n{} candidates:\n", cands.len());
    for (i, c) in cands.iter().enumerate() {
        m.push_str(&format!("\n[{}] #{} · {} · {} · {}\n{}\n", i + 1, c.room, c.author, c.date, c.id, c.text));
    }
    m
}

/// The candidate numbers in a reply, in order, each once, as ids; anything else in the reply is ignored.
pub fn parse_order(reply: &str, cands: &[Cand]) -> Vec<String> {
    let mut seen = HashSet::new();
    reply
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|t| t.parse::<usize>().ok())
        .filter(|n| *n >= 1 && *n <= cands.len() && seen.insert(*n))
        .map(|n| cands[n - 1].id.clone())
        .collect()
}

/// The reranker's order first, then the candidates it left out in `live`'s order; at most `want`.
pub fn merge_order(order: &[String], cands: &[Cand], want: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in order.iter().chain(cands.iter().map(|c| &c.id)) {
        if !out.contains(id) && cands.iter().any(|c| &c.id == id) {
            out.push(id.clone());
        }
    }
    out.truncate(want);
    out
}

/// Claude through the AXXIS vault's `anthropic` capability, over its unix socket, with a scoped token (never printed).
pub struct VaultRerank {
    pub socket: String,
    pub token: String,
    pub model: String,
}

impl Rerank for VaultRerank {
    fn order(&self, framing: &str, cands: &[Cand]) -> Result<(Vec<String>, Usage), String> {
        let body = json!({"model": self.model, "max_tokens": 400, "system": RERANK_PROMPT,
            "messages": [{"role": "user", "content": rerank_message(framing, cands)}]})
        .to_string();
        let reply = unix_post(&self.socket, "/capability/anthropic/messages", &self.token, &body)?;
        let v: Value = serde_json::from_str(&reply).map_err(|e| format!("the vault's reply is not JSON: {e}"))?;
        let text = v["content"].as_array().and_then(|a| a.iter().find_map(|c| c["text"].as_str())).ok_or("the reply has no text")?;
        let usage = Usage { input: v["usage"]["input_tokens"].as_u64().unwrap_or(0), output: v["usage"]["output_tokens"].as_u64().unwrap_or(0) };
        Ok((parse_order(text, cands), usage))
    }
}

#[cfg(not(unix))]
fn unix_post(socket: &str, _path: &str, _token: &str, _body: &str) -> Result<String, String> {
    Err(format!("vault socket {socket}: a Unix socket, which this system does not have"))
}

/// One HTTP/1.1 POST over a unix socket, the body of a 200 back (Content-Length or chunked).
#[cfg(unix)]
fn unix_post(socket: &str, path: &str, token: &str, body: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    let mut st = std::os::unix::net::UnixStream::connect(socket).map_err(|e| format!("vault socket {socket}: {e}"))?;
    st.set_read_timeout(Some(std::time::Duration::from_secs(300))).map_err(|e| e.to_string())?;
    let head = format!("POST {path} HTTP/1.1\r\nHost: vault\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    st.write_all(head.as_bytes()).and_then(|_| st.write_all(body.as_bytes())).map_err(|e| format!("vault write: {e}"))?;
    let mut raw = Vec::new();
    st.read_to_end(&mut raw).map_err(|e| format!("vault read: {e}"))?;
    http_body(&raw)
}

/// The body of an HTTP/1.1 response if its status is 200, decoding chunked transfer; otherwise the status and the
/// start of the body as the error.
pub fn http_body(raw: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("no HTTP header in the reply")?;
    let status: u16 = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or("no HTTP status in the reply")?;
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
        let (mut out, mut rest) = (String::new(), body);
        loop {
            let (size, after) = rest.split_once("\r\n").ok_or("a chunk without its size line")?;
            let n = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16).map_err(|_| "a bad chunk size")?;
            if n == 0 {
                break;
            }
            let chunk = after.get(..n).ok_or("a chunk shorter than its size")?;
            out.push_str(chunk);
            rest = after.get(n..).unwrap_or("").trim_start_matches("\r\n");
        }
        out
    } else {
        body.to_string()
    };
    if status != 200 {
        return Err(format!("vault {status}: {}", body.chars().take(300).collect::<String>()));
    }
    Ok(body)
}

/// The words of a text as the gates compare them: lowercase, letters and digits.
fn words(t: &str) -> Vec<String> {
    t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect()
}

/// Emmy's leakage gate: a framing whose words already stand in a record, in order,
/// is found by copying, not by search. Each framing as a phrase on the content column of the full-text index (porter
/// stems, so a little looser than exact); up to 20 ids each. Framings of under four words are listed apart: a short
/// keyword query is meant to be found verbatim.
pub fn leaks(s: &Store, gold: &[Question]) -> anyhow::Result<Value> {
    let (mut long, mut short) = (Vec::new(), 0usize);
    let mut checked = 0usize;
    for q in gold {
        for f in &q.framings {
            let w = words(f);
            if w.is_empty() {
                continue;
            }
            checked += 1;
            let phrase = format!("content : \"{}\"", w.join(" "));
            let ids: Vec<String> = s
                .with_conn(|c| {
                    let mut st = c.prepare("SELECT o.id FROM obs_fts f JOIN observations o ON o.seq = f.rowid WHERE obs_fts MATCH ?1 LIMIT 20")?;
                    let rows = st.query_map([&phrase], |r| r.get::<_, String>(0))?;
                    Ok(rows.filter_map(|r| r.ok()).collect())
                })
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            if ids.is_empty() {
                continue;
            }
            if w.len() < 4 {
                short += 1;
            } else {
                long.push(json!({"question": q.code, "split": q.split, "source": q.source, "framing": f, "words": w.len(), "ids": ids}));
            }
        }
    }
    Ok(json!({"framings_checked": checked, "found_verbatim": long, "short_queries_found": short}))
}

/// Emmy's gate: pairs of questions across `dev` and `test` that share a truth id, or whose framings share at least half
/// their words (Jaccard over words of three letters or more), so a choice tuned on dev is not tested on its twin.
pub fn near_duplicates(gold: &[Question]) -> Vec<Value> {
    let bag = |q: &Question| -> HashSet<String> { q.framings.iter().flat_map(|f| words(f)).filter(|w| w.len() >= 3).collect() };
    let mut out = Vec::new();
    for a in gold.iter().filter(|q| q.split.as_deref() == Some("dev")) {
        for b in gold.iter().filter(|q| q.split.as_deref() == Some("test")) {
            let shared: Vec<&String> = a.relevant.iter().filter(|r| b.relevant.contains(r)).collect();
            let (wa, wb) = (bag(a), bag(b));
            let union = wa.union(&wb).count().max(1);
            let jac = wa.intersection(&wb).count() as f64 / union as f64;
            if !shared.is_empty() || jac >= 0.5 {
                out.push(json!({"dev": a.code, "test": b.code, "shared_truth": shared, "word_jaccard": (jac * 1000.0).round() / 1000.0}));
            }
        }
    }
    out
}

/// What a run of the rerank arms sent out (Emmy): for every framing the run would
/// score (the split's questions with a positive at the cutoff), `live`'s top `candidates` as the arm built them, each
/// sent once per rerank arm named. Ids, rooms and counts only, never text; no reranker is called.
pub fn egress_inventory(s: &Store, sem: Option<&Semantic>, gold: &[Question], pin: &Pin, split: Option<&str>, rerank_arms: usize) -> anyhow::Result<Value> {
    let r = pin.rerank.as_ref().ok_or_else(|| anyhow::anyhow!("refused: the pin has no rerank"))?;
    let hit_cutoff = if pin.store_sha256.is_some() { i64::MAX } else { pin.cutoff_seq };
    let mut sends: BTreeMap<String, (String, usize)> = BTreeMap::new();
    let (mut calls, mut pairs) = (0usize, 0usize);
    for q in gold.iter().filter(|q| split.is_none() || q.split.as_deref() == split) {
        if !q.relevant.iter().any(|id| matches!(s.get(id), Ok(o) if o.seq <= pin.cutoff_seq)) {
            continue;
        }
        for f in &q.framings {
            let hits = s.search_ranked(f, None, None, r.candidates as i64, sem, crate::store::Ranker::parse(None)).map_err(|e| anyhow::anyhow!(e.to_string()))?.0;
            let cands: Vec<_> = hits.into_iter().filter(|h| h.obs.seq <= hit_cutoff).take(r.candidates).collect();
            if cands.is_empty() {
                continue; // no call is made for an empty list
            }
            calls += rerank_arms;
            pairs += cands.len() * rerank_arms;
            for h in cands {
                sends.entry(h.obs.id).or_insert((h.obs.channel, 0)).1 += rerank_arms;
            }
        }
    }
    let mut by_room: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (room, n) in sends.values() {
        let e = by_room.entry(room.clone()).or_default();
        e.0 += 1;
        e.1 += n;
    }
    Ok(json!({
        "schema": "egress-inventory/1", "model": r.model, "candidates": r.candidates, "split": split.unwrap_or("all"),
        "rerank_arms": rerank_arms, "calls": calls, "request_document_pairs": pairs, "unique_records": sends.len(),
        "by_room": by_room.iter().map(|(k, (u, n))| (k.clone(), json!({"records": u, "sends": n}))).collect::<serde_json::Map<_, _>>(),
        "records": sends.iter().map(|(id, (room, n))| json!({"id": id, "room": room, "sends": n})).collect::<Vec<_>>(),
    }))
}

/// `rerank:*`: `live`'s top `n` for the framing (whole texts), ordered by the reranker; the ids at or below the cutoff,
/// best first, at most `want`.
pub fn answer_rerank(s: &Store, sem: Option<&Semantic>, rr: &dyn Rerank, n: usize, framing: &str, cutoff: i64, want: usize) -> anyhow::Result<(Vec<String>, Usage)> {
    let hits = s.search_ranked(framing, None, None, n as i64, sem, crate::store::Ranker::parse(None)).map_err(|e| anyhow::anyhow!(e.to_string()))?.0;
    let cands: Vec<Cand> = hits
        .into_iter()
        .filter(|h| h.obs.seq <= cutoff)
        .take(n)
        .map(|h| Cand { date: crate::store::ymd(h.obs.created_ms), id: h.obs.id, room: h.obs.channel, author: h.obs.author, text: h.obs.content })
        .collect();
    if cands.is_empty() {
        return Ok((vec![], Usage::default()));
    }
    let (order, usage) = rr.order(framing, &cands).map_err(|e| anyhow::anyhow!("rerank: {e}"))?;
    Ok((merge_order(&order, &cands, want), usage))
}

/// Whether an arm reads the vector store (stage 2, docs/RECALL-SCOREBOARD.md 2c).
pub fn needs_meaning(arm: &str) -> bool {
    arm == "live" || arm == "live-aa" || arm.starts_with("hybrid:") || is_rerank(arm)
}

/// The sha256 of a file, streamed (the store is over a gigabyte).
pub fn file_sha256(path: &str) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// The fixed text whose query vector pins the embedder (Emmy): the vectors are frozen
/// in the snapshot, but each framing is embedded by a service at run time.
pub const PROBE: &str = "oracle scoreboard probe: what did Blaze decide about the bridge";

/// The probe's query vector as a sha256 of its little-endian f32 bytes, and its dimension.
pub fn probe(sem: &Semantic) -> anyhow::Result<(String, usize)> {
    let v = sem.query_vector(PROBE).map_err(|e| anyhow::anyhow!("refused: the probe could not be embedded: {e}"))?;
    let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    Ok((oracle_proto::sha_hex(&bytes), v.len()))
}

/// The files a run reads, hashed before the store is opened (opening may checkpoint): the run refuses unless they are
/// the pin's snapshot (Emmy: one frozen store and its matching vector index).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub store_sha256: Option<String>,
    pub vectors_sha256: Option<String>,
}
pub const K: [usize; 3] = [1, 5, 10];
pub const MIN_POSITIVES: usize = 20;
pub const RESAMPLES: usize = 1000;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Question {
    pub code: String,
    pub framings: Vec<String>,
    #[serde(rename = "seed_ids")]
    pub relevant: Vec<String>,
    /// Where the question came from (gold 100: `canon`, `behaviour`, `memory`); the receipt has a table per source.
    #[serde(default)]
    pub source: Option<String>,
    /// `dev` (tune on it) or `test` (scored once, on the final candidate); `--split` scores one half.
    #[serde(default)]
    pub split: Option<String>,
}

/// The gold file: one question per line, as `/var/lib/oracle-search-eval/gold.jsonl` has it.
pub fn read_gold(text: &str) -> anyhow::Result<Vec<Question>> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let q: Question = serde_json::from_str(line).map_err(|e| anyhow::anyhow!("gold line {}: {e}", n + 1))?;
        if q.framings.is_empty() || q.relevant.is_empty() {
            anyhow::bail!("gold line {}: a question needs framings and seed_ids", n + 1);
        }
        out.push(q);
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pin {
    pub gold_sha256: String,
    /// Which gold ids count: those at or below this seq. Without a snapshot (stage 1) it also drops later hits, so a
    /// growing store cannot move the answer; in a snapshot run (the pin names `store_sha256`) it drops nothing (2c).
    pub cutoff_seq: i64,
    #[serde(default)]
    pub terms_sha256: Option<String>,
    pub arms: Vec<String>,
    #[serde(default = "default_k")]
    pub k: Vec<usize>,
    /// How many questions must have a relevant id at or below the cutoff.
    #[serde(default = "default_positives")]
    pub positives: usize,
    #[serde(default)]
    pub seed: u64,
    #[serde(default = "one")]
    pub warmup: usize,
    #[serde(default = "one")]
    pub concurrency: usize,
    /// Stage 2: the snapshot every arm reads. Required when the pin names a meaning arm.
    #[serde(default)]
    pub store_sha256: Option<String>,
    #[serde(default)]
    pub vectors_sha256: Option<String>,
    /// The snapshot's highest seq, and the query embedder (model and URL), recorded for the receipt.
    #[serde(default)]
    pub max_seq: Option<i64>,
    #[serde(default)]
    pub embed: Option<String>,
    /// The probe's query-vector hash and dimension (`oracle scoreboard --probe` prints both).
    #[serde(default)]
    pub embed_probe_sha256: Option<String>,
    #[serde(default)]
    pub embed_dim: Option<usize>,
    /// Section 2e: the reranker for `rerank:*` arms.
    #[serde(default)]
    pub rerank: Option<RerankPin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankPin {
    /// The Claude model id.
    pub model: String,
    /// How many of `live`'s results it reorders.
    pub candidates: usize,
    /// `rerank_prompt_sha()`: the prompt and the candidate format this build sends.
    pub prompt_sha256: String,
    /// Dollars per million input and output tokens, for the receipt's cost.
    pub usd_per_mtok_in: f64,
    pub usd_per_mtok_out: f64,
}
fn default_k() -> Vec<usize> {
    K.to_vec()
}
fn default_positives() -> usize {
    MIN_POSITIVES
}
fn one() -> usize {
    1
}

/// The pin message: tagged `scoreboard:pin`, signed (envelope ≥ 1), by a trusted seat, cargo = the pin.
pub fn read_pin(s: &Store, id: &str, trusted: &[&str]) -> anyhow::Result<(Pin, Observation)> {
    let o = s.get(id).map_err(|e| anyhow::anyhow!("pin {id}: {e}"))?;
    if !o.tags.iter().any(|t| t == "scoreboard:pin") {
        anyhow::bail!("refused: {id} is not tagged scoreboard:pin");
    }
    if o.envelope < 1 || o.sig.is_empty() {
        anyhow::bail!("refused: the pin {id} is not a signed message");
    }
    if !trusted.contains(&o.author.as_str()) {
        anyhow::bail!("refused: the pin's signer {} is not trusted ({})", o.author, trusted.join(", "));
    }
    let cargo = o.cargo.as_deref().ok_or_else(|| anyhow::anyhow!("refused: the pin {id} carries no cargo"))?;
    let pin: Pin = serde_json::from_str(cargo).map_err(|e| anyhow::anyhow!("refused: the pin's cargo is not a pin: {e}"))?;
    for a in &pin.arms {
        if !ARMS.contains(&a.as_str()) {
            anyhow::bail!("refused: the pin names an arm this build does not have: {a}");
        }
    }
    if pin.arms.iter().any(|a| needs_meaning(a)) {
        if pin.store_sha256.is_none() || pin.vectors_sha256.is_none() {
            anyhow::bail!("refused: the pin names a meaning arm but no store_sha256 and vectors_sha256: a meaning arm reads the index as it is, so only a frozen snapshot is a fixed test");
        }
        if pin.embed_probe_sha256.is_none() || pin.embed_dim.is_none() {
            anyhow::bail!("refused: the pin names a meaning arm but no embed_probe_sha256 and embed_dim: framings are embedded at run time, so the embedder must be pinned too");
        }
    }
    if pin.arms.iter().any(|a| is_rerank(a)) {
        let r = pin.rerank.as_ref().ok_or_else(|| anyhow::anyhow!("refused: the pin names a rerank arm but no rerank (model, candidates, prompt_sha256, prices)"))?;
        if r.prompt_sha256 != rerank_prompt_sha() {
            anyhow::bail!("refused: the pin's rerank prompt_sha256 {} is not this build's {}: the prompt changed", r.prompt_sha256, rerank_prompt_sha());
        }
        if r.candidates == 0 || r.candidates > 100 {
            anyhow::bail!("refused: rerank candidates must be 1 to 100, not {}", r.candidates);
        }
    }
    Ok((pin, o))
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub question: String,
    pub framing: usize,
    pub arm: String,
    pub hits: Vec<String>,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The ids an arm answers for a framing, at or below the cutoff, best first, at most `want`. `bm25-messages` leaves out
/// observations whose kind is `state` and nothing else; `live` and `live-aa` are /search's default ranker; `hybrid:<r>`
/// names one of the rankers /search?ranker= accepts.
pub fn answer(s: &Store, sem: Option<&Semantic>, arm: &str, framing: &str, cutoff: i64, want: usize) -> StoreResult<Vec<String>> {
    let n = (want * 4).clamp(10, 100) as i64;
    let keep = |hits: Vec<crate::store::SearchHit>, drop_state: bool| -> Vec<String> {
        hits.into_iter().filter(|h| h.obs.seq <= cutoff && !(drop_state && h.obs.kind == "state")).map(|h| h.obs.id).take(want).collect()
    };
    match arm {
        "bm25-messages" => Ok(keep(s.search(framing, None, None, n)?, true)),
        a if needs_meaning(a) => {
            let ranker = crate::store::Ranker::parse(a.strip_prefix("hybrid:"));
            Ok(keep(s.search_ranked(framing, None, None, n, sem, ranker)?.0, false))
        }
        _ => bm25(s, framing, cutoff, want),
    }
}

/// The ids `bm25` answers for a framing, at or below the cutoff, best first, at most `want`.
fn bm25(s: &Store, framing: &str, cutoff: i64, want: usize) -> StoreResult<Vec<String>> {
    let hits = s.search(framing, None, None, (want * 4).clamp(10, 100) as i64)?;
    Ok(hits.into_iter().filter(|h| h.obs.seq <= cutoff).map(|h| h.obs.id).take(want).collect())
}

fn hit_at(hits: &[String], relevant: &HashSet<String>, k: usize) -> bool {
    hits.iter().take(k).any(|h| relevant.contains(h))
}
fn recall_at(hits: &[String], relevant: &HashSet<String>, k: usize) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    hits.iter().take(k).filter(|h| relevant.contains(*h)).count() as f64 / relevant.len() as f64
}
fn rr(hits: &[String], relevant: &HashSet<String>) -> f64 {
    hits.iter().position(|h| relevant.contains(h)).map(|p| 1.0 / (p as f64 + 1.0)).unwrap_or(0.0)
}
fn pct(v: &mut [u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// xorshift64*: a seeded stream so a receipt can be re-made from its pin.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0.max(1);
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Paired bootstrap of per-question deltas (arm minus control): the 2.5th and 97.5th percentiles of the resampled mean.
pub fn paired_bootstrap(deltas: &[f64], seed: u64, resamples: usize) -> (f64, f64, f64) {
    if deltas.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
    let mut rng = Rng(seed ^ 0x9E3779B97F4A7C15);
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| (0..deltas.len()).map(|_| deltas[rng.below(deltas.len())]).sum::<f64>() / deltas.len() as f64)
        .collect();
    means.sort_by(|a, b| a.total_cmp(b));
    let at = |p: f64| means[((means.len() - 1) as f64 * p).round() as usize];
    (mean, at(0.025), at(0.975))
}

/// The shuffled truth: question i is scored against question i+1's relevant ids, minus any id the two share
/// (reported), so a hit cannot come from an id that is also truly relevant.
pub fn rotated_truth(gold: &[Question]) -> (Vec<HashSet<String>>, usize) {
    let n = gold.len();
    let mut removed = 0;
    let truth = (0..n)
        .map(|i| {
            let own: HashSet<&str> = gold[i].relevant.iter().map(String::as_str).collect();
            let next = &gold[(i + 1) % n].relevant;
            let keep: HashSet<String> = next.iter().filter(|r| !own.contains(r.as_str())).cloned().collect();
            removed += next.len() - keep.len();
            keep
        })
        .collect();
    (truth, removed)
}

/// What one run is made of: the gold as read, the pin as read, which of its arms (empty = all), the terms file's hash
/// when the pin names one, and the build that runs it.
pub struct Setup<'a> {
    pub gold: &'a [Question],
    pub gold_text: &'a str,
    pub pin: &'a Pin,
    pub pin_id: &'a str,
    pub only: &'a [String],
    pub terms_sha: Option<&'a str>,
    pub build: &'a str,
    /// The vector store, for the meaning arms; `None` when meaning search is off.
    pub sem: Option<&'a Semantic>,
    /// The hashes of the files this run reads, taken before the store was opened.
    pub snapshot: &'a Snapshot,
    /// `Some("dev")` or `Some("test")`: score only the questions marked so; `None` scores the whole file.
    pub split: Option<&'a str>,
    /// The reranker for `rerank:*` arms (section 2e).
    pub reranker: Option<&'a dyn Rerank>,
}

/// Run the arms the pin names (or `only`, a subset of them) and make the receipt. Read-only on the store.
pub fn run(s: &Store, setup: &Setup) -> anyhow::Result<Value> {
    let Setup { gold, gold_text, pin, pin_id, only, terms_sha, build, sem, snapshot, split, reranker } = *setup;
    for (what, want, have) in [("store", &pin.store_sha256, &snapshot.store_sha256), ("vector index", &pin.vectors_sha256, &snapshot.vectors_sha256)] {
        if let Some(want) = want {
            match have {
                Some(have) if have == want => {}
                Some(have) => anyhow::bail!("refused: the {what} file's sha256 {have} is not the pin's {want}"),
                None => anyhow::bail!("refused: the pin freezes the {what} ({want}) but this run could not hash it"),
            }
        }
    }
    let sha = oracle_proto::sha_hex(gold_text.as_bytes());
    if sha != pin.gold_sha256 {
        anyhow::bail!("refused: the gold file's sha256 {sha} is not the pin's {}", pin.gold_sha256);
    }
    // Emmy's gate: dev/test twins are listed whatever half runs (they are a property of the file).
    let twins = if gold.iter().any(|q| q.split.is_some()) { near_duplicates(gold) } else { vec![] };
    // One half of the file (section 2d). The hash above and the pin's positive count still cover the whole file; the
    // half must itself hold at least MIN_POSITIVES.
    let selected: Vec<Question>;
    let gold: &[Question] = match split {
        None => gold,
        Some(want) => {
            if want != "dev" && want != "test" {
                anyhow::bail!("refused: split {want} is not dev or test");
            }
            let whole = gold.iter().filter(|q| q.relevant.iter().any(|r| matches!(s.get(r), Ok(o) if o.seq <= pin.cutoff_seq))).count();
            if whole < pin.positives.max(MIN_POSITIVES) {
                anyhow::bail!("refused: {whole} questions in the whole file have a relevant id at or below seq {}; the pin wants {}", pin.cutoff_seq, pin.positives.max(MIN_POSITIVES));
            }
            selected = gold.iter().filter(|q| q.split.as_deref() == Some(want)).cloned().collect();
            if selected.is_empty() {
                anyhow::bail!("refused: no question in the gold file is marked split {want}");
            }
            &selected
        }
    };
    if let (Some(want), Some(have)) = (pin.terms_sha256.as_deref(), terms_sha) {
        if want != have {
            anyhow::bail!("refused: the terms file's sha256 {have} is not the pin's {want}");
        }
    }
    let arms: Vec<&str> = if only.is_empty() { pin.arms.iter().map(String::as_str).collect() } else { only.iter().map(String::as_str).collect() };
    for a in &arms {
        if !pin.arms.iter().any(|p| p == a) {
            anyhow::bail!("refused: arm {a} is not in the pin");
        }
        if needs_meaning(a) && sem.is_none() {
            let why = crate::semantic::OFF_REASON.get().cloned().unwrap_or_else(|| "not loaded".into());
            anyhow::bail!("refused: arm {a} needs meaning search, which is off: {why}");
        }
        if is_rerank(a) && reranker.is_none() {
            anyhow::bail!("refused: arm {a} needs a reranker, and none was given");
        }
    }
    // The embedder and the index must be the pin's: the probe's vector, its dimension, and the index's dimension.
    let mut meaning_check = Value::Null;
    if arms.iter().any(|a| needs_meaning(a)) {
        if let Some(sem) = sem {
            let (sha, dim) = probe(sem)?;
            if Some(&sha) != pin.embed_probe_sha256.as_ref() {
                anyhow::bail!("refused: the probe's query vector sha256 {sha} is not the pin's {}: the embedder changed", pin.embed_probe_sha256.as_deref().unwrap_or("?"));
            }
            let (index_dim, windows, covered) = sem.coverage();
            if Some(dim) != pin.embed_dim || index_dim != dim {
                anyhow::bail!("refused: dimensions disagree: probe {dim}, index {index_dim}, pin {:?}", pin.embed_dim);
            }
            meaning_check = json!({"probe_sha256": sha, "dim": dim, "index_dim": index_dim, "windows": windows, "observations_covered": covered});
        }
    }
    // A snapshot run (the pin names store_sha256) reads one frozen store: every arm answers from all of it, as /search
    // would at that moment, and the cutoff only decides which gold ids count. Without a snapshot the cutoff also drops
    // later hits, as stage 1 did (Emmy: never filter after top-k in a snapshot run).
    let hit_cutoff = if pin.store_sha256.is_some() { i64::MAX } else { pin.cutoff_seq };
    // The truth at the cutoff: a relevant id that does not exist, or came after the cutoff, is not a positive.
    let mut truth: Vec<HashSet<String>> = Vec::new();
    let mut dropped = 0usize;
    for q in gold {
        let mut keep = HashSet::new();
        for r in &q.relevant {
            match s.get(r) {
                Ok(o) if o.seq <= pin.cutoff_seq => {
                    keep.insert(r.clone());
                }
                _ => dropped += 1,
            }
        }
        truth.push(keep);
    }
    let positives = truth.iter().filter(|t| !t.is_empty()).count();
    let want_positives = if split.is_some() { MIN_POSITIVES } else { pin.positives.max(MIN_POSITIVES) };
    if positives < want_positives {
        anyhow::bail!("refused: {positives} questions have a relevant id at or below seq {}; the run wants {want_positives}", pin.cutoff_seq);
    }
    let (rotated, overlap_removed) = rotated_truth(gold);
    let leakage = leaks(s, gold)?;
    let kmax = pin.k.iter().copied().max().unwrap_or(10).max(10);
    // One warm pass per arm (the pin's protocol), then the measured pass; the store is one connection, concurrency is 1.
    // Prewarm (Emmy): every framing's query vector once, before any timed arm, so no
    // meaning arm pays a cold embedding that a later arm then reuses from the cache. Timed apart, never in an arm.
    let mut prewarm = Value::Null;
    if let Some(sem) = sem.filter(|_| arms.iter().any(|a| needs_meaning(a))) {
        let t0 = Instant::now();
        let (mut n, mut failed) = (0usize, 0usize);
        for (qi, q) in gold.iter().enumerate() {
            if truth[qi].is_empty() {
                continue;
            }
            for f in &q.framings {
                n += 1;
                if sem.query_vector(f).is_err() {
                    failed += 1;
                }
            }
        }
        if n > crate::semantic::QUERY_CACHE {
            anyhow::bail!("refused: {n} framings do not fit the query cache ({}); a prewarm would not hold for every arm", crate::semantic::QUERY_CACHE);
        }
        prewarm = json!({"framings": n, "failed": failed, "ms": t0.elapsed().as_millis() as u64, "cache": crate::semantic::QUERY_CACHE});
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut tables = serde_json::Map::new();
    let mut per_question: BTreeMap<String, Vec<f64>> = BTreeMap::new(); // arm -> hit@5 mean per question
    // The source of each positive question, in per_question's order, and per arm and source: hit@5, hit@10, MRR sums
    // over framings and the framing count.
    let q_source: Vec<String> = gold.iter().zip(&truth).filter(|(_, t)| !t.is_empty()).map(|(q, _)| q.source.clone().unwrap_or_else(|| "all".into())).collect();
    let mut by_source: BTreeMap<String, BTreeMap<String, [f64; 4]>> = BTreeMap::new();
    for arm in &arms {
        for _ in 0..pin.warmup {
            if let Some(q) = gold.first() {
                // A rerank arm warms the search it reorders; the reranker itself has no cache, and a call costs money.
                let _ = answer(s, sem, if is_rerank(arm) { "live" } else { arm }, &q.framings[0], hit_cutoff, kmax);
            }
        }
        let mut lat: Vec<u64> = Vec::new();
        let mut sums: BTreeMap<String, f64> = BTreeMap::new();
        let mut n = 0usize;
        let mut errors = 0usize;
        let mut q_hit5: Vec<f64> = Vec::new();
        let mut used = Usage::default();
        let mut max_input = 0u64;
        for (qi, q) in gold.iter().enumerate() {
            let rel = if *arm == "shuffled" { &rotated[qi] } else { &truth[qi] };
            if truth[qi].is_empty() {
                continue; // not a positive: no row
            }
            let mut q_sum = 0.0;
            for (fi, f) in q.framings.iter().enumerate() {
                let t0 = Instant::now();
                let got: anyhow::Result<Vec<String>> = match (is_rerank(arm), reranker, pin.rerank.as_ref()) {
                    (true, Some(rr), Some(rp)) => answer_rerank(s, sem, rr, rp.candidates, f, hit_cutoff, kmax).map(|(h, u)| {
                        used.input += u.input;
                        max_input = max_input.max(u.input);
                        used.output += u.output;
                        h
                    }),
                    _ => answer(s, sem, arm, f, hit_cutoff, kmax).map_err(|e| anyhow::anyhow!(e.to_string())),
                };
                let ms = t0.elapsed().as_millis() as u64;
                lat.push(ms);
                n += 1;
                let (hits, error) = match got {
                    Ok(h) => (h, None),
                    Err(e) => {
                        errors += 1;
                        (vec![], Some(e.to_string()))
                    }
                };
                for k in &pin.k {
                    *sums.entry(format!("hit@{k}")).or_default() += hit_at(&hits, rel, *k) as u8 as f64;
                }
                *sums.entry("recall@10".into()).or_default() += recall_at(&hits, rel, 10);
                *sums.entry("mrr".into()).or_default() += rr(&hits, rel);
                q_sum += hit_at(&hits, rel, 5) as u8 as f64;
                let acc = by_source.entry(q.source.clone().unwrap_or_else(|| "all".into())).or_default().entry(arm.to_string()).or_default();
                acc[0] += hit_at(&hits, rel, 5) as u8 as f64;
                acc[1] += hit_at(&hits, rel, 10) as u8 as f64;
                acc[2] += rr(&hits, rel);
                acc[3] += 1.0;
                rows.push(Row { question: q.code.clone(), framing: fi, arm: arm.to_string(), hits, latency_ms: ms, error });
            }
            q_hit5.push(q_sum / q.framings.len() as f64);
        }
        let mut table = serde_json::Map::new();
        for (m, v) in &sums {
            table.insert(m.clone(), json!((v / n.max(1) as f64 * 10000.0).round() / 10000.0));
        }
        table.insert("framings".into(), json!(n));
        table.insert("errors".into(), json!(errors));
        table.insert("p50_ms".into(), json!(pct(&mut lat.clone(), 0.5)));
        table.insert("p95_ms".into(), json!(pct(&mut lat, 0.95)));
        if let (true, Some(rp)) = (is_rerank(arm), pin.rerank.as_ref()) {
            let usd = used.input as f64 / 1e6 * rp.usd_per_mtok_in + used.output as f64 / 1e6 * rp.usd_per_mtok_out;
            table.insert("input_tokens".into(), json!(used.input));
            table.insert("output_tokens".into(), json!(used.output));
            // One call's largest prompt: over 100k tokens some models charge more for the whole call than the pin's price.
            table.insert("max_input_tokens".into(), json!(max_input));
            table.insert("usd".into(), json!((usd * 10000.0).round() / 10000.0));
            table.insert("usd_per_framing".into(), json!((usd / n.max(1) as f64 * 1_000_000.0).round() / 1_000_000.0));
        }
        tables.insert(arm.to_string(), Value::Object(table));
        per_question.insert(arm.to_string(), q_hit5);
    }
    // Paired deltas against the control, by question, bootstrapped with the pin's seed.
    let mut deltas = serde_json::Map::new();
    if let Some(control) = per_question.get("bm25").cloned() {
        for (arm, v) in &per_question {
            if arm == "bm25" || v.len() != control.len() {
                continue;
            }
            let d: Vec<f64> = v.iter().zip(&control).map(|(a, c)| a - c).collect();
            let (mean, lo, hi) = paired_bootstrap(&d, pin.seed, RESAMPLES);
            deltas.insert(arm.clone(), json!({"metric": "hit@5", "against": "bm25", "mean": (mean * 10000.0).round() / 10000.0, "lo": (lo * 10000.0).round() / 10000.0, "hi": (hi * 10000.0).round() / 10000.0, "questions": d.len()}));
        }
    }
    // The same, against `live`: a change to /search must beat what /search does today.
    let mut deltas_live = serde_json::Map::new();
    if let Some(control) = per_question.get("live").cloned() {
        for (arm, v) in &per_question {
            if arm == "live" || arm == "live-aa" || v.len() != control.len() {
                continue;
            }
            let d: Vec<f64> = v.iter().zip(&control).map(|(a, c)| a - c).collect();
            let (mean, lo, hi) = paired_bootstrap(&d, pin.seed, RESAMPLES);
            deltas_live.insert(arm.clone(), json!({"metric": "hit@5", "against": "live", "mean": (mean * 10000.0).round() / 10000.0, "lo": (lo * 10000.0).round() / 10000.0, "hi": (hi * 10000.0).round() / 10000.0, "questions": d.len()}));
        }
    }
    // Per source (gold 100): a winner on the pool-biased `behaviour` half alone is visible (section 2d).
    let round = |x: f64| (x * 10000.0).round() / 10000.0;
    let mut tables_by_source = serde_json::Map::new();
    let mut deltas_live_by_source = serde_json::Map::new();
    if by_source.len() > 1 {
        for (src, arms_acc) in &by_source {
            let t: serde_json::Map<String, Value> = arms_acc
                .iter()
                .map(|(arm, a)| (arm.clone(), json!({"hit@5": round(a[0] / a[3].max(1.0)), "hit@10": round(a[1] / a[3].max(1.0)), "mrr": round(a[2] / a[3].max(1.0)), "framings": a[3] as usize})))
                .collect();
            tables_by_source.insert(src.clone(), Value::Object(t));
            let idx: Vec<usize> = q_source.iter().enumerate().filter(|(_, s)| *s == src).map(|(i, _)| i).collect();
            let mut d_src = serde_json::Map::new();
            if let Some(control) = per_question.get("live") {
                for (arm, v) in &per_question {
                    if arm == "live" || arm == "live-aa" || v.len() != control.len() {
                        continue;
                    }
                    let d: Vec<f64> = idx.iter().map(|&i| v[i] - control[i]).collect();
                    let (mean, lo, hi) = paired_bootstrap(&d, pin.seed, RESAMPLES);
                    d_src.insert(arm.clone(), json!({"metric": "hit@5", "against": "live", "mean": round(mean), "lo": round(lo), "hi": round(hi), "questions": d.len()}));
                }
            }
            deltas_live_by_source.insert(src.clone(), Value::Object(d_src));
        }
    }
    // The A/A arm must agree on every quality number; the clock is reported, never compared.
    let quality = |t: &Value| -> Value {
        let mut q = t.as_object().cloned().unwrap_or_default();
        for k in ["p50_ms", "p95_ms", "input_tokens", "output_tokens", "max_input_tokens", "usd", "usd_per_framing"] {
            q.remove(k);
        }
        Value::Object(q)
    };
    let verdicts = json!({
        "aa_agrees": tables.get("aa").zip(tables.get("bm25")).map(|(a, b)| quality(a) == quality(b)),
        "live_aa_agrees": tables.get("live-aa").zip(tables.get("live")).map(|(a, b)| quality(a) == quality(b)),
        "rerank_aa_agrees": tables.get("rerank:llm-aa").zip(tables.get("rerank:llm")).map(|(a, b)| quality(a) == quality(b)),
        "shuffled_under_0_05": tables.get("shuffled").and_then(|t| t["hit@10"].as_f64()).map(|h| h < 0.05),
    });
    let arms_json: Vec<Value> = arms.iter().map(|a| json!(a)).collect();
    Ok(json!({
        "schema": "scoreboard/1",
        "pin": pin_id,
        "evidence": format!("pilot: {} questions, {} framings; not a measure of Oracle in general", gold.len(), gold.iter().map(|q| q.framings.len()).sum::<usize>()),
        "cutoff_seq": pin.cutoff_seq,
        "positives": positives,
        "relevant_dropped_after_cutoff_or_missing": dropped,
        "shuffled_overlap_removed": overlap_removed,
        "arms": arms_json,
        "tables": tables,
        "deltas": deltas,
        "deltas_vs_live": deltas_live,
        "split": split.unwrap_or("all"),
        "tables_by_source": tables_by_source,
        "deltas_vs_live_by_source": deltas_live_by_source,
        "snapshot": {"store_sha256": snapshot.store_sha256, "vectors_sha256": snapshot.vectors_sha256, "max_seq": pin.max_seq, "embed": pin.embed},
        "meaning": meaning_check,
        "prewarm": prewarm,
        "leakage": leakage,
        "near_duplicates_across_splits": twins,
        "latency_verdict": "none: p50/p95 are reported, never compared; on a shared host they do not separate arms (Emmy, OBS-oracle-20001020-24c366310064)",
        "rerank": pin.rerank.as_ref().filter(|_| arms.iter().any(|a| is_rerank(a))).map(|r| json!({"model": r.model, "candidates": r.candidates, "prompt_sha256": r.prompt_sha256})),
        "cutoff_role": if pin.store_sha256.is_some() { "truth eligibility only: a snapshot run; arms answer from the whole snapshot; not numerically comparable with a cutoff-filtered run" } else { "truth eligibility and hit filter (stage 1)" },
        "verdicts": verdicts,
        "rows": rows.len(),
        "errors": rows.iter().filter(|r| r.error.is_some()).map(|r| json!({"question": r.question, "framing": r.framing, "arm": r.arm, "error": r.error})).collect::<Vec<_>>(),
        "build": build,
        "store": {"observations_at_cutoff": pin.cutoff_seq},
    }))
}

/// The store error type, re-exported so `main` can speak of refusals in one voice.
pub fn refused(e: &anyhow::Error) -> bool {
    e.to_string().starts_with("refused:")
}

#[allow(dead_code)]
fn _store_error(_: StoreError) {}
#[allow(dead_code)]
fn _ids(_: BTreeSet<String>) {}
