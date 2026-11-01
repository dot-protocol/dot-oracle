//! A head's lineage (`GET /states?subject=&history=<n>`), ported from graph-Oracle PR #14 with Emmy's limit+1 fix.
//!
//! A subject's revisions are its state writes (`kind: state`, tagged `subject:<s>`). Nothing stored changes: `current` and `superseded_by` are worked out
//! here, at read time, from the revisions' own links.
//! - A revision that names earlier ones with `supersedes:<id>` replaced exactly those.
//! - A revision that names none replaced the one written just before it (a head is replaced by the next write to its subject).
//! - So two revisions that both name the same parent are a **fork**: neither replaced the other, both are heads, and a later revision naming both merges them.
//!
//! The chain comes newest first. It is cut to `history` entries by asking for one more than that: `truncated` is true only when another one exists.

use crate::store::{Result, Store, StoreError};
use serde_json::{json, Value};
use std::collections::HashMap;

pub const MAX_HISTORY: i64 = 200;
/// A subject with more revisions than this is read from its newest ones only.
const SCAN_MAX: i64 = 5000;

struct Rev {
    seq: i64,
    id: String,
    room: String,
    author: String,
    created_ms: i64,
    note: String,
    state: Option<String>,
    owner: Option<String>,
    next_touch: Option<String>,
    supersedes: Vec<String>,
    signed: bool,
}

/// The chain of `subject`'s revisions, newest first, at most `history` of them.
pub fn chain(s: &Store, subject: &str, history: i64) -> Result<Value> {
    if subject.is_empty() || subject.len() > 200 {
        return Err(StoreError::Rejected("subject is required (at most 200 characters)".into()));
    }
    if !(1..=MAX_HISTORY).contains(&history) {
        return Err(StoreError::Rejected(format!("history must be 1 to {MAX_HISTORY}, not {history}")));
    }
    s.with_conn(|c| {
        let mut revs: Vec<Rev> = {
            let mut st = c.prepare(
                "SELECT o.seq, o.id, o.channel, o.author, o.created_ms, o.content, o.cargo, o.tags_json, o.envelope \
                 FROM tags t JOIN observations o ON o.seq=t.obs_seq WHERE t.name=?1 AND o.kind='state' ORDER BY o.seq DESC LIMIT ?2",
            )?;
            let rows = st
                .query_map(rusqlite::params![format!("subject:{subject}"), SCAN_MAX], |r| {
                    let cargo: Option<String> = r.get(6)?;
                    let tags: Vec<String> = serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or_default();
                    let cv: Value = cargo.as_deref().and_then(|c| serde_json::from_str(c).ok()).unwrap_or(Value::Null);
                    let text = |k: &str| cv[k].as_str().map(str::to_owned).or_else(|| tags.iter().find_map(|t| t.strip_prefix(&format!("{k}:")).map(str::to_owned)));
                    let content: String = r.get(5)?;
                    Ok(Rev {
                        seq: r.get(0)?,
                        id: r.get(1)?,
                        room: r.get(2)?,
                        author: r.get::<_, String>(3)?,
                        created_ms: r.get(4)?,
                        note: content.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(200).collect(),
                        state: text("state"),
                        owner: text("owner"),
                        next_touch: text("next_touch"),
                        supersedes: tags.iter().filter_map(|t| t.strip_prefix("supersedes:").map(str::to_owned)).collect(),
                        signed: r.get::<_, i64>(8)? == 2,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        if revs.is_empty() {
            return Err(StoreError::NotFound);
        }
        revs.reverse(); // oldest first
        let index: HashMap<&str, usize> = revs.iter().enumerate().map(|(i, r)| (r.id.as_str(), i)).collect();
        // who replaced whom
        let mut by: Vec<Vec<usize>> = vec![vec![]; revs.len()];
        for (i, r) in revs.iter().enumerate() {
            let named: Vec<usize> = r.supersedes.iter().filter_map(|id| index.get(id.as_str()).copied()).filter(|p| *p < i).collect();
            if !named.is_empty() {
                for p in named {
                    by[p].push(i);
                }
            } else if i > 0 {
                by[i - 1].push(i);
            }
        }
        let heads: Vec<&str> = (0..revs.len()).filter(|i| by[*i].is_empty()).map(|i| revs[i].id.as_str()).collect();
        let head: Option<String> = c
            .query_row("SELECT o.id FROM state_heads h JOIN observations o ON o.seq=h.obs_seq WHERE h.subject=?1", [subject], |r| r.get(0))
            .ok();
        let mut out: Vec<Value> = Vec::new();
        for i in (0..revs.len()).rev() {
            let r = &revs[i];
            out.push(json!({
                "id": r.id, "seq": r.seq, "room": r.room, "author": r.author, "created_ms": r.created_ms, "note": r.note,
                "state": r.state, "owner": r.owner.clone().unwrap_or_else(|| r.author.clone()), "next_touch": r.next_touch,
                "current": by[i].is_empty(), "superseded_by": by[i].iter().map(|j| revs[*j].id.as_str()).collect::<Vec<_>>(),
                "supersedes": r.supersedes, "signed": r.signed, "verification": if r.signed { "verified" } else { "imported_unverified" },
            }));
        }
        // one more than asked for decides whether anything was cut
        let truncated = out.len() as i64 > history;
        out.truncate(history as usize);
        Ok(json!({"subject": subject, "revisions": out, "count": out.len(), "limit": history, "truncated": truncated,
            "heads": heads, "fork": heads.len() > 1, "head": head, "scanned_all": (revs.len() as i64) < SCAN_MAX}))
    })
}
