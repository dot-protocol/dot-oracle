//! A room's map (docs/ROOM-MAP.md): the written map (the `map` field of the room's signed State) beside what the index
//! computes about the room, in one read, so a seat that wakes knows what each of its rooms holds before it reads any of it.
use crate::store::{Result, Store};
use rusqlite::params;
use serde_json::{json, Value};

/// How long ago, as a seat reads it next to the Oracle clock.
fn ago(now: i64, at: i64) -> String {
    let m = (now - at).max(0) / 60_000;
    if m < 120 {
        format!("{m}m ago")
    } else if m < 48 * 60 {
        format!("{}h ago", m / 60)
    } else {
        format!("{}d ago", m / 1440)
    }
}

fn thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// The first line of a written map, cut to 140 characters.
fn first_line(text: &str) -> String {
    let l = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if l.chars().count() > 140 { format!("{}…", l.chars().take(139).collect::<String>()) } else { l.to_string() }
}

/// The words the hourly ranking says this room says most: `rooms[room]` of every ranked term (`term`; `word` read too), the top `n` by count.
pub fn words_of(terms: &Value, room: &str, n: usize) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = terms["terms"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| Some((t["term"].as_str().or(t["word"].as_str())?.to_string(), t["rooms"][room].as_i64()?)))
        .filter(|(_, k)| *k > 0)
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out.truncate(n);
    out
}

/// The map of one room: `written` (or null), `computed`, and `line` (what the brief prints).
pub fn map_of(s: &Store, room: &str, terms: Option<&Value>, now: i64) -> Result<Value> {
    let st = s.room_state_at(room, None)?;
    let written = if st["state"] == "alive" {
        st["fields"]["map"].as_str().map(str::trim).filter(|m| !m.is_empty()).map(|text| {
            let head = st["heads"][0].as_str().unwrap_or("").to_string();
            let ev = st["evidence"].as_array().into_iter().flatten().find(|e| e["id"] == head.as_str());
            json!({"text": text, "head": head, "by": ev.and_then(|e| e["author"].as_str()), "at_ms": ev.and_then(|e| e["received_ms"].as_i64())})
        })
    } else {
        None
    };
    let state_fields: Vec<&str> = ["purpose", "goal", "current", "next", "questions"]
        .into_iter()
        .filter(|k| st["fields"][*k].as_str().is_some_and(|v| !v.trim().is_empty()))
        .collect();
    let c = s.conn()?;
    let (records, first_ms, last_ms): (i64, Option<i64>, Option<i64>) =
        c.query_row("SELECT COUNT(*), MIN(created_ms), MAX(created_ms) FROM observations WHERE channel=?1", params![room], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let mut by_seat = c.prepare_cached("SELECT author, COUNT(*) n FROM observations WHERE channel=?1 AND created_ms>?2 GROUP BY author ORDER BY n DESC, author LIMIT 5")?;
    let seats: Vec<(String, i64)> = by_seat
        .query_map(params![room, now - 30 * 86_400_000], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    // Roots not yet answered or settled (docs/ROOM-INDEX.md `thread`): every plain message is one, so this is a count for the
    // computed map, not a fact for the line. An old store without the index has none, which is not an error.
    let open_threads: i64 = c.query_row("SELECT COUNT(*) FROM thread WHERE channel=?1 AND state='open'", params![room], |r| r.get(0)).unwrap_or(0);
    let words = terms.map(|t| words_of(t, room, 8)).unwrap_or_default();
    // How much of the room is an import (a claude.ai export): its seats line counts the last 30 days of writers, so a room of
    // imported chats read "seats noor 6, shannon 1" for 20,396 records (31 Oct 2026, #illuminati).
    let imported: i64 = c
        .query_row("SELECT COUNT(*) FROM observations WHERE channel=?1 AND content LIKE '[CLAUDE-EXPORT%'", params![room], |r| r.get(0))
        .unwrap_or(0);
    let mut facts = vec![format!("{} record{}{}", thousands(records), if records == 1 { "" } else { "s" },
        if imported > 0 { format!(" ({} imported chats)", thousands(imported)) } else { String::new() })];
    if let Some(l) = last_ms {
        facts.push(format!("last {}", ago(now, l)));
    }
    if !seats.is_empty() {
        // three in the line (it is read at wake), five in the computed map
        facts.push(format!("seats {}", seats.iter().take(3).map(|(h, n)| format!("{h} {}", thousands(*n))).collect::<Vec<_>>().join(", ")));
    }
    if !words.is_empty() {
        facts.push(format!("words {}", words.iter().map(|(w, _)| w.as_str()).collect::<Vec<_>>().join(" ")));
    }
    let facts = facts.join(" · ");
    let line = match &written {
        Some(w) => format!("#{room}  {} · {facts}", first_line(w["text"].as_str().unwrap_or(""))),
        None => format!("#{room}  no map yet · {facts}"),
    };
    Ok(json!({
        "schema": "room-map/1", "room": room, "written": written,
        "computed": {"records": records, "imported": imported, "first_ms": first_ms, "last_ms": last_ms, "open_threads": open_threads,
                     "seats": seats.iter().map(|(h, n)| json!({"seat": h, "n": n})).collect::<Vec<_>>(),
                     "words": words.iter().map(|(w, n)| json!({"word": w, "n": n})).collect::<Vec<_>>(),
                     "state_fields": state_fields},
        "line": line,
    }))
}
