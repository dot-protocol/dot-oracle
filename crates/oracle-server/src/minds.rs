//! Minds (Blaze, 3 Nov 2026: "minds is supposed to be a list of minds in oracle with their statements", shown in the shape of
//! Brain cases and the Names index): every author, carbon or silicon, ranked by how much they have said; and one mind's
//! statements whole, newest first. A statement is a say or a memory the mind wrote; a reaction mark (react:*) and a task-head
//! write are not. What the reader may not see (a private room it is not in, a room outside its scope) is never shown.
use crate::{
    privaterooms,
    store::{Observation, Result, Store, StoreError},
};
use serde_json::{json, Value};

/// Handles that are machinery, not minds: estate digests, the desk's book feeds, imports.
pub fn is_mind(handle: &str) -> bool {
    !handle.is_empty() && !matches!(handle, "unknown" | "anonymous" | "estate" | "system" | "import")
}

/// Whether a record is a statement of its author: signed by it (envelope 2; an import is not its word), a say or a memory, not a
/// reaction mark.
pub fn is_statement(o: &Observation) -> bool {
    o.envelope == 2 && matches!(o.kind.as_str(), "say" | "memory") && !o.tags.iter().any(|t| t.starts_with("react:"))
}

/// `GET /minds` (schema minds/1): every mind with how many records it wrote, newest statement first among equals; ranked by
/// how much each has said.
pub fn list(s: &Store) -> Result<Value> {
    let mut rows: Vec<(String, i64, i64, i64)> = s.author_counts()?.into_iter().filter(|r| is_mind(&r.0)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(b.3.cmp(&a.3)).then(a.0.cmp(&b.0)));
    let minds: Vec<Value> = rows.iter().map(|(h, n, seq, ms)| json!({"handle": h, "records": n, "last_seq": seq, "last_ms": ms})).collect();
    Ok(json!({"schema": "minds/1", "total": minds.len(), "minds": minds}))
}

/// `GET /minds/:handle?before=&limit=` (schema mind-statements/1): what `handle` said, newest first, at most `limit` (default 50,
/// at most 200) statements the reader `seat` may see; `next` is the cursor for older ones, absent at the start of the record.
pub fn statements(s: &Store, seat: &str, handle: &str, before: Option<i64>, limit: Option<usize>) -> Result<Value> {
    if handle.is_empty() || handle.len() > 64 {
        return Err(StoreError::Rejected("a mind's handle is 1 to 64 bytes".into()));
    }
    let want = limit.unwrap_or(50).clamp(1, 200);
    let hidden = privaterooms::hidden_for(&s.private_rooms_table()?, seat);
    let scope = s.seat_rooms_for(seat)?;
    let visible = |room: &str| !hidden.contains(room) && scope.as_ref().is_none_or(|r| r.may_read(room));
    let (mut out, mut cursor, mut exhausted) = (Vec::new(), before, false);
    // Walk back in pages until `want` statements are found or the author's record ends; reactions and hidden rooms are skipped.
    for _ in 0..20 {
        let page = s.authored(handle, cursor, 250)?;
        if page.is_empty() { exhausted = true; break; }
        cursor = page.last().map(|o| o.seq);
        for o in page.iter().filter(|o| is_statement(o) && visible(&o.channel)) {
            out.push(json!({"id": o.id, "seq": o.seq, "room": o.channel, "kind": o.kind, "created_ms": o.created_ms, "text": o.content, "tags": o.tags}));
            if out.len() == want { break; }
        }
        if out.len() == want || page.len() < 250 { exhausted = out.len() < want; break; }
    }
    let next = if exhausted { None } else { out.last().and_then(|o| o["seq"].as_i64()) };
    Ok(json!({"schema": "mind-statements/1", "handle": handle, "statements": out, "next": next}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn obs(kind: &str, tags: &[&str]) -> Observation {
        Observation { seq: 1, id: "OBS-x".into(), channel: "oracle".into(), author: "rocky".into(), kind: kind.into(), content: "c".into(), cargo: None,
            tags: tags.iter().map(|t| t.to_string()).collect(), created_ms: 1, authored_ms: None, pubkey: String::new(), sig: String::new(), envelope: 2 }
    }
    #[test]
    fn a_statement_is_a_say_or_a_memory_and_never_a_reaction_or_a_task_head() {
        assert!(is_statement(&obs("say", &["reply:OBS-y"])));
        assert!(is_statement(&obs("memory", &[])));
        assert!(!is_statement(&obs("say", &["react:done", "reply:OBS-y"])), "a reaction mark is not a statement");
        assert!(!is_statement(&obs("state", &[])), "a task-head write is not a statement");
        let mut import = obs("say", &[]);
        import.envelope = 1;
        assert!(!is_statement(&import), "an import (a claude.ai export, the graph Oracle) is not the mind's own word");
        assert!(!is_mind("estate") && !is_mind("") && is_mind("noor"));
    }
}
