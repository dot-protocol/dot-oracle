//! Seat aliases (card seat-alias-migration). A label a seat once used (`emmy-core`) can still be addressed; when its seat (`emmy`) answers, the request is
//! answered. Not a second signer and not a rewrite of history: one signed record, `seat-alias/1`, a whole table `{label: seat}`, posted in #oracle under the
//! face `[Oracle aliases]` by an owner-key seat (the trust `[Room members]` already has). The newest valid record is the table; a record that is not the owner's, or
//! not valid, changes nothing. Readers resolve a name through the table to compare seats and keep the name that was written beside it.

use std::collections::BTreeMap;

pub const ROOM: &str = "oracle";
pub const FACE: &str = "[Oracle aliases]";
pub const SCHEMA: &str = "seat-alias/1";

/// `label → seat`.
pub type Aliases = BTreeMap<String, String>;

use crate::naming::handle_ok;

/// A record's table, or why it cannot be honoured. The CLI and the reader use this one function, so what `oracle alias set` accepts is what is read back.
pub fn parse(cargo: &str) -> Result<Aliases, String> {
    let v: serde_json::Value = serde_json::from_str(cargo).map_err(|_| "the record is not JSON".to_string())?;
    if v["schema"] != SCHEMA {
        return Err(format!("the record's schema is not {SCHEMA}"));
    }
    let obj = v["aliases"].as_object().ok_or("the record has no aliases table")?;
    let mut t = Aliases::new();
    for (label, seat) in obj {
        let seat = seat.as_str().unwrap_or("");
        if !handle_ok(label) || !handle_ok(seat) {
            return Err(format!("{label:?} → {seat:?}: both must be a seat handle (lowercase letters, digits, - and _)"));
        }
        if label == seat {
            return Err(format!("{label} cannot be an alias of itself"));
        }
        t.insert(label.clone(), seat.to_owned());
    }
    if let Some((label, seat)) = t.iter().find(|(_, seat)| t.contains_key(*seat)) {
        return Err(format!("{label} → {seat}, and {seat} is itself an alias: that is a chain; name the final seat"));
    }
    Ok(t)
}

/// The record's cargo for a table.
pub fn cargo(t: &Aliases) -> String {
    serde_json::json!({"schema": SCHEMA, "aliases": t}).to_string()
}

/// The seat a name stands for.
pub fn canon<'a>(t: &'a Aliases, name: &'a str) -> &'a str {
    t.get(name).map(String::as_str).unwrap_or(name)
}

/// Do two names stand for the same seat?
pub fn same(t: &Aliases, a: &str, b: &str) -> bool {
    canon(t, a) == canon(t, b)
}
