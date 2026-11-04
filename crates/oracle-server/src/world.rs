//! `GET /world` (schema world/1): WORLD STATE, the page a mind reads to know where the world stands (Blaze's priority,
//! 1 Nov 2026, relayed by Noor; Noor's schema; the
//! five-verbs card: `brief where=world`). Composed here from signed heads, never a
//! document anyone retypes: six sections, each bounded, each saying how many more and where the rest is. Every line is
//! an object carrying its record id and room, so the private-scope layer can remove what a reader may not see; records in
//! private rooms are left out of the page altogether, so neither a line nor a count comes from one. Bounded reads only:
//! the head table, tag index seeks with a limit, one State fold per seat, one asks walk for the owner; never a room read
//! whole. The route keeps the answer 30 s. The text (`oracle_mcp::world_render`) is drawn by the client.
use crate::asks::{asks_for, StateFilter};
use crate::store::{room_revisions, Observation, Result, Store};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// The room whose newest `queue/1` record is the owner's queue (AXXIS /jobs, 1 Nov 2026: Rocky ranks it, Blaze drags it).
pub const QUEUE_ROOM: &str = "rocky";
pub const IDENTITIES_MAX: usize = 6;
pub const STEPS_MAX: usize = 6;
pub const QUEUE_ITEMS: usize = 5;
pub const PROJECTS_MAX: usize = 12;
pub const DECISIONS_MAX: usize = 8;
pub const DECISIONS_DAYS: i64 = 7;
pub const SEATS_MAX: usize = 6;
pub const QUESTIONS_MAX: usize = 8;
/// The one door (Blaze, 4 Nov 2026, on Noor's card: "every room's State + every seat's NOW on one screen"): every room
/// with a signed State, and every seat's newest NOW, whole lists for a screen; the text page shows the first few.
pub const ROOMS_MAX: usize = 120;
pub const ROOMS_DAYS: i64 = 60;
pub const MINDS_MAX: usize = 60;

fn cargo_of(o: &Observation) -> Value {
    o.cargo.as_deref().and_then(|c| serde_json::from_str(c).ok()).unwrap_or(Value::Null)
}

/// The first sentence of a text (to the first ". ", "! ", "? " or line end), whole; the reader's line cuts it to fit.
pub fn first_sentence(s: &str) -> String {
    let line = s.trim().lines().next().unwrap_or("").trim();
    let end = [". ", "! ", "? "].iter().filter_map(|p| line.find(p).map(|i| i + 1)).min().unwrap_or(line.len());
    line[..end].trim().to_string()
}

/// A seat's own signed record: written by it, in its own room, verified here (envelope 2).
fn own(o: &Observation) -> bool {
    o.author == o.channel && o.envelope == 2
}

/// WORLD STATE as of `now_ms`.
pub fn compose(s: &Store, now_ms: i64) -> Result<Value> {
    let private: BTreeSet<String> = s.private_rooms_table()?.keys().cloned().collect();
    let open = |room: &str| !private.contains(room);
    let keys = s.keys()?;
    let owner = keys.iter().find(|k| k.role == "owner" && k.revoked_ms.is_none()).map(|k| k.handle.clone());
    let seats: BTreeSet<String> = keys.iter().filter(|k| k.revoked_ms.is_none()).map(|k| k.handle.clone()).collect();

    // NORTH STAR: the newest canon head, and one line per seat's identity head (its newest, by itself, in its own room).
    let canon = s.by_tag("canon", 50)?.into_iter().filter(|o| own(o) && open(&o.channel)).find_map(|o| {
        let c = cargo_of(&o);
        (c["schema"] == "canon/1").then(|| json!({"id": o.id, "room": o.channel, "seat": o.author, "version": c["version"],
            "rules": c["rules"].as_array().map_or(0, Vec::len), "open": c["open"].as_array().map_or(0, Vec::len), "at_ms": o.created_ms}))
    });
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut identities: Vec<Value> = vec![];
    for o in s.by_tag("identity", 200)?.into_iter().filter(|o| own(o) && open(&o.channel)) {
        let c = cargo_of(&o);
        if c["schema"] != "identity-head/1" || c["seat"].as_str() != Some(o.author.as_str()) || !seen.insert(o.author.clone()) {
            continue;
        }
        identities.push(json!({"seat": o.author, "id": o.id, "room": o.channel, "purpose": first_sentence(c["purpose"].as_str().unwrap_or("")), "at_ms": o.created_ms}));
    }
    let identities_more = identities.len().saturating_sub(IDENTITIES_MAX);
    identities.truncate(IDENTITIES_MAX);

    // NOW: every step marked doing in a plan head, and the owner's queue.
    let heads = s.state_heads_in(None, None)?;
    let plan_heads: Vec<&crate::store::StateRow> = heads.iter().filter(|h| h.subject.starts_with("plan.") && open(&h.channel)).collect();
    let plan_rows = s.get_many(&plan_heads.iter().map(|h| h.id.clone()).collect::<Vec<_>>())?;
    let mut steps: Vec<(i64, Value)> = vec![];
    for o in &plan_rows {
        let c = cargo_of(o);
        let plan = c["subject"].as_str().unwrap_or("");
        for st in c["data"]["steps"].as_array().into_iter().flatten().filter(|st| st["status"] == "doing") {
            steps.push((o.created_ms, json!({"plan": plan, "id": o.id, "room": o.channel, "step": st["id"], "title": st["title"],
                "owner": st["owner"].as_str().or(st["by"].as_str())})));
        }
    }
    steps.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    let steps_more = steps.len().saturating_sub(STEPS_MAX);
    let steps: Vec<Value> = steps.into_iter().take(STEPS_MAX).map(|(_, v)| v).collect();
    let queue = if open(QUEUE_ROOM) {
        s.by_tag("queue", 20)?.into_iter().filter(|o| o.channel == QUEUE_ROOM).find_map(|o| {
            let c = cargo_of(&o);
            let items = c["items"].as_array().filter(|_| c["schema"] == "queue/1")?.clone();
            let shown: Vec<Value> = items.iter().filter(|q| q["subject"].is_string()).take(QUEUE_ITEMS)
                .map(|q| json!({"subject": q["subject"], "title": q["title"]})).collect();
            Some(json!({"id": o.id, "room": o.channel, "by": o.author, "at_ms": o.created_ms, "items": shown, "more": items.len().saturating_sub(QUEUE_ITEMS)}))
        })
    } else {
        None
    };

    // ACTIVE PROJECTS: task heads running or blocked, newest touched first (plans are in NOW).
    let mut projects: Vec<&crate::store::StateRow> = heads.iter()
        .filter(|h| matches!(h.state.as_str(), "running" | "blocked") && !h.subject.starts_with("plan.") && open(&h.channel)).collect();
    projects.sort_by_key(|h| std::cmp::Reverse(h.at_ms));
    let projects_more = projects.len().saturating_sub(PROJECTS_MAX);
    let projects: Vec<Value> = projects.iter().take(PROJECTS_MAX).map(|h| json!({"id": h.id, "room": h.channel, "subject": h.subject, "state": h.state,
        "owner": h.owner, "next": h.next_touch, "note": first_sentence(&h.note), "at_ms": h.at_ms})).collect();

    // KEY DECISIONS: #memory records tagged memory:decision in the last seven days, newest first.
    let week = now_ms - DECISIONS_DAYS * 86_400_000;
    let recent: Vec<Observation> = s.by_tag("memory:decision", 200)?.into_iter().filter(|o| o.channel == "memory" && o.created_ms >= week && open(&o.channel)).collect();
    let decisions_more = recent.len().saturating_sub(DECISIONS_MAX);
    let decisions: Vec<Value> = recent.iter().take(DECISIONS_MAX).map(|o| json!({"id": o.id, "room": o.channel, "by": o.author, "at_ms": o.created_ms,
        "text": first_sentence(&crate::store::clean_text(&o.content))})).collect();

    // ACTIVE SEATS: every registered seat whose own room has a signed State: its current line, when it was written.
    let mut with_state: Vec<Value> = vec![];
    for seat in seats.iter().filter(|r| open(r)) {
        let rv = s.with_conn(|c| room_revisions(c, seat, None))?;
        let heads = rv.heads();
        let Some(head) = rv.revisions.iter().filter(|r| heads.contains(&r.id)).max_by_key(|r| r.seq) else { continue };
        with_state.push(json!({"seat": seat, "id": head.id, "room": seat, "current": first_sentence(head.fields["current"].as_str().unwrap_or("")),
            "at_ms": head.created_ms, "degraded": heads.len() > 1 || !rv.bad.is_empty()}));
    }
    with_state.sort_by_key(|v| std::cmp::Reverse(v["at_ms"].as_i64().unwrap_or(0)));
    let seats_more = with_state.len().saturating_sub(SEATS_MAX);
    with_state.truncate(SEATS_MAX);

    // EVERY ROOM: each open room touched in the last ROOMS_DAYS whose State is signed: its current and next lines.
    let since = now_ms - ROOMS_DAYS * 86_400_000;
    let mut rooms: Vec<Value> = vec![];
    for (room, _, at, _) in s.channels()?.into_iter().filter(|(r, _, at, _)| *at >= since && open(r)) {
        let rv = s.with_conn(|c| room_revisions(c, &room, None))?;
        let heads = rv.heads();
        let Some(head) = rv.revisions.iter().filter(|r| heads.contains(&r.id)).max_by_key(|r| r.seq) else { continue };
        let f = |k: &str| first_sentence(head.fields[k].as_str().unwrap_or(""));
        rooms.push(json!({"room": room, "id": head.id, "purpose": f("purpose"), "current": f("current"), "next": f("next"),
            "state_at_ms": head.created_ms, "active_ms": at, "degraded": heads.len() > 1 || !rv.bad.is_empty()}));
        if rooms.len() >= ROOMS_MAX {
            break;
        }
    }

    // EVERY MIND'S NOW: each seat's newest now/1, written by it in its own room (docs/NOW.md): what it is on, what is next.
    let mut seen_now: BTreeSet<String> = BTreeSet::new();
    let mut minds: Vec<Value> = vec![];
    for o in s.by_tag("now", 600)?.into_iter().filter(|o| own(o) && open(&o.channel)) {
        let c = cargo_of(&o);
        if c["schema"] != "now/1" || !seen_now.insert(o.author.clone()) {
            continue;
        }
        let f = |k: &str| first_sentence(c[k].as_str().unwrap_or(""));
        minds.push(json!({"seat": o.author, "id": o.id, "room": o.channel, "task": f("task"), "next": f("next"), "waiting": f("waiting"), "at_ms": o.created_ms}));
        if minds.len() >= MINDS_MAX {
            break;
        }
    }

    // OPEN QUESTIONS: the decide asks addressed to the owner that nobody has answered, oldest first.
    let (questions, questions_more, capped) = match &owner {
        Some(o) => {
            let a = asks_for(s, o, None, StateFilter::Open, now_ms)?;
            let mut q: Vec<&crate::asks::AskRow> = a.waiting_on_me.iter().filter(|r| r.ask == "decide" && r.state == "open" && open(&r.room)).collect();
            q.sort_by_key(|r| r.created_ms);
            let more = q.len().saturating_sub(QUESTIONS_MAX);
            let rows: Vec<Value> = q.iter().take(QUESTIONS_MAX).map(|r| json!({"id": r.id, "room": r.room, "from": r.author, "head": r.card, "since_ms": r.created_ms})).collect();
            (rows, more, a.capped)
        }
        None => (vec![], 0, false),
    };

    Ok(json!({
        "schema": "world/1", "now_ms": now_ms, "now": oracle_proto::iso_ms(now_ms), "local": oracle_proto::local_clock_env(now_ms), "owner": owner,
        "north_star": {"canon": canon, "identities": identities, "more": identities_more, "rest": "ox identity --seat <seat>"},
        "now_section": {"steps": steps, "more": steps_more, "rest": "ox plan <room> <name>", "plans": plan_heads.len(), "queue": queue},
        "projects": {"items": projects, "more": projects_more, "rest": "ox head --all (oracle_task owner=*)"},
        "decisions": {"items": decisions, "more": decisions_more, "days": DECISIONS_DAYS, "rest": "ox recall --room memory decision (oracle_read room=memory tag=memory:decision)"},
        "seats": {"items": with_state, "more": seats_more, "rest": "ox state <seat>"},
        "rooms": {"items": rooms, "days": ROOMS_DAYS, "rest": "ox state <room>"},
        "minds": {"items": minds, "rest": "ox now --seat <seat>"},
        "questions": {"items": questions, "more": questions_more, "capped": capped, "rest": "ox asks --seat <owner> (oracle_inbox as the owner)"},
        "next": ["ox plan <room> <name>", "ox get <id>"],
    }))
}
