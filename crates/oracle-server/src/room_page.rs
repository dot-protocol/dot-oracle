//! `GET /rooms/:ch/page` (schema room-page/1): a room's page, its live state (Blaze, 3 Nov 2026: "a page is a state of the
//! room, and it changes and updates as the room's messages come, as the embedder works, as the state updates ... It should
//! not be a questionnaire filled by you at a random time"). Composed on every read from the room's signed records, as
//! `GET /world` is for the estate: what the room is (its State head and how far the room has moved past it), what was
//! decided, what is being built, what waits on someone, what moved, who is here, how fresh the meaning index is, and the
//! newest reading a seat wrote about it with how many records arrived since. No model reads or writes anything here. Every
//! line is an object carrying its record id, so a reader opens the source. Bounded reads only: index seeks with limits.
use crate::store::{clean_text, room_revisions, Result, Store};
use crate::world::first_sentence;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

pub const DECIDED_MAX: usize = 8;
pub const DECIDED_DAYS: i64 = 30;
pub const BUILDING_MAX: usize = 10;
pub const WAITING_MAX: usize = 8;
pub const WAITING_DAYS: i64 = 14;
pub const MOVING_MAX: usize = 8;
pub const WHO_DAYS: i64 = 7;
pub const WHO_MAX: usize = 12;
const DAY: i64 = 86_400_000;

/// The tags that make a record a decision in its room: a decision said as one, a choice picked on a decide, a remembered decision.
fn is_decision(tags: &[String]) -> bool {
    tags.iter().any(|t| t == "intent:decision" || t == "memory:decision" || t.starts_with("choice:"))
}

fn tags_of(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn tag_value<'a>(tags: &'a [String], key: &str) -> Option<&'a str> {
    tags.iter().find_map(|t| t.strip_prefix(key))
}

fn line(content: &str) -> String {
    first_sentence(&clean_text(content))
}

/// How many records `room` received after `seq` (capped at 10,000: the page says "10000+").
fn since(c: &Connection, room: &str, seq: i64) -> rusqlite::Result<i64> {
    c.query_row("SELECT count(*) FROM (SELECT 1 FROM obs_live WHERE channel=?1 AND seq>?2 LIMIT 10000)", params![room, seq], |r| r.get(0))
}

/// The page of `room` as of `now_ms`. `meaning_through`: the highest record seq the meaning index holds, when it is on.
pub fn compose(s: &Store, room: &str, now_ms: i64, meaning_through: Option<i64>) -> Result<Value> {
    let heads = s.state_heads_in(None, Some(room))?;
    s.with_conn(|c| {
        // FRESHNESS: the room's newest record, how much it said today and this week.
        let newest: Option<(i64, String, i64)> = c
            .query_row("SELECT seq, id, created_ms FROM obs_live WHERE channel=?1 ORDER BY seq DESC LIMIT 1", params![room], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let count_after = |ms: i64| -> rusqlite::Result<i64> {
            c.query_row("SELECT count(*) FROM (SELECT 1 FROM obs_live WHERE channel=?1 AND created_ms>=?2 LIMIT 10000)", params![room, ms], |r| r.get(0))
        };
        let (today, week) = (count_after(now_ms - DAY)?, count_after(now_ms - WHO_DAYS * DAY)?);
        let meaning = match (meaning_through, &newest) {
            (Some(through), Some((top, _, _))) => json!({"on": true, "through_seq": through, "behind": if *top > through { since(c, room, through)? } else { 0 }}),
            (Some(through), None) => json!({"on": true, "through_seq": through, "behind": 0}),
            (None, _) => json!({"on": false}),
        };

        // WHAT IT IS: the State head, and how far the room has moved past it.
        let rv = room_revisions(c, room, None)?;
        let hs = rv.heads();
        let head = rv.revisions.iter().filter(|r| hs.contains(&r.id)).max_by_key(|r| r.seq);
        let state = match head {
            Some(h) => json!({"id": h.id, "by": h.author, "at_ms": h.created_ms, "fields": h.fields, "revisions": rv.revisions.len(),
                "records_since": since(c, room, h.seq)?, "forked": hs.len() > 1}),
            None => Value::Null,
        };

        // DECIDED: decisions said in this room in the last 30 days, newest first.
        let mut decided: Vec<Value> = vec![];
        let mut decided_more = 0;
        {
            let mut st = c.prepare_cached(
                "SELECT o.id, o.author, o.created_ms, o.content, o.tags_json FROM observations o JOIN obs_live l ON l.seq=o.seq \
                 WHERE o.channel=?1 AND o.created_ms>=?2 AND o.seq IN (SELECT obs_seq FROM tags WHERE name IN ('intent:decision','memory:decision') \
                 OR (name>='choice:' AND name<'choice;')) ORDER BY o.seq DESC LIMIT 60",
            )?;
            let rows = st.query_map(params![room, now_ms - DECIDED_DAYS * DAY], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?)))?;
            for row in rows {
                let (id, by, at, content, tags) = row?;
                let tags = tags_of(&tags);
                if !is_decision(&tags) {
                    continue;
                }
                if decided.len() >= DECIDED_MAX {
                    decided_more += 1;
                    continue;
                }
                decided.push(json!({"id": id, "by": by, "at_ms": at, "text": line(&content), "answers": tag_value(&tags, "reply:"),
                    "choice": tag_value(&tags, "choice:")}));
            }
        }

        // BUILDING: the room's task heads that are running, blocked or open, newest touched first.
        let mut building: Vec<&crate::store::StateRow> = heads.iter().filter(|h| matches!(h.state.as_str(), "running" | "blocked" | "open")).collect();
        building.sort_by_key(|h| (h.state != "blocked", std::cmp::Reverse(h.at_ms)));
        let building_more = building.len().saturating_sub(BUILDING_MAX);
        let building: Vec<Value> = building.iter().take(BUILDING_MAX).map(|h| json!({"id": h.id, "subject": h.subject, "state": h.state, "owner": h.owner,
            "next": h.next_touch, "note": first_sentence(&h.note), "at_ms": h.at_ms})).collect();

        // THREADS: what waits on someone (open threads whose root asks for something, last 14 days) and what moved most recently.
        let mut st = c.prepare_cached(
            "SELECT t.root_seq, t.state, t.replies, t.last_ms, t.participants, o.id, o.author, o.content, o.tags_json FROM thread t \
             JOIN observations o ON o.seq=t.root_seq WHERE t.channel=?1 AND t.state NOT IN ('archived','superseded','ambient') \
             ORDER BY t.last_seq DESC LIMIT 200",
        )?;
        let rows = st.query_map(params![room], |r| {
            Ok((r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?,
                r.get::<_, String>(7)?, r.get::<_, String>(8)?))
        })?;
        let (mut waiting, mut moving, mut waiting_more) = (vec![], vec![], 0usize);
        for row in rows {
            let (tstate, replies, last_ms, participants, id, author, content, tags) = row?;
            let tags = tags_of(&tags);
            // Waiting means someone must act, decide, reply or review; an ask to read (a digest) waits on no one (Blaze's #oracle page,
            // 3 Nov 2026: five estate digests filled Waiting on someone).
            let ask = tag_value(&tags, "ask:").filter(|a| *a != "none" && *a != "read");
            let to: Vec<&str> = tags.iter().filter_map(|t| t.strip_prefix("to:")).collect();
            let entry = json!({"id": id, "by": author, "state": tstate, "replies": replies, "last_ms": last_ms, "text": line(&content),
                "ask": ask, "to": to, "participants": serde_json::from_str::<Value>(&participants).unwrap_or(Value::Null)});
            if tstate == "open" && ask.is_some() && last_ms >= now_ms - WAITING_DAYS * DAY {
                if waiting.len() < WAITING_MAX { waiting.push(entry.clone()); } else { waiting_more += 1; }
            }
            if moving.len() < MOVING_MAX {
                moving.push(entry);
            }
        }

        // WHO: the voices of the last seven days, most recent first.
        let mut st = c.prepare_cached(
            "SELECT author, count(*), max(created_ms) FROM obs_live WHERE channel=?1 AND created_ms>=?2 GROUP BY author ORDER BY max(created_ms) DESC LIMIT ?3",
        )?;
        let who: Vec<Value> = st
            .query_map(params![room, now_ms - WHO_DAYS * DAY, WHO_MAX as i64], |r| Ok(json!({"seat": r.get::<_, String>(0)?, "records": r.get::<_, i64>(1)?, "last_ms": r.get::<_, i64>(2)?})))?
            .collect::<rusqlite::Result<_>>()?;

        // READING: the newest page a seat wrote about this room (tag page:room), and how many records arrived since.
        let reading = c
            .query_row(
                "SELECT o.id, o.author, o.created_ms, o.seq FROM observations o JOIN tags t ON t.obs_seq=o.seq \
                 WHERE t.name='page:room' AND o.channel=?1 ORDER BY o.seq DESC LIMIT 1",
                params![room],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)),
            )
            .optional()?;
        let reading = match reading {
            Some((id, by, at, seq)) => json!({"id": id, "by": by, "at_ms": at, "records_since": since(c, room, seq)?}),
            None => Value::Null,
        };

        Ok(json!({
            "schema": "room-page/1", "room": room, "now_ms": now_ms, "now": oracle_proto::iso_ms(now_ms),
            "freshness": {"newest": newest.map(|(seq, id, at)| json!({"seq": seq, "id": id, "at_ms": at})), "today": today, "week": week, "meaning": meaning},
            "state": state,
            "decided": {"items": decided, "more": decided_more, "days": DECIDED_DAYS},
            "building": {"items": building, "more": building_more},
            "waiting": {"items": waiting, "more": waiting_more, "days": WAITING_DAYS},
            "moving": {"items": moving},
            "who": {"items": who, "days": WHO_DAYS},
            "reading": reading,
        }))
    })
}
