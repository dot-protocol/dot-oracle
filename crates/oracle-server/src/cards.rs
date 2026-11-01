//! The global card board: every room's cards, folded the way the AXXIS web board folds them (docs/rooms/, Blaze 2026-09-29: "agents
//! can't see what is pending on them"). A port of src/lib/project-board.ts + room-task-revisions.mjs + the task contract in AXXIS;
//! crates/oracle-server/tests/cards.rs holds it to a fixture generated from the web code itself (ops/cards_golden.mjs), so the
//! web page and `GET /cards` cannot disagree. Nothing is written; a card is whatever the signed messages say now.
//!
//! What a revision is: a `room-task/1` cargo (one card) or a `room-task-event/1` cargo (a batch of 1 to 25 moves, all or none),
//! on a message whose face starts `[Room task:<id>]` or that carries the schema tag. A native row (envelope 2) was verified by this
//! server; an imported row (envelope 1) counts only with its own `signed:ok` and `cargo-signed:ok` tags, its revision wrapped in an
//! evidence envelope. Revisions form a lineage by `parents` (message ids); a card's current state is its head; two heads are a
//! conflict and read `blocked`. Where the web board throws for the whole room (a face that disagrees with its cargo, a supersession
//! cycle), this reader refuses just that revision or card and says so in `invalid`.
use crate::store::{Observation, Result, Store};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub const GROUPS: [&str; 4] = ["Serve customers", "Release safely", "Protect funds", "Run reliably"];
pub const STATUSES: [&str; 5] = ["queue", "in_progress", "review", "blocked", "done"];
pub const OPEN: [&str; 4] = ["queue", "in_progress", "review", "blocked"];
const IMPACTS: [&str; 3] = ["critical", "high", "medium"];
const DELIVERIES: [&str; 4] = ["not_live", "preview", "live", "not_applicable"];
pub const TASK_EVENT: &str = "room-task-event/1";
const MAX_MOVES: usize = 25;
/// Candidate rows read per room and walk; a room with more says so in `capped`.
pub const ROOM_CAP: usize = 20_000;
const MALFORMED: &str = "Latest update is malformed; showing the last valid report pending correction";

fn jslen(s: &str) -> usize {
    s.encode_utf16().count()
}
pub(crate) fn valid_channel(c: &str) -> bool {
    let b = c.as_bytes();
    !b.is_empty() && b.len() <= 64 && b[0].is_ascii_lowercase() && b.iter().all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || *x == b'-')
}
fn slug(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// One accepted revision of a card.
#[derive(Debug, Clone)]
pub struct Rev {
    pub id: String,
    pub title: String,
    pub group: String,
    pub impact: String,
    pub owner: String,
    pub status: String,
    pub delivery: String,
    pub summary: String,
    pub acceptance: String,
    pub evidence: Vec<String>,
    pub depends_on: Vec<String>,
    pub parents: Vec<String>,
    pub reviewer: Option<String>,
    pub handoff: Option<String>,
    pub unblock: Option<String>,
    pub priority: Option<String>,
    pub superseded_by: Option<String>,
    pub event: String,
    pub author: String,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Refusal {
    pub event: String,
    pub task: String,
    pub reason: String,
    pub at_ms: i64,
}

fn text(v: &Value, key: &str, max: usize) -> std::result::Result<String, String> {
    match v.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() && jslen(s) <= max => Ok(s.clone()),
        _ => Err(format!("Invalid {key}")),
    }
}
fn list(v: &Value, key: &str, max: usize) -> std::result::Result<Vec<String>, String> {
    match v.get(key) {
        Some(Value::Array(a)) if a.len() <= max && a.iter().all(|x| matches!(x, Value::String(s) if !s.is_empty() && jslen(s) <= 600)) => {
            Ok(a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
        }
        _ => Err(format!("Invalid {key}")),
    }
}

/// The task contract (public/vendor/axxis-room-contract.js parseTask), rule for rule.
pub fn parse_task(v: &Value, channel: &str) -> std::result::Result<Rev, String> {
    let schema = v.get("schema").and_then(Value::as_str).unwrap_or("");
    if v.get("channel").and_then(Value::as_str) != Some(channel) || !valid_channel(channel) || !(schema == "room-task/1" || (channel == "astra" && schema == "astra-task/1")) {
        return Err("Invalid task schema".into());
    }
    let mut r = Rev {
        id: text(v, "id", 80)?, title: text(v, "title", 160)?, group: text(v, "group", 80)?, impact: text(v, "impact", 20)?, owner: text(v, "owner", 80)?,
        status: text(v, "status", 30)?, delivery: text(v, "delivery", 30)?, summary: text(v, "summary", 1600)?, acceptance: text(v, "acceptance", 1600)?,
        evidence: list(v, "evidence", 12)?, depends_on: list(v, "dependsOn", 12)?, parents: list(v, "parents", 12)?,
        reviewer: None, handoff: None, unblock: None, priority: None, superseded_by: None, event: String::new(), author: String::new(), at: 0,
    };
    // Historical signed agent events used the unambiguous abbreviation med.
    if r.impact == "med" {
        r.impact = "medium".into();
    }
    if v.get("reviewer").is_some() {
        r.reviewer = Some(text(v, "reviewer", 80)?);
    }
    if v.get("handoff").is_some() {
        r.handoff = Some(text(v, "handoff", 1600)?);
    }
    if v.get("unblock").is_some() {
        r.unblock = Some(text(v, "unblock", 80)?);
    }
    if let Some(p) = v.get("priority") {
        match p.as_str() {
            Some(s) if ["P0", "P1", "P2", "P3"].contains(&s) => r.priority = Some(s.to_owned()),
            _ => return Err("Invalid priority".into()),
        }
    }
    let distinct: HashSet<&String> = r.depends_on.iter().collect();
    if r.depends_on.iter().any(|d| !slug(d) || *d == r.id) || distinct.len() != r.depends_on.len() {
        return Err("Invalid prerequisites".into());
    }
    if let Some(s) = v.get("supersededBy") {
        match s.as_str() {
            Some(t) if slug(t) && t != r.id => r.superseded_by = Some(t.to_owned()),
            _ => return Err("Invalid supersession".into()),
        }
    }
    if !slug(&r.id) || !GROUPS.contains(&r.group.as_str()) || !STATUSES.contains(&r.status.as_str()) || !IMPACTS.contains(&r.impact.as_str()) || !DELIVERIES.contains(&r.delivery.as_str()) {
        return Err("Invalid task classification".into());
    }
    if (r.status == "done" || r.delivery == "live") && r.evidence.is_empty() {
        return Err("Completion requires evidence".into());
    }
    Ok(r)
}

/// `[Room task:<id>] …` → the id, when the room is this channel (taskId in the contract; the id class is case-insensitive there).
pub fn task_id(face: &str, channel: Option<&str>) -> Option<String> {
    let rest = face.strip_prefix('[')?;
    let end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).unwrap_or(rest.len());
    let room = &rest[..end];
    if room.is_empty() || room.len() > 64 || !room.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    let rest = rest[end..].strip_prefix(" task:")?;
    let rest = rest.trim_start_matches(char::is_whitespace);
    let idend = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).unwrap_or(rest.len());
    if idend == 0 || !rest[idend..].starts_with(']') {
        return None;
    }
    match channel {
        Some(c) if !room.eq_ignore_ascii_case(c) => None,
        _ => Some(rest[..idend].to_owned()),
    }
}

/// A `room-task-event/1` batch: every move's revision as its own room-task/1, all or none.
pub fn moves_of(cargo: &Value, channel: &str) -> std::result::Result<Vec<Rev>, String> {
    if cargo.get("schema").and_then(Value::as_str) != Some(TASK_EVENT) {
        return Err("not a room-task-event/1".into());
    }
    if let Some(c) = cargo.get("channel") {
        if c.as_str() != Some(channel) {
            return Err("batch channel mismatch".into());
        }
    }
    let moves = match cargo.get("moves") {
        Some(Value::Array(m)) if !m.is_empty() && m.len() <= MAX_MOVES => m,
        _ => return Err(format!("a batch carries 1 to {MAX_MOVES} moves")),
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (i, m) in moves.iter().enumerate() {
        let Some(rev) = m.get("revision").filter(|r| r.is_object()) else { return Err(format!("move {} has no revision", i + 1)) };
        let t = parse_task(rev, channel)?;
        if let Some(task) = m.get("task") {
            if task.as_str() != Some(t.id.as_str()) {
                return Err(format!("move {} names {} but revises {}", i + 1, task.as_str().unwrap_or("?"), t.id));
            }
        }
        if let Some(to) = m.get("to") {
            if to.as_str() != Some(t.status.as_str()) {
                return Err(format!("move {} says {} but its revision is {}", i + 1, to.as_str().unwrap_or("?"), t.status));
            }
        }
        if !seen.insert(t.id.clone()) {
            return Err(format!("card {} moves twice in one batch", t.id));
        }
        out.push(t);
    }
    Ok(out)
}

pub struct Accepted {
    pub revisions: Vec<Rev>,
    pub refused: Vec<Refusal>,
    pub skipped: bool,
}

/// The board's rule for one row (acceptObservation): a task message, a refused one, or nothing to do with cards.
pub fn accept(o: &Observation, channel: &str) -> Accepted {
    let none = |skipped| Accepted { revisions: vec![], refused: vec![], skipped };
    let native = o.envelope == 2;
    let has = |t: &str| o.tags.iter().any(|x| x == t);
    // A native row was verified when it was written; an imported one must carry its own proof.
    let signed = native || has("signed:ok");
    let cargo_signed = if native { o.cargo.is_some() } else { has("cargo-signed:ok") };
    if !signed || !cargo_signed {
        return none(true);
    }
    let face_id = task_id(&o.content, Some(channel));
    // Imported rows keep their revision inside an evidence envelope: {source, cargo: "<json>"}.
    let cargo: Option<Value> = if native {
        o.cargo.as_deref().and_then(|c| serde_json::from_str(c).ok())
    } else {
        o.cargo.as_deref().and_then(|c| serde_json::from_str::<Value>(c).ok()).and_then(|e| e.get("cargo").and_then(Value::as_str).and_then(|c| serde_json::from_str(c).ok()))
    };
    let author = if native { o.author.clone() } else { o.tags.iter().find_map(|t| t.strip_prefix("from:")).unwrap_or("unknown").to_owned() };
    let refusal = |task: String, reason: String| Refusal { event: o.id.clone(), task, reason, at_ms: o.created_ms };
    let stamp = |mut r: Rev| {
        r.event = o.id.clone();
        r.author = author.clone();
        r.at = o.created_ms;
        r
    };
    let is_event = cargo.as_ref().and_then(|c| c.get("schema")).and_then(Value::as_str) == Some(TASK_EVENT) || has(&format!("schema:{TASK_EVENT}"));
    if is_event {
        let empty = Value::Null;
        let c = cargo.as_ref().unwrap_or(&empty);
        let result = moves_of(c, channel).and_then(|t| match &face_id {
            Some(f) if t.len() != 1 || t[0].id != *f => Err("Task face and moves disagree".to_owned()),
            _ => Ok(t),
        });
        return match result {
            Ok(tasks) => Accepted { revisions: tasks.into_iter().map(stamp).collect(), refused: vec![], skipped: false },
            Err(e) => {
                let mut ids: Vec<String> = Vec::new();
                if let Some(Value::Array(ms)) = c.get("moves") {
                    for m in ms {
                        let id = m.get("task").and_then(Value::as_str).filter(|s| !s.is_empty()).or_else(|| m.get("revision").and_then(|r| r.get("id")).and_then(Value::as_str)).unwrap_or("");
                        if !id.is_empty() && !ids.iter().any(|x| x == id) {
                            ids.push(id.to_owned());
                        }
                    }
                }
                if ids.is_empty() {
                    ids.push(face_id.clone().unwrap_or_else(|| "unknown".into()));
                }
                Accepted { revisions: vec![], refused: ids.into_iter().map(|t| refusal(t, format!("Card-move batch refused: {e}; no move in it was applied"))).collect(), skipped: false }
            }
        };
    }
    let Some(face_id) = face_id else { return none(true) };
    let empty = Value::Null;
    match parse_task(cargo.as_ref().unwrap_or(&empty), channel) {
        Err(_) => Accepted { revisions: vec![], refused: vec![refusal(face_id, "Incomplete or invalid task fields; author must publish a complete corrected revision".into())], skipped: false },
        Ok(t) if t.id != face_id => Accepted { revisions: vec![], refused: vec![refusal(face_id, "Task face and cargo disagree; author must publish a corrected revision".into())], skipped: false },
        Ok(t) => Accepted { revisions: vec![stamp(t)], refused: vec![], skipped: false },
    }
}

/// A card as the board shows it now.
#[derive(Debug, Clone, Serialize)]
pub struct Card {
    pub room: String,
    pub id: String,
    pub title: String,
    /// What the head revision says.
    pub status: String,
    /// What the board makes of it: `blocked` while a prerequisite is open or missing, or the latest update was refused.
    pub effective_status: String,
    pub owner: String,
    pub unblock: Option<String>,
    pub handoff: Option<String>,
    pub evidence: Vec<String>,
    /// When it entered its current status.
    pub since_ms: i64,
    pub group: String,
    pub impact: String,
    pub priority: Option<String>,
    pub delivery: String,
    pub summary: String,
    pub acceptance: String,
    pub reviewer: Option<String>,
    pub depends_on: Vec<String>,
    pub blocking: Vec<String>,
    pub issues: Vec<String>,
    pub conflict: bool,
    pub heads: Vec<String>,
    pub superseded_by: Option<String>,
    /// The message that wrote the head revision, its author, and when.
    pub event: String,
    pub author: String,
    pub at_ms: i64,
}

#[derive(Debug, Default)]
pub struct RoomBoard {
    pub room: String,
    pub cards: Vec<Card>,
    pub superseded: Vec<Card>,
    pub invalid: Vec<Refusal>,
    pub resolved_invalid: Vec<Refusal>,
}

fn rkey(event: &str, id: &str) -> (String, String) {
    (event.to_owned(), id.to_owned())
}

/// Fold one room's revisions into its cards (readProjectBoard + foldTasks).
pub fn fold_room(room: &str, revisions: Vec<Rev>, mut refused: Vec<Refusal>) -> RoomBoard {
    // A malformed ancestor degrades its own card, not the room: remove invalid chains transitively before the strict fold.
    let mut usable: HashMap<(String, String), Rev> = revisions.iter().map(|r| (rkey(&r.event, &r.id), r.clone())).collect();
    let mut children: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
    let mut invalid: Vec<(String, String)> = Vec::new();
    for r in &revisions {
        for p in &r.parents {
            children.entry(rkey(p, &r.id)).or_default().push(rkey(&r.event, &r.id));
        }
        if r.parents.iter().any(|p| usable.get(&rkey(p, &r.id)).is_none_or(|parent| parent.at > r.at)) {
            invalid.push(rkey(&r.event, &r.id));
        }
    }
    let mut i = 0;
    while i < invalid.len() {
        if let Some(r) = usable.remove(&invalid[i]) {
            refused.push(Refusal { event: r.event.clone(), task: r.id.clone(), reason: "Revision refers to a missing or invalid ancestor; publish a correction from the last valid revision".into(), at_ms: r.at });
            if let Some(kids) = children.get(&invalid[i]) {
                invalid.extend(kids.iter().cloned());
            }
        }
        i += 1;
    }
    let revs: Vec<Rev> = revisions.into_iter().filter(|r| usable.contains_key(&rkey(&r.event, &r.id))).collect();

    // foldTasks
    let superseded: HashSet<(String, String)> = revs.iter().flat_map(|r| r.parents.iter().map(|p| rkey(p, &r.id))).collect();
    let mut ids: Vec<String> = Vec::new();
    for r in &revs {
        if !ids.contains(&r.id) {
            ids.push(r.id.clone());
        }
    }
    struct T {
        head: Rev,
        status: String,
        conflict: bool,
        heads: Vec<String>,
        history: Vec<Rev>,
        superseded_by: Option<String>,
    }
    let mut tasks: Vec<T> = Vec::new();
    for id in &ids {
        let mut history: Vec<Rev> = revs.iter().filter(|r| &r.id == id).cloned().collect();
        history.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.event.cmp(&a.event)));
        let heads: Vec<&Rev> = history.iter().filter(|r| !superseded.contains(&rkey(&r.event, &r.id))).collect();
        let Some(first) = heads.first().copied() else {
            refused.push(Refusal { event: history[0].event.clone(), task: id.clone(), reason: "Task lineage is cyclic; publish a correction".into(), at_ms: history[0].at });
            continue;
        };
        let conflict = heads.len() > 1;
        tasks.push(T { head: first.clone(), status: if conflict { "blocked".into() } else { first.status.clone() }, conflict, heads: heads.iter().map(|r| r.event.clone()).collect(), history: history.clone(), superseded_by: first.superseded_by.clone() });
    }
    let impact_rank = |i: &str| IMPACTS.iter().position(|x| *x == i).unwrap_or(9);
    tasks.sort_by(|a, b| impact_rank(&a.head.impact).cmp(&impact_rank(&b.head.impact)).then_with(|| a.head.title.to_lowercase().cmp(&b.head.title.to_lowercase())).then_with(|| a.head.id.cmp(&b.head.id)));
    let index: HashMap<String, usize> = tasks.iter().enumerate().map(|(i, t)| (t.head.id.clone(), i)).collect();
    // A supersession that points nowhere, or loops, hides nothing: the card stays and says why.
    let mut problems: HashMap<String, String> = HashMap::new();
    for t in &tasks {
        let mut seen = HashSet::new();
        let mut cur = t;
        while let Some(next) = &cur.superseded_by {
            if !seen.insert(cur.head.id.clone()) {
                problems.insert(t.head.id.clone(), "Supersession cycle".into());
                break;
            }
            match index.get(next) {
                Some(i) => cur = &tasks[*i],
                None => {
                    problems.insert(t.head.id.clone(), "Supersession target missing".into());
                    break;
                }
            }
        }
    }
    for t in tasks.iter_mut() {
        if problems.contains_key(&t.head.id) {
            t.superseded_by = None;
        }
    }
    let resolve = |id: &str| -> Option<usize> {
        let mut i = *index.get(id)?;
        let mut hops = 0;
        while let Some(next) = &tasks[i].superseded_by {
            i = *index.get(next)?;
            hops += 1;
            if hops > tasks.len() {
                return None;
            }
        }
        Some(i)
    };
    let mut memo: HashMap<String, bool> = HashMap::new();
    fn cyclic(id: &str, resolve: &dyn Fn(&str) -> Option<usize>, deps: &dyn Fn(usize) -> Vec<String>, name: &dyn Fn(usize) -> String, memo: &mut HashMap<String, bool>, visiting: &mut HashSet<String>) -> bool {
        if visiting.contains(id) {
            return true;
        }
        if let Some(v) = memo.get(id) {
            return *v;
        }
        visiting.insert(id.to_owned());
        let result = match resolve(id) {
            Some(i) => deps(i).iter().any(|d| {
                let next = resolve(d).map(&name).unwrap_or_else(|| d.clone());
                cyclic(&next, resolve, deps, name, memo, visiting)
            }),
            None => false,
        };
        visiting.remove(id);
        memo.insert(id.to_owned(), result);
        result
    }
    let deps_of = |i: usize| tasks[i].head.depends_on.clone();
    let name_of = |i: usize| tasks[i].head.id.clone();
    let mut board = RoomBoard { room: room.to_owned(), ..Default::default() };
    let mut cards: Vec<Card> = Vec::new();
    let mut hidden: Vec<Card> = Vec::new();
    for t in &tasks {
        let h = &t.head;
        let prerequisites: Vec<(String, String, bool)> = h.depends_on.iter().map(|d| match resolve(d) {
            Some(i) => (tasks[i].head.id.clone(), tasks[i].status.clone(), tasks[i].status == "done" && !tasks[i].conflict),
            None => (d.clone(), "missing".into(), false),
        }).collect();
        let blocking: Vec<String> = prerequisites.iter().filter(|p| !p.2).map(|p| p.0.clone()).collect();
        let mut issues: Vec<String> = Vec::new();
        if prerequisites.iter().any(|p| p.1 == "missing") {
            issues.push("Missing prerequisite".into());
        }
        if cyclic(&h.id, &resolve, &deps_of, &name_of, &mut memo, &mut HashSet::new()) {
            issues.push("Dependency cycle".into());
        }
        if let Some(p) = problems.get(&h.id) {
            issues.push(p.clone());
        }
        let effective = if t.status == "done" { "done".to_owned() } else if !issues.is_empty() || !blocking.is_empty() { "blocked".to_owned() } else { t.status.clone() };
        // since: the oldest revision of the run that ends at the head with the same status
        let mut since = h.at;
        for r in t.history.iter().skip_while(|r| r.event != h.event) {
            if r.status == t.status || r.event == h.event {
                since = r.at;
            } else {
                break;
            }
        }
        let card = Card {
            room: room.to_owned(), id: h.id.clone(), title: h.title.clone(), status: t.status.clone(), effective_status: effective, owner: h.owner.clone(), unblock: h.unblock.clone(), handoff: h.handoff.clone(),
            evidence: h.evidence.clone(), since_ms: since, group: h.group.clone(), impact: h.impact.clone(), priority: h.priority.clone(), delivery: h.delivery.clone(), summary: h.summary.clone(),
            acceptance: h.acceptance.clone(), reviewer: h.reviewer.clone(), depends_on: h.depends_on.clone(), blocking, issues, conflict: t.conflict, heads: t.heads.clone(),
            superseded_by: t.superseded_by.clone(), event: h.event.clone(), author: h.author.clone(), at_ms: h.at,
        };
        if t.superseded_by.is_some() { hidden.push(card) } else { cards.push(card) }
    }
    // An update the board refused, newer than the card's head, blocks the card until the author corrects it.
    let by_id: HashMap<&str, i64> = cards.iter().chain(hidden.iter()).map(|c| (c.id.as_str(), c.at_ms)).collect();
    let (unresolved, resolved): (Vec<Refusal>, Vec<Refusal>) = refused.into_iter().partition(|u| by_id.get(u.task.as_str()).is_none_or(|at| u.at_ms >= *at));
    for c in cards.iter_mut() {
        if unresolved.iter().any(|u| u.task == c.id && u.at_ms >= c.at_ms) {
            c.effective_status = "blocked".into();
            c.issues.push(MALFORMED.into());
        }
    }
    board.cards = cards;
    board.superseded = hidden;
    board.invalid = unresolved;
    board.resolved_invalid = resolved;
    board
}

/// Every row of one room that could be a card revision, oldest first: the schema tags, and the `[Room task:` face.
fn candidates(s: &Store, room: &str, after_seq: i64) -> Result<(Vec<Observation>, bool)> {
    let mut seen: HashMap<i64, Observation> = HashMap::new();
    let mut capped = false;
    let prefix = format!("[{}{} task:", room[..1].to_uppercase(), &room[1..]);
    for (tag, starts) in [(Some("schema:room-task/1"), None), (Some(&*format!("schema:{TASK_EVENT}")), None), (None, Some(prefix.as_str()))] {
        let mut before: Option<i64> = None;
        loop {
            let page = s.recent_filtered_after(Some(room), 500, before, after_seq, tag, starts)?;
            let n = page.len();
            before = page.last().map(|o| o.seq);
            for o in page {
                seen.entry(o.seq).or_insert(o);
            }
            if n < 500 {
                break;
            }
            if seen.len() >= ROOM_CAP {
                capped = true;
                break;
            }
        }
    }
    let mut rows: Vec<Observation> = seen.into_values().collect();
    rows.sort_by_key(|o| o.seq);
    Ok((rows, capped))
}

/// One room's board, read from the store.
pub fn read_room(s: &Store, room: &str) -> Result<(RoomBoard, bool)> {
    let (rows, capped) = candidates(s, room, 0)?;
    Ok((fold_rows(room, &rows), capped))
}

fn fold_rows(room: &str, rows: &[Observation]) -> RoomBoard {
    let (mut revisions, mut refused) = (Vec::new(), Vec::new());
    let mut seen = HashSet::new();
    for o in rows.iter().filter(|o| o.kind == "say" && o.channel == room && seen.insert(o.id.clone())) {
        let a = accept(o, room);
        revisions.extend(a.revisions);
        refused.extend(a.refused);
    }
    fold_room(room, revisions, refused)
}

/// What the reader already knows of each room: its candidate rows up to `seen_seq` and the board folded from them.
/// A room whose newest message is still `seen_seq` costs one indexed lookup; one that has grown is scanned from `seen_seq`
/// on, not from the start. A scan of the whole ledger is the first request after a start, and never again.
#[derive(Default)]
pub struct BoardCache(std::sync::Mutex<HashMap<String, Known>>);

struct Known {
    seen_seq: i64,
    rows: Vec<Observation>,
    capped: bool,
    board: std::sync::Arc<(RoomBoard, bool)>,
}

impl BoardCache {
    /// Drop what is kept for a room, so the next read folds it from the log again: after a room is moved or deleted, the rows a
    /// cached board holds may no longer be there.
    pub fn forget(&self, room: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(room);
    }

    fn room(&self, s: &Store, room: &str) -> Result<std::sync::Arc<(RoomBoard, bool)>> {
        // The newest seq is read before scanning: a row that lands during the scan is seen by the next one, and `seq` dedupes it.
        let newest = s.recent(Some(room), 1, None)?.first().map_or(0, |o| o.seq);
        let known = self.0.lock().unwrap_or_else(|e| e.into_inner()).get(room).map(|k| (k.seen_seq, k.board.clone()));
        if let Some((seen, board)) = &known {
            if *seen == newest {
                return Ok(board.clone());
            }
        }
        let after = known.as_ref().map_or(0, |k| k.0);
        let (fresh, capped_now) = candidates(s, room, after)?;
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows = guard.remove(room).map(|k| (k.rows, k.capped)).unwrap_or_default();
        let capped = rows.1 || capped_now;
        let have: HashSet<i64> = rows.0.iter().map(|o| o.seq).collect();
        rows.0.extend(fresh.into_iter().filter(|o| !have.contains(&o.seq)));
        rows.0.sort_by_key(|o| o.seq);
        let board = std::sync::Arc::new((fold_rows(room, &rows.0), capped));
        guard.insert(room.to_string(), Known { seen_seq: newest, rows: rows.0, capped, board: board.clone() });
        Ok(board)
    }
}

#[derive(Debug, Default, Clone)]
pub struct CardFilter {
    pub owner: Option<String>,
    pub room: Option<String>,
    /// Declared statuses; `open` stands for queue, in_progress, review and blocked.
    pub status: Option<Vec<String>>,
    /// Cards in review or blocked whose `unblock` names this seat (`dinesh: what`).
    pub waiting_on: Option<String>,
    pub limit: usize,
}

/// Does an `unblock` line ("dinesh: push the fix; rocky: merge when green") name the seat?
pub fn names_seat(unblock: &str, seat: &str) -> bool {
    let seat = seat.to_lowercase();
    unblock.to_lowercase().split([';', ',']).any(|clause| {
        let c = clause.trim();
        let at = c.starts_with('@');
        let c = c.trim_start_matches('@');
        match c.strip_prefix(seat.as_str()) {
            Some(rest) => rest.starts_with(':') || (at && (rest.is_empty() || rest.starts_with(char::is_whitespace))),
            None => false,
        }
    })
}

#[derive(Debug, Serialize)]
pub struct RoomCount {
    pub room: String,
    pub cards: usize,
    pub invalid: usize,
    pub capped: bool,
}

#[derive(Debug, Serialize)]
pub struct Cards {
    pub cards: Vec<Card>,
    pub rooms: Vec<RoomCount>,
    /// Revisions the board refused and nobody has corrected yet.
    pub invalid: Vec<InvalidRow>,
}

#[derive(Debug, Serialize)]
pub struct InvalidRow {
    pub room: String,
    pub event: String,
    pub task: String,
    pub reason: String,
    pub at_ms: i64,
}

/// The cards of every room (or one), filtered. Rooms with no cards are left out of `rooms`.
pub fn cards_for(s: &Store, f: &CardFilter) -> Result<Cards> {
    cards_cached(s, f, &BoardCache::default())
}

/// `cards_for` reading through a cache the server keeps between requests.
pub fn cards_cached(s: &Store, f: &CardFilter, cache: &BoardCache) -> Result<Cards> {
    let rooms: Vec<String> = match &f.room {
        Some(r) if valid_channel(r) => vec![r.clone()],
        Some(_) => vec![],
        None => s.channels()?.into_iter().map(|c| c.0).filter(|c| valid_channel(c)).collect(),
    };
    let (mut cards, mut counts, mut invalid) = (Vec::new(), Vec::new(), Vec::new());
    for room in rooms {
        let known = cache.room(s, &room)?;
        let (b, capped) = (&known.0, known.1);
        if b.cards.is_empty() && b.superseded.is_empty() && b.invalid.is_empty() {
            continue;
        }
        counts.push(RoomCount { room: room.clone(), cards: b.cards.len(), invalid: b.invalid.len(), capped });
        invalid.extend(b.invalid.iter().map(|u| InvalidRow { room: room.clone(), event: u.event.clone(), task: u.task.clone(), reason: u.reason.clone(), at_ms: u.at_ms }));
        cards.extend(b.cards.iter().cloned());
    }
    let want: Option<Vec<&str>> = f.status.as_ref().map(|v| v.iter().flat_map(|x| if x == "open" { OPEN.to_vec() } else { vec![x.as_str()] }).collect());
    cards.retain(|c| {
        f.owner.as_ref().is_none_or(|o| &c.owner == o)
            && want.as_ref().is_none_or(|w| w.contains(&c.status.as_str()))
            && f.waiting_on.as_ref().is_none_or(|seat| matches!(c.status.as_str(), "review" | "blocked") && c.unblock.as_deref().is_some_and(|u| names_seat(u, seat)))
    });
    // Waiting longest first is what someone with a queue wants; ties by room and id so the order is stable.
    cards.sort_by(|a, b| a.since_ms.cmp(&b.since_ms).then_with(|| a.room.cmp(&b.room)).then_with(|| a.id.cmp(&b.id)));
    let limit = if f.limit == 0 { 500 } else { f.limit.min(2000) };
    cards.truncate(limit);
    Ok(Cards { cards, rooms: counts, invalid })
}
