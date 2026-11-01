//! The read side of the room index (docs/ROOM-INDEX.md, step 3): a page is a query on `msg`, `thread`, `item`, `mark` and `vote`
//! joined to the log for its words. Nothing here folds. Every list is `{items, next_before, next_after, limit, count}`:
//! keyset on `seq`, newest first (`before`) or, for a page catching up, oldest first (`after`). `next_before: null` means the
//! room has nothing older; it is never a guess, because a page asks for one row more than it returns.
//!
//! Bodies are never cut. A body over [`BODY_MAX`] bytes comes back as `content: null` with a typed `body_omitted: {reason, bytes, limit, fetch}`;
//! `fetch` is `GET /observations/{id}/content`, an item endpoint with the same auth that returns the exact bytes. Cargo is sent only when asked for
//! (`include_cargo=true`) and follows the same rule (`cargo_omitted`, `/observations/{id}/cargo`).
//!
//! `signed` is true only for a message this server verified when it was written. An imported row's own `signed:ok` / `cargo-signed:ok` tags are a
//! claim by whoever exported it, not a proof, and nothing records a trusted importer's provenance: such a row is `signed: false` with
//! `verification: "imported_unverified"`.

use crate::store::{Result, Store, StoreError};
use rusqlite::{params_from_iter, types::Value as Sql, Connection};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};

pub const DEFAULT_LIMIT: i64 = 50;
pub const MAX_LIMIT: i64 = 200;
pub const BODY_MAX: i64 = 64 * 1024;

/// The classes a row can have (`roomindex::class_parts`, and `history` for imported rows).
pub const CLASSES: [&str; 7] = ["message", "mark", "card", "state", "annotation", "status", "history"];
pub const THREAD_STATES: [&str; 10] = ["open", "answered", "settled", "closed", "superseded", "archived", "ambient", "history", "stale", "acked"];

fn bad(what: impl Into<String>) -> StoreError {
    StoreError::Rejected(what.into())
}

/// A page's parameters, checked.
#[derive(Debug, Clone, Default)]
pub struct PageQ {
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub limit: Option<i64>,
    pub class: Vec<String>,
    pub author: Option<String>,
    pub state: Vec<String>,
    pub include_cargo: bool,
    /// #cortex: list the graph's links, verdicts and concept nodes too (folded by default, [`crate::store::is_scaffold`]).
    pub scaffold: bool,
}

impl PageQ {
    fn limit(&self) -> Result<i64> {
        match self.limit {
            None => Ok(DEFAULT_LIMIT),
            Some(n) if (1..=MAX_LIMIT).contains(&n) => Ok(n),
            Some(n) => Err(bad(format!("limit must be 1 to {MAX_LIMIT}, not {n}"))),
        }
    }
    fn check(&self) -> Result<()> {
        if self.before.is_some() && self.after.is_some() {
            return Err(bad("before= and after= cannot be used together"));
        }
        for c in &self.class {
            if !CLASSES.contains(&c.as_str()) {
                return Err(bad(format!("unknown class {c}; use {}", CLASSES.join(", "))));
            }
        }
        for c in &self.state {
            if !THREAD_STATES.contains(&c.as_str()) {
                return Err(bad(format!("unknown state {c}; use {}", THREAD_STATES.join(", "))));
            }
        }
        Ok(())
    }
}

/// #cortex's graph scaffolding ([`crate::store::is_scaffold`]) as SQL on the record at `seq`, to leave out of a page of the room.
fn not_scaffold(seq: &str) -> String {
    format!(
        " AND NOT (EXISTS(SELECT 1 FROM tags g WHERE g.obs_seq={seq} AND g.name IN ('cortex:link','cortex:verdict')) \
         OR (EXISTS(SELECT 1 FROM tags g WHERE g.obs_seq={seq} AND g.name='cortex:node') \
             AND coalesce((SELECT CASE WHEN json_valid(o.cargo) THEN json_extract(o.cargo,'$.kind') END FROM observations o WHERE o.seq={seq}),'') <> 'observation'))"
    )
}

/// Whether a page of `room` folds the scaffolding, and if so the approved set to mark its readings with (none when the
/// cortex views cannot be read: the readings are still listed, unmarked).
fn cortex_fold(s: &Store, room: &str, q: &PageQ) -> (bool, Option<std::sync::Arc<crate::cortex::ConceptIndex>>) {
    let fold = room == "cortex" && !q.scaffold;
    (fold, if fold { s.approved_index().ok() } else { None })
}

fn mark_approved(items: &mut [Value], approved: Option<&crate::cortex::ConceptIndex>) {
    let Some(a) = approved else { return };
    for i in items {
        if i["id"].as_str().is_some_and(|id| a.approved.contains(id)) {
            i["approved"] = json!(true);
        }
    }
}

const FOLDED: &str = "links, verdicts and concept nodes of the graph; scaffold=true lists them";

/// Room names are lowercase words with dashes, as everywhere else.
fn room_ok(room: &str) -> Result<()> {
    if crate::cards::valid_channel(room) {
        Ok(())
    } else {
        Err(bad(format!("{room:?} is not a room name")))
    }
}

/// The index is either whole or not there: a reader must never take a half-built one for the room.
fn ready(c: &Connection) -> Result<()> {
    if crate::roomindex::ready(c)? {
        Ok(())
    } else {
        Err(StoreError::Unavailable("the room index is not built yet: run `oracle reindex` (see docs/ROOM-INDEX.md)".into()))
    }
}

fn marks_of(rows: &[(String, String)]) -> Value {
    let mut m: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (react, seat) in rows {
        m.entry(react.as_str()).or_default().push(seat.as_str());
    }
    json!(m)
}

/// The full item for each of `seqs`, in that order. Rows that are not in the index are left out.
fn items_for(c: &Connection, seqs: &[i64], include_cargo: bool) -> Result<Vec<Value>> {
    if seqs.is_empty() {
        return Ok(vec![]);
    }
    let holes = vec!["?"; seqs.len()].join(",");
    let ints = || params_from_iter(seqs.iter().map(|s| Sql::Integer(*s)));
    struct Base {
        v: Map<String, Value>,
        id: String,
        tags: Vec<String>,
    }
    let mut base: HashMap<i64, Base> = HashMap::new();
    {
        let sql = format!(
            "SELECT m.seq, m.channel, m.author, m.class, m.root_seq, m.parent_seq, m.depth, m.intent, m.ask, m.by_ms, m.expires_ms, m.created_ms, \
                    o.id, o.tags_json, o.envelope, \
                    length(CAST(o.content AS BLOB)), CASE WHEN length(CAST(o.content AS BLOB))>{BODY_MAX} THEN NULL ELSE o.content END, \
                    length(CAST(o.cargo AS BLOB)), CASE WHEN {inc}=0 OR length(CAST(o.cargo AS BLOB))>{BODY_MAX} THEN NULL ELSE o.cargo END, \
                    po.id, ro.id \
             FROM msg m JOIN observations o ON o.seq=m.seq \
             LEFT JOIN observations po ON po.seq=m.parent_seq LEFT JOIN observations ro ON ro.seq=m.root_seq \
             WHERE m.seq IN ({holes})",
            inc = include_cargo as i64
        );
        let mut st = c.prepare(&sql)?;
        let mut rows = st.query(ints())?;
        while let Some(r) = rows.next()? {
            let seq: i64 = r.get(0)?;
            let tags_json: String = r.get(13)?;
            let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
            let envelope: i64 = r.get(14)?;
            let signed = envelope == 2;
            let content_bytes: i64 = r.get(15)?;
            let content: Option<String> = r.get(16)?;
            let cargo_bytes: Option<i64> = r.get(17)?;
            let cargo: Option<String> = r.get(18)?;
            let id: String = r.get(12)?;
            let mut v = Map::new();
            v.insert("id".into(), json!(id));
            v.insert("seq".into(), json!(seq));
            v.insert("room".into(), json!(r.get::<_, String>(1)?));
            v.insert("author".into(), json!(r.get::<_, String>(2)?));
            v.insert("class".into(), json!(r.get::<_, String>(3)?));
            v.insert("created_ms".into(), json!(r.get::<_, i64>(11)?));
            v.insert("signed".into(), json!(signed));
            v.insert("verification".into(), json!(if signed { "verified" } else { "imported_unverified" }));
            v.insert("content_bytes".into(), json!(content_bytes));
            match content {
                Some(t) => v.insert("content".into(), json!(t)),
                None => {
                    v.insert("body_omitted".into(), json!({"reason": "too_large", "bytes": content_bytes, "limit": BODY_MAX, "fetch": format!("/observations/{id}/content")}));
                    v.insert("content".into(), Value::Null)
                }
            };
            v.insert("cargo_bytes".into(), json!(cargo_bytes.unwrap_or(0)));
            if include_cargo && cargo_bytes.is_some() {
                match cargo {
                    Some(t) => v.insert("cargo".into(), json!(t)),
                    None => {
                        v.insert("cargo_omitted".into(), json!({"reason": "too_large", "bytes": cargo_bytes, "limit": BODY_MAX, "fetch": format!("/observations/{id}/cargo")}));
                        v.insert("cargo".into(), Value::Null)
                    }
                };
            }
            v.insert("thread".into(), json!({"root": r.get::<_, Option<String>>(20)?, "root_seq": r.get::<_, i64>(4)?, "parent": r.get::<_, Option<String>>(19)?, "depth": r.get::<_, i64>(6)?}));
            v.insert("intent".into(), json!(r.get::<_, Option<String>>(7)?));
            v.insert("ask".into(), json!(r.get::<_, Option<String>>(8)?));
            v.insert("by_ms".into(), json!(r.get::<_, Option<i64>>(9)?));
            v.insert("expires_ms".into(), json!(r.get::<_, Option<i64>>(10)?));
            base.insert(seq, Base { v, id, tags });
        }
    }
    // Supersession comes from the log's own links, and the original is always kept: who replaced this, who withdrew it, what this replaced.
    let mut by_name: HashMap<String, Vec<i64>> = HashMap::new();
    {
        let names: Vec<String> = base.values().flat_map(|b| [format!("supersedes:{}", b.id), format!("archives:{}", b.id)]).collect();
        for chunk in names.chunks(400) {
            let holes = vec!["?"; chunk.len()].join(",");
            let mut st = c.prepare(&format!("SELECT name, obs_seq FROM tags WHERE name IN ({holes})"))?;
            let mut rows = st.query(params_from_iter(chunk.iter().map(|n| Sql::Text(n.clone()))))?;
            while let Some(r) = rows.next()? {
                by_name.entry(r.get::<_, String>(0)?).or_default().push(r.get(1)?);
            }
        }
    }
    let id_of = |c: &Connection, seq: i64| -> Result<Option<String>> {
        Ok(c.query_row("SELECT id FROM observations WHERE seq=?1", [seq], |r| r.get(0)).ok())
    };
    let mut items: HashMap<i64, Vec<Value>> = HashMap::new();
    let mut marks: HashMap<i64, Vec<(String, String)>> = HashMap::new();
    let mut votes: HashMap<i64, Vec<Value>> = HashMap::new();
    {
        let mut st = c.prepare(&format!("SELECT msg_seq, k, kind, state, owner FROM item WHERE msg_seq IN ({holes}) ORDER BY msg_seq, k"))?;
        let mut rows = st.query(ints())?;
        while let Some(r) = rows.next()? {
            items.entry(r.get(0)?).or_default().push(json!({"k": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?, "state": r.get::<_, String>(3)?, "owner": r.get::<_, Option<String>>(4)?}));
        }
        let mut st = c.prepare(&format!("SELECT msg_seq, react, seat FROM mark WHERE msg_seq IN ({holes}) ORDER BY reply_seq"))?;
        let mut rows = st.query(ints())?;
        while let Some(r) = rows.next()? {
            marks.entry(r.get(0)?).or_default().push((r.get(1)?, r.get(2)?));
        }
        let mut st = c.prepare(&format!("SELECT msg_seq, k, seat, stance FROM vote WHERE msg_seq IN ({holes}) ORDER BY msg_seq, k, seat"))?;
        let mut rows = st.query(ints())?;
        while let Some(r) = rows.next()? {
            votes.entry(r.get(0)?).or_default().push(json!({"k": r.get::<_, String>(1)?, "seat": r.get::<_, String>(2)?, "stance": r.get::<_, String>(3)?}));
        }
    }
    let stale = stale_subjects(c, &base.iter().map(|(seq, b)| (*seq, &b.v, b.tags.as_slice())).collect::<Vec<_>>())?;
    let mut out = Vec::new();
    for seq in seqs {
        let Some(mut b) = base.remove(seq) else { continue };
        b.v.insert("stale".into(), json!(stale.get(seq).cloned().unwrap_or_default()));
        let newest = |name: String| by_name.get(&name).and_then(|v| v.iter().max().copied());
        let superseded_by = match newest(format!("supersedes:{}", b.id)) {
            Some(s) => id_of(c, s)?,
            None => None,
        };
        let archived_by = match newest(format!("archives:{}", b.id)) {
            Some(s) => id_of(c, s)?,
            None => None,
        };
        let supersedes: Vec<&str> = b.tags.iter().filter_map(|t| t.strip_prefix("supersedes:")).collect();
        b.v.insert("superseded_by".into(), json!(superseded_by));
        b.v.insert("supersedes".into(), json!(supersedes));
        b.v.insert("archived_by".into(), json!(archived_by));
        b.v.insert("items".into(), json!(items.remove(seq).unwrap_or_default()));
        b.v.insert("marks".into(), marks_of(&marks.remove(seq).unwrap_or_default()));
        b.v.insert("votes".into(), json!(votes.remove(seq).unwrap_or_default()));
        out.push(Value::Object(b.v));
    }
    Ok(out)
}

/// The words of a message that could name a subject: runs of letters, digits and `.-_`, holding a `.` or `-`
/// (policy.p1, market-pulse-nan, r-104-retire-old-oracle), plus the subject a `ref:`/`subject:` tag names.
fn subject_words(content: &str, tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = content
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_')))
        .map(|w| w.trim_matches(|ch: char| matches!(ch, '.' | '-' | '_')))
        .filter(|w| w.len() >= 3 && w.len() <= 80 && (w.contains('.') || w.contains('-')))
        .map(str::to_ascii_lowercase)
        .collect();
    out.extend(tags.iter().filter_map(|t| t.strip_prefix("ref:").or_else(|| t.strip_prefix("subject:"))).map(str::to_string));
    out.sort();
    out.dedup();
    out
}

/// Blaze, 2026-09-30: "nothing should be guessed". A message that names a subject (by its exact name, or a `ref:`
/// tag) is read with the subject's state as it is NOW when that state was written after the message: the message
/// may say "P1 waits on Blaze" while the record says it was retired a day ago. "After" is the log order (seq). `stale` lists each such subject:
/// {subject, state, owner, id, at}. A state record never flags itself.
fn stale_subjects(c: &Connection, base: &[(i64, &Map<String, Value>, &[String])]) -> Result<HashMap<i64, Vec<Value>>> {
    let mut words: HashMap<i64, Vec<String>> = HashMap::new();
    for (seq, v, tags) in base {
        let own = if v.get("class").or_else(|| v.get("kind")).and_then(Value::as_str) == Some("state") {
            tags.iter().find_map(|t| t.strip_prefix("subject:")).map(str::to_string)
        } else {
            None
        };
        let w: Vec<String> = subject_words(v.get("content").and_then(Value::as_str).unwrap_or(""), tags)
            .into_iter()
            .filter(|w| Some(w) != own.as_ref())
            .collect();
        if !w.is_empty() {
            words.insert(*seq, w);
        }
    }
    let mut all: Vec<String> = words.values().flatten().cloned().collect();
    all.sort();
    all.dedup();
    let mut heads: HashMap<String, (i64, String, String, String, i64)> = HashMap::new();
    for chunk in all.chunks(400) {
        let holes = vec!["?"; chunk.len()].join(",");
        let mut st = c.prepare(&format!(
            "SELECT h.subject, h.obs_seq, h.state, h.owner, o.id, o.created_ms FROM state_heads h JOIN observations o ON o.seq = h.obs_seq WHERE h.subject IN ({holes})"
        ))?;
        let mut rows = st.query(params_from_iter(chunk.iter().map(|n| Sql::Text(n.clone()))))?;
        while let Some(r) = rows.next()? {
            heads.insert(r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?));
        }
    }
    let mut out: HashMap<i64, Vec<Value>> = HashMap::new();
    for (seq, ws) in words {
        for w in ws {
            if let Some((hseq, state, owner, id, hat)) = heads.get(&w) {
                if *hseq > seq {  // the log order is the truth; `at` is reported, never compared
                    out.entry(seq).or_default().push(json!({"subject": w, "state": state, "owner": owner, "id": id, "at": hat}));
                }
            }
        }
    }
    Ok(out)
}

fn seqs_of(c: &Connection, sql: &str, p: Vec<Sql>) -> Result<Vec<i64>> {
    let mut st = c.prepare(sql)?;
    let v = st.query_map(params_from_iter(p), |r| r.get::<_, i64>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(v)
}

fn in_list(field: &str, values: &[String], p: &mut Vec<Sql>) -> String {
    if values.is_empty() {
        return String::new();
    }
    for v in values {
        p.push(Sql::Text(v.clone()));
    }
    format!(" AND {field} IN ({})", vec!["?"; values.len()].join(","))
}

/// `GET /rooms/{ch}/messages`: the whole room, every class unless asked to narrow, newest first (`before`) or oldest first from a seq (`after`).
/// The same `stale` marks on any list of observations (the `/search` listing ox read and the MCP tools use): each
/// item needs `seq`, `content` and `tags`.
pub fn mark_stale(s: &Store, items: &mut [Value]) -> Result<()> {
    let parts: Vec<(i64, Map<String, Value>, Vec<String>)> = items
        .iter()
        .filter_map(|it| {
            let tags = it["tags"].as_array().map(|t| t.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
            Some((it["seq"].as_i64()?, it.as_object()?.clone(), tags))
        })
        .collect();
    let view: Vec<(i64, &Map<String, Value>, &[String])> = parts.iter().map(|(q, v, t)| (*q, v, t.as_slice())).collect();
    let stale = s.with_conn(|c| stale_subjects(c, &view))?;
    for it in items.iter_mut() {
        if let (Some(q), Some(o)) = (it["seq"].as_i64(), it.as_object_mut()) {
            o.insert("stale".into(), json!(stale.get(&q).cloned().unwrap_or_default()));
        }
    }
    Ok(())
}

pub fn messages(s: &Store, room: &str, q: &PageQ) -> Result<Value> {
    room_ok(room)?;
    q.check()?;
    let limit = q.limit()?;
    let (fold, approved) = cortex_fold(s, room, q);
    s.with_conn(|c| {
        ready(c)?;
        let mut p: Vec<Sql> = vec![Sql::Text(room.to_owned())];
        let mut sql = String::from("SELECT m.seq FROM msg m WHERE m.channel=?1");
        if fold {
            sql.push_str(&not_scaffold("m.seq"));
        }
        let (cmp, order, bound) = match q.after {
            Some(a) => (">", "ASC", a),
            None => ("<", "DESC", q.before.unwrap_or(i64::MAX)),
        };
        p.push(Sql::Integer(bound));
        sql.push_str(&format!(" AND m.seq{cmp}?2"));
        sql.push_str(&in_list("m.class", &q.class, &mut p));
        if let Some(a) = &q.author {
            p.push(Sql::Text(a.clone()));
            sql.push_str(&format!(" AND m.author=?{}", p.len()));
        }
        // One more than asked for: whether it exists is the only honest answer to "is there more".
        sql.push_str(&format!(" ORDER BY m.seq {order} LIMIT {}", limit + 1));
        let mut seqs = seqs_of(c, &sql, p)?;
        let more = seqs.len() as i64 > limit;
        seqs.truncate(limit as usize);
        let mut items = items_for(c, &seqs, q.include_cargo)?;
        mark_approved(&mut items, approved.as_deref());
        let last = items.last().and_then(|i| i["seq"].as_i64());
        // Going back, `next_before` is null when the room has nothing older. Going forward the cursor is never null: it is the last seq returned, or the
        // `after` asked for when nothing is newer, so a live page resumes from it after a reconnect and misses or repeats nothing. `more` says whether to ask again now.
        let mut out = match q.after {
            Some(after) => json!({"room": room, "items": items, "count": seqs.len(), "limit": limit, "next_before": Value::Null, "next_after": last.unwrap_or(after), "more": more}),
            None => json!({"room": room, "items": items, "count": seqs.len(), "limit": limit, "next_before": if more { json!(last) } else { Value::Null }, "next_after": Value::Null}),
        };
        if fold {
            out["folded"] = json!(FOLDED);
        }
        Ok(out)
    })
}

/// `GET /rooms/{ch}/threads`: thread rows by their newest message, each with its root item. History-only threads have no row (nothing judged them).
pub fn threads(s: &Store, room: &str, q: &PageQ) -> Result<Value> {
    room_ok(room)?;
    q.check()?;
    if q.after.is_some() {
        return Err(bad("threads pages go back with before=; after= is for messages"));
    }
    let limit = q.limit()?;
    let (fold, approved) = cortex_fold(s, room, q);
    s.with_conn(|c| {
        ready(c)?;
        let mut p: Vec<Sql> = vec![Sql::Text(room.to_owned()), Sql::Integer(q.before.unwrap_or(i64::MAX))];
        let mut sql = String::from("SELECT t.last_seq FROM thread t WHERE t.channel=?1 AND t.last_seq<?2");
        if fold {
            sql.push_str(&not_scaffold("t.root_seq"));
        }
        sql.push_str(&in_list("t.state", &q.state, &mut p));
        sql.push_str(&format!(" ORDER BY t.last_seq DESC LIMIT {}", limit + 1));
        let mut lasts = seqs_of(c, &sql, p)?;
        let more = lasts.len() as i64 > limit;
        lasts.truncate(limit as usize);
        let mut out = Vec::new();
        for last in &lasts {
            let row = c.query_row(
                "SELECT t.root_seq, t.state, t.replies, t.last_seq, t.last_ms, t.participants, t.answered_by, t.settled_by, t.settled_word, \
                        (SELECT id FROM observations WHERE seq=t.answered_by), (SELECT id FROM observations WHERE seq=t.settled_by) \
                 FROM thread t WHERE t.channel=?1 AND t.last_seq=?2",
                rusqlite::params![room, last],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?, r.get::<_, Option<String>>(9)?, r.get::<_, Option<String>>(10)?, r.get::<_, Option<String>>(8)?)),
            )?;
            let mut root = items_for(c, &[row.0], q.include_cargo)?;
            mark_approved(&mut root, approved.as_deref());
            let root = root.pop().unwrap_or(Value::Null);
            out.push(json!({"root": root, "state": row.1, "replies": row.2, "last_seq": row.3, "last_ms": row.4,
                "participants": serde_json::from_str::<Value>(&row.5).unwrap_or(Value::Null), "answered_by": row.6, "settled_by": row.7, "settled_word": row.8}));
        }
        let next = if more { json!(lasts.last()) } else { Value::Null };
        let mut page = json!({"room": room, "threads": out, "count": lasts.len(), "limit": limit, "next_before": next});
        if fold {
            page["folded"] = json!(FOLDED);
        }
        Ok(page)
    })
}

/// `GET /threads/{root}`: one thread. `root` is an observation id or a seq. The root comes first, as `root`; the rest is paged newest first.
pub fn thread(s: &Store, root: &str, q: &PageQ) -> Result<Value> {
    q.check()?;
    if q.after.is_some() || !q.state.is_empty() || !q.class.is_empty() {
        return Err(bad("a thread page takes before=, limit= and include_cargo= only"));
    }
    let limit = q.limit()?;
    s.with_conn(|c| {
        ready(c)?;
        let root_seq: i64 = match root.parse::<i64>() {
            Ok(n) => n,
            Err(_) => c.query_row("SELECT seq FROM observations WHERE id=?1", [root], |r| r.get(0)).map_err(|_| StoreError::NotFound)?,
        };
        // Any message names its thread: asking for a reply returns the thread it belongs to.
        let root_seq: i64 = c.query_row("SELECT root_seq FROM msg WHERE seq=?1", [root_seq], |r| r.get(0)).map_err(|_| StoreError::NotFound)?;
        let head = items_for(c, &[root_seq], q.include_cargo)?.pop().ok_or(StoreError::NotFound)?;
        let meta = c
            .query_row(
                "SELECT t.state, t.replies, t.last_seq, t.last_ms, t.participants, t.settled_word, t.settled_by, \
                        (SELECT id FROM observations WHERE seq=t.settled_by), (SELECT id FROM observations WHERE seq=t.answered_by) \
                 FROM thread t WHERE t.root_seq=?1",
                [root_seq],
                |r| {
                    // `settled_word` is what a decider's mark made of the root (`accepted`, `declined`, `answered`); `settled_by` the id of that mark. Both absent while
                    // nothing settled it. A State revision that adopts a proposal is checked against these (docs/ROOM-INDEX.md).
                    Ok(json!({"state": r.get::<_, String>(0)?, "replies": r.get::<_, i64>(1)?, "last_seq": r.get::<_, i64>(2)?, "last_ms": r.get::<_, i64>(3)?,
                        "participants": serde_json::from_str::<Value>(&r.get::<_, String>(4)?).unwrap_or(Value::Null),
                        "settled_word": r.get::<_, Option<String>>(5)?, "settled_seq": r.get::<_, Option<i64>>(6)?,
                        "settled_by": r.get::<_, Option<String>>(7)?, "answered_by": r.get::<_, Option<String>>(8)?}))
                },
            )
            .unwrap_or(Value::Null);
        let mut seqs = seqs_of(
            c,
            &format!("SELECT m.seq FROM msg m WHERE m.root_seq=?1 AND m.seq<>?1 AND m.seq<?2 ORDER BY m.seq DESC LIMIT {}", limit + 1),
            vec![Sql::Integer(root_seq), Sql::Integer(q.before.unwrap_or(i64::MAX))],
        )?;
        let more = seqs.len() as i64 > limit;
        seqs.truncate(limit as usize);
        let items = items_for(c, &seqs, q.include_cargo)?;
        let last = items.last().and_then(|i| i["seq"].as_i64());
        Ok(json!({"root": head, "thread": meta, "items": items, "count": seqs.len(), "limit": limit, "next_before": if more { json!(last) } else { Value::Null }}))
    })
}

/// One body, exactly as the log holds it: `field` is `content` or `cargo`. The bytes and their sha-256, for `GET /observations/{id}/content|cargo`.
pub fn body(s: &Store, id: &str, field: &str) -> Result<(String, String)> {
    let o = s.get(id)?;
    let text = match field {
        "content" => o.content,
        "cargo" => o.cargo.ok_or(StoreError::NotFound)?,
        _ => return Err(StoreError::NotFound),
    };
    use sha2::{Digest, Sha256};
    let sha = hex::encode(Sha256::digest(text.as_bytes()));
    Ok((text, sha))
}

/// `GET /asked`: the request ledger. Every item of every message `seat` wrote, grouped by message, newest message first; `state=` (a comma list of item
/// states) keeps only the items in those states and only the messages that have any. Paged by the message's seq like every list: `before` is the last seq
/// returned, `next_before: null` means nothing older. Each item carries its words (`text`, from the cargo that declared it), owner, and who moved it last.
pub fn asked(s: &Store, seat: &str, q: &PageQ) -> Result<Value> {
    if seat.is_empty() || seat.len() > 64 {
        return Err(bad("seat is required"));
    }
    if q.after.is_some() || !q.class.is_empty() {
        return Err(bad("asked pages go back with before=; it takes state=, limit= and before= only"));
    }
    for st in &q.state {
        if !crate::projection::ITEM_STATES.contains(&st.as_str()) {
            return Err(bad(format!("unknown item state {st}; use {}", crate::projection::ITEM_STATES.join(", "))));
        }
    }
    let limit = q.limit()?;
    s.with_conn(|c| {
        ready(c)?;
        let mut p: Vec<Sql> = vec![Sql::Text(seat.to_owned()), Sql::Integer(q.before.unwrap_or(i64::MAX))];
        let mut sql = String::from("SELECT DISTINCT i.msg_seq FROM item i WHERE i.author=?1 AND i.msg_seq<?2");
        sql.push_str(&in_list("i.state", &q.state, &mut p));
        sql.push_str(&format!(" ORDER BY i.msg_seq DESC LIMIT {}", limit + 1));
        let mut msgs = seqs_of(c, &sql, p)?;
        let more = msgs.len() as i64 > limit;
        msgs.truncate(limit as usize);
        let mut out = Vec::new();
        let mut item_count = 0usize;
        for m in &msgs {
            let (id, room, created_ms, content, root, intent, ask): (String, String, i64, String, Option<String>, Option<String>, Option<String>) = c.query_row(
                "SELECT o.id, o.channel, o.created_ms, substr(o.content,1,400), (SELECT id FROM observations WHERE seq=m.root_seq), m.intent, m.ask \
                 FROM msg m JOIN observations o ON o.seq=m.seq WHERE m.seq=?1",
                [m],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )?;
            let mut p2: Vec<Sql> = vec![Sql::Integer(*m)];
            let filter = in_list("i.state", &q.state, &mut p2);
            let mut st = c.prepare(&format!(
                "SELECT i.k, i.kind, i.text, i.state, i.owner, (SELECT id FROM observations WHERE seq=i.moved_by), i.moved_ms, i.by_model \
                 FROM item i WHERE i.msg_seq=?1{filter} ORDER BY i.k"
            ))?;
            let items = st
                .query_map(params_from_iter(p2), |r| {
                    Ok(json!({"k": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "text": r.get::<_, Option<String>>(2)?, "state": r.get::<_, String>(3)?,
                        "owner": r.get::<_, Option<String>>(4)?, "moved_by": r.get::<_, Option<String>>(5)?, "moved_ms": r.get::<_, Option<i64>>(6)?, "by_model": r.get::<_, i64>(7)? != 0}))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            item_count += items.len();
            let card: String = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(160).collect();
            out.push(json!({"message": {"id": id, "seq": m, "room": room, "created_ms": created_ms, "card": card, "thread_root": root, "intent": intent, "ask": ask}, "items": items}));
        }
        Ok(json!({"seat": seat, "asked": out, "count": msgs.len(), "item_count": item_count, "limit": limit, "next_before": if more { json!(msgs.last()) } else { Value::Null }}))
    })
}

/// `GET /rooms/{ch}/unread`: what a seat has not read in a room. The cursor is the seat's read mark; the count is of other seats' speech (messages, statuses,
/// cards) after it, capped at 1000; page them with `GET /rooms/{ch}/messages?after=<cursor>`.
pub fn unread(s: &Store, room: &str, seat: &str) -> Result<Value> {
    room_ok(room)?;
    if seat.is_empty() || seat.len() > 64 {
        return Err(bad("seat is required"));
    }
    s.with_conn(|c| {
        ready(c)?;
        let cursor: i64 = c
            .query_row("SELECT mark_seq FROM cursors WHERE agent=?1 AND channel=?2", rusqlite::params![seat, room], |r| r.get(0))
            .unwrap_or(0);
        let (count, first): (i64, Option<i64>) = c.query_row(
            "SELECT count(*), min(seq) FROM (SELECT seq FROM msg WHERE channel=?1 AND seq>?2 AND author<>?3 AND class IN ('message','status','card') ORDER BY seq LIMIT 1000)",
            rusqlite::params![room, cursor, seat],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let latest: Option<i64> = c.query_row("SELECT max(seq) FROM msg WHERE channel=?1", [room], |r| r.get(0))?;
        Ok(json!({"room": room, "seat": seat, "cursor": cursor, "unread": count, "capped": count >= 1000, "first_unread_seq": first, "latest_seq": latest}))
    })
}

/// A search page's parameters.
#[derive(Debug, Clone, Default)]
pub struct SearchQ {
    pub q: String,
    pub room: Option<String>,
    pub author: Option<String>,
    pub class: Vec<String>,
    pub before: Option<i64>,
    pub limit: Option<i64>,
    /// `newest` (the default: a chain by seq, like every list) or `rank` (the best matches, one page).
    pub order: Option<String>,
    /// Any word matches (OR); by default every word must.
    pub any: bool,
    pub include_cargo: bool,
    /// Match the words in the text only (`content:` column), not in the tags.
    pub text_only: bool,
    /// `order=rank|date`: how many matches to skip.
    pub offset: Option<i64>,
    /// Also say where the words are: every room with a keyword match, its count and the first and last date (`rooms`).
    pub by_room: bool,
}

/// Says where a hit matched (`match_in`: content | cargo | tags, with `match_pointer` and `match_span` for cargo and content; null when only the meaning
/// search found it). fts_cargo_v1, lexical: see `store::match_at`. The search itself, its filters and its order are not touched by this.
fn add_match(c: &Connection, seq: i64, expr: &str, o: &mut Map<String, Value>) -> Result<()> {
    match crate::store::match_at(c, seq, expr)? {
        Some(at) => {
            if let Value::Object(m) = serde_json::to_value(at).map_err(|e| StoreError::Internal(e.to_string()))? {
                o.extend(m);
            }
        }
        None => {
            o.insert("match_in".into(), Value::Null);
        }
    }
    Ok(())
}

/// `GET /search` as a page of the room's items: the words match, the room, author and class are asked for by name (the search row carries the room and class
/// as tokens, so a room's search only ranks that room's rows), and each match is the same item every other route returns, plus a `score`.
/// `order=newest` pages by seq (`before`, `next_before` null when exhausted); `order=rank` is the best `limit` and says `truncated` when there were more.
pub fn search(s: &Store, q: &SearchQ) -> Result<Value> {
    search_with(s, q, None)
}

/// `search`, fusing in meaning search when `sem` is on and the order is `rank`; with `by_room`, plus `rooms`.
pub fn search_with(s: &Store, q: &SearchQ, sem: Option<&crate::semantic::Semantic>) -> Result<Value> {
    let mut page = search_page(s, q, sem)?;
    if q.by_room {
        page["rooms"] = rooms_of(s, q)?;
    }
    Ok(page)
}

/// The room and class a match must carry, as FTS terms on the tags column ("" when neither is asked).
fn scope_of(q: &SearchQ) -> String {
    let mut scope = String::new();
    if let Some(r) = &q.room {
        scope.push_str(&format!(" AND tags:{}", crate::roomindex::fts_tokens(r, "x").split_once(' ').map_or("", |t| t.0)));
    }
    if !q.class.is_empty() {
        let cls: Vec<String> = q.class.iter().map(|k| format!("tags:{}", crate::roomindex::fts_tokens("x", k).split_once(' ').map_or("", |t| t.1))).collect();
        scope.push_str(&format!(" AND ({})", cls.join(" OR ")));
    }
    scope
}

/// Where the words are said (Noor, 2026-10-08: "find every Oppenheimer observation across all 79 rooms"): every room
/// with a keyword match under the same words, room, class and author as the page, how many, and the first and last
/// date (the message's own time, so imported history dates as written). Keyword matches only: a meaning match has no
/// count. Most matches first; at most 500 rooms.
pub fn rooms_of(s: &Store, q: &SearchQ) -> Result<Value> {
    let terms = crate::store::fts_query(&q.q, if q.any { " OR " } else { " AND " });
    if terms.is_empty() {
        return Ok(json!([]));
    }
    s.with_conn(|c| {
        ready(c)?;
        let words = if q.text_only { format!("{{content cargo}}:({terms})") } else { format!("({terms})") };
        let mut p: Vec<Sql> = vec![Sql::Text(format!("{words}{}", scope_of(q)))];
        let mut filter = String::from(" FROM obs_fts f JOIN observations o ON o.seq=f.rowid WHERE obs_fts MATCH ?1");
        if let Some(a) = q.author.as_ref().filter(|a| !a.is_empty()) {
            p.push(Sql::Text(a.clone()));
            filter.push_str(" AND o.author=?2");
        }
        let mut st = c.prepare(&format!(
            "SELECT o.channel, count(*), min(COALESCE(o.authored_ms, o.created_ms)), max(COALESCE(o.authored_ms, o.created_ms)){filter} \
             GROUP BY o.channel ORDER BY 2 DESC, 1 LIMIT 500"
        ))?;
        let rows = st
            .query_map(params_from_iter(p.iter()), |r| {
                Ok(json!({"room": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)?, "first_ms": r.get::<_, i64>(2)?, "last_ms": r.get::<_, i64>(3)?}))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Value::Array(rows))
    })
}

fn search_page(s: &Store, q: &SearchQ, sem: Option<&crate::semantic::Semantic>) -> Result<Value> {
    let limit = PageQ { limit: q.limit, ..Default::default() }.limit()?;
    PageQ { class: q.class.clone(), ..Default::default() }.check()?;
    if let Some(r) = &q.room {
        room_ok(r)?;
    }
    // newest: by seq, the order Oracle received them (before= pages); rank: best match first; date: the message's own time,
    // newest first (an imported message sorts by when it was written, not when it was imported). rank and date page by offset=.
    let order = match q.order.as_deref() {
        None | Some("newest") => "newest",
        Some("rank") => "rank",
        Some("date") => "date",
        Some(o) => return Err(bad(format!("order must be newest, rank or date, not {o}"))),
    };
    if order != "newest" && q.before.is_some() {
        return Err(bad(format!("order={order} pages with offset=; before= is for order=newest")));
    }
    if order == "newest" && q.offset.is_some() {
        return Err(bad("order=newest pages with before=, not offset="));
    }
    let offset = q.offset.unwrap_or(0);
    if !(0..=100_000).contains(&offset) {
        return Err(bad("offset must be 0 to 100000"));
    }
    let terms = crate::store::fts_query(&q.q, if q.any { " OR " } else { " AND " });
    if terms.is_empty() {
        return Ok(json!({"q": q.q, "room": q.room, "items": [], "count": 0, "total": 0, "limit": limit, "order": order, "truncated": false, "next_before": Value::Null, "next_offset": Value::Null}));
    }
    s.with_conn(|c| {
        ready(c)?;
        let scope = scope_of(q);
        let words = |t: &str| if q.text_only { format!("{{content cargo}}:({t})") } else { format!("({t})") };
        let m = format!("{}{scope}", words(&terms));
        let expr = m.clone();
        let mut p: Vec<Sql> = vec![Sql::Text(m), Sql::Integer(q.before.unwrap_or(i64::MAX))];
        let mut filter = String::from(" FROM obs_fts f JOIN observations o ON o.seq=f.rowid WHERE obs_fts MATCH ?1 AND f.rowid<?2");
        if let Some(a) = q.author.as_ref().filter(|a| !a.is_empty()) {
            p.push(Sql::Text(a.clone()));
            filter.push_str(" AND o.author=?3");
        }
        // Every match, not only this page: the reader is told how many there are.
        let total: i64 = c.query_row(&format!("SELECT count(*){filter}"), params_from_iter(p.iter()), |r| r.get(0))?;
        // Best match with meaning search on: the any-word keyword list and the meaning list, fused by reciprocal rank
        // (measured on the canon gold set: 53.3% hit@5 / 61.7% hit@10 against 43.3% / 55.0% for keyword alone).
        let mut engine = String::from("keyword");
        if order == "rank" {
            if let Some(sem) = sem.filter(|x| x.windows_loaded() > 0) {
                // One order per query, whatever the page: RRF over the measured window (SIDE keyword + SIDE meaning
                // candidates), then the remaining keyword hits in bm25 order. Growing the window with offset= would
                // re-score items between pages, so a later page could repeat or skip one.
                let side = crate::semantic::SIDE as i64;
                let kw_side = side.max(offset + limit + 1);
                match sem.query(&q.q, (side as usize) * 8) {
                    Ok(near) => {
                        let any = crate::store::fts_query(&q.q, " OR ");
                        let mut kp: Vec<Sql> = vec![Sql::Text(format!("{}{scope}", words(&any))), Sql::Integer(i64::MAX)];
                        let mut kf = String::from(" FROM obs_fts f JOIN observations o ON o.seq=f.rowid WHERE obs_fts MATCH ?1 AND f.rowid<?2");
                        if let Some(a) = q.author.as_ref().filter(|a| !a.is_empty()) {
                            kp.push(Sql::Text(a.clone()));
                            kf.push_str(" AND o.author=?3");
                        }
                        let kw_total: i64 = c.query_row(&format!("SELECT count(*){kf}"), params_from_iter(kp.iter()), |r| r.get(0))?;
                        let mut st = c.prepare(&format!("SELECT f.rowid{kf} ORDER BY bm25(obs_fts) * {}, f.rowid DESC LIMIT {kw_side}", crate::store::room_weight_sql("o.channel")))?;
                        let kw: Vec<i64> = st.query_map(params_from_iter(kp.iter()), |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
                        // A meaning match must be in the same room/class/author the keyword side was held to.
                        // Same room/class (bound as an FTS match on the tags) and author as the keyword side.
                        let mut check = String::from("SELECT 1 FROM obs_fts f JOIN observations o ON o.seq=f.rowid WHERE f.rowid=?1");
                        let mut fixed: Vec<Sql> = Vec::new();
                        if !scope.is_empty() {
                            fixed.push(Sql::Text(scope.trim_start_matches(" AND ").to_string()));
                            check.push_str(&format!(" AND obs_fts MATCH ?{}", fixed.len() + 1));
                        }
                        if let Some(a) = q.author.as_ref().filter(|a| !a.is_empty()) {
                            fixed.push(Sql::Text(a.clone()));
                            check.push_str(&format!(" AND o.author=?{}", fixed.len() + 1));
                        }
                        let mut ok = c.prepare(&check)?;
                        let mut meaning = Vec::new();
                        for (seq, _) in near {
                            let mut args: Vec<Sql> = vec![Sql::Integer(seq)];
                            args.extend(fixed.iter().cloned());
                            if ok.exists(params_from_iter(args.iter()))? {
                                meaning.push(seq);
                                if meaning.len() as i64 == side {
                                    break;
                                }
                            }
                        }
                        let kw_set: std::collections::HashSet<i64> = kw.iter().copied().collect();
                        let window: Vec<i64> = kw.iter().take(side as usize).copied().collect();
                        let mut fused = crate::semantic::rrf(&[window, meaning], crate::semantic::RRF_K);
                        crate::store::reweigh(c, &mut fused)?;
                        // A meaning hit outside the fetched keyword prefix may still be a keyword hit.
                        // Check membership against the full filtered match set, so total is page-independent.
                        let mut keyword_member = c.prepare(&format!("SELECT 1{kf} AND f.rowid=?{}", kp.len() + 1))?;
                        let mut meaning_only = 0i64;
                        for (seq, _) in fused.iter().filter(|(seq, _)| !kw_set.contains(seq)) {
                            let mut args = kp.clone();
                            args.push(Sql::Integer(*seq));
                            if !keyword_member.exists(params_from_iter(args.iter()))? { meaning_only += 1; }
                        }
                        let shown: std::collections::HashSet<i64> = fused.iter().map(|h| h.0).collect();
                        fused.extend(kw.iter().skip(side as usize).filter(|x| !shown.contains(x)).map(|x| (*x, 0.0)));
                        let page: Vec<(i64, f64)> = fused.iter().skip(offset as usize).take(limit as usize + 1).copied().collect();
                        let more = page.len() as i64 > limit;
                        let seqs: Vec<i64> = page.iter().take(limit as usize).map(|h| h.0).collect();
                        let score: HashMap<i64, f64> = page.iter().copied().collect();
                        let mut items = items_for(c, &seqs, q.include_cargo)?;
                        let any_expr = format!("{}{scope}", words(&any));
                        for i in &mut items {
                            if let (Some(seq), Some(o)) = (i["seq"].as_i64(), i.as_object_mut()) {
                                o.insert("score".into(), json!(score.get(&seq).copied().unwrap_or(0.0)));
                                add_match(c, seq, &any_expr, o)?;
                            }
                        }
                        let next_offset = if more { json!(offset + seqs.len() as i64) } else { Value::Null };
                        return Ok(json!({"q": q.q, "room": q.room, "items": items, "count": seqs.len(), "total": kw_total + meaning_only,
                                         "offset": offset, "limit": limit, "order": order, "in": if q.text_only { "text" } else { "all" },
                                         "truncated": more, "next_before": Value::Null, "next_offset": next_offset, "engine": "hybrid"}));
                    }
                    Err(e) => engine = format!("keyword (meaning search unavailable: {e})"),
                }
            }
        }
        // Every order ends on the seq, so equal scores page the same way every time (offset= paging).
        let weighted = format!("bm25(obs_fts) * {}", crate::store::room_weight_sql("o.channel"));
        let by = match order { "rank" => format!("{weighted}, f.rowid DESC"), "date" => "o.created_ms DESC, f.rowid DESC".into(), _ => "f.rowid DESC".into() };
        let sql = format!("SELECT f.rowid, {weighted}{filter} ORDER BY {by} LIMIT {} OFFSET {offset}", limit + 1);
        let mut st = c.prepare(&sql)?;
        let mut hits: Vec<(i64, f64)> = st.query_map(params_from_iter(p), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
        let more = hits.len() as i64 > limit;
        hits.truncate(limit as usize);
        let seqs: Vec<i64> = hits.iter().map(|h| h.0).collect();
        let score: HashMap<i64, f64> = hits.iter().map(|h| (h.0, -h.1)).collect();
        let mut items = items_for(c, &seqs, q.include_cargo)?;
        for i in &mut items {
            if let (Some(seq), Some(o)) = (i["seq"].as_i64(), i.as_object_mut()) {
                o.insert("score".into(), json!(score.get(&seq).copied().unwrap_or(0.0)));
                add_match(c, seq, &expr, o)?;
            }
        }
        let last = items.last().and_then(|i| i["seq"].as_i64());
        let next = if order == "newest" && more { json!(last) } else { Value::Null };
        let next_offset = if order != "newest" && more { json!(offset + seqs.len() as i64) } else { Value::Null };
        Ok(json!({"q": q.q, "room": q.room, "items": items, "count": seqs.len(), "total": total, "offset": offset, "limit": limit, "order": order,
                  "in": if q.text_only { "text" } else { "all" }, "truncated": more, "next_before": next, "next_offset": next_offset, "engine": engine}))
    })
}
