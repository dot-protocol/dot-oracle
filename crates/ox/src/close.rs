//! `ox close`: the asker ends its own asks (docs/ASKS-AND-EVENTS.md "Whoever opens an ask closes it"). One ask, or a
//! batch validated as a whole before anything is written: every id must be the caller's own open or answered ask and
//! every entry complete, or nothing is written and every failing entry is named. Each ask gets its own close record,
//! threaded to it; a batch adds one notice per room. Nothing is erased or rewritten.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub end: String,
    #[serde(default)]
    pub answered_in: Option<String>,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub id: String,
    pub room: String,
    pub end: String,
    pub text: String,
    pub tags: Vec<String>,
}

fn answered_in_ok(w: &str) -> bool {
    matches!(w, "chat" | "bridge" | "call")
        || w.strip_prefix("room:").is_some_and(|r| {
            !r.is_empty()
                && r.len() <= 64
                && r.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// The caller's own asks that may be closed, by id: (room, state). From `/asks`' `im_waiting_on`.
pub fn mine(asks: &Value) -> BTreeMap<String, (String, String)> {
    asks["im_waiting_on"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some((
                r["id"].as_str()?.to_string(),
                (
                    r["room"].as_str()?.to_string(),
                    r["state"].as_str().unwrap_or("open").to_string(),
                ),
            ))
        })
        .collect()
}

fn plan_one(
    e: &Entry,
    own: &BTreeMap<String, (String, String)>,
    capped: bool,
) -> Result<Planned, String> {
    let Some((room, _)) = own.get(&e.id) else {
        return Err(format!(
            "{}: not one of your open or answered asks{}",
            e.id,
            if capped {
                " (your list is capped; if it is yours and older, close it with ox say --closes)"
            } else {
                ""
            }
        ));
    };
    let why = e.why.as_deref().map(str::trim).filter(|w| !w.is_empty());
    if let Some(w) = &e.answered_in {
        if !answered_in_ok(w) {
            return Err(format!(
                "{}: answered_in must be chat, bridge, call or room:<name>, not {w}",
                e.id
            ));
        }
    }
    let mut tags = vec![
        format!("reply:{}", e.id),
        format!("closes:{}", e.id),
        format!("end:{}", e.end),
    ];
    let text = match e.end.as_str() {
        "closed" => {
            let w = e.answered_in.as_deref().unwrap_or("thread");
            if w == "chat" && why.is_none() {
                return Err(format!("{}: answered in chat needs why (the answer's words): a close must say what settled it", e.id));
            }
            if let Some(a) = &e.answered_in {
                tags.push(format!("answered-in:{a}"));
            }
            match why {
                Some(w2) => format!("Closed, answered in {w}: {w2}"),
                None => format!("Closed, answered in {w}."),
            }
        }
        "done" => {
            tags.push("intent:result".into());
            match why {
                Some(w) => format!("Done: {w}"),
                None => "Done: the work arrived.".into(),
            }
        }
        "superseded" => {
            let by = e
                .by
                .as_deref()
                .filter(|b| b.starts_with("OBS-"))
                .ok_or_else(|| format!("{}: superseded needs by: the newer ask's OBS id", e.id))?;
            tags.push(format!("ref:{by}"));
            format!(
                "Superseded by {by}.{}",
                why.map(|w| format!(" {w}")).unwrap_or_default()
            )
        }
        "dropped" => {
            let w = why.ok_or_else(|| format!("{}: dropped needs why (the reason)", e.id))?;
            format!("Dropped: {w}")
        }
        other => {
            return Err(format!(
                "{}: end must be closed, done, superseded or dropped, not {other}",
                e.id
            ))
        }
    };
    Ok(Planned {
        id: e.id.clone(),
        room: room.clone(),
        end: e.end.clone(),
        text,
        tags,
    })
}

/// Validate every entry; all or nothing. A duplicate id is an error too.
pub fn plan(entries: &[Entry], asks: &Value) -> Result<Vec<Planned>, Vec<String>> {
    let own = mine(asks);
    let capped = asks["capped"] == true;
    let mut seen = std::collections::BTreeSet::new();
    let (mut ok, mut bad) = (vec![], vec![]);
    for e in entries {
        if !seen.insert(e.id.clone()) {
            bad.push(format!("{}: listed twice", e.id));
            continue;
        }
        match plan_one(e, &own, capped) {
            Ok(p) => ok.push(p),
            Err(why) => bad.push(why),
        }
    }
    if entries.is_empty() {
        bad.push("the batch is empty".into());
    }
    if bad.is_empty() {
        Ok(ok)
    } else {
        Err(bad)
    }
}

/// One notice per room for a batch: the asks, their ends and their close records.
pub fn notices(done: &[(Planned, String)]) -> Vec<(String, String, Value, Vec<String>)> {
    let mut by: BTreeMap<String, Vec<&(Planned, String)>> = BTreeMap::new();
    for d in done {
        by.entry(d.0.room.clone()).or_default().push(d);
    }
    by.into_iter().map(|(room, rows)| {
        let lines: Vec<String> = rows.iter().map(|(p, rec)| format!("- {} → {} ({rec})", p.id, p.end)).collect();
        let text = format!("Closed {} of my asks in #{room}:\n{}", rows.len(), lines.join("\n"));
        let cargo = json!({"closes": rows.iter().map(|(p, rec)| json!({"ask": p.id, "end": p.end, "record": rec})).collect::<Vec<_>>()});
        (room, text, cargo, vec!["intent:status".into(), "closes-batch".into()])
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asks() -> Value {
        json!({"im_waiting_on": [
            {"id": "OBS-rocky-1", "room": "rocky", "state": "answered"},
            {"id": "OBS-dot-2", "room": "dot", "state": "open"},
        ], "waiting_on_me": [{"id": "OBS-dot-9", "room": "dot", "state": "open"}]})
    }
    fn e(id: &str, end: &str) -> Entry {
        Entry {
            id: id.into(),
            end: end.into(),
            answered_in: None,
            why: None,
            by: None,
        }
    }

    #[test]
    fn someone_elses_ask_is_refused() {
        let bad = plan(&[e("OBS-dot-9", "done")], &asks()).unwrap_err();
        assert!(
            bad[0].contains("not one of your open or answered asks"),
            "{bad:?}"
        );
    }

    #[test]
    fn answered_in_chat_needs_the_answer() {
        let mut x = e("OBS-rocky-1", "closed");
        x.answered_in = Some("chat".into());
        assert!(plan(&[x.clone()], &asks()).unwrap_err()[0].contains("needs why"));
        x.why = Some("Blaze picked the earlier designs".into());
        let p = plan(&[x], &asks()).unwrap();
        assert_eq!(p[0].room, "rocky");
        assert!(
            p[0].tags.contains(&"closes:OBS-rocky-1".into())
                && p[0].tags.contains(&"reply:OBS-rocky-1".into())
        );
        assert!(p[0].tags.contains(&"answered-in:chat".into()));
        assert!(p[0].text.contains("Blaze picked"));
    }

    #[test]
    fn one_bad_entry_writes_nothing_and_every_bad_entry_is_named() {
        let mut sup = e("OBS-dot-2", "superseded");
        sup.by = None;
        let bad = plan(
            &[
                e("OBS-rocky-1", "done"),
                sup,
                e("OBS-dot-9", "done"),
                e("OBS-rocky-1", "done"),
            ],
            &asks(),
        )
        .unwrap_err();
        assert_eq!(bad.len(), 3, "{bad:?}");
        assert!(bad.iter().any(|b| b.contains("superseded needs by")));
        assert!(bad.iter().any(|b| b.contains("OBS-dot-9")));
        assert!(bad.iter().any(|b| b.contains("listed twice")));
    }

    #[test]
    fn a_good_batch_is_one_record_per_ask_and_one_notice_per_room() {
        let mut d = e("OBS-dot-2", "dropped");
        d.why = Some("no longer needed".into());
        let p = plan(&[e("OBS-rocky-1", "done"), d], &asks()).unwrap();
        assert_eq!(p.len(), 2);
        let done: Vec<(Planned, String)> = p
            .into_iter()
            .enumerate()
            .map(|(i, p)| (p, format!("OBS-rec-{i}")))
            .collect();
        let n = notices(&done);
        assert_eq!(n.len(), 2);
        assert!(n.iter().any(|(room, text, cargo, _)| room == "dot"
            && text.contains("OBS-dot-2 → dropped")
            && cargo["closes"][0]["record"] == "OBS-rec-1"));
    }

    #[test]
    fn ends_and_places_are_checked() {
        assert!(plan(&[e("OBS-dot-2", "maybe")], &asks()).unwrap_err()[0].contains("end must be"));
        let mut x = e("OBS-dot-2", "closed");
        x.answered_in = Some("email".into());
        assert!(plan(&[x.clone()], &asks()).unwrap_err()[0].contains("answered_in must be"));
        x.answered_in = Some("room:hustler".into());
        assert!(plan(&[x], &asks()).is_ok());
        assert!(plan(&[], &asks()).is_err());
    }
}
