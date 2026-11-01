//! The hallway (Round 45, Jobs' second page): every room in one read, each with its signed State's current and next
//! lines and its oldest open ask, every line naming the observation it came from. Read on request from the room index
//! and the State fold; nothing is stored. A room whose State is missing says so; it is never filled in.
use crate::store::{Result, Store};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

/// Rooms with a message in the last `days` days, newest activity first.
pub fn read(s: &Store, days: i64, now_ms: i64) -> Result<Value> {
    let since = now_ms - days.clamp(1, 3650) * 86_400_000;
    let archived = s.archived_rooms()?;
    let rooms: Vec<(String, i64, i64)> = s
        .channels()?
        .into_iter()
        .filter(|(ch, _, last_ms, _)| *last_ms >= since && !archived.contains_key(ch))
        .map(|(ch, total, last_ms, _)| (ch, total, last_ms))
        .collect();
    let mut out = Vec::with_capacity(rooms.len());
    for (room, total, last_ms) in rooms {
        let state = s.room_state_at(&room, None).ok();
        let field = |k: &str| state.as_ref().and_then(|v| v["fields"][k].as_str()).filter(|x| !x.is_empty()).map(str::to_owned);
        let (open_asks, oldest): (i64, Option<Value>) = s.with_conn(|c| {
            let open: i64 = c.query_row(
                "SELECT count(*) FROM msg m JOIN thread t ON t.root_seq=m.root_seq \
                 WHERE m.channel=?1 AND m.ask IN ('reply','act','decide','review') AND t.state='open' AND m.class<>'history'",
                params![room],
                |r| r.get(0),
            )?;
            let first: Option<(String, String, i64, String, String)> = c
                .query_row(
                    "SELECT o.id, o.author, o.created_ms, o.content, o.tags_json FROM msg m JOIN thread t ON t.root_seq=m.root_seq \
                     JOIN observations o ON o.seq=m.seq \
                     WHERE m.channel=?1 AND m.ask IN ('reply','act','decide','review') AND t.state='open' AND m.class<>'history' \
                     ORDER BY m.seq ASC LIMIT 1",
                    params![room],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            Ok((open, first.map(|(id, author, at, content, tags)| {
                let to: Vec<String> = serde_json::from_str::<Vec<String>>(&tags).unwrap_or_default().into_iter().filter_map(|t| t.strip_prefix("to:").map(str::to_owned)).collect();
                json!({"id": id, "author": author, "at_ms": at, "to": to,
                       "card": content.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(160).collect::<String>()})
            })))
        })?;
        out.push(json!({
            "room": room, "messages": total, "last_ms": last_ms,
            "current": field("current"), "next": field("next"), "purpose": field("purpose"),
            "has_state": field("current").is_some() || field("purpose").is_some(),
            "open_asks": open_asks, "oldest_open_ask": oldest,
        }));
    }
    Ok(json!({"now_ms": now_ms, "days": days, "rooms": out}))
}
