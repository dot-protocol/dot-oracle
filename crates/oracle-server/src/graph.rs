//! The graph of Oracle (Blaze 2026-10-18: "add a graph beside Tools … like Obsidian, with separate filters to view just
//! the cortex or the full oracle core"; docs/GRAPH.md). Read-only, computed on each read from what is stored.
//!
//! - `core`: rooms and seats. A seat is joined to a room by the messages it wrote there (at least `min`); two rooms are
//!   joined by replies and refs that cross between them.
//! - `cortex`: the cortex's nodes (question, entity, concept; docs/CORTEX.md) and the claims (claims.rs). Two nodes are
//!   joined when one live mark links both (`same-mark`), or when marks on the same source message link them
//!   (`same-source`); plus the cortex's node-to-node links and the claims' typed edges.
use crate::store::{Result, Store};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const VIEWS: [&str; 3] = ["core", "cortex", "seo"];

fn edge(edges: &mut BTreeMap<(String, String, String), i64>, a: &str, b: &str, kind: &str, w: i64) {
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    *edges.entry((x.to_owned(), y.to_owned(), kind.to_owned())).or_insert(0) += w;
}

fn out(nodes: Vec<Value>, edges: BTreeMap<(String, String, String), i64>, view: &str) -> Value {
    let edges: Vec<Value> = edges.into_iter().map(|((a, b, k), w)| json!({"source": a, "target": b, "kind": k, "weight": w})).collect();
    json!({"view": view, "nodes": nodes, "edges": edges})
}

/// Rooms and seats, joined by who writes where (at least `min` messages) and by replies or refs that cross rooms.
pub fn core(s: &Store, min: i64) -> Result<Value> {
    let channels = s.channels()?; // before taking a connection: a memory store has one, and channels() takes it too
    let c = s.conn()?;
    let mut nodes = Vec::new();
    for (room, count, last_ms, _) in channels {
        nodes.push(json!({"id": format!("room:{room}"), "kind": "room", "label": format!("#{room}"), "size": count, "last_ms": last_ms}));
    }
    let mut edges = BTreeMap::new();
    let mut seats: BTreeMap<String, i64> = BTreeMap::new();
    let mut st = c.prepare("SELECT channel, author, count(*) FROM observations GROUP BY channel, author")?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
    for row in rows {
        let (room, author, n) = row?;
        *seats.entry(author.clone()).or_insert(0) += n;
        if n >= min {
            edge(&mut edges, &format!("seat:{author}"), &format!("room:{room}"), "writes", n);
        }
    }
    for (seat, n) in &seats {
        nodes.push(json!({"id": format!("seat:{seat}"), "kind": "seat", "label": seat, "size": n}));
    }
    for prefix in ["reply:", "ref:"] {
        let hi = format!("{}\u{10FFFF}", prefix);
        let mut st = c.prepare(
            "SELECT o.channel, t.channel, count(*) FROM tags g JOIN observations o ON o.seq = g.obs_seq \
             JOIN observations t ON t.id = substr(g.name, ?3) WHERE g.name >= ?1 AND g.name < ?2 AND t.channel <> o.channel GROUP BY 1, 2",
        )?;
        let rows = st.query_map(rusqlite::params![prefix, hi, (prefix.len() + 1) as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
        for row in rows {
            let (a, b, n) = row?;
            edge(&mut edges, &format!("room:{a}"), &format!("room:{b}"), prefix.trim_end_matches(':'), n);
        }
    }
    Ok(out(nodes, edges, "core"))
}

/// The cortex's nodes and the claims, joined by the marks that link them and by their own links and edges.
/// `approved_only`: only marks Blaze approved count and join (the default read); otherwise drafts (no verdict) count too.
/// Rejected marks never count or join; every node reports approved, drafts and rejected (docs/GRAPH-MODEL.md §4).
pub fn cortex(s: &Store, approved_only: bool) -> Result<Value> {
    let cx = crate::cortex::fold(&s.tagged_any_live(&crate::cortex::TAGS)?);
    let cl = crate::claims::fold(&s.tagged_any(&crate::claims::TAGS)?);
    let mut nodes = Vec::new();
    let known: BTreeSet<String> = cx.nodes.iter().map(|n| n.nid.clone()).collect();
    let rank = |m: &crate::cortex::Mark| match m.verdict.as_ref().map(|v| v.verdict.as_str()) { Some("approve") => 2, Some("reject") => 0, _ => 1 };
    let counts = |m: &crate::cortex::Mark| rank(m) == 2 || (!approved_only && rank(m) == 1);
    let mut tally: BTreeMap<String, [i64; 3]> = BTreeMap::new(); // nid -> [approved, drafts, rejected]
    // Who drafted the marks that hold a node (a seat, or blaze): the Brain cases segment by maker (Blaze, 18 Oct).
    let mut drafted: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for m in cx.marks.iter().filter(|m| m.superseded_by.is_none()) {
        for l in m.links.iter().filter(|l| known.contains(&l.to)) {
            if counts(m) {
                drafted.entry(l.to.clone()).or_default().insert(m.by.clone());
            }
            let t = tally.entry(l.to.clone()).or_insert([0; 3]);
            match rank(m) { 2 => t[0] += 1, 1 => t[1] += 1, _ => t[2] += 1 }
        }
    }
    // Since 25 Oct Brain cases publishes each approved mark as a node carrying Blaze's verdict, joined by cortex:link
    // records, not by marks: such a node is held by its verdict, and a node an approved one links to (the concept or
    // entity it is about) is held by that link. A rejected node never is.
    fn verdict(n: &crate::cortex::Node) -> Option<&str> {
        n.verdict.as_ref().map(|v| v.verdict.as_str())
    }
    let approved: BTreeSet<&str> = cx.nodes.iter().filter(|n| verdict(n) == Some("approve")).map(|n| n.nid.as_str()).collect();
    for n in &cx.nodes {
        let t = tally.get(&n.nid).copied().unwrap_or([0; 3]);
        let linked = n.out.iter().map(|l| l.to.as_str()).chain(n.inbound.iter().map(|l| l.from.as_str())).filter(|x| approved.contains(x)).count() as i64;
        let own = i64::from(approved.contains(n.nid.as_str()));
        let size = if verdict(n) == Some("reject") { 0 } else if approved_only { t[0] + own + linked } else { t[0] + t[1] + own + linked };
        // origin: a Brain-cases node (from Blaze's own prompts, #brain) or a claim (minted in a research room); never mixed by default in the view.
        nodes.push(json!({"id": format!("node:{}", n.nid), "kind": n.kind, "label": n.title, "size": size.max(1), "room": n.room, "origin": "brain",
            "by": drafted.get(&n.nid).map(|s| s.iter().cloned().collect::<Vec<_>>()).or_else(|| n.drafted_by.clone().map(|d| vec![d])).unwrap_or_default(),
            "approved": t[0], "drafts": t[1], "rejected": t[2], "held": size > 0, "form": n.form,
            "verdict": n.verdict.as_ref().map(|v| v.verdict.clone()), "status": n.status.as_ref().map(|v| v.verdict.clone()).unwrap_or_else(|| "open".into())}));
    }
    for c in &cl.claims {
        let label: String = c.statement.chars().take(90).collect();
        nodes.push(json!({"id": format!("claim:{}", c.cid), "kind": "claim", "label": label, "size": 1 + c.out.len() + c.inbound.len(), "room": c.room,
            "status": c.status, "type": c.kind, "origin": "claims", "author": c.author}));
    }
    let mut edges = BTreeMap::new();
    let mut by_source: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for m in cx.marks.iter().filter(|m| m.superseded_by.is_none() && counts(m)) {
        let to: Vec<&str> = m.links.iter().filter(|l| known.contains(&l.to)).map(|l| l.to.as_str()).collect();
        for (i, a) in to.iter().enumerate() {
            for b in &to[i + 1..] {
                if a != b {
                    edge(&mut edges, &format!("node:{a}"), &format!("node:{b}"), "same-mark", 1);
                }
            }
        }
        if let Some(src) = m.source["message"].as_str() {
            by_source.entry(src.to_owned()).or_default().extend(to.iter().map(|t| t.to_string()));
        }
    }
    for set in by_source.values() {
        let v: Vec<&String> = set.iter().collect();
        for (i, a) in v.iter().enumerate() {
            for b in &v[i + 1..] {
                edge(&mut edges, &format!("node:{a}"), &format!("node:{b}"), "same-source", 1);
            }
        }
    }
    for n in &cx.nodes {
        for l in &n.out {
            if known.contains(&l.to) {
                // A weighed link draws at its weight on a 1-4 scale (mention, related, strong, core); an unweighed one as related.
                let w = l.weight.map_or(2, |w| (w * 4.0).round().clamp(1.0, 4.0) as i64);
                edge(&mut edges, &format!("node:{}", n.nid), &format!("node:{}", l.to), &l.kind, w);
            }
        }
    }
    for c in &cl.claims {
        for e in &c.out {
            edge(&mut edges, &format!("claim:{}", c.cid), &format!("claim:{}", e.cid), &e.kind, 1);
        }
    }
    let mut v = out(nodes, edges, "cortex");
    v["marks"] = json!(if approved_only { "approved" } else { "all" });
    Ok(v)
}

/// The SEO view (Blaze, 18 Oct: "the rank of the words as a graph: click a word, see the rooms it was said in and
/// how many times; a room links to the other words ranking in it"): the ranked words from the terms file
/// (docs/TERMS.md, `rooms` per word) and the rooms, joined by `said` edges weighted by the count there. `top` words.
pub fn seo(terms: &Value, top: usize) -> Value {
    let mut nodes = Vec::new();
    let mut edges = BTreeMap::new();
    let mut rooms: BTreeMap<String, i64> = BTreeMap::new();
    for t in terms["terms"].as_array().into_iter().flatten().take(top) {
        let Some(word) = t["term"].as_str() else { continue };
        // A word's size is how often the Time Machine's chats say it (Blaze, 18 Oct: "node sizes dependent on how many
        // times each word is in Time Machine"); the Oracle rooms' count rides beside it.
        let chats = t["chats_count"].as_i64().unwrap_or(0);
        let oracle = t["oracle_count"].as_i64().unwrap_or(0);
        nodes.push(json!({"id": format!("word:{word}"), "kind": "word", "label": word, "size": chats.max(1), "chats_count": chats, "oracle_count": oracle,
            "count": t["count"], "rank": t["rank"], "weight": t["weight"], "documents": t["documents"], "common": t["common"].as_bool().unwrap_or(false)}));
        if let Some(rs) = t["rooms"].as_object() {
            for (room, n) in rs {
                let n = n.as_i64().unwrap_or(0);
                if n > 0 {
                    *rooms.entry(room.clone()).or_insert(0) += n;
                    edge(&mut edges, &format!("word:{word}"), &format!("room:{room}"), "said", n);
                }
            }
        }
    }
    for (room, n) in &rooms {
        nodes.push(json!({"id": format!("room:{room}"), "kind": "room", "label": if room == "chats" { "Time Machine chats".to_string() } else { format!("#{room}") }, "size": n}));
    }
    let mut v = out(nodes, edges, "seo");
    v["generated_at"] = terms["generated_at"].clone();
    v
}
