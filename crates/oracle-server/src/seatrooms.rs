//! Seat rooms (Blaze, 22 Oct 2026: a mind that serves a person outside the studio is confined to that person's rooms by Oracle itself, not
//! by a wrapper and not by its instructions). One owner-signed record, `seat-rooms/1`, under the face `[Seat rooms]` in #oracle, holds the
//! whole table `{seat: {write: [rooms], read: [rooms]}}`; the newest valid owner record is the table (the trust `seat-alias/1` has). The
//! store keeps it in its own row (`seat_scope`), not in the room index, so an index that is not built never unscopes anyone.
//!
//! A seat in the table is scoped: it writes only in its write rooms, it reads only its rooms, and every route that is not one room's is
//! refused to it (`http::seat_scope`). A seat that is not in the table is unchanged.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const ROOM: &str = "oracle";
pub const FACE: &str = "[Seat rooms]";
pub const SCHEMA: &str = "seat-rooms/1";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rooms {
    #[serde(default)]
    pub write: BTreeSet<String>,
    #[serde(default)]
    pub read: BTreeSet<String>,
}

impl Rooms {
    pub fn may_read(&self, room: &str) -> bool {
        self.write.contains(room) || self.read.contains(room)
    }
    pub fn may_write(&self, room: &str) -> bool {
        self.write.contains(room)
    }
}

/// `seat → its rooms`.
pub type Table = BTreeMap<String, Rooms>;

fn name_ok(h: &str) -> bool {
    let b = h.as_bytes();
    !b.is_empty() && b.len() <= 64 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

/// A record's table, or why it cannot be honoured. The CLI and the store use this one function.
pub fn parse(cargo: &str) -> Result<Table, String> {
    let v: serde_json::Value = serde_json::from_str(cargo).map_err(|_| "the record is not JSON".to_string())?;
    if v["schema"] != SCHEMA {
        return Err(format!("the record's schema is not {SCHEMA}"));
    }
    let t: Table = serde_json::from_value(v["seats"].clone()).map_err(|_| "the record has no seats table".to_string())?;
    for (seat, r) in &t {
        if !name_ok(seat) {
            return Err(format!("{seat:?} is not a seat handle"));
        }
        if r.write.is_empty() && r.read.is_empty() {
            return Err(format!("{seat} has no rooms: remove it from the table instead"));
        }
        if let Some(bad) = r.write.iter().chain(r.read.iter()).find(|x| !name_ok(x)) {
            return Err(format!("{seat}: {bad:?} is not a room name"));
        }
    }
    Ok(t)
}

/// The record's cargo for a table.
pub fn cargo(t: &Table) -> String {
    serde_json::json!({"schema": SCHEMA, "seats": t}).to_string()
}

/// The fields of a signed record itself. Everything else on an answer (thread state, counts, summaries, cards, asks, snippets) is
/// derived, possibly from records in other rooms, so a scoped seat gets only these (Emmy, 22 Oct: a derived field carries no room label).
pub const RAW: [&str; 13] = ["id", "seq", "channel", "room", "author", "kind", "content", "created_ms", "tags", "cargo", "pubkey", "sig", "envelope"];

/// A scoped seat's view of a list or a single record: only raw records of rooms it may read, only their own fields; pagination kept.
pub fn raw(v: serde_json::Value, rooms: &Rooms) -> Option<serde_json::Value> {
    use serde_json::{Map, Value};
    let record = |o: &Map<String, Value>| -> Option<Value> {
        let room = o.get("channel").or_else(|| o.get("room")).and_then(Value::as_str)?;
        if !rooms.may_read(room) {
            return None;
        }
        Some(Value::Object(o.iter().filter(|(k, _)| RAW.contains(&k.as_str())).map(|(k, x)| (k.clone(), x.clone())).collect()))
    };
    let Value::Object(top) = v else { return None };
    if top.contains_key("channel") || (top.contains_key("room") && top.contains_key("id")) {
        return record(&top);
    }
    if let Some(Value::Object(o)) = top.get("observation") {
        return record(o).map(|r| serde_json::json!({"observation": r}));
    }
    let mut out = Map::new();
    for key in ["count", "limit", "more", "next_after", "next_before", "scanned", "room"] {
        if let Some(x) = top.get(key) {
            out.insert(key.into(), x.clone());
        }
    }
    for key in ["items", "messages", "hits", "results"] {
        if let Some(Value::Array(a)) = top.get(key) {
            out.insert(key.into(), Value::Array(a.iter().filter_map(|x| x.as_object().and_then(record)).collect()));
        }
    }
    Some(Value::Object(out))
}

/// Does any object in this answer name a room the seat may not read?
pub fn foreign(v: &serde_json::Value, rooms: &Rooms) -> bool {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            ["channel", "room"].iter().any(|k| matches!(map.get(*k), Some(Value::String(r)) if !rooms.may_read(r))) || map.values().any(|x| foreign(x, rooms))
        }
        Value::Array(items) => items.iter().any(|x| foreign(x, rooms)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tara() -> Rooms {
        parse(&cargo(&Table::from([("tara".into(), Rooms { write: ["imran".into()].into(), read: ["career".into()].into() })]))).unwrap()["tara"].clone()
    }

    #[test]
    fn a_table_round_trips_and_bad_records_are_refused() {
        assert!(tara().may_write("imran") && tara().may_read("career") && !tara().may_write("career") && !tara().may_read("blaze"));
        for bad in [r#"{"schema":"seat-rooms/0","seats":{}}"#, r#"{"schema":"seat-rooms/1","seats":{"Tara":{"write":["imran"]}}}"#,
                    r#"{"schema":"seat-rooms/1","seats":{"tara":{"write":["Imran Room"]}}}"#, r#"{"schema":"seat-rooms/1","seats":{"tara":{}}}"#, "not json"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn derived_answers_are_foreign_and_raw_keeps_only_records_of_its_rooms() {
        let r = tara();
        // A thread whose summary was derived from a foreign reply is foreign as a whole.
        let derived = json!({"root": {"channel": "imran"}, "summary": "PRIVATE_SUMMARY", "replies": [{"channel": "blaze", "author": "PRIVATE_AUTHOR"}]});
        assert!(foreign(&derived, &r));
        assert!(!foreign(&json!({"root": {"channel": "imran"}, "replies": [{"channel": "imran"}]}), &r));
        // Raw: derived fields are stripped and foreign records dropped.
        let feed = json!({"count": 2, "items": [{"id": "a", "channel": "imran", "content": "hi", "state": "answered", "intent": "q"}, {"id": "b", "channel": "blaze", "content": "secret"}]});
        assert_eq!(raw(feed, &r).unwrap(), json!({"count": 2, "items": [{"id": "a", "channel": "imran", "content": "hi"}]}));
        assert!(raw(json!({"observation": {"id": "x", "channel": "astra"}}), &r).is_none());
    }
}
