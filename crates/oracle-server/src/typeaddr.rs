//! Type addresses (Blaze, 31 Oct 2026, plan.oracle.search T7): a record's name is `domain.form.topic`, a signed tag
//! `type:<addr>` judged by the seat that writes it, and routed by prefix: `oracle.decision` reaches every
//! `oracle.decision.*`. It is reassignable with history: a later record tagged `retype:<id>` and `type:<new>` by the
//! record's own author renames it; every retype stays in the ledger and is shown, newest last.

use crate::store::{Observation, Result, Store, StoreError};
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The type a record names for itself: its first `type:` tag.
pub fn own_type(o: &Observation) -> Option<&str> {
    o.tags.iter().find_map(|t| t.strip_prefix("type:"))
}

/// Does `addr` sit under `prefix` (itself, or a dotted child of it)?
pub fn under(addr: &str, prefix: &str) -> bool {
    prefix.is_empty() || addr == prefix || addr.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('.'))
}

/// The records whose type is `prefix` or under it, newest first, in rooms the caller may read (`hidden` are the
/// private rooms it is not a member of), with each one's retype history; and the count per type.
pub fn typed(s: &Store, prefix: &str, hidden: &BTreeSet<String>, limit: usize) -> Result<Value> {
    if !prefix.is_empty() && !oracle_proto::type_addr_ok(prefix) {
        return Err(StoreError::Rejected(format!("{prefix:?} is not a type address (domain.form.topic: lowercase parts joined by dots)")));
    }
    // One connection at a time: each query's connection is dropped before `get_many` takes its own (a pool of one in tests).
    // Candidates: records tagged with a type under the prefix, and retype records naming such a type. The tag index
    // answers both as range scans: `type:p` itself, then `type:p.` up to `type:p/` ('/' follows '.').
    let (lo, hi) = if prefix.is_empty() { ("type:".to_string(), "type;".to_string()) } else { (format!("type:{prefix}."), format!("type:{prefix}/")) };
    let ids: Vec<String> = {
        let c = s.conn()?;
        let mut st = c.prepare_cached(
            "SELECT o.id FROM tags t JOIN obs_live o ON o.seq = t.obs_seq
              WHERE (t.name = ?1 OR (t.name >= ?2 AND t.name < ?3)) ORDER BY o.seq DESC LIMIT 20000",
        )?;
        let v = st.query_map(params![format!("type:{prefix}"), lo, hi], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        v
    };
    let found = s.get_many(&ids)?;
    // Every record any candidate retypes, and every retype of any of them.
    let mut targets: BTreeSet<String> = BTreeSet::new();
    for o in &found {
        match o.tags.iter().find_map(|t| t.strip_prefix("retype:")) {
            Some(t) => targets.insert(t.to_string()),
            None => targets.insert(o.id.clone()),
        };
    }
    let target_list: Vec<String> = targets.iter().cloned().collect();
    let records: BTreeMap<String, Observation> = s.get_many(&target_list)?.into_iter().map(|o| (o.id.clone(), o)).collect();
    let mut retypes: BTreeMap<String, Vec<Observation>> = BTreeMap::new();
    for chunk in target_list.chunks(2_000) {
        let holes = vec!["?"; chunk.len()].join(",");
        let names: Vec<String> = chunk.iter().map(|id| format!("retype:{id}")).collect();
        let p: Vec<&dyn rusqlite::ToSql> = names.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
        let rids: Vec<String> = {
            let c = s.conn()?;
            let mut q = c.prepare(&format!("SELECT o.id FROM tags t JOIN obs_live o ON o.seq = t.obs_seq WHERE t.name IN ({holes}) ORDER BY o.seq"))?;
            let v = q.query_map(p.as_slice(), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            v
        };
        for r in s.get_many(&rids)? {
            if let Some(t) = r.tags.iter().find_map(|t| t.strip_prefix("retype:")) {
                retypes.entry(t.to_string()).or_default().push(r);
            }
        }
    }
    let mut items: Vec<(i64, Value)> = Vec::new();
    let mut per_type: BTreeMap<String, usize> = BTreeMap::new();
    for (id, o) in &records {
        if hidden.contains(&o.channel) {
            continue;
        }
        let mut history: Vec<Value> = Vec::new();
        let mut current = own_type(o).map(str::to_string);
        let mut rs = retypes.remove(id).unwrap_or_default();
        rs.sort_by_key(|r| r.seq);
        for r in &rs {
            let to = own_type(r).map(str::to_string);
            // Only the record's own author renames it; anyone else's retype is kept and shown as a proposal.
            let applied = r.author == o.author && to.as_deref().is_some_and(oracle_proto::type_addr_ok);
            history.push(json!({"id": r.id, "by": r.author, "at_ms": r.created_ms, "from": current, "to": to, "applied": applied}));
            if applied {
                current = to;
            }
        }
        let Some(t) = current.filter(|t| under(t, prefix)) else { continue };
        *per_type.entry(t.clone()).or_default() += 1;
        items.push((o.seq, json!({
            "id": o.id, "code": code_of(&o.id), "type": t, "room": o.channel, "author": o.author, "created_ms": o.created_ms,
            "text": o.content.chars().take(240).collect::<String>(), "retypes": history,
        })));
    }
    items.sort_by_key(|i| std::cmp::Reverse(i.0));
    let count = items.len();
    let mut types: Vec<(String, usize)> = per_type.into_iter().collect();
    types.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Ok(json!({
        "prefix": prefix, "count": count,
        "types": types.iter().map(|(t, n)| json!({"type": t, "count": n})).collect::<Vec<_>>(),
        "items": items.into_iter().take(limit).map(|(_, v)| v).collect::<Vec<_>>(),
    }))
}

/// A record's code: the last twelve characters of a signed record's id (the first twelve of sha256 of its signature).
pub fn code_of(id: &str) -> Option<&str> {
    let tail = id.get(id.len().saturating_sub(12)..)?;
    (id.starts_with("OBS-") && tail.bytes().all(|b| b.is_ascii_hexdigit())).then_some(tail)
}
