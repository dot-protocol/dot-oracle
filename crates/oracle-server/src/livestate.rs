//! Live state keeps its head, not every revision (Rocky and Blaze, 2026-09-30: `#feeds` refilled at about 3,100 rows a day, each a full copy of a status that
//! only the newest of which is true).
//!
//! A state write is *live* when it is tagged `live-state`, or its owner is a machine ([`MACHINE_OWNERS`]). When a live write becomes the head of its subject
//! and the head it replaced was live and named the SAME state word, the replaced observation is deleted, with everything derived from it (search row, tags,
//! room-index rows, source, the room's count). A change of state word (`running` to `failed`) is history and stays, so a subject keeps one row per stretch of
//! one word, the newest of the stretch. A revision that something points at (a reply, an annotation, a mark: a `re:<id>` tag or a thread of its own) is kept.
//!
//! [`compact`] applies the same rule to the rows already there, per room, for the one-time cleanup: it keeps a row only if it is the last of its subject or the
//! next one names a different word, which is exactly what the incremental rule would have left.

use crate::store::Result;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::BTreeMap;

/// Owners whose state is a reading of a machine, live without the tag.
pub const MACHINE_OWNERS: [&str; 2] = ["estate", "hustler-book"];

fn machine(owner: &str) -> bool {
    MACHINE_OWNERS.contains(&owner)
}

/// The head a subject has now: (seq, state word, owner). Read it before the head moves.
pub fn prior_head(tx: &Transaction, subject: &str) -> Result<Option<(i64, String, String)>> {
    Ok(tx
        .query_row("SELECT obs_seq, state, owner FROM state_heads WHERE subject=?1", [subject], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?)
}

/// After `new_seq` was offered as the head of `subject`: if it is the head now, and both it and the head it replaced are live and name the same word, delete
/// the replaced one. True when a row was deleted.
pub fn retire_prior(tx: &Transaction, prior: Option<(i64, String, String)>, subject: &str, new_seq: i64, new_state: &str, new_live: bool) -> Result<bool> {
    let Some((old, old_state, old_owner)) = prior else { return Ok(false) };
    if old == new_seq || !new_live || old_state != new_state {
        return Ok(false);
    }
    let now_head: Option<i64> = tx.query_row("SELECT obs_seq FROM state_heads WHERE subject=?1", [subject], |r| r.get(0)).optional()?;
    if now_head != Some(new_seq) {
        return Ok(false);
    }
    let tagged: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM tags WHERE name='live-state' AND obs_seq=?1)", [old], |r| r.get(0))?;
    if !(tagged || machine(&old_owner)) {
        return Ok(false);
    }
    let same_room: bool = tx.query_row("SELECT (SELECT channel FROM observations WHERE seq=?1) = (SELECT channel FROM observations WHERE seq=?2)", [old, new_seq], |r| r.get(0))?;
    if !same_room {
        return Ok(false);
    }
    delete_observation(tx, old)
}

/// Remove one observation and everything derived from it, unless something points at it. True when it was removed.
pub fn delete_observation(tx: &Transaction, seq: i64) -> Result<bool> {
    let Some((id, channel)) = tx
        .query_row("SELECT id, channel FROM observations WHERE seq=?1", [seq], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .optional()?
    else {
        return Ok(false);
    };
    let pointed_at: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM tags WHERE name=?1) OR EXISTS(SELECT 1 FROM msg WHERE root_seq=?2 AND seq<>?2)",
        params![format!("re:{id}"), seq],
        |r| r.get(0),
    )?;
    if pointed_at {
        return Ok(false);
    }
    tx.execute("DELETE FROM obs_fts WHERE rowid=?1", [seq])?;
    tx.execute("DELETE FROM tags WHERE obs_seq=?1", [seq])?;
    tx.execute("DELETE FROM legacy_sources WHERE obs_id=?1", [&id])?;
    for (table, col) in [("msg", "seq"), ("thread", "root_seq"), ("item", "msg_seq"), ("vote", "msg_seq"), ("mark", "msg_seq")] {
        tx.execute(&format!("DELETE FROM {table} WHERE {col}=?1"), [seq])?;
    }
    tx.execute("DELETE FROM observations WHERE seq=?1", [seq])?;
    // Its search row went above, so this one source change leaves search valid: say so, or every restart rebuilds the
    // whole search index (2026-10-10: 45 uncertified changes in 15 minutes, a 40 s start on every release).
    crate::store::fts_note_projector_write(tx)?;
    tx.execute(
        "UPDATE channel_heads SET total = total - 1, max_seq = CASE WHEN max_seq = ?2 THEN COALESCE((SELECT MAX(seq) FROM observations WHERE channel = ?1), 0) ELSE max_seq END WHERE channel = ?1",
        params![channel, seq],
    )?;
    Ok(true)
}

/// What compacting one room did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    pub room: String,
    pub states: usize,
    pub deleted: usize,
}

struct Rev {
    seq: i64,
    state: String,
    live: bool,
}

/// (subject, state word, owner, tagged live) of a stored state observation, from its tags first (an earlier Oracle's form), else its cargo.
fn read_state(author: &str, tags: &[String], cargo: Option<&str>) -> Option<(String, String, String, bool)> {
    let tag = |p: &str| tags.iter().find_map(|t| t.strip_prefix(p)).filter(|v| !v.is_empty()).map(str::to_owned);
    let cargo: Option<serde_json::Value> = cargo.and_then(|c| serde_json::from_str(c).ok());
    let field = |k: &str| cargo.as_ref().and_then(|v| v.get(k)).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_owned);
    let subject = tag("subject:").or_else(|| field("subject"))?;
    let state = tag("state:").or_else(|| field("state"))?;
    let state = if state == "ruled" { "done".to_string() } else { state };
    let owner = tag("owner:").or_else(|| field("owner")).unwrap_or_else(|| author.to_owned());
    Some((subject, state, owner, tags.iter().any(|t| t == "live-state")))
}

/// Apply the live-state rule to the rows a room already has. With `dry`, count and change nothing.
pub fn compact(c: &mut Connection, rooms: &[String], dry: bool) -> Result<Vec<Compacted>> {
    let mut out = Vec::new();
    for room in rooms {
        let tx = c.transaction()?;
        let mut subjects: BTreeMap<String, Vec<Rev>> = BTreeMap::new();
        let mut states = 0;
        {
            let mut st = tx.prepare("SELECT seq, author, tags_json, cargo FROM observations WHERE channel=?1 AND kind='state' ORDER BY seq")?;
            let rows = st.query_map([room], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)))?;
            for row in rows {
                let (seq, author, tags_json, cargo) = row?;
                let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
                if let Some((subject, state, owner, tagged)) = read_state(&author, &tags, cargo.as_deref()) {
                    states += 1;
                    subjects.entry(subject).or_default().push(Rev { seq, state, live: tagged || machine(&owner) });
                }
            }
        }
        let mut deleted = 0;
        for revs in subjects.values() {
            for w in revs.windows(2) {
                if w[0].live && w[1].live && w[0].state == w[1].state {
                    if dry {
                        let pointed: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM tags WHERE name = 're:' || (SELECT id FROM observations WHERE seq=?1)) OR EXISTS(SELECT 1 FROM msg WHERE root_seq=?1 AND seq<>?1)",
                            [w[0].seq],
                            |r| r.get(0),
                        )?;
                        deleted += usize::from(!pointed);
                    } else if delete_observation(&tx, w[0].seq)? {
                        deleted += 1;
                    }
                }
            }
        }
        if dry {
            drop(tx);
        } else {
            tx.commit()?;
        }
        out.push(Compacted { room: room.clone(), states, deleted });
    }
    Ok(out)
}
