//! `ox eval <set.jsonl>`: run a frozen query set against `/search` and say, per query and in sum, whether the right
//! observation came first and how the result reads (Noor's Round 50, 2026-10-10: "right about 80% of the time and
//! reads badly 100% of the time"). One line per case: {"q", "expect": [ids], "none": bool, "note"}. `expect` is the
//! gold (an empty list measures only how it reads); `none` says nothing before today answers it, so the right answer
//! is an explicit "no prior", not low-score noise. Every fix to search ships with this run before and after.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Deserialize)]
pub struct Case {
    pub q: String,
    #[serde(default)]
    pub expect: Vec<String>,
    #[serde(default)]
    pub none: bool,
    /// Words that must all appear (any case) in the wake brief for the answer to count as in it (RECALL-SCOREBOARD.md 2f).
    #[serde(default)]
    pub brief: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Row {
    pub q: String,
    /// 1-based rank of the first expected id in the hits; None when absent or when nothing is expected.
    pub rank: Option<usize>,
    pub hits: usize,
    pub top_score: f64,
    pub bytes: usize,
    /// Hits that repeat another hit's content (an import read twice).
    pub dups: usize,
    /// Of the top five, hits from synthetic sources (#graph-synthesis, STAGED/SYN ids).
    pub synthetic_top5: usize,
    /// Hits carrying export UI noise.
    pub noise: usize,
    /// Edge tags (reply, ref, cites, supersedes, closes, answers) summed over the hits.
    pub edges: usize,
    /// For a `none` case: the answer said "no prior" (or returned nothing).
    pub said_none: Option<bool>,
    /// The server's raw strength readout (records holding every word; those older than a day; best meaning similarity).
    pub all_words: i64,
    pub all_words_before: i64,
    pub best_meaning: Option<f64>,
    pub ms: u128,
    /// With `--brief`, for a case with `brief` words: whether the wake brief holds them all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_brief: Option<bool>,
}

const NOISE: &str = "This block is not supported on your current device";
const EDGE: [&str; 9] = ["reply:", "re:", "in_reply_to:", "ref:", "cites:", "supersedes:", "closes:", "answers:", "is-same-as:"];

fn synthetic(h: &Value) -> bool {
    let id = h["id"].as_str().unwrap_or("");
    h["channel"] == "graph-synthesis" || id.contains("STAGED") || id.starts_with("SYN-") || id.contains("-SYN-")
}

/// The content an import repeats: a CLAUDE-EXPORT header line names the import, not the turn, so it is left out.
fn body(h: &Value) -> &str {
    let c = h["content"].as_str().unwrap_or("").trim();
    match c.split_once('\n') {
        Some((first, rest)) if first.contains("CLAUDE-EXPORT") => rest.trim(),
        _ => c,
    }
}

/// One case scored from one `/search` answer.
pub fn score(case: &Case, answer: &Value, ms: u128) -> Row {
    let hits: Vec<Value> = answer["hits"].as_array().or_else(|| answer["items"].as_array()).cloned().unwrap_or_default();
    let mut seen = HashSet::new();
    let dups = hits.iter().filter(|h| !seen.insert(body(h).to_owned())).count();
    let rank = hits.iter().position(|h| case.expect.iter().any(|e| h["id"].as_str() == Some(e))).map(|i| i + 1);
    let edges = hits
        .iter()
        .flat_map(|h| h["tags"].as_array().cloned().unwrap_or_default())
        .filter(|t| t.as_str().is_some_and(|t| EDGE.iter().any(|p| t.starts_with(p))))
        .count();
    Row {
        q: case.q.clone(),
        rank,
        hits: hits.len(),
        top_score: hits.first().and_then(|h| h["score"].as_f64()).unwrap_or(0.0),
        bytes: hits.iter().map(|h| h["content"].as_str().map_or(0, str::len)).sum(),
        dups,
        synthetic_top5: hits.iter().take(5).filter(|h| synthetic(h)).count(),
        noise: hits.iter().filter(|h| h["content"].as_str().is_some_and(|c| c.contains(NOISE))).count(),
        edges,
        said_none: case.none.then(|| answer["no_prior"] == true || hits.is_empty()),
        all_words: answer["strength"]["all_words"].as_i64().unwrap_or(-1),
        all_words_before: answer["strength"]["all_words_before"].as_i64().unwrap_or(-1),
        best_meaning: answer["strength"]["best_meaning"].as_f64(),
        ms,
        in_brief: None,
    }
}

/// Whether the brief answers a case: every one of its `brief` words is in the text, any case. None when it names none.
pub fn brief_covers(case: &Case, brief: &str) -> Option<bool> {
    if case.brief.is_empty() {
        return None;
    }
    let text = brief.to_lowercase();
    Some(case.brief.iter().all(|w| text.contains(&w.to_lowercase())))
}

/// The sum line: hit@1/@5/@10 and MRR over the cases with gold, and how the whole set reads.
pub fn summary(rows: &[Row], cases: &[Case]) -> Value {
    let gold: Vec<&Row> = rows.iter().zip(cases).filter(|(_, c)| !c.expect.is_empty()).map(|(r, _)| r).collect();
    let n = gold.len().max(1) as f64;
    let at = |k: usize| gold.iter().filter(|r| r.rank.is_some_and(|x| x <= k)).count() as f64 / n;
    let mrr = gold.iter().map(|r| r.rank.map_or(0.0, |x| 1.0 / x as f64)).sum::<f64>() / n;
    let hits: usize = rows.iter().map(|r| r.hits).sum::<usize>().max(1);
    let mut ms: Vec<u128> = rows.iter().map(|r| r.ms).collect();
    ms.sort();
    let pct = |p: f64| ms.get(((ms.len() as f64 * p).ceil() as usize).saturating_sub(1)).copied().unwrap_or(0);
    let none: Vec<bool> = rows.iter().filter_map(|r| r.said_none).collect();
    let in_brief: Vec<bool> = rows.iter().filter_map(|r| r.in_brief).collect();
    serde_json::json!({
        "cases": rows.len(), "with_gold": gold.len(),
        "hit_at_1": at(1), "hit_at_5": at(5), "hit_at_10": at(10), "mrr": mrr,
        "bytes_per_hit": rows.iter().map(|r| r.bytes).sum::<usize>() / hits,
        "dup_share": rows.iter().map(|r| r.dups).sum::<usize>() as f64 / hits as f64,
        "synthetic_share_top5": rows.iter().map(|r| r.synthetic_top5).sum::<usize>() as f64 / (rows.len().max(1) * 5) as f64,
        "noise_share": rows.iter().map(|r| r.noise).sum::<usize>() as f64 / hits as f64,
        "edges_per_hit": rows.iter().map(|r| r.edges).sum::<usize>() as f64 / hits as f64,
        "no_prior_said": format!("{}/{}", none.iter().filter(|x| **x).count(), none.len()),
        "brief_coverage": format!("{}/{}", in_brief.iter().filter(|x| **x).count(), in_brief.len()),
        "p50_ms": pct(0.5), "p95_ms": pct(0.95),
    })
}

pub fn read_cases(path: &std::path::Path) -> Result<Vec<Case>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("{}: line {}", path.display(), i + 1)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// RECALL-SCOREBOARD.md 2f: a case counts as in the brief only when every one of its words is there.
    #[test]
    fn the_brief_covers_a_case_only_with_every_word() {
        let case = |w: &[&str]| Case { q: "q".into(), expect: vec![], none: false, brief: w.iter().map(|s| s.to_string()).collect() };
        let brief = "AUTHORITY  I release oracle-core and ship prod myself; Astra production waits for his go.";
        assert_eq!(brief_covers(&case(&["release oracle-core", "astra PRODUCTION"]), brief), Some(true));
        assert_eq!(brief_covers(&case(&["release oracle-core", "VPS reboot"]), brief), Some(false));
        assert_eq!(brief_covers(&case(&[]), brief), None);
        let mut r = score(&case(&["x"]), &json!({"hits": []}), 1);
        r.in_brief = Some(true);
        let mut r2 = score(&case(&["y"]), &json!({"hits": []}), 1);
        r2.in_brief = Some(false);
        let r3 = score(&case(&[]), &json!({"hits": []}), 1);
        let cases = vec![case(&["x"]), case(&["y"]), case(&[])];
        assert_eq!(summary(&[r, r2, r3], &cases)["brief_coverage"], "1/2");
    }

    #[test]
    fn a_case_is_scored_on_rank_duplicates_noise_synthetics_and_edges() {
        let case = Case { q: "channel capacity".into(), expect: vec!["OBS-real".into()], none: false, brief: vec![] };
        let turn = "[CLAUDE-EXPORT · blaze · 2026-05-13 · room:a]\nthe same turn";
        let turn2 = "[CLAUDE-EXPORT · blaze · 2026-05-23 · room:b]\nthe same turn";
        let a = json!({"hits": [
            {"id": "SYN-1", "channel": "graph-synthesis", "content": "paraphrase", "score": 0.03, "tags": []},
            {"id": "OBS-a", "channel": "dot", "content": turn, "tags": ["reply:OBS-x"]},
            {"id": "OBS-b", "channel": "dot", "content": turn2, "tags": []},
            {"id": "OBS-n", "channel": "lab", "content": format!("x {NOISE}"), "tags": ["supersedes:OBS-y", "kind:say"]},
            {"id": "OBS-real", "channel": "illuminati", "content": "THREE SENSES OF CHANNEL", "tags": []}]});
        let r = score(&case, &a, 7);
        assert_eq!((r.rank, r.dups, r.synthetic_top5, r.noise, r.edges, r.said_none), (Some(5), 1, 1, 1, 2, None));
        // Negative control: a none-case answered with noise did not say "no prior"; an empty or flagged answer did.
        let none = Case { q: "Krein".into(), expect: vec![], none: true, brief: vec![] };
        assert_eq!(score(&none, &a, 1).said_none, Some(false));
        assert_eq!(score(&none, &json!({"hits": []}), 1).said_none, Some(true));
        assert_eq!(score(&none, &json!({"hits": [{"id": "x"}], "no_prior": true}), 1).said_none, Some(true));
        let s = summary(&[r], &[case]);
        assert_eq!((s["hit_at_1"].as_f64(), s["hit_at_5"].as_f64(), s["mrr"].as_f64()), (Some(0.0), Some(1.0), Some(0.2)));
    }
}
