//! Board moves and progress as structured messages (2026-09-27). A card move is a status, not
//! speech: `ox card` posts ONE `room-task-event/1` message per invocation — a one-card move or a
//! `--batch` of many — whose `moves[]` each carry the full `room-task/1` revision the board applies.
//! `ox status` posts a `status/1`: live stage, n of total, when it started. Both are tagged
//! `schema:<name> intent:status ask:none`, so a reader can parse them without reading the prose.
use super::{urlencode, Kind, Ox};
use anyhow::{bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
pub use oracle_mcp::verbs::{annotation, archive, is_obs_id, reaction};

pub const TASK_EVENT: &str = "room-task-event/1";
pub const STATUS: &str = "status/1";
pub const STATUSES: [&str; 5] = ["queue", "in_progress", "review", "blocked", "done"];
const GROUPS: [&str; 4] = [
    "Serve customers",
    "Release safely",
    "Protect funds",
    "Run reliably",
];
/// One message holds at most this many moves: tags are capped at 32 per message and each move
/// carries a `task:<id>` tag so its next revision can find it.
pub const MAX_MOVES: usize = 25;

/// One card move. Anything left out is inherited from the card's head on the board.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Move {
    pub id: String,
    pub status: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub acceptance: Option<String>,
    pub owner: Option<String>,
    pub group: Option<String>,
    pub impact: Option<String>,
    pub delivery: Option<String>,
    pub priority: Option<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub depends: Option<Vec<String>>,
    /// The other rooms this card touches; it is counted on their boards too. Inherited when left out.
    pub rooms: Option<Vec<String>>,
    pub handoff: Option<String>,
    pub unblock: Option<String>,
    /// The message this card carries out (MESSAGE-UNIVERSE: a linked card moves its thread).
    #[serde(rename = "ref")]
    pub link: Option<String>,
}

fn ok_id(x: &str) -> bool {
    !x.is_empty()
        && x.len() <= 120
        && x.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The board's own acceptance test (axxis-room-contract.js parseTask): a revision it refused is
/// never a parent.
pub fn board_accepts(c: &Value) -> bool {
    c["group"].as_str().is_some_and(|g| GROUPS.contains(&g))
        && c["impact"]
            .as_str()
            .is_some_and(|i| ["critical", "high", "medium", "med"].contains(&i))
        && c["delivery"]
            .as_str()
            .is_some_and(|d| ["not_live", "preview", "live", "not_applicable"].contains(&d))
}

fn parse_cargo(o: &Value) -> Option<Value> {
    let mut c: Value = serde_json::from_str(o["cargo"].as_str()?).ok()?;
    if let Some(inner) = c["cargo"]
        .as_str()
        .and_then(|x| serde_json::from_str::<Value>(x).ok())
    {
        c = inner;
    }
    Some(c)
}

/// Every revision of `id` inside one stored message (room-task/1, and astra-task/1 in #astra): a legacy card message holds one,
/// a room-task-event/1 holds one per move.
pub fn revisions_in(o: &Value, id: &str) -> Vec<Value> {
    let Some(c) = parse_cargo(o) else {
        return vec![];
    };
    let all: Vec<Value> = match c["schema"].as_str() {
        Some("room-task/1") => vec![c],
        // #astra's board also reads its own schema (cards.rs parse_task): a head in either is a head.
        Some("astra-task/1") if o["channel"].as_str() == Some("astra") && c["channel"].as_str() == Some("astra") => vec![c],
        Some(TASK_EVENT) => c["moves"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| m["revision"].clone())
            .collect(),
        _ => vec![],
    };
    all.into_iter()
        .filter(|r| r["id"].as_str() == Some(id))
        .collect()
}

/// The card's heads on the board: the accepted revisions nothing newer replaces, newest last.
/// Returns (parent message ids, newest head revision).
pub fn heads(items: &[Value], id: &str) -> (Vec<String>, Option<Value>) {
    let mut rows: Vec<(i64, String, Value)> = vec![];
    let mut seen = std::collections::HashSet::new();
    for o in items {
        let oid = o["id"].as_str().unwrap_or_default().to_owned();
        if oid.is_empty() || !seen.insert(oid.clone()) {
            continue;
        }
        for r in revisions_in(o, id) {
            if board_accepts(&r) {
                rows.push((o["seq"].as_i64().unwrap_or(0), oid.clone(), r));
            }
        }
    }
    rows.sort_by_key(|r| r.0);
    // The board refuses a revision whose ancestor it refused or cannot find ("missing or invalid ancestor"), so a well-formed
    // revision on top of a refused one is not on the board and must never be parented on. Oldest first: a parent is judged before its child.
    let mut on_board = std::collections::HashSet::new();
    rows.retain(|(_, oid, r)| {
        let ok = r["parents"].as_array().into_iter().flatten().all(|p| p.as_str().is_some_and(|p| on_board.contains(p)));
        if ok {
            on_board.insert(oid.clone());
        }
        ok
    });
    let replaced: std::collections::HashSet<String> = rows
        .iter()
        .flat_map(|(_, _, r)| {
            r["parents"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str().map(str::to_owned))
        })
        .collect();
    let live: Vec<&(i64, String, Value)> = rows
        .iter()
        .filter(|(_, oid, _)| !replaced.contains(oid))
        .collect();
    (
        live.iter().map(|(_, oid, _)| oid.clone()).collect(),
        live.last().map(|(_, _, r)| r.clone()),
    )
}

fn text(v: &Option<String>, head: &Option<Value>, key: &str) -> Option<String> {
    v.clone()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            head.as_ref()
                .and_then(|h| h[key].as_str().map(str::to_owned))
        })
}

/// Validate one move against the board's contract and build its `moves[]` entry.
pub fn build_move(
    m: &Move,
    room: &str,
    me: &str,
    parents: Vec<String>,
    head: Option<Value>,
) -> Result<Value> {
    let deps = m
        .depends
        .clone()
        .or_else(|| {
            head.as_ref().map(|h| {
                h["dependsOn"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|d| d.as_str().map(str::to_owned))
                    .collect()
            })
        })
        .unwrap_or_default();
    if !ok_id(&m.id) || !deps.iter().all(|d| ok_id(d)) {
        bail!("{}: ids are lowercase letters, digits and dashes", m.id);
    }
    let status = m.status.as_str();
    if !STATUSES.contains(&status) {
        bail!(
            "{}: status is one of queue, in_progress, review, blocked, done",
            m.id
        );
    }
    let (Some(title), Some(summary), Some(acceptance)) = (
        text(&m.title, &head, "title"),
        text(&m.summary, &head, "summary"),
        text(&m.acceptance, &head, "acceptance"),
    ) else {
        bail!(
            "{}: a new card needs a title, --summary and --acceptance",
            m.id
        );
    };
    let group = text(&m.group, &head, "group").unwrap_or_else(|| "Run reliably".into());
    let impact = text(&m.impact, &head, "impact").unwrap_or_else(|| "high".into());
    let delivery = text(&m.delivery, &head, "delivery").unwrap_or_else(|| "not_live".into());
    let owner = text(&m.owner, &head, "owner").unwrap_or_else(|| me.to_string());
    if (status == "done" || delivery == "live") && m.evidence.is_empty() {
        bail!(
            "{}: done needs --evidence: the board refuses a completion with no proof",
            m.id
        );
    }
    let handoff = m
        .handoff
        .as_ref()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty());
    if (status == "review" || status == "done") && handoff.is_none() {
        bail!(
            "{}: {status} needs --handoff \"what changed · how to test · the gotcha\"",
            m.id
        );
    }
    let unblock = m
        .unblock
        .as_ref()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty());
    if status == "blocked" && unblock.is_none() {
        bail!(
            "{}: blocked needs --unblock <who can unblock it>, and --summary says why",
            m.id
        );
    }
    if handoff.as_ref().is_some_and(|h| h.len() > 1600)
        || unblock.as_ref().is_some_and(|u| u.len() > 80)
    {
        bail!(
            "{}: --handoff is at most 1600 characters and --unblock at most 80",
            m.id
        );
    }
    if !GROUPS.contains(&group.as_str()) {
        bail!("{}: --group must be one of: {}", m.id, GROUPS.join(" · "));
    }
    // "med" is an older spelling the board still reads; it may be inherited, never newly written.
    if !(["critical", "high", "medium"].contains(&impact.as_str())
        || (impact == "med" && m.impact.is_none()))
    {
        bail!("{}: --impact must be critical, high or medium", m.id);
    }
    if !["not_live", "preview", "live", "not_applicable"].contains(&delivery.as_str()) {
        bail!(
            "{}: --delivery must be not_live, preview, live or not_applicable",
            m.id
        );
    }
    let priority = text(&m.priority, &head, "priority");
    if let Some(p) = &priority {
        if !["P0", "P1", "P2", "P3"].contains(&p.as_str()) {
            bail!("{}: --priority must be P0, P1, P2 or P3", m.id);
        }
    }
    let mut rev = json!({"schema": "room-task/1", "channel": room, "id": m.id, "title": title,
        "group": group, "impact": impact, "owner": owner, "status": status, "delivery": delivery,
        "summary": summary, "acceptance": acceptance, "evidence": m.evidence, "dependsOn": deps,
        "parents": parents});
    if let Some(p) = priority {
        rev["priority"] = Value::String(p);
    }
    let rooms: Vec<String> = m.rooms.clone().unwrap_or_else(|| {
        head.as_ref()
            .and_then(|h| h["rooms"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(str::to_owned))
            .collect()
    });
    let channel_ok = |r: &str| {
        !r.is_empty()
            && r.len() <= 64
            && r.as_bytes()[0].is_ascii_lowercase()
            && r.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let distinct: std::collections::BTreeSet<&str> = rooms.iter().map(String::as_str).collect();
    if rooms.len() > 8 || distinct.len() != rooms.len() || rooms.iter().any(|r| !channel_ok(r) || r == room) {
        bail!("{}: --rooms names up to 8 other rooms, each once (not this card's own room)", m.id);
    }
    if !rooms.is_empty() {
        rev["rooms"] = json!(rooms);
    }
    if let Some(h) = &handoff {
        rev["handoff"] = Value::String(h.clone());
    }
    if let Some(u) = &unblock {
        rev["unblock"] = Value::String(u.clone());
    }
    if let Some(l) = &m.link {
        if !is_obs_id(l) {
            bail!(
                "{}: --ref is the message id this card carries out (OBS-…)",
                m.id
            );
        }
    }
    let from = head
        .as_ref()
        .and_then(|h| h["status"].as_str().map(str::to_owned));
    let mut mv = json!({"task": m.id, "title": rev["title"], "from": from, "to": status, "owner": rev["owner"],
        "refs": m.evidence, "reason": handoff.or(unblock), "revision": rev});
    if let Some(l) = &m.link {
        mv["link"] = Value::String(l.clone()); // becomes the message's ref: tag, not a cargo field
    }
    Ok(mv)
}

/// The one message for a set of moves: face, tags, cargo.
pub fn task_event(room: &str, moves: Vec<Value>) -> Result<(String, Vec<String>, Value)> {
    if moves.is_empty() || moves.len() > MAX_MOVES {
        bail!("one message carries 1 to {MAX_MOVES} card moves");
    }
    let mut counts = serde_json::Map::new();
    for s in STATUSES {
        let n = moves.iter().filter(|m| m["to"] == s).count();
        if n > 0 {
            counts.insert(s.into(), json!(n));
        }
    }
    let face = if moves.len() == 1 {
        let mut face_room = room.to_string();
        if let Some(c) = face_room.get_mut(0..1) {
            c.make_ascii_uppercase();
        }
        format!(
            "[{face_room} task:{}] {}: {}",
            moves[0]["task"].as_str().unwrap_or("?"),
            moves[0]["to"].as_str().unwrap_or("?"),
            moves[0]["title"].as_str().unwrap_or("")
        )
    } else {
        // board order (queue → done), not the map's alphabetical one
        let tally: Vec<String> = STATUSES
            .iter()
            .filter_map(|k| counts.get(*k).map(|v| format!("{v} {k}")))
            .collect();
        format!("{} card moves: {}", moves.len(), tally.join(", "))
    };
    let mut tags = vec![
        format!("schema:{TASK_EVENT}"),
        "intent:status".into(),
        "ask:none".into(),
    ];
    tags.extend(
        moves
            .iter()
            .map(|m| format!("task:{}", m["task"].as_str().unwrap_or(""))),
    );
    let mut links: Vec<String> = moves
        .iter()
        .filter_map(|m| m["link"].as_str().map(|l| format!("ref:{l}")))
        .collect();
    links.dedup();
    tags.extend(links);
    let moves: Vec<Value> = moves
        .into_iter()
        .map(|mut m| {
            if let Some(o) = m.as_object_mut() {
                o.remove("link");
            }
            m
        })
        .collect();
    Ok((
        face,
        tags,
        json!({"schema": TASK_EVENT, "channel": room, "counts": counts, "moves": moves}),
    ))
}

/// Everything already on the board about `id`: new-style (tagged) and legacy (face prefix) messages.
fn history(ox: &Ox, room: &str, id: &str) -> Result<Vec<Value>> {
    let mut face_room = room.to_string();
    if let Some(c) = face_room.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    let mut items = vec![];
    for q in [
        format!("tag={}", urlencode(&format!("task:{id}"))),
        format!("starts={}", urlencode(&format!("[{face_room} task:{id}]"))),
    ] {
        let v = ox.get(&format!(
            "/search?channel={}&{q}&limit=200",
            urlencode(room)
        ))?;
        items.extend(v["items"].as_array().cloned().unwrap_or_default());
    }
    Ok(items)
}

/// Does this ox write card moves as room-task-event/1? Not until the AXXIS board reads them: the
/// board still finds cards by face prefix and parses room-task/1 cargo, so an event would show as a
/// malformed update. `OX_CARD_EVENTS=1` turns it on; the default flips once the board ships.
pub fn card_events_enabled() -> bool {
    std::env::var("OX_CARD_EVENTS").is_ok_and(|v| v == "1")
}

/// Post the moves: one room-task-event/1 when `events`, else (legacy, one card only) the card's
/// room-task/1 revision exactly as the board reads it today. Returns the stored write receipt.
pub fn post_moves(ox: &Ox, room: &str, moves: &[Move]) -> Result<(String, Value)> {
    post_moves_as(ox, room, card_events_enabled(), moves)
}

pub fn post_moves_as(ox: &Ox, room: &str, events: bool, moves: &[Move]) -> Result<(String, Value)> {
    if !events && moves.len() != 1 {
        bail!("--batch waits for the board to read room-task-event/1 (OX_CARD_EVENTS=1 once it does): move one card per call");
    }
    let mut ids = std::collections::HashSet::new();
    if !moves.iter().all(|m| ids.insert(m.id.as_str())) {
        bail!("a card appears twice in one batch: move it once");
    }
    let mut built = vec![];
    for m in moves {
        if !ok_id(&m.id) {
            bail!("{}: ids are lowercase letters, digits and dashes", m.id);
        }
        let (parents, head) = heads(&history(ox, room, &m.id)?, &m.id);
        built.push(build_move(m, room, &ox.cfg.handle, parents, head)?);
    }
    let (face, mut tags, cargo) = task_event(room, built)?;
    if !events {
        let rev = cargo["moves"][0]["revision"].clone();
        tags[0] = "schema:room-task/1".into();
        let v = ox.write(Kind::Say, room, &face, Some(rev.to_string()), tags)?;
        return Ok((face, v));
    }
    let v = ox.write(Kind::Say, room, &face, Some(cargo.to_string()), tags)?;
    Ok((face, v))
}

/// A progress update, validated: n needs its denominator, and nothing counts past it.
#[derive(Debug, Default, Clone)]
pub struct Status {
    pub key: Option<String>,
    pub task: Option<String>,
    pub stage: String,
    pub state: String,
    pub n: Option<u64>,
    pub of: Option<u64>,
    pub eta_ms: Option<i64>,
    pub refs: Vec<String>,
}

pub fn status_cargo(s: &Status, me: &str, started_ms: i64) -> Result<(String, Value)> {
    let stage = s.stage.trim();
    if stage.is_empty() || stage.len() > 40 {
        bail!("--stage is one short word or phrase (1–40 characters)");
    }
    if !["running", "stalled", "failed", "done"].contains(&s.state.as_str()) {
        bail!("--state is running, stalled, failed or done");
    }
    if (s.stage == "done" || s.stage == "failed") && s.state != s.stage {
        bail!("a stage of {} ends the run: its --state is {} (or leave --state out)", s.stage, s.stage);
    }
    if s.n.is_some() && s.of.is_none() {
        bail!("--n needs --of: a count without its denominator says nothing");
    }
    if let (Some(n), Some(of)) = (s.n, s.of) {
        if n > of || of == 0 {
            bail!("--n {n} --of {of}: n is between 0 and of, and of is at least 1");
        }
    }
    if let Some(t) = &s.task {
        if !ok_id(t) {
            bail!("--task is a card id: lowercase letters, digits and dashes");
        }
    }
    let key = s
        .key
        .clone()
        .or_else(|| s.task.clone())
        .unwrap_or_else(|| format!("run:{me}"));
    if key.is_empty() || key.len() > 120 || key.chars().any(char::is_whitespace) {
        bail!("--key is 1–120 characters with no spaces");
    }
    Ok((
        key.clone(),
        json!({"schema": STATUS, "key": key, "task": s.task, "stage": stage, "state": s.state,
        "n": s.n, "of": s.of, "started_ms": started_ms, "eta_ms": s.eta_ms, "refs": s.refs}),
    ))
}

/// Post one status/1. started_ms carries over from the key's last status while that run is live.
pub fn post_status(
    ox: &Ox,
    room: &str,
    face: &str,
    s: &Status,
    now_ms: i64,
) -> Result<(String, Value)> {
    let (key, _) = status_cargo(s, &ox.cfg.handle, now_ms)?;
    let prior = ox.get(&format!(
        "/search?channel={}&tag={}&limit=20",
        urlencode(room),
        urlencode(&format!("status-key:{key}"))
    ))?;
    let started = prior["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["author"].as_str() == Some(ox.cfg.handle.as_str()))
        .max_by_key(|o| o["seq"].as_i64().unwrap_or(0))
        .and_then(parse_cargo)
        .filter(|c| {
            c["schema"] == STATUS && !matches!(c["state"].as_str(), Some("done") | Some("failed"))
        })
        .and_then(|c| c["started_ms"].as_i64())
        .unwrap_or(now_ms);
    let (_, cargo) = status_cargo(s, &ox.cfg.handle, started)?;
    let face = face.trim();
    if face.is_empty() {
        bail!("say what is happening in one line");
    }
    let tags = vec![
        format!("schema:{STATUS}"),
        "intent:status".into(),
        "ask:none".into(),
        format!("status-key:{key}"),
    ];
    let v = ox.write(Kind::Say, room, face, Some(cargo.to_string()), tags)?;
    Ok((face.to_string(), v))
}

/// The four marks (MESSAGE-UNIVERSE §3): 👁 seen · ✓ go · ♥ like · ✕ no, on a message or a card.
/// Written as the room's reaction form (say + reply:<id> + react:<kind>, PR #9) until Teo's readers take
/// kind annotate; `no` may carry a why. An agent's seen is its read mark (ox seen), not a reaction.
pub fn post_reaction(
    ox: &Ox,
    room: &str,
    target: &str,
    kind: &str,
    why: Option<&str>,
) -> Result<(String, Value)> {
    let (face, tags) = reaction(kind, target, why)?;
    let v = ox.write(Kind::Say, room, &face, None, tags)?;
    Ok((face, v))
}

/// A state update on a message is not a message (Blaze, 2026-09-27): intent, ask, items, who it is
/// for. Until oracle-core has its own state-update kind this writes the shape the room already folds —
/// a say tagged `annotates:<id>` + `set:k=v` / `item:K=v` + `confidence:x`, ambient (`intent:status`,
/// `ttl:0`) — so switching to the new kind is this one function.
pub fn post_annotation(
    ox: &Ox,
    room: &str,
    id: &str,
    sets: &[String],
    why: &str,
    confidence: Option<f64>,
    cargo: Option<String>,
) -> Result<(String, Value)> {
    let tags = annotation(id, sets, confidence)?;
    let face = why.trim();
    if face.is_empty() || face.len() > 1200 {
        bail!("--why says in one line what decided it (1–1200 characters)");
    }
    let v = ox.write(Kind::Say, room, face, cargo, tags)?;
    Ok((face.to_string(), v))
}

/// Tags a plain `ox say` must not carry: each is an update with its own command (MESSAGE-UNIVERSE).
#[cfg(test)]
pub fn say_refuses(tags: &[String]) -> Result<()> {
    say_refuses_on(tags, false)
}

pub fn say_refuses_on(tags: &[String], reply: bool) -> Result<()> {
    for t in tags {
        if t.starts_with("react:") {
            bail!("{t}: a reaction is not a message — ox react <room> <id> seen|go|like");
        }
        if let Some(item) = t.strip_prefix("item:") {
            item_tag(item, reply)?; // a raw item tag is checked like --item
        }
        if t.starts_with("annotates:") || t.starts_with("set:") {
            bail!(
                "{t}: a state update is not a message — ox annotate <room> <id> --set k=v --why …"
            );
        }
    }
    Ok(())
}

// The same kinds as the server's projection (maps: the session a decision needs, and the map's destination).
pub const ITEM_KINDS: [&str; 10] =
    ["requirement", "question", "ask", "decision", "complaint", "research", "prototype", "grill", "task", "destination"];

/// `ox say --item R1=<kind>[:text]`: an item is DECLARED by its kind — Oracle declares items only from a
/// kind tag. A state word here declared nothing (2026-09-27): it is refused and points to ox move.
/// Returns (tag, optional text).
#[cfg(test)]
pub fn item_decl(spec: &str) -> Result<(String, Option<String>)> {
    item_tag(spec, false)
}

/// On a reply, `R1=<state>` is a move of the parent's item (oracle-core reads a reply's or an
/// annotation's item state); on a root message only a kind declares one.
pub fn item_tag(spec: &str, reply: bool) -> Result<(String, Option<String>)> {
    let (k, rest) = spec.split_once('=').ok_or_else(|| {
        anyhow::anyhow!(
            "--item is R1=<kind>[:text], kind one of {}",
            ITEM_KINDS.join(" ")
        )
    })?;
    let (kind, text) = match rest.split_once(':') {
        Some((kd, t)) => (kd, Some(t.trim().to_string()).filter(|t| !t.is_empty())),
        None => (rest, None),
    };
    if k.is_empty()
        || k.len() > 24
        || !k
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("--item key is letters, digits, dashes (R1, Q2, …)");
    }
    if ITEM_STATES.contains(&kind) && reply {
        return Ok((format!("item:{k}={kind}"), None));
    }
    if ITEM_STATES.contains(&kind) {
        bail!("--item {k}={kind}: {kind} is a state (a move needs --reply <id>, or ox move) — declare the item with its kind ({}), move it with ox move <room> <id> {k}={kind}", ITEM_KINDS.join(" "));
    }
    if !ITEM_KINDS.contains(&kind) {
        bail!(
            "--item {k}={kind}: the kind is one of {}",
            ITEM_KINDS.join(" ")
        );
    }
    if text.as_ref().is_some_and(|t| t.len() > 240) {
        bail!("--item {k}: the text is at most 240 characters");
    }
    Ok((format!("item:{k}={kind}"), text))
}

pub const ITEM_STATES: [&str; 7] = [
    "open", "building", "built", "live", "accepted", "blocked", "dropped",
];

/// `ox move`: an item's state (`R2=built`), a shorthand for `ox annotate … --set item:R2=built`.
/// A decider's no to the whole message is `ox react … no --why` (MESSAGE-UNIVERSE §3).
pub fn move_sets(what: &str) -> Result<Vec<String>> {
    let Some((k, v)) = what.split_once('=') else {
        bail!("{what}: ox move moves an item (R2=built); to decline or drop the message, ox react <room> <id> no --why …");
    };
    let key_ok = !k.is_empty()
        && k.len() <= 24
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !key_ok || !ITEM_STATES.contains(&v) {
        bail!(
            "{what}: an item move is K=state, state one of {}",
            ITEM_STATES.join(" ")
        );
    }
    Ok(vec!["op=move".into(), format!("item:{k}={v}")])
}

// ── presence from reads (Blaze 2026-09-27 09:16): ox is the one tool; a read is presence ─────────

/// A program's read (--json, OX_PEEK=1) marks nothing read; an agent's read in text does.
pub fn peek(json: bool) -> bool {
    json || std::env::var("OX_PEEK").is_ok_and(|v| v == "1")
}

/// Move this seat's read mark in `room` to `seq` (forward only; the fold's own ack).
pub fn ack_to(ox: &Ox, room: &str, seq: i64) -> Result<()> {
    if seq > 0 {
        ox.post("/fold", &json!({"marks": {room: seq}}))?;
    }
    Ok(())
}

/// The room as far as the fold has it: what `ox context` just showed the agent.
pub fn ack_room(ox: &Ox, room: &str) -> Result<()> {
    let v = ox.get("/fold?limit=200")?;
    match v["next_marks"][room].as_i64() {
        Some(seq) => ack_to(ox, room, seq),
        None => Ok(()),
    }
}

/// The one line every ox call ends with when something addressed to this seat is unread:
/// `· 3 new for you in #hustler (2 asks) — ox inbox`. None when nothing is — no tokens spent.
pub fn nudge_line(fold: &Value, me: &str) -> Option<String> {
    let mut rooms: Vec<(String, usize)> = vec![];
    let mut asks = 0;
    for it in fold["items"].as_array().into_iter().flatten() {
        if it["for_me"].as_bool() != Some(true) || it["author"].as_str() == Some(me) {
            continue;
        }
        let tags: Vec<&str> = it["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str())
            .collect();
        // updates are not messages (until the server folds them away itself)
        if tags
            .iter()
            .any(|t| t.starts_with("annotates:") || t.starts_with("react:"))
        {
            continue;
        }
        if tags
            .iter()
            .any(|t| matches!(*t, "ask:reply" | "ask:act" | "intent:question"))
        {
            asks += 1;
        }
        let ch = it["channel"].as_str().unwrap_or("?").to_string();
        match rooms.iter_mut().find(|(c, _)| *c == ch) {
            Some(r) => r.1 += 1,
            None => rooms.push((ch, 1)),
        }
    }
    let total: usize = rooms.iter().map(|r| r.1).sum();
    if total == 0 {
        return None;
    }
    let asks = if asks > 0 {
        format!(" ({asks} ask{})", if asks == 1 { "" } else { "s" })
    } else {
        String::new()
    };
    Some(if rooms.len() == 1 {
        format!("· {total} new for you in #{}{asks} — ox inbox", rooms[0].0)
    } else {
        let list: Vec<String> = rooms.iter().map(|(c, n)| format!("#{c} {n}")).collect();
        format!(
            "· {total} new for you: {}{asks} — ox inbox",
            list.join(", ")
        )
    })
}

/// The footer's whole budget: the inbox summary and the open asks together, one attempt each; the asks read
/// gets what the fold left. Advisory text after a signed write never waits for a slow fold (Emmy;
/// 21 Oct: 131 s per `ox say` was the footer, not the write).
pub const FOOTER_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// The footer: the lines to print and what was unavailable, each named. Pure; `nudge` prints.
pub fn footer(ox: &Ox, budget: std::time::Duration) -> (Vec<String>, Vec<String>) {
    let start = std::time::Instant::now();
    let (mut lines, mut missing) = (Vec::new(), Vec::new());
    match ox.advisory_get("/fold?limit=50", budget) {
        Ok(v) => {
            if let Some(line) = nudge_line(&v, &ox.cfg.handle) {
                lines.push(line);
            }
        }
        Err(e) => missing.push(format!("inbox summary: {e}")),
    }
    // The asks this seat opened and hasn't ended (docs/ASKS-AND-EVENTS.md): they ride every footer until closed. Counts
    // only (Rocky, 1 Nov 2026: the whole list was 168 KB for rocky, 2.5 s over the tailnet, past this budget, so every
    // command said "footer unavailable"); `ox asks` keeps the full list. A server older than the count answer sends the
    // list whole, and it is counted here as before.
    match ox.advisory_get("/asks?state=open&count=1", budget.saturating_sub(start.elapsed())) {
        Ok(v) => {
            let line = oracle_mcp::my_asks_count_nudge(&v)
                .unwrap_or_else(|| oracle_mcp::my_asks_nudge(&oracle_mcp::my_asks(&v, v["now_ms"].as_i64().unwrap_or(0))));
            if let Some(line) = line {
                lines.push(line);
            }
        }
        Err(e) => missing.push(format!("open asks: {e}")),
    }
    (lines, missing)
}

/// Cross-process admission for advisory reads only. Fixed windows can admit two reads
/// across a boundary; explicit asks/inbox never use this throttle. Store no message text,
/// counts or credentials. Atomic creation prevents parallel CLI calls stampeding Oracle.
const FOOTER_WINDOW_MS: u64 = 20_000;
fn admit_footer(home: &std::path::Path, identity: &str, now_ms: u64) -> std::io::Result<bool> {
    use std::fs::{self, OpenOptions};
    let dir = home.join("footer-windows");
    fs::create_dir_all(&dir)?;
    let key = oracle_proto::sha_hex(identity.as_bytes());
    let window = now_ms / FOOTER_WINDOW_MS;
    let prefix = format!("{key}-");
    // Prune only this identity's completed windows. Never unlink a current claim.
    for entry in fs::read_dir(&dir)?.flatten() {
        let name = entry.file_name();
        if let Some(old) = name.to_str().and_then(|n| n.strip_prefix(&prefix)).and_then(|n| n.parse::<u64>().ok()) {
            if old < window.saturating_sub(1) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    match OpenOptions::new().write(true).create_new(true).open(dir.join(format!("{prefix}{window}"))) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn nudge(ox: &Ox) {
    nudge_at(ox, super::now_ms().max(0) as u64);
}

pub(super) fn nudge_at(ox: &Ox, now_ms: u64) {
    if std::env::var("OX_QUIET").is_ok_and(|v| v == "1") {
        return;
    }
    let identity = serde_json::to_string(&(&ox.cfg.url, &ox.cfg.handle, &ox.cfg.token)).expect("strings serialize");
    match admit_footer(&ox.home, &identity, now_ms) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("· footer refresh deferred (20s window); read current state: ox inbox --mine / ox asks");
            return;
        }
        Err(_) => {
            eprintln!("· footer unavailable (local refresh throttle); read current state: ox inbox --mine / ox asks");
            return;
        }
    }
    let (lines, missing) = footer(ox, FOOTER_BUDGET);
    for line in lines {
        // stderr: the agent sees it; a script reading stdout (`ox say … | awk`) still gets one
        // line per write (core-ci e2e, 406cb02).
        eprintln!("{line}");
    }
    if !missing.is_empty() {
        // said, never swallowed: a missing footer is not zero unread
        eprintln!("· footer unavailable ({}); unread and open asks not known: ox inbox --mine", missing.join("; "));
    }
}

/// A room's State, written without hand-built JSON: the given fields over the current head's, with the
/// parents filled from the heads (all of them, which also reconciles concurrent heads).
///
/// The eight project fields are always present. A seat's room (Blaze, 2026-09-28: "every agent should
/// have a room in oracle, that's their worldline and memory … their state in which you have your
/// bootstrap, tools, live projects and their responsibility in each") adds the seat fields, present only
/// once written: `kind` (mind | human | project), `boot` (who I am and how I wake), `tools` (what I may
/// call), `rooms` (each room I am in and my responsibility there), `rules` (what I earned).
// The State fields and revision builder live in oracle-mcp, shared by the CLI and both MCP doors (one code path).
pub use oracle_mcp::state_revision;

// ── declared presence (Blaze 09:18): "waiting for input → idle"; "Rocky takes q3" ─────────────────

/// A seat's own room: its handle without -core (#bobby, #wags, #rocky).
pub fn own_room(seat: &str) -> String {
    seat.strip_suffix("-core").unwrap_or(seat).to_string()
}

/// What a seat declares (Blaze 09:26): WORKING — on one task, checking the room itself as it goes
/// (ox's "N new for you" line), so rings OFF; WAITING — done, no next action, so rings ON. A seat that
/// declares nothing is rung (the safe failure). A status/1 keyed presence:<seat> in the seat's own room
/// (newest per key wins) until oracle-core's /presence holds it.
pub enum Mode<'a> {
    Working {
        what: &'a str,
        room: Option<&'a str>,
        card: Option<&'a str>,
    },
    Waiting {
        what: Option<&'a str>,
    },
}

pub fn presence_status(seat: &str, mode: Mode, now_ms: i64) -> (String, Vec<String>, Value) {
    let key = format!("presence:{seat}");
    let (face, presence) = match mode {
        Mode::Working { what, room, card } => (
            format!("working on {what} · rings off"),
            json!({"mode": "working", "state": "working", "what": what, "room": room, "card": card, "since_ms": now_ms, "rings": "off"}),
        ),
        Mode::Waiting { what } => {
            let what = what.map(str::trim).filter(|w| !w.is_empty());
            (
                match what {
                    Some(w) => format!("waiting for {w} · rings on"),
                    None => "waiting · rings on".to_string(),
                },
                json!({"mode": "waiting", "state": "idle", "what": what, "why": what, "since_ms": now_ms, "rings": "on"}),
            )
        }
    };
    let cargo = json!({"schema": STATUS, "key": key, "task": presence["card"], "stage": presence["mode"],
        "state": "running", "n": null, "of": null, "started_ms": now_ms, "eta_ms": null, "refs": [], "presence": presence});
    (
        face,
        vec![
            format!("schema:{STATUS}"),
            "intent:status".into(),
            "ask:none".into(),
            format!("status-key:{key}"),
        ],
        cargo,
    )
}

pub fn post_presence(ox: &Ox, mode: Mode, now_ms: i64) -> Result<(String, Value)> {
    let seat = ox.cfg.handle.clone();
    let (face, tags, cargo) = presence_status(&seat, mode, now_ms);
    let v = ox.write(
        Kind::Say,
        &own_room(&seat),
        &face,
        Some(cargo.to_string()),
        tags,
    )?;
    Ok((face, v))
}

#[cfg(test)]
mod footer_window_tests {
    use super::*;
    #[test]
    fn parallel_commands_share_one_refresh_and_next_window_recovers() {
        let dir = std::env::temp_dir().join(format!("ox-footer-window-{}-{}", std::process::id(), super::super::now_ms()));
        let results: Vec<_> = (0..16).map(|_| {
            let dir = dir.clone();
            std::thread::spawn(move || admit_footer(&dir, "server-seat-token", 100_000).unwrap())
        }).collect();
        assert_eq!(results.into_iter().map(|r| r.join().unwrap()).filter(|admitted| *admitted).count(), 1);
        assert!(!admit_footer(&dir, "server-seat-token", 119_999).unwrap());
        assert!(admit_footer(&dir, "server-seat-token", 120_000).unwrap());
        assert!(admit_footer(&dir, "another-seat", 120_000).unwrap());
        assert!(admit_footer(&dir, "rotated-token", 120_000).unwrap());
        assert!(admit_footer(&dir, "server-seat-token", 180_000).unwrap());
        assert_eq!(std::fs::read_dir(dir.join("footer-windows")).unwrap().count(), 3);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
