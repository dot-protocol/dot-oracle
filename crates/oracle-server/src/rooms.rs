//! Rooms as the owner manages them: delete a room, or move its messages into another room.
//!
//! Both act on the log itself and on everything derived from it, in one transaction: the search rows, the tags, the
//! imported sources, the room index (messages, threads, items, votes, marks), the room's head, read marks and the state
//! heads that pointed at a removed row. Nothing is left behind for a later read to trip on.
//!
//! Three rules:
//! * **Preview first.** Every operation can run `dry`: it says exactly what it would touch and changes nothing.
//! * **A move never forges a signature.** A native message's signature covers its room, so it cannot be re-homed: a move
//!   that would carry one is refused as a whole. Imported history (`envelope=1`) carries no signature here and moves.
//! * **Every change is recorded.** `room_ops` keeps who did what, when, to how many rows; it is never deleted from.
//!
//! The semantic vectors of deleted rows are pruned by the indexer within ten minutes; moved rows keep theirs (a vector is
//! keyed by observation, not room). The server drops its cached board for both rooms after a change.

use rusqlite::{params, params_from_iter, Connection, Transaction};
use serde::Serialize;

use crate::store::{Result, Store, StoreError};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS room_ops (
  at_ms   INTEGER NOT NULL,
  actor   TEXT NOT NULL,
  op      TEXT NOT NULL,
  room    TEXT NOT NULL,
  target  TEXT,
  rows    INTEGER NOT NULL,
  detail  TEXT NOT NULL
);
";

/// What an operation did, or (with `dry`) would do.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RoomReport {
    pub op: &'static str,
    pub room: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub dry_run: bool,
    /// Observations deleted or moved.
    pub rows: usize,
    /// Messages in OTHER rooms that reply to, close, supersede, archive or annotate a message here.
    pub referenced_from_outside: usize,
    /// Subjects whose state head pointed at a deleted row: moved back to their previous revision, or removed.
    pub state_heads_changed: usize,
    /// Native signed messages: a move cannot carry them (their signature names this room).
    pub signed: usize,
    /// Done, or why not.
    pub outcome: String,
}

fn room_ok(room: &str) -> Result<()> {
    let ok = !room.is_empty()
        && room.len() <= 64
        && room.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(StoreError::Rejected(format!("{room:?} is not a room name (a-z 0-9 - _, at most 64)")))
    }
}

fn seqs_of(c: &Connection, room: &str) -> Result<Vec<(i64, String)>> {
    let mut st = c.prepare("SELECT seq, id FROM observations WHERE channel=?1 ORDER BY seq")?;
    let rows = st.query_map(params![room], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Messages outside `room` that point at a message in it: by a link tag (`re:` is derived from replies; the rest are
/// signed tags), or by hanging in a thread rooted here.
fn outside_refs(c: &Connection, room: &str, ids: &[(i64, String)]) -> Result<Vec<i64>> {
    let mut found: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    let mut tag = c.prepare_cached(
        "SELECT t.obs_seq FROM tags t JOIN observations o ON o.seq=t.obs_seq WHERE t.name=?1 AND o.channel<>?2",
    )?;
    for (_, id) in ids {
        for prefix in ["re:", "reply:", "in_reply_to:", "closes:", "supersedes:", "archives:", "annotates:"] {
            for s in tag.query_map(params![format!("{prefix}{id}"), room], |r| r.get::<_, i64>(0))? {
                found.insert(s?);
            }
        }
    }
    let mut thread = c.prepare_cached("SELECT seq FROM msg WHERE root_seq=?1 AND channel<>?2")?;
    for (seq, _) in ids {
        for s in thread.query_map(params![seq, room], |r| r.get::<_, i64>(0))? {
            found.insert(s?);
        }
    }
    Ok(found.into_iter().collect())
}

fn count_signed(c: &Connection, room: &str) -> Result<usize> {
    let n: i64 = c.query_row("SELECT count(*) FROM observations WHERE channel=?1 AND envelope=2", params![room], |r| r.get(0))?;
    Ok(n as usize)
}

/// The room's head row, from what is in the log now.
fn refresh_head(tx: &Transaction, room: &str) -> Result<()> {
    tx.execute("DELETE FROM channel_heads WHERE channel=?1", params![room])?;
    tx.execute(
        "INSERT INTO channel_heads(channel, max_seq, total) SELECT ?1, max(seq), count(*) FROM observations WHERE channel=?1 HAVING count(*) > 0",
        params![room],
    )?;
    Ok(())
}

/// What a state row says about its subject: the native cargo first, then an earlier Oracle's `live-state` tags.
fn head_of(kind: &str, author: &str, cargo: Option<&str>, tags: &[String]) -> Option<(String, String, String, Option<String>)> {
    if kind != "state" {
        return None;
    }
    if let Some(v) = cargo.and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok()) {
        if let (Some(subject), Some(state)) = (v["subject"].as_str(), v["state"].as_str()) {
            let owner = v["owner"].as_str().unwrap_or(author).to_string();
            return Some((subject.to_string(), state.to_string(), owner, v["next_touch"].as_str().map(str::to_string)));
        }
    }
    let tag = |p: &str| tags.iter().find_map(|t| t.strip_prefix(p)).filter(|v| !v.is_empty()).map(str::to_string);
    let subject = tag("subject:")?;
    let state = match tag("state:")?.as_str() {
        "ruled" => "done".to_string(),
        s => s.to_string(),
    };
    Some((subject, state, tag("owner:").unwrap_or_else(|| author.to_string()), tag("clock:")))
}

/// Subjects whose head is one of `deleted`: point each at its newest remaining state row, or remove it.
fn repair_heads(tx: &Transaction, deleted: &[i64]) -> Result<usize> {
    let mut subjects = Vec::new();
    {
        let mut st = tx.prepare_cached("SELECT subject FROM state_heads WHERE obs_seq=?1")?;
        for s in deleted {
            for r in st.query_map(params![s], |r| r.get::<_, String>(0))? {
                subjects.push(r?);
            }
        }
    }
    for subject in &subjects {
        tx.execute("DELETE FROM state_heads WHERE subject=?1", params![subject])?;
        // Candidates are the state rows that name the subject, newest first; the first that still parses is the head.
        let rows: Vec<(i64, String, String, Option<String>, String)> = {
            let mut st = tx.prepare_cached(
                "SELECT o.seq, o.kind, o.author, o.cargo, o.tags_json FROM observations o WHERE o.kind='state' AND \
                 (o.seq IN (SELECT obs_seq FROM tags WHERE name=?1) OR (CASE WHEN json_valid(o.cargo) THEN json_extract(o.cargo, '$.subject') END)=?2) ORDER BY o.seq DESC LIMIT 50",
            )?;
            let v = st
                .query_map(params![format!("subject:{subject}"), subject], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            v
        };
        for (seq, kind, author, cargo, tags_json) in rows {
            let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
            if let Some((s, state, owner, next)) = head_of(&kind, &author, cargo.as_deref(), &tags) {
                if &s == subject {
                    tx.execute(
                        "INSERT INTO state_heads(subject, obs_seq, owner, state, next_touch) VALUES(?1,?2,?3,?4,?5)",
                        params![subject, seq, owner, state, next],
                    )?;
                    break;
                }
            }
        }
    }
    Ok(subjects.len())
}

/// The end of an operation: a dry run has done every step and is rolled back (so a preview proves the change would go
/// through, not only what it would touch); a real one is recorded and committed.
fn finish(tx: Transaction, actor: &str, mut report: RoomReport, done: &str, now_ms: i64) -> Result<RoomReport> {
    if report.dry_run {
        tx.rollback()?;
        report.outcome = "dry run: nothing changed".into();
        return Ok(report);
    }
    report.outcome = done.into();
    record(&tx, actor, &report, now_ms)?;
    tx.commit()?;
    Ok(report)
}

fn record(tx: &Transaction, actor: &str, report: &RoomReport, now_ms: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO room_ops(at_ms, actor, op, room, target, rows, detail) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![now_ms, actor, report.op, report.room, report.to, report.rows as i64, serde_json::to_string(report).unwrap_or_default()],
    )?;
    Ok(())
}

impl Store {
    /// Delete a room: every observation in it and everything derived from them. Refused when another room's messages point
    /// into it, unless `force` (those messages stay, re-placed as their own threads). With `dry`, only reports.
    pub fn delete_room(&self, room: &str, force: bool, dry: bool, actor: &str, now_ms: i64) -> Result<RoomReport> {
        room_ok(room)?;
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        let ids = seqs_of(&tx, room)?;
        if ids.is_empty() {
            return Err(StoreError::NotFound);
        }
        let outside = outside_refs(&tx, room, &ids)?;
        let signed = count_signed(&tx, room)?;
        let deleted: Vec<i64> = ids.iter().map(|(s, _)| *s).collect();
        let heads: i64 = {
            let marks = deleted.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            tx.query_row(&format!("SELECT count(*) FROM state_heads WHERE obs_seq IN ({marks})"), params_from_iter(deleted.iter()), |r| r.get(0))?
        };
        let mut report = RoomReport {
            op: "delete",
            room: room.to_string(),
            to: None,
            dry_run: dry,
            rows: ids.len(),
            referenced_from_outside: outside.len(),
            state_heads_changed: heads as usize,
            signed,
            outcome: String::new(),
        };
        if !outside.is_empty() && !force {
            report.outcome = format!("refused: {} message(s) in other rooms point into #{room}; pass force to delete anyway", outside.len());
            return Ok(report);
        }
        for (seq, id) in &ids {
            tx.execute("DELETE FROM obs_fts WHERE rowid=?1", params![seq])?;
            tx.execute("DELETE FROM tags WHERE obs_seq=?1", params![seq])?;
            tx.execute("DELETE FROM legacy_sources WHERE obs_id=?1", params![id])?;
            for (table, col) in [("msg", "seq"), ("thread", "root_seq"), ("item", "msg_seq"), ("vote", "msg_seq"), ("mark", "msg_seq")] {
                tx.execute(&format!("DELETE FROM {table} WHERE {col}=?1"), params![seq])?;
            }
            tx.execute("DELETE FROM observations WHERE seq=?1", params![seq])?;
            crate::store::fts_note_projector_write(&tx)?;
        }
        report.state_heads_changed = repair_heads(&tx, &deleted)?;
        tx.execute("DELETE FROM cursors WHERE channel=?1", params![room])?;
        refresh_head(&tx, room)?;
        // What pointed in from outside now hangs from nothing: place it again, as its own thread.
        crate::roomindex::reproject(&tx, &outside)?;
        finish(tx, actor, report, "deleted", now_ms)
    }

    /// Move every message of `from` into `to` (which may already exist). Refused as a whole when `from` holds a native
    /// signed message. With `dry`, only reports.
    pub fn move_room(&self, from: &str, to: &str, dry: bool, actor: &str, now_ms: i64) -> Result<RoomReport> {
        room_ok(from)?;
        room_ok(to)?;
        if from == to {
            return Err(StoreError::Rejected("a room cannot be moved into itself".into()));
        }
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        let ids = seqs_of(&tx, from)?;
        if ids.is_empty() {
            return Err(StoreError::NotFound);
        }
        let signed = count_signed(&tx, from)?;
        let mut report = RoomReport {
            op: "move",
            room: from.to_string(),
            to: Some(to.to_string()),
            dry_run: dry,
            rows: ids.len(),
            referenced_from_outside: outside_refs(&tx, from, &ids)?.len(),
            state_heads_changed: 0,
            signed,
            outcome: String::new(),
        };
        if signed > 0 {
            report.outcome = format!(
                "refused: #{from} holds {signed} signed message(s); a signature names its room, so moving one would break it"
            );
            return Ok(report);
        }
        let seqs: Vec<i64> = ids.iter().map(|(s, _)| *s).collect();
        tx.execute("UPDATE observations SET channel=?2 WHERE channel=?1", params![from, to])?;
        tx.execute("DELETE FROM cursors WHERE channel=?1", params![from])?;
        refresh_head(&tx, from)?;
        refresh_head(&tx, to)?;
        crate::roomindex::reproject(&tx, &seqs)?;
        finish(tx, actor, report, "moved", now_ms)
    }

    /// The newest room operations, newest first.
    pub fn room_ops(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let c = self.conn()?;
        let mut st = c.prepare("SELECT at_ms, actor, op, room, target, rows, detail FROM room_ops ORDER BY rowid DESC LIMIT ?1")?;
        let rows = st
            .query_map(params![limit.clamp(1, 500)], |r| {
                let detail: String = r.get(6)?;
                Ok(serde_json::json!({"at_ms": r.get::<_, i64>(0)?, "actor": r.get::<_, String>(1)?, "op": r.get::<_, String>(2)?,
                    "room": r.get::<_, String>(3)?, "to": r.get::<_, Option<String>>(4)?, "rows": r.get::<_, i64>(5)?,
                    "report": serde_json::from_str::<serde_json::Value>(&detail).unwrap_or(serde_json::Value::Null)}))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

