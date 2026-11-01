//! The waking read (`GET /delta`, `oracle_delta`, `ox delta`; Blaze, 1 Nov 2026: food for an agent is the diff between
//! the world's last known state and its current state). One seat, one point in the log (its last RESUME by default),
//! and what changed after it: the rooms it follows, the asks that involve it, Blaze's words, the machine. Counts over
//! seq ranges on the indexes the log already has (obs_channel_seq, obs_author_seq, tags by name), and a few head lines;
//! never a room read whole. Nothing is stored.
use crate::asks::AskRow;
use crate::public::PublicRoom;
use crate::roomview::access;
use crate::store::{Observation, Principal, Result, Store, StoreError, LIVE};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The rooms whose new records are Blaze's words (the brief's WORDS: the rooms he writes in).
pub const WORD_ROOMS: [&str; 2] = ["blaze", "cortex"];
/// A count stops here: past it the honest answer is "a lot" (the fold's PENDING_CAP).
pub const COUNT_CAP: i64 = 1000;
/// Head lines shown per room, and for WORDS and the seat's own records.
pub const LINES: i64 = 5;
const WEEK_MS: i64 = 7 * 86_400_000;

/// Where the look back starts: the record or time given, resolved to an exclusive seq cursor and a time.
#[derive(Debug, Clone)]
pub struct Since {
    /// `resume`, `record` or `time`.
    pub kind: &'static str,
    pub given: Option<String>,
    pub id: Option<String>,
    /// Records after this seq are new.
    pub seq: i64,
    pub ms: i64,
}

/// `since` as given (an OBS id or a code, a time, a span like `6h`), or the seat's last RESUME: the newest record it wrote
/// in its own room tagged `resume`, read by tag, never by ranked search (the brief's rule).
pub fn resolve_since(s: &Store, seat: &str, given: Option<&str>, now_ms: i64) -> Result<Since> {
    let given = given.map(str::trim).filter(|g| !g.is_empty());
    let Some(g) = given else {
        let resume: Option<(String, i64, i64)> = s.with_conn(|c| {
            Ok(c.query_row(
                "SELECT o.id, o.seq, o.created_ms FROM tags t JOIN observations o ON o.seq = t.obs_seq \
                 WHERE t.name = 'resume' AND o.author = ?1 AND o.channel = ?1 ORDER BY t.obs_seq DESC LIMIT 1",
                params![seat],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ).optional()?)
        })?;
        let (id, seq, ms) = resume.ok_or_else(|| StoreError::Rejected(format!(
            "{seat} has no RESUME in #{seat} to start from; give since: an OBS id, a time like 2026-10-31T08:00 or a span like 6h")))?;
        return Ok(Since { kind: "resume", given: None, id: Some(id), seq, ms });
    };
    // An id, or a code (hex with a letter in it: a unix time is all digits and is a time).
    let looks_like_id = g.to_ascii_uppercase().starts_with("OBS-") || (g.len() >= 6 && g.bytes().all(|b| b.is_ascii_hexdigit()) && g.bytes().any(|b| b.is_ascii_alphabetic()));
    if looks_like_id {
        let id = s.resolve_id(g)?;
        let o = s.get(&id)?;
        return Ok(Since { kind: "record", given: Some(g.to_owned()), id: Some(o.id), seq: o.seq, ms: o.created_ms });
    }
    let ms = oracle_mcp::parse_since_at(g, now_ms).map_err(|e| StoreError::Rejected(e.to_string()))?;
    Ok(Since { kind: "time", given: Some(g.to_owned()), id: None, seq: seq_before(s, ms)?, ms })
}

/// The exclusive seq cursor for a time: everything at or after `ms` in time order is after it (the fold's day floor: one
/// seek on obs_created). Nothing written since: the newest seq, so nothing is new.
fn seq_before(s: &Store, ms: i64) -> Result<i64> {
    let floor: Option<i64> = s.with_conn(|c| {
        Ok(c.query_row("SELECT seq - 1 FROM observations INDEXED BY obs_created WHERE created_ms >= ?1 ORDER BY created_ms LIMIT 1", params![ms], |r| r.get(0)).optional()?)
    })?;
    match floor {
        Some(f) => Ok(f),
        None => s.newest_seq(),
    }
}

/// The rooms a seat follows, each with why: `state` (named in its own State's `rooms` field), `posted` (it wrote there in
/// the last 7 days), `public` (a public room). Only rooms it may read (`roomview::access`).
pub fn followed(s: &Store, who: &Principal, public: &[PublicRoom], now_ms: i64) -> Result<BTreeMap<String, Vec<&'static str>>> {
    let seat = who.handle.as_str();
    let mut rooms: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
    // Its State: the newest revision it wrote in its own room (a cheap read of one cargo, not the whole State fold).
    let cargo: Option<String> = s.with_conn(|c| {
        Ok(c.query_row(
            "SELECT o.cargo FROM tags t JOIN observations o ON o.seq = t.obs_seq \
             WHERE t.name = 'room-context:v1' AND o.channel = ?1 AND o.author = ?1 ORDER BY t.obs_seq DESC LIMIT 1",
            params![seat],
            |r| r.get(0),
        ).optional()?.flatten())
    })?;
    let named = cargo
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .and_then(|v| v["fields"]["rooms"].as_str().map(oracle_mcp::rooms_named))
        .unwrap_or_default();
    for r in named {
        rooms.entry(r).or_default().push("state");
    }
    let week = seq_before(s, now_ms - WEEK_MS)?;
    let posted: Vec<String> = s.with_conn(|c| {
        let mut st = c.prepare_cached("SELECT DISTINCT channel FROM observations WHERE author = ?1 AND seq > ?2")?;
        let v = st.query_map(params![seat, week], |r| r.get(0))?.collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(v)
    })?;
    for r in posted {
        rooms.entry(r).or_default().push("posted");
    }
    for p in public {
        rooms.entry(p.name.clone()).or_default().push("public");
    }
    let mut out = BTreeMap::new();
    for (room, why) in rooms {
        if access(s, public, who, &room)?.is_some() {
            out.insert(room, why);
        }
    }
    Ok(out)
}

/// Messages in `room` after `seq`, counted up to COUNT_CAP: marks and annotations are not news (the fold's rule).
fn count_after(s: &Store, room: &str, seq: i64) -> Result<i64> {
    s.with_conn(|c| {
        Ok(c.query_row(
            &format!("SELECT count(*) FROM (SELECT 1 FROM observations WHERE channel = ?1 AND seq > ?2 AND kind <> 'annotate' AND {LIVE} \
              AND NOT EXISTS(SELECT 1 FROM tags ta WHERE ta.obs_seq = observations.seq AND (ta.name LIKE 'annotates:%' OR ta.name LIKE 'react:%')) LIMIT ?3)"),
            params![room, seq, COUNT_CAP],
            |r| r.get(0),
        )?)
    })
}

/// One line for a record: id, author, time, and its first non-empty line (at most 160 characters).
fn line(o: &Observation) -> Value {
    let head: String = o.content.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(160).collect();
    json!({"id": o.id, "room": o.channel, "author": o.author, "ms": o.created_ms, "line": head})
}

fn lines_of(s: &Store, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Value>> {
    let ids: Vec<String> = s.with_conn(|c| {
        let mut st = c.prepare_cached(sql)?;
        let v = st.query_map(args, |r| r.get(0))?.collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(v)
    })?;
    let mut rows = s.get_many(&ids)?;
    rows.sort_by_key(|o| std::cmp::Reverse(o.seq));
    Ok(rows.iter().map(line).collect())
}

/// One room since `seq`: how many new, the new ones addressed to `seat` as the fold rings them (to:, mention:, to:all, or
/// a reply, close or supersede of something the seat wrote, by the exact relation prefix), and whether its State moved (a
/// State revision stored since), with the newest revision's id.
pub fn room_delta(s: &Store, room: &str, seat: &str, seq: i64) -> Result<Value> {
    let new = count_after(s, room, seq)?;
    let (to_me, mention_me) = (format!("to:{seat}"), format!("mention:{seat}"));
    let addressed = "FROM observations o WHERE o.channel = ?1 AND o.seq > ?2 AND o.author <> ?3 AND o.kind <> 'annotate' \
                     AND (EXISTS(SELECT 1 FROM tags t WHERE t.obs_seq = o.seq AND t.name IN (?4, ?5, 'to:all')) \
                       OR EXISTS(SELECT 1 FROM tags t WHERE t.obs_seq = o.seq \
                                 AND substr(t.name, 1, instr(t.name, ':')) COLLATE BINARY IN ('reply:', 're:', 'in_reply_to:', 'closes:', 'supersedes:') \
                                 AND EXISTS(SELECT 1 FROM observations p WHERE p.id = substr(t.name, instr(t.name, ':') + 1) AND p.author = ?3)))";
    let to_me_count: i64 = s.with_conn(|c| {
        Ok(c.query_row(&format!("SELECT count(*) FROM (SELECT 1 {addressed} LIMIT ?6)"), params![room, seq, seat, to_me, mention_me, COUNT_CAP], |r| r.get(0))?)
    })?;
    let to_me_lines = if to_me_count > 0 {
        lines_of(s, &format!("SELECT o.id {addressed} ORDER BY o.seq DESC LIMIT ?6"), &[&room, &seq, &seat, &to_me, &mention_me, &LINES])?
    } else {
        vec![]
    };
    let (revisions, head): (i64, Option<String>) = s.with_conn(|c| {
        Ok(c.query_row(
            "SELECT count(*), (SELECT o2.id FROM tags t2 JOIN observations o2 ON o2.seq = t2.obs_seq WHERE t2.name = 'room-context:v1' AND t2.obs_seq > ?2 AND o2.channel = ?1 ORDER BY t2.obs_seq DESC LIMIT 1) \
             FROM tags t JOIN observations o ON o.seq = t.obs_seq WHERE t.name = 'room-context:v1' AND t.obs_seq > ?2 AND o.channel = ?1",
            params![room, seq],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    })?;
    Ok(json!({"room": room, "new": new, "capped": new >= COUNT_CAP, "to_me": to_me_count, "to_me_lines": to_me_lines,
              "state": {"changed": revisions > 0, "revisions": revisions, "head": head}}))
}

fn ask_line(r: &AskRow) -> Value {
    json!({"id": r.id, "room": r.room, "author": r.author, "ask": r.ask, "state": r.state, "held_by": r.held_by,
           "card": r.card, "created_ms": r.created_ms, "settled_ms": r.settled_ms, "answered_by": r.answered_by})
}

/// The asks that involve `seat` and moved since: opened (written since), answered (an answer written since, still open),
/// closed (settled since). From the asks view itself (`asks::asks_since`), so it agrees with `ox asks`.
pub fn asks_delta(s: &Store, seat: &str, since: &Since, now_ms: i64) -> Result<Value> {
    let a = crate::asks::asks_since(s, seat, since.ms, now_ms)?;
    let open: Vec<&AskRow> = a.waiting_on_me.iter().chain(&a.im_waiting_on).chain(&a.acked).chain(&a.stuck).chain(&a.later).collect();
    let mut seen = std::collections::BTreeSet::new();
    let opened: Vec<Value> = open.iter().copied().chain(&a.done_today).filter(|r| r.created_ms >= since.ms && seen.insert(r.id.clone())).map(ask_line).collect();
    let answer_ids: Vec<String> = open.iter().filter(|r| r.state == "answered").filter_map(|r| r.answered_by.clone()).collect();
    let fresh: std::collections::BTreeSet<String> = s.get_many(&answer_ids)?.into_iter().filter(|o| o.seq > since.seq).map(|o| o.id).collect();
    let answered: Vec<Value> = open.iter().filter(|r| r.answered_by.as_ref().is_some_and(|id| fresh.contains(id))).map(|r| ask_line(r)).collect();
    let closed: Vec<Value> = a.done_today.iter().map(ask_line).collect();
    Ok(json!({"opened": opened, "answered": answered, "closed": closed, "capped": a.capped}))
}

/// The whole delta for `who` since `since`. `started_ms` is when this server process started (its last restart).
pub fn delta(s: &Store, who: &Principal, public: &[PublicRoom], since: &Since, started_ms: i64, now_ms: i64) -> Result<Value> {
    let seat = who.handle.as_str();
    let mut rooms = Vec::new();
    let mut quiet = Vec::new();
    for (room, why) in followed(s, who, public, now_ms)? {
        let mut d = room_delta(s, &room, seat, since.seq)?;
        if d["new"] == 0 && d["state"]["changed"] == false {
            quiet.push(room);
            continue;
        }
        d["why"] = json!(why);
        rooms.push(d);
    }
    let mut words = Vec::new();
    for room in WORD_ROOMS {
        if access(s, public, who, room)?.is_none() {
            continue;
        }
        let new = count_after(s, room, since.seq)?;
        let items = if new > 0 {
            lines_of(s, &format!("SELECT id FROM observations WHERE channel = ?1 AND seq > ?2 AND kind <> 'annotate' AND {LIVE} ORDER BY seq DESC LIMIT ?3"), &[&room, &since.seq, &LINES])?
        } else {
            vec![]
        };
        words.push(json!({"room": room, "new": new, "items": items}));
    }
    let mine_since: i64 = s.with_conn(|c| {
        Ok(c.query_row("SELECT count(*) FROM (SELECT 1 FROM observations WHERE author = ?1 AND seq > ?2 LIMIT ?3)", params![seat, since.seq, COUNT_CAP], |r| r.get(0))?)
    })?;
    // The seat's own records since, newest first: what it did itself after the point (its RESUME is the point, not after it).
    let mine = lines_of(s, "SELECT id FROM observations WHERE author = ?1 AND seq > ?2 ORDER BY seq DESC LIMIT ?3", &[&seat, &since.seq, &LINES])?;
    Ok(json!({
        "schema": "delta/1", "seat": seat, "now_ms": now_ms, "now": oracle_proto::iso_ms(now_ms),
        "since": {"kind": since.kind, "given": since.given, "id": since.id, "seq": since.seq, "ms": since.ms, "iso": oracle_proto::iso_ms(since.ms)},
        "rooms": rooms, "quiet": quiet,
        "asks": asks_delta(s, seat, since, now_ms)?,
        "words": words,
        "machine": {"started_ms": started_ms, "started": oracle_proto::iso_ms(started_ms), "restarted_since": started_ms > since.ms,
                    "mine_since": mine_since, "mine": mine},
    }))
}
