//! `GET /answerable?q=&room=` (schema answerable/1): whether Oracle can answer a question, said before anyone recalls and reads
//! the hits. NVIDIA Auto Ontology's `check_answerable` (mcp/auto_ontology_mcp/tools.py) grades how well its semantic layer covers
//! a question as "a cheap pre-flight"; Blaze, 3 Nov 2026: "add check_answerable and partial state to oracle". Ours asks no model.
//! Each content word is counted in the record; the records that hold all of them are checked against what the owner approved;
//! the graph says which concepts the question names and how much of each he has approved ([`crate::cortex::certification`]).
//!
//! `yes`: every word is on record and an approved reading answers it (it holds all the words, or the graph reaches one through
//! a concept the question names). `partial`: some of it is on record, or every word but in no one record, or all of it with
//! nothing approved. `no`: no record
//! holds any of the words and the graph reaches nothing.

use std::collections::BTreeSet;

use rusqlite::params;
use serde_json::{json, Value};

use crate::store::{fts_query, Result, Store};

/// How many record ids a list carries; the counts are whole up to [`CAP`].
const SHOWN: usize = 5;
/// The content words checked, each one count: a question is rarely about more.
const WORDS: usize = 8;
/// The count a word or a question stops at: past it, "1000 or more".
const CAP: i64 = 1000;

/// What `room` and the caller's hidden rooms leave in: ?2 the room or null, ?3 the hidden rooms as a JSON array.
const SCOPE: &str = "(?2 IS NULL OR o.channel=?2) AND o.channel NOT IN (SELECT value FROM json_each(?3))";

pub fn check(s: &Store, q: &str, room: Option<&str>, hidden: &BTreeSet<String>) -> Result<Value> {
    let mut content = crate::store::content_words(q);
    content.truncate(WORDS);
    if content.is_empty() {
        return Ok(json!({"schema": "answerable/1", "query": q, "room": room, "verdict": "no",
            "says": "the question has no content words to look up", "words": [], "missing": []}));
    }
    let ix = s.approved_index()?;
    let hidden_json = json!(hidden).to_string();
    let approved_json = json!(ix.approved).to_string();
    let c = s.conn()?;
    let count = |expr: &str, approved_only: bool| -> Result<i64> {
        let only = if approved_only { " AND o.id IN (SELECT value FROM json_each(?4))" } else { " AND ?4 IS NOT NULL" };
        let sql = format!("SELECT count(*) FROM (SELECT 1 FROM obs_fts f JOIN obs_live o ON o.seq=f.rowid \
            WHERE obs_fts MATCH ?1 AND {SCOPE}{only} LIMIT {CAP})");
        Ok(c.prepare_cached(&sql)?.query_row(params![expr, room, hidden_json, approved_json], |r| r.get(0))?)
    };
    let ids = |expr: &str, approved_only: bool| -> Result<Vec<String>> {
        let only = if approved_only { "o.id IN" } else { "o.id NOT IN" };
        let sql = format!("SELECT o.id FROM obs_fts f JOIN obs_live o ON o.seq=f.rowid \
            WHERE obs_fts MATCH ?1 AND {SCOPE} AND {only} (SELECT value FROM json_each(?4)) ORDER BY o.seq DESC LIMIT {SHOWN}");
        let mut st = c.prepare_cached(&sql)?;
        let rows = st.query_map(params![expr, room, hidden_json, approved_json], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<String>, _>>()?)
    };

    let mut words = vec![];
    let mut missing = vec![];
    for w in &content {
        let expr = fts_query(w, " AND ");
        let (records, approved) = if expr.is_empty() { (0, 0) } else { (count(&expr, false)?, count(&expr, true)?) };
        if records == 0 {
            missing.push(w.clone());
        }
        words.push(json!({"word": w, "records": records, "approved": approved}));
    }
    let all = fts_query(&content.join(" "), " AND ");
    let (all_records, all_approved, others) = if all.is_empty() || !missing.is_empty() {
        (0, vec![], vec![])
    } else {
        (count(&all, false)?, ids(&all, true)?, ids(&all, false)?)
    };

    // The graph reaches approved readings in the cortex room; with a room named it is left out, and a reading in a room the
    // caller may not read is never counted.
    let reach = room.is_none().then(|| ix.reach(q));
    let graph_records: Vec<&String> = reach.iter().flat_map(|g| g.records.iter())
        .filter(|id| crate::privaterooms::room_of_id(id).is_none_or(|r| !hidden.contains(r))).collect();
    let graph_answers = reach.as_ref().is_some_and(|g| g.unnamed < 2) && !graph_records.is_empty();
    let nodes: Vec<Value> = reach.iter().flat_map(|g| g.nodes.iter()).map(|t| {
        let cert = ix.certified(t);
        json!({"title": t, "certification": cert.map(|c| c.state), "approved": cert.map(|c| c.approved), "readings": cert.map(|c| c.readings)})
    }).collect();
    let partial_nodes: Vec<&str> = nodes.iter().filter(|n| n["certification"] == "partial").filter_map(|n| n["title"].as_str()).collect();

    let held: Vec<&String> = content.iter().filter(|w| !missing.contains(w)).collect();
    let (verdict, mut says) = if held.is_empty() && !graph_answers {
        ("no", format!("nothing on record holds any of these words: {}", content.join(", ")))
    } else if missing.is_empty() && (!all_approved.is_empty() || graph_answers) {
        let basis = if all_approved.is_empty() { "the graph reaches approved readings".to_owned() } else { if content.len() == 1 { "approved records hold the word".to_owned() } else { format!("approved records hold all {} words", content.len()) } };
        ("yes", basis)
    } else if !missing.is_empty() {
        ("partial", format!("on record: {}; not on record: {}", join(&held), missing.join(", ")))
    } else if all_records == 0 {
        ("partial", "each word is on record, but no record holds them all: the question joins things nobody has joined".to_owned())
    } else {
        ("partial", format!("{} records hold all these words and none is approved: read them, they are not law", cap(all_records)))
    };
    if !partial_nodes.is_empty() {
        says.push_str(&format!("; partly approved: {}", partial_nodes.join(", ")));
    }
    let next = match verdict {
        "yes" => "recall it and cite the approved records",
        "partial" => "recall what is on record, and say what is not",
        _ => "nothing to recall: ask, or record the answer once it is known",
    };
    Ok(json!({"schema": "answerable/1", "query": q, "room": room, "verdict": verdict, "says": says, "next": next,
        "words": words, "missing": missing,
        "all_words": {"records": all_records, "capped": all_records >= CAP, "approved": all_approved, "others": others},
        "graph": reach.as_ref().map(|g| json!({"nodes": nodes, "form": g.form, "records": graph_records.iter().take(SHOWN).collect::<Vec<_>>(),
            "reached": graph_records.len(), "unnamed": g.unnamed}))}))
}

fn join(w: &[&String]) -> String {
    w.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
}

fn cap(n: i64) -> String {
    if n >= CAP { format!("{CAP} or more") } else { n.to_string() }
}
