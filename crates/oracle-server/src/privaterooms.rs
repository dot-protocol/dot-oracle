//! Private rooms (Blaze, 26 Oct 2026: Imran's room is read by "only him, Tara and me"). One owner-signed record, `private-rooms/1`,
//! under the face `[Private rooms]` in #oracle, holds the whole table `{room: [members]}`; the newest valid owner record is the table,
//! kept in its own row (`private_rooms`) as `seat-rooms/1` is. A private room is closed to every seat that is not one of its members,
//! studio seats included: its routes are refused, writes in it are refused, and every other answer is scrubbed of its records
//! (`http::private_scope`). This is the first half of docs/ROOMS.md's "a room is private by default"; the operator of the machine can
//! still read the database, which only end-to-end keys (Black Box) can close.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const ROOM: &str = "oracle";
pub const FACE: &str = "[Private rooms]";
pub const SCHEMA: &str = "private-rooms/1";

/// `room → its members`.
pub type Table = BTreeMap<String, BTreeSet<String>>;

fn name_ok(h: &str) -> bool {
    let b = h.as_bytes();
    !b.is_empty() && b.len() <= 64 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

/// A record's table, or why it cannot be honoured. The CLI and the store use this one function.
pub fn parse(cargo: &str) -> Result<Table, String> {
    let v: Value = serde_json::from_str(cargo).map_err(|_| "the record is not JSON".to_string())?;
    if v["schema"] != SCHEMA {
        return Err(format!("the record's schema is not {SCHEMA}"));
    }
    let t: Table = serde_json::from_value(v["rooms"].clone()).map_err(|_| "the record has no rooms table".to_string())?;
    for (room, members) in &t {
        if !name_ok(room) {
            return Err(format!("{room:?} is not a room name"));
        }
        // The room the table itself lives in can never be closed: that would hide the policy from the seats it binds.
        if room == ROOM {
            return Err(format!("#{ROOM} holds this table and cannot be private"));
        }
        if members.is_empty() {
            return Err(format!("#{room} has no members: remove it from the table instead"));
        }
        if let Some(bad) = members.iter().find(|m| !name_ok(m)) {
            return Err(format!("#{room}: {bad:?} is not a seat handle"));
        }
    }
    Ok(t)
}

/// The record's cargo for a table.
pub fn cargo(t: &Table) -> String {
    serde_json::json!({"schema": SCHEMA, "rooms": t}).to_string()
}

/// The private rooms this seat is not a member of: everything it must not see.
pub fn hidden_for(t: &Table, handle: &str) -> BTreeSet<String> {
    t.iter().filter(|(_, m)| !m.contains(handle)).map(|(r, _)| r.clone()).collect()
}

/// The room an observation id names (`OBS-<room>-<date>-<hex>`), when it has that shape.
pub fn room_of_id(id: &str) -> Option<&str> {
    let rest = id.strip_prefix("OBS-")?;
    // The room may itself contain '-': the date and the hash are the last two parts.
    let mut parts = rest.rsplitn(3, '-');
    let (_hex, _date, room) = (parts.next()?, parts.next()?, parts.next()?);
    Some(room)
}

fn names_hidden(o: &serde_json::Map<String, Value>, hidden: &BTreeSet<String>) -> bool {
    let room_field = ["channel", "room"].iter().any(|k| matches!(o.get(*k), Some(Value::String(r)) if hidden.contains(r)));
    let id_field = ["id", "root", "from", "to", "reply", "ref"]
        .iter()
        .any(|k| matches!(o.get(*k), Some(Value::String(s)) if room_of_id(s).is_some_and(|r| hidden.contains(r))));
    room_field || id_field
}

/// Is this answer, as a whole, one record of a hidden room?
pub fn is_hidden(v: &Value, hidden: &BTreeSet<String>) -> bool {
    match v {
        Value::Object(o) => names_hidden(o, hidden) || matches!(o.get("observation"), Some(Value::Object(x)) if names_hidden(x, hidden)),
        _ => false,
    }
}

/// The answer with every object of a hidden room removed from every list, at any depth.
pub fn scrub(v: Value, hidden: &BTreeSet<String>) -> Value {
    match v {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .filter(|x| !matches!(x, Value::Object(o) if names_hidden(o, hidden)))
                .map(|x| scrub(x, hidden))
                .collect(),
        ),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .filter(|(_, x)| !matches!(x, Value::Object(inner) if names_hidden(inner, hidden)))
                .map(|(k, x)| (k, scrub(x, hidden)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn imran() -> Table {
        parse(&cargo(&Table::from([("imran".into(), ["imran".into(), "tara".into(), "blaze".into()].into())]))).unwrap()
    }

    #[test]
    fn a_table_round_trips_and_bad_records_are_refused() {
        let t = imran();
        assert!(hidden_for(&t, "rocky").contains("imran") && hidden_for(&t, "tara").is_empty() && hidden_for(&t, "blaze").is_empty());
        for bad in [r#"{"schema":"private-rooms/0","rooms":{}}"#, r#"{"schema":"private-rooms/1","rooms":{"Imran":["imran"]}}"#,
                    r#"{"schema":"private-rooms/1","rooms":{"imran":[]}}"#, r#"{"schema":"private-rooms/1","rooms":{"oracle":["blaze"]}}"#,
                    r#"{"schema":"private-rooms/1","rooms":{"imran":["Tara X"]}}"#, "not json"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ids_name_their_room_even_with_dashes() {
        assert_eq!(room_of_id("OBS-imran-20001022-f5ad6db9ca5b"), Some("imran"));
        assert_eq!(room_of_id("OBS-riemann-room-20001019-60cd7df0843a"), Some("riemann-room"));
        assert_eq!(room_of_id("decision-2c39743b7d24"), None);
    }

    #[test]
    fn scrub_drops_every_object_of_a_hidden_room_at_any_depth() {
        let h = hidden_for(&imran(), "rocky");
        let feed = json!({"count": 3, "items": [
            {"id": "OBS-rocky-20001026-aaaaaaaaaaaa", "channel": "rocky", "content": "mine"},
            {"id": "OBS-imran-20001026-bbbbbbbbbbbb", "channel": "imran", "content": "PRIVATE"},
            {"root": {"channel": "rocky"}, "replies": [{"channel": "imran", "content": "PRIVATE_REPLY"}]},
            {"from": "OBS-rocky-20001026-aaaaaaaaaaaa", "to": "OBS-imran-20001026-bbbbbbbbbbbb", "kind": "answers"}]});
        let out = scrub(feed, &h).to_string();
        assert!(out.contains("mine") && !out.contains("PRIVATE") && !out.contains("bbbbbbbbbbbb"), "{out}");
        assert!(is_hidden(&json!({"id": "OBS-imran-20001026-bbbbbbbbbbbb", "content": "x"}), &h));
        assert!(is_hidden(&json!({"observation": {"channel": "imran"}}), &h));
        assert!(!is_hidden(&json!({"channel": "imran"}), &hidden_for(&imran(), "tara")));
    }
}
