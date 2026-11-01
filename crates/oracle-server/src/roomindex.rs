//! The room index (docs/ROOM-INDEX.md): projections of the log, written in the transaction of the observation that
//! changes them and rebuildable from it (`oracle reindex`). The log stays the only truth; nothing here is ever read
//! back into it.
//!
//! The rules are not restated. Each affected thread is judged by [`projection::judge_in`], the fold every reader
//! already trusts, over the thread's messages and whatever points into them from outside (a `supersedes:`, an
//! `archives:`); its verdicts become rows. So the index cannot disagree with the read-time fold about a rule, only
//! about what it was given, and `tests/room_index.rs` holds it to that.
//!
//! Two differences from the fold, both on purpose:
//!  - time: rows are judged at `now = 0`, so nothing here has expired or is overdue; `msg.by_ms` and `msg.expires_ms`
//!    carry the clocks and a reader compares them to its own now;
//!  - reach: the fold reads a thread up to 4 links deep, at most 300 messages; the index has no such limit.

use crate::projection::{self, Item, State, Verdict};
use crate::store::{cargo_reply_to, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS msg (
  seq        INTEGER PRIMARY KEY,
  channel    TEXT NOT NULL,
  author     TEXT NOT NULL,
  class      TEXT NOT NULL,
  root_seq   INTEGER NOT NULL,
  parent_seq INTEGER,
  depth      INTEGER NOT NULL,
  intent     TEXT, ask TEXT, by_ms INTEGER, expires_ms INTEGER,
  created_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS msg_room      ON msg(channel, seq DESC);
CREATE INDEX IF NOT EXISTS msg_room_cls  ON msg(channel, class, seq DESC);
CREATE INDEX IF NOT EXISTS msg_thread    ON msg(root_seq, seq);
CREATE INDEX IF NOT EXISTS msg_author    ON msg(channel, author, seq DESC);
CREATE TABLE IF NOT EXISTS thread (
  root_seq     INTEGER PRIMARY KEY,
  channel      TEXT NOT NULL,
  state        TEXT NOT NULL,
  replies      INTEGER NOT NULL,
  last_seq     INTEGER NOT NULL, last_ms INTEGER NOT NULL,
  participants TEXT NOT NULL,
  answered_by  INTEGER, settled_by INTEGER, settled_word TEXT
);
CREATE INDEX IF NOT EXISTS thread_room  ON thread(channel, last_seq DESC);
CREATE INDEX IF NOT EXISTS thread_state ON thread(channel, state, last_seq DESC);
CREATE TABLE IF NOT EXISTS item (
  msg_seq INTEGER NOT NULL, k TEXT NOT NULL,
  channel TEXT NOT NULL, author TEXT NOT NULL,
  kind TEXT NOT NULL, text TEXT, state TEXT NOT NULL,
  owner TEXT, moved_by INTEGER, moved_ms INTEGER, by_model INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (msg_seq, k)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS item_author ON item(author, state, msg_seq DESC);
CREATE INDEX IF NOT EXISTS item_owner  ON item(owner, state, msg_seq DESC);
CREATE TABLE IF NOT EXISTS vote (msg_seq INTEGER, k TEXT, seat TEXT, stance TEXT NOT NULL, reply_seq INTEGER NOT NULL,
                                 PRIMARY KEY (msg_seq, k, seat)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS mark (msg_seq INTEGER, seat TEXT, react TEXT, reply_seq INTEGER NOT NULL,
                                 PRIMARY KEY (msg_seq, seat, react)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS index_meta (k TEXT PRIMARY KEY, v TEXT NOT NULL) WITHOUT ROWID;
"#;

/// The tables a projection owns, in the order a dump lists them.
pub const TABLES: [&str; 5] = ["msg", "thread", "item", "vote", "mark"];

/// Called when a database opens. A ledger with nothing in it starts ready: every write will project. A ledger that already
/// has messages is not ready until `reindex` has run once, so a half-built index is never written to as if it were whole.
pub fn init(c: &Connection) -> Result<()> {
    c.execute_batch(SCHEMA)?;
    let have: Option<String> = c.query_row("SELECT v FROM index_meta WHERE k='ready'", [], |r| r.get(0)).optional()?;
    if have.is_none() {
        let any: i64 = c.query_row("SELECT count(*) FROM (SELECT 1 FROM observations LIMIT 1)", [], |r| r.get(0))?;
        c.execute("INSERT INTO index_meta(k,v) VALUES('ready',?1)", params![if any == 0 { "1" } else { "0" }])?;
    }
    Ok(())
}

pub fn ready(c: &Connection) -> Result<bool> {
    Ok(c.query_row("SELECT v FROM index_meta WHERE k='ready'", [], |r| r.get::<_, String>(0)).optional()?.as_deref() == Some("1"))
}

/// What one observation is, for the index.
struct Row {
    seq: i64,
    id: String,
    channel: String,
    author: String,
    kind: String,
    content: String,
    cargo: Option<String>,
    tags: Vec<String>,
    created_ms: i64,
    /// False for imported history: in the room, never judged.
    news: bool,
}

fn load_any(c: &Connection, seq: i64) -> Result<Option<Row>> {
    Ok(c.query_row(
        "SELECT seq,id,channel,author,kind,content,cargo,tags_json,created_ms,news,envelope FROM observations WHERE seq=?1",
        params![seq],
        |r| {
            let tags_json: String = r.get(7)?;
            let mut tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
            let cargo: Option<String> = r.get(6)?;
            crate::store::project_imported_reply(&mut tags, cargo.as_deref(), r.get(10)?);
            Ok(Row {
                seq: r.get(0)?, id: r.get(1)?, channel: r.get(2)?, author: r.get(3)?, kind: r.get(4)?, content: r.get(5)?, cargo,
                tags, created_ms: r.get(8)?, news: r.get::<_, i64>(9)? != 0,
            })
        },
    ).optional()?)
}

/// A row the fold judges: history is in the room and out of the judgement.
fn load(c: &Connection, seq: i64) -> Result<Option<Row>> {
    Ok(load_any(c, seq)?.filter(|r| r.news))
}

const LINKS: [&str; 7] = ["closes:", "supersedes:", "archives:", "re:", "reply:", "in_reply_to:", "annotates:"];

fn tag_targets<'a>(tags: &'a [String], prefixes: &'a [&'a str]) -> impl Iterator<Item = &'a str> + 'a {
    tags.iter().filter_map(move |t| prefixes.iter().find_map(|p| t.strip_prefix(*p)))
}

/// What a message is to a reader of the room. Imported history is its own class: threaded, listed, searched, never judged.
fn class_of(r: &Row) -> &'static str {
    if !r.news {
        return "history";
    }
    class_parts(&r.kind, &r.tags, &r.content)
}

/// [`class_of`] for a message that is being written (the write path needs it for the search row before the row exists).
pub fn class_parts(kind: &str, tags: &[String], content: &str) -> &'static str {
    let is_card = tags.iter().any(|t| t == "schema:room-task/1" || t == "schema:room-task-event/1")
        || content.split_once(" task:").is_some_and(|(a, _)| a.starts_with('[') && !a.contains(']'));
    if tags.iter().any(|t| t.starts_with("annotates:")) || kind == "annotate" {
        return if projection::mark_of(tags).is_some() { "mark" } else { "annotation" };
    }
    if projection::reaction_of(tags).is_some() {
        return "mark";
    }
    if kind == "state" {
        return "state";
    }
    if is_card {
        return "card";
    }
    if projection::intent_of(tags) == projection::Intent::Status {
        return "status";
    }
    "message"
}

/// The two words a search row carries so a room, or a class, can be asked for by name: one alphanumeric token each (the room's
/// bytes in hex), so the tokenizer keeps them whole. Query `tags:zzch<hex>` and `tags:zzcls<class>`.
pub fn fts_tokens(channel: &str, class: &str) -> String {
    let hex: String = channel.bytes().map(|b| format!("{b:02x}")).collect();
    format!("zzch{hex} zzcls{class}")
}

/// The search row's tags with the room and class tokens made current: what `reindex` does for every row, and what a write does for its own.
fn refresh_fts(c: &Connection, seq: i64, channel: &str, class: &str) -> Result<()> {
    let current: Option<String> = c.query_row("SELECT tags FROM obs_fts WHERE rowid=?1", params![seq], |r| r.get(0)).optional()?;
    let Some(current) = current else { return Ok(()) };
    let mut words: Vec<&str> = current.split_whitespace().filter(|w| !w.starts_with("zzch") && !w.starts_with("zzcls")).collect();
    let tokens = fts_tokens(channel, class);
    words.extend(tokens.split(' '));
    c.execute("UPDATE obs_fts SET tags=?2 WHERE rowid=?1", params![seq, words.join(" ")])?;
    Ok(())
}

/// What a message replies to: an annotation or a mark names its message; a reply names its parent (the same order
/// `store::parent_of` reads: the tags, then a `replyTo` in the cargo).
fn parent_id(r: &Row) -> Option<String> {
    if let Some(t) = r.tags.iter().find_map(|t| t.strip_prefix("annotates:")) {
        return Some(t.to_owned());
    }
    r.tags
        .iter()
        .find_map(|t| ["re:", "reply:", "in_reply_to:", "closes:"].iter().find_map(|p| t.strip_prefix(p).map(str::to_owned)))
        .or_else(|| cargo_reply_to(r.cargo.as_deref()))
}

fn seq_of(c: &Connection, id: &str) -> Result<Option<i64>> {
    Ok(c.query_row("SELECT m.seq FROM observations o JOIN msg m ON m.seq=o.seq WHERE o.id=?1", params![id], |r| r.get(0)).optional()?)
}

fn is_members_record(r: &Row) -> bool {
    let mut name = r.channel.clone();
    if let Some(ch) = name.get_mut(0..1) {
        ch.make_ascii_uppercase();
    }
    r.content.starts_with(&format!("[{name} members]"))
}

/// Insert the structural row for one observation (its thread, its parent, its depth) and say which threads it touches.
/// Nothing is judged here. Imported history is placed (threaded by its reply tags, class `history`) and touches no thread's judgement.
/// `Ok(None)` for a seq that is not in the log.
fn place(c: &Connection, seq: i64) -> Result<Option<Vec<i64>>> {
    let Some(r) = load_any(c, seq)? else { return Ok(None) };
    let parent = match parent_id(&r) {
        Some(id) => seq_of(c, &id)?.filter(|p| *p < seq),
        None => None,
    };
    let (root, depth) = match parent {
        Some(p) => c.query_row("SELECT root_seq, depth FROM msg WHERE seq=?1", params![p], |x| Ok((x.get::<_, i64>(0)?, x.get::<_, i64>(1)? + 1)))?,
        None => (seq, 0),
    };
    c.execute(
        "INSERT OR REPLACE INTO msg(seq,channel,author,class,root_seq,parent_seq,depth,created_ms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![seq, r.channel, r.author, class_of(&r), root, parent, depth, r.created_ms],
    )?;
    if !r.news {
        return Ok(Some(Vec::new()));
    }
    let mut dirty: BTreeSet<i64> = BTreeSet::from([root]);
    for target in tag_targets(&r.tags, &LINKS).map(str::to_owned).collect::<Vec<_>>() {
        if let Some(t) = seq_of(c, &target)? {
            dirty.insert(c.query_row("SELECT root_seq FROM msg WHERE seq=?1", params![t], |x| x.get(0))?);
        }
    }
    // A new list of deciders changes what every ✓ in the room settles. It is read once here (a scan of the channel) and kept,
    // so judging a thread is a key lookup: scanning the channel for every thread took 19 minutes to reindex 27k messages.
    if is_members_record(&r) {
        let roots = deciders(c, &r.channel)?;
        c.execute(
            "INSERT OR REPLACE INTO index_meta(k,v) VALUES(?1,?2)",
            params![format!("roots:{}", r.channel), serde_json::to_string(&roots).unwrap_or_default()],
        )?;
        let mut st = c.prepare("SELECT DISTINCT root_seq FROM msg WHERE channel=?1")?;
        for root in st.query_map(params![r.channel], |x| x.get::<_, i64>(0))? {
            dirty.insert(root?);
        }
    }
    // A new seat-alias table changes who a name in a `to:` list is: the threads that name a label it adds, moves or drops are judged again.
    if r.channel == crate::alias::ROOM && r.content.starts_with(crate::alias::FACE) && is_owner(c, &r.author)? {
        if let Some(new) = r.cargo.as_deref().and_then(|g| crate::alias::parse(g).ok()) {
            let old = aliases(c)?;
            c.execute("INSERT OR REPLACE INTO index_meta(k,v) VALUES('aliases',?1)", params![serde_json::to_string(&new).unwrap_or_default()])?;
            let labels: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
            let mut st = c.prepare("SELECT DISTINCT m.root_seq FROM tags t JOIN msg m ON m.seq=t.obs_seq WHERE t.name=?1 OR t.name=?2 OR t.name=?3")?;
            for l in labels {
                for root in st.query_map(params![format!("to:{l}"), format!("mention:{l}"), format!("set:to={l}")], |x| x.get::<_, i64>(0))? {
                    dirty.insert(root?);
                }
            }
        }
    }
    Ok(Some(dirty.into_iter().collect()))
}

fn is_owner(c: &Connection, handle: &str) -> Result<bool> {
    let n: i64 = c.query_row("SELECT count(*) FROM keys WHERE handle=?1 AND role='owner' AND revoked_ms IS NULL", params![handle], |x| x.get(0))?;
    Ok(n > 0)
}

/// The seat aliases in force (`alias.rs`): the table of the newest valid owner-signed record, kept in `index_meta` when it is placed.
pub fn aliases(c: &Connection) -> Result<crate::alias::Aliases> {
    let v: Option<String> = c.query_row("SELECT v FROM index_meta WHERE k='aliases'", [], |x| x.get(0)).optional()?;
    Ok(v.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
}

/// A room's deciders on this connection: `Store::room_roots`, which needs its own.
fn deciders(c: &Connection, channel: &str) -> Result<Vec<String>> {
    let mut name = channel.to_owned();
    if let Some(ch) = name.get_mut(0..1) {
        ch.make_ascii_uppercase();
    }
    let face = format!("[{name} members]");
    let mut st = c.prepare_cached(
        "SELECT author, cargo FROM observations WHERE channel=?1 AND news=1 AND substr(content,1,?3)=?2 ORDER BY seq DESC LIMIT 20",
    )?;
    let rows = st
        .query_map(params![channel, face, face.chars().count() as i64], |x| Ok((x.get::<_, String>(0)?, x.get::<_, Option<String>>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (author, cargo) in rows {
        let owner: i64 = c.query_row("SELECT count(*) FROM keys WHERE handle=?1 AND role='owner' AND revoked_ms IS NULL", params![author], |x| x.get(0))?;
        let Some(cargo) = cargo.filter(|_| owner > 0) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&cargo) else { continue };
        if v["schema"] == "room-members/1" {
            return Ok(v["roots"].as_array().map(|a| a.iter().filter_map(|r| r.as_str().map(str::to_owned)).collect()).unwrap_or_default());
        }
    }
    Ok(Vec::new())
}

/// The thread's word, from its root's verdict. `expired` is never written (see the module note).
fn thread_state(v: &Verdict) -> &'static str {
    if v.settled.is_some() {
        return "settled";
    }
    match v.state {
        State::Closed => "closed",
        State::Superseded => "superseded",
        State::Archived => "archived",
        State::Expired => "expired",
        // What was said became of it (`set:done=`: settled, stale, superseded, acked) is the thread's state, unless a real link or a decider's mark already decided it.
        // `answered` is never taken from a verdict: it stays derived from real replies.
        _ if matches!(v.done, Some(d) if d != "none") => v.done.unwrap_or("open"),
        State::Ambient => "ambient",
        State::Open if v.answered_by.is_some() => "answered",
        State::Open => "open",
    }
}

/// Judge one thread again and rewrite everything derived from it.
fn rejudge(c: &Connection, root: i64) -> Result<()> {
    let members: Vec<i64> = {
        let mut st = c.prepare_cached("SELECT seq FROM msg WHERE root_seq=?1 ORDER BY seq")?;
        let v = st.query_map(params![root], |x| x.get(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        v
    };
    if members.is_empty() {
        return Ok(());
    }
    let mut rows: BTreeMap<i64, Row> = BTreeMap::new();
    for s in &members {
        if let Some(r) = load(c, *s)? {
            rows.insert(*s, r);
        }
    }
    if rows.is_empty() {
        return Ok(()); // nothing here for the fold to judge
    }
    let member_set: HashSet<i64> = rows.keys().copied().collect();
    let channel = rows.get(&root).or_else(|| rows.values().next()).map(|r| r.channel.clone()).unwrap_or_default();
    // What points into the thread from outside it: a later message closing, superseding or archiving one of its own, or an
    // annotation from another thread. They are judged with the thread and belong to it for no other purpose.
    let names: Vec<String> = rows.values().flat_map(|r| LINKS.iter().map(move |p| format!("{p}{}", r.id))).collect();
    let mut outside: BTreeSet<i64> = BTreeSet::new();
    for chunk in names.chunks(400) {
        let holes = vec!["?"; chunk.len()].join(",");
        let sql = format!("SELECT DISTINCT obs_seq FROM tags WHERE name IN ({holes})");
        let mut st = c.prepare(&sql)?;
        let p: Vec<&dyn rusqlite::ToSql> = chunk.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
        for s in st.query_map(p.as_slice(), |x| x.get::<_, i64>(0))? {
            let s = s?;
            if !member_set.contains(&s) {
                outside.insert(s);
            }
        }
    }
    let mut ext: BTreeMap<i64, Row> = BTreeMap::new();
    for s in outside {
        if let Some(r) = load(c, s)? {
            ext.insert(s, r);
        }
    }
    let all: Vec<&Row> = rows.values().chain(ext.values()).collect();
    let items: Vec<Item> = all.iter().map(|r| Item { id: &r.id, author: &r.author, created_ms: r.created_ms, tags: &r.tags }).collect();
    let seq_by_id: HashMap<&str, i64> = all.iter().map(|r| (r.id.as_str(), r.seq)).collect();
    let by_seq: HashMap<i64, &Row> = all.iter().map(|r| (r.seq, *r)).collect();
    let roots: Vec<String> = c
        .query_row("SELECT v FROM index_meta WHERE k=?1", params![format!("roots:{channel}")], |x| x.get::<_, String>(0))
        .optional()?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default();
    let verdicts = projection::judge_in_with(&items, 0, &roots, &aliases(c)?);

    c.execute("DELETE FROM item WHERE msg_seq IN (SELECT seq FROM msg WHERE root_seq=?1)", params![root])?;
    c.execute("DELETE FROM vote WHERE msg_seq IN (SELECT seq FROM msg WHERE root_seq=?1)", params![root])?;
    c.execute("DELETE FROM mark WHERE msg_seq IN (SELECT seq FROM msg WHERE root_seq=?1)", params![root])?;

    // Who withdrew what, for the marks and votes still standing.
    let archived: HashSet<&str> = all.iter().flat_map(|r| tag_targets(&r.tags, &["archives:"])).collect();
    // Marks, votes and owners: each is a tag on a later message that points at a member.
    let mut marks: BTreeMap<(i64, String, &'static str), i64> = BTreeMap::new();
    let mut votes: BTreeMap<(i64, String, String), (i64, String, i64)> = BTreeMap::new();
    let mut owners: BTreeMap<(i64, String), (i64, String)> = BTreeMap::new();
    for r in &all {
        let withdrawn = archived.contains(r.id.as_str());
        for target in tag_targets(&r.tags, &["annotates:", "re:", "reply:", "in_reply_to:"]) {
            let Some(t) = seq_by_id.get(target).copied().filter(|t| member_set.contains(t) && *t < r.seq) else { continue };
            if let (Some(m), false) = (projection::mark_of(&r.tags), withdrawn) {
                marks.entry((t, r.author.clone(), m)).or_insert(r.seq);
            }
            for tag in &r.tags {
                if let Some((k, seat)) = tag.strip_prefix("owner:").and_then(|x| x.split_once('=')) {
                    let e = owners.entry((t, k.to_owned())).or_insert((r.created_ms, seat.to_owned()));
                    if r.created_ms >= e.0 {
                        *e = (r.created_ms, seat.to_owned());
                    }
                }
                if let Some((k, stance)) = tag.strip_prefix("vote:").and_then(|x| x.split_once('=')) {
                    if !withdrawn {
                        let e = votes.entry((t, k.to_owned(), r.author.clone())).or_insert((r.created_ms, stance.to_owned(), r.seq));
                        if (r.created_ms, r.seq) >= (e.0, e.2) {
                            *e = (r.created_ms, stance.to_owned(), r.seq);
                        }
                    }
                }
            }
        }
    }
    // The words of each declared item live in a cargo `{"items":[{"k","kind","text"}]}`: the message's own, else the newest annotation's that carries them.
    let mut own_text: BTreeMap<(i64, String), String> = BTreeMap::new();
    let mut note_text: BTreeMap<(i64, String), (i64, String)> = BTreeMap::new();
    for r in &all {
        let Some(list) = r.cargo.as_deref().and_then(|c| serde_json::from_str::<Value>(c).ok()).and_then(|v| v["items"].as_array().cloned()) else { continue };
        let note_of = tag_targets(&r.tags, &["annotates:"]).next().and_then(|t| seq_by_id.get(t).copied());
        for it in &list {
            let (Some(k), Some(text)) = (it["k"].as_str(), it["text"].as_str()) else { continue };
            match note_of {
                Some(t) if member_set.contains(&t) => {
                    let e = note_text.entry((t, k.to_owned())).or_insert((r.seq, text.to_owned()));
                    if r.seq >= e.0 {
                        *e = (r.seq, text.to_owned());
                    }
                }
                Some(_) => {}
                None if member_set.contains(&r.seq) => {
                    own_text.insert((r.seq, k.to_owned()), text.to_owned());
                }
                None => {}
            }
        }
    }
    for ((t, seat, react), reply) in &marks {
        c.execute("INSERT OR IGNORE INTO mark(msg_seq,seat,react,reply_seq) VALUES(?1,?2,?3,?4)", params![t, seat, react, reply])?;
    }
    for ((t, k, seat), (_, stance, reply)) in &votes {
        c.execute("INSERT OR REPLACE INTO vote(msg_seq,k,seat,stance,reply_seq) VALUES(?1,?2,?3,?4,?5)", params![t, k, seat, stance, reply])?;
    }

    // The messages themselves.
    for (seq, r) in &rows {
        let Some(v) = verdicts.get(&r.id) else { continue };
        let expires = match v.state {
            State::Open => projection::ttl_ms(&r.tags, v.intent).map(|ttl| r.created_ms.saturating_add(ttl)),
            _ => None,
        };
        c.execute(
            "UPDATE msg SET intent=?2, ask=?3, by_ms=?4, expires_ms=?5 WHERE seq=?1",
            params![seq, v.intent.as_str(), v.ask.as_str(), v.by_ms, expires],
        )?;
        for it in &v.items {
            let mover = it.by.as_deref().and_then(|id| seq_by_id.get(id)).copied();
            let mover_row = mover.and_then(|m| by_seq.get(&m));
            c.execute(
                "INSERT OR REPLACE INTO item(msg_seq,k,channel,author,kind,text,state,owner,moved_by,moved_ms,by_model) VALUES(?1,?2,?3,?4,?5,?11,?6,?7,?8,?9,?10)",
                params![
                    seq, it.k, r.channel, r.author, it.kind, it.state, owners.get(&(*seq, it.k.clone())).map(|o| o.1.clone()), mover,
                    mover_row.map(|m| m.created_ms), mover_row.is_some_and(|m| m.tags.iter().any(|t| t == "set:by=model")) as i64,
                    own_text.get(&(*seq, it.k.clone())).cloned().or_else(|| note_text.get(&(*seq, it.k.clone())).map(|t| t.1.clone()))
                ],
            )?;
        }
    }

    // The thread.
    // A thread that opens with imported history has no verdict of its own for its root: it is listed as `history`, its live replies judged as any.
    // Its state is then that of the newest live message that asks for something, so an open ask is not hidden under an old root.
    let rv = rows.get(&root).and_then(|r| verdicts.get(&r.id)).or_else(|| {
        rows.values().rev().filter(|r| class_of(r) == "message").filter_map(|r| verdicts.get(&r.id)).find(|v| v.ask.as_str() != "none")
    });
    let speech: Vec<&Row> = rows.values().filter(|r| matches!(class_of(r), "message" | "status" | "card")).collect();
    let mut participants: Vec<&str> = Vec::new();
    for r in &speech {
        if !participants.contains(&r.author.as_str()) {
            participants.push(&r.author);
        }
    }
    let replies = speech.iter().filter(|r| r.seq != root).count() as i64;
    let last = rows.values().next_back().map(|r| (r.seq, r.created_ms)).unwrap_or((root, 0));
    c.execute(
        "INSERT OR REPLACE INTO thread(root_seq,channel,state,replies,last_seq,last_ms,participants,answered_by,settled_by,settled_word) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            root, channel, rv.map_or("history", thread_state), replies, last.0, last.1, serde_json::to_string(&participants).unwrap_or_default(),
            rv.and_then(|v| v.answered_by.as_deref()).and_then(|id| seq_by_id.get(id)).copied(),
            rv.and_then(|v| v.settled_by.as_deref()).and_then(|id| seq_by_id.get(id)).copied(), rv.and_then(|v| v.settled)
        ],
    )?;
    Ok(())
}

/// Project one observation that has just been stored. Called inside the transaction that stored it.
pub fn project_new(c: &Connection, seq: i64) -> Result<()> {
    if !ready(c)? {
        return Ok(());
    }
    if let Some(dirty) = place(c, seq)? {
        for root in dirty {
            rejudge(c, root)?;
        }
    }
    Ok(())
}

/// Place these messages again (their room changed, or the message they hang under is gone) and judge every thread they were
/// in or are in now. The same steps `reindex` takes, for a few rows instead of the whole log; a room move or delete runs it
/// inside its own transaction.
pub(crate) fn reproject(c: &Connection, seqs: &[i64]) -> Result<()> {
    if !ready(c)? || seqs.is_empty() {
        return Ok(());
    }
    let mut roots: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    for s in seqs {
        let old: Option<i64> = c.query_row("SELECT root_seq FROM msg WHERE seq=?1", params![s], |r| r.get(0)).optional()?;
        roots.extend(old);
        for (table, col) in [("msg", "seq"), ("thread", "root_seq"), ("item", "msg_seq"), ("vote", "msg_seq"), ("mark", "msg_seq")] {
            c.execute(&format!("DELETE FROM {table} WHERE {col}=?1"), params![s])?;
        }
    }
    let mut ordered = seqs.to_vec();
    ordered.sort_unstable();
    for s in &ordered {
        if let Some(dirty) = place(c, *s)? {
            roots.extend(dirty);
        }
    }
    for root in roots {
        // A root with nothing left under it has no thread; `rejudge` leaves an empty one alone, so clear it here.
        let left: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM msg WHERE root_seq=?1)", params![root], |r| r.get(0))?;
        if left {
            rejudge(c, root)?;
        } else {
            c.execute("DELETE FROM thread WHERE root_seq=?1", params![root])?;
        }
    }
    for s in &ordered {
        let at: Option<(String, String)> = c.query_row("SELECT channel, class FROM msg WHERE seq=?1", params![s], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if let Some((channel, class)) = at {
            refresh_fts(c, *s, &channel, &class)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReindexStats {
    pub messages: usize,
    pub threads: usize,
}

/// Drop every projection and build it again from the log: structure in one pass, then each thread judged once. The result
/// is what the write path leaves behind, and the tests prove it is. One transaction: stop the server or run it on a copy.
pub fn reindex(c: &mut Connection) -> Result<ReindexStats> {
    let tx = c.transaction()?;
    for t in TABLES {
        tx.execute(&format!("DELETE FROM {t}"), [])?;
    }
    tx.execute("DELETE FROM index_meta WHERE k LIKE 'roots:%' OR k='aliases'", [])?;
    tx.execute("INSERT OR REPLACE INTO index_meta(k,v) VALUES('ready','0')", [])?;
    let seqs: Vec<i64> = {
        let mut st = tx.prepare("SELECT seq FROM observations ORDER BY seq")?;
        let v = st.query_map([], |x| x.get(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        v
    };
    for s in &seqs {
        place(&tx, *s)?;
    }
    // Only a thread with something in it the fold judges is judged; a thread of history alone is only listed.
    let roots: Vec<i64> = {
        let mut st = tx.prepare("SELECT DISTINCT root_seq FROM msg WHERE class<>'history' ORDER BY root_seq")?;
        let v = st.query_map([], |x| x.get(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        v
    };
    for r in &roots {
        rejudge(&tx, *r)?;
    }
    // The search row carries the room and the class, so a room can be searched by itself.
    let rows: Vec<(i64, String, String)> = {
        let mut st = tx.prepare("SELECT seq, channel, class FROM msg ORDER BY seq")?;
        let v = st.query_map([], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
        v
    };
    for (seq, channel, class) in &rows {
        refresh_fts(&tx, *seq, channel, class)?;
    }
    tx.execute("INSERT OR REPLACE INTO index_meta(k,v) VALUES('ready','1')", [])?;
    tx.commit()?;
    Ok(ReindexStats { messages: seqs.len(), threads: roots.len() })
}

/// Every projection row as text, table by table in a fixed order: what the two builds are compared by.
pub fn dump(c: &Connection) -> Result<Vec<(String, Vec<String>)>> {
    let mut out = Vec::new();
    for t in TABLES {
        let cols: Vec<String> = c.prepare(&format!("PRAGMA table_info({t})"))?.query_map([], |x| x.get::<_, String>(1))?.collect::<std::result::Result<_, _>>()?;
        let sql = format!(
            "SELECT {} FROM {t} ORDER BY {}",
            cols.iter().map(|k| format!("ifnull({k},'∅')")).collect::<Vec<_>>().join("||'|'||"),
            cols.join(",")
        );
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map([], |x| x.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
        out.push((t.to_owned(), rows));
    }
    Ok(out)
}
