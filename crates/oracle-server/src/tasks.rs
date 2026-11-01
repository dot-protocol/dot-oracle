//! Tasks, derived from a room's messages (docs/ASKS-AND-EVENTS.md, "Tasks"). Shadow: read-only, beside the cards.
//!
//! A task is keyed by its `subject:`. Each `ask:act|review|decide` on it is an obligation with its own holder and
//! state; evidence (`intent:result|receipt` with `rev:`) moves the subject's current revision; `verdict:` binds the
//! revision it names; `ci:` and `merged:` count only from trusted issuers. Nothing here is written: every field
//! names the message ids that set it.
//!
//! Not yet folded (stated, so a reader does not assume them): revocation by a decider's ✕, and room-declared
//! issuers (the trusted issuers are [`TRUSTED`] for every room until Layer 2 config lands).
use crate::alias::{same, Aliases};
use crate::store::{Observation, Result, Store, StoreError};
use serde::Serialize;
use std::collections::BTreeMap;

/// Receipts that count only from these seats: (tag prefix, issuer).
pub const TRUSTED: &[(&str, &str)] = &[("ci", "core-ci"), ("merged", "github")];

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Check {
    pub name: String,
    pub value: String,
    pub by: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Verdict {
    pub by: String,
    pub verdict: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Revision {
    pub rev: String,
    /// The evidence messages posted at this revision.
    pub evidence: Vec<String>,
    /// Checks from trusted issuers.
    pub checks: Vec<Check>,
    /// The same words from anyone else: shown, never counted.
    pub claims: Vec<Check>,
    pub verdicts: Vec<Verdict>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Obligation {
    /// The ask's message id.
    pub id: String,
    /// act | review | decide (or reply/decide for a routed question/proposal).
    pub kind: String,
    pub asker: String,
    /// One obligation per holder: an ask to two seats is two obligations, each answered on its own.
    pub holder: String,
    /// open | owned | answered | changes | done | cancelled
    pub state: String,
    /// The revision the state is about (answered: the evidence's rev; a verdict: the current rev).
    pub rev: Option<String>,
    /// The message ids that set the state.
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub subject: String,
    pub room: String,
    /// The first line of the first message on the subject, whole.
    pub title: String,
    pub current_rev: Option<String>,
    /// open | done | cancelled | unrouted
    pub state: String,
    pub summary: String,
    pub obligations: Vec<Obligation>,
    pub revisions: Vec<Revision>,
    /// Proposals and asks nobody was asked to answer: they ring nobody.
    pub unrouted: Vec<String>,
    /// Every message on the subject, oldest first.
    pub messages: Vec<String>,
    pub last_seq: i64,
    /// What was recorded but moved nothing, and why (a link cycle, evidence without authority, a revision that would go back).
    pub notes: Vec<String>,
}

fn tag<'a>(o: &'a Observation, prefix: &str) -> Option<&'a str> {
    o.tags.iter().find_map(|t| t.strip_prefix(prefix))
}

fn has(o: &Observation, t: &str) -> bool {
    o.tags.iter().any(|x| x == t)
}

fn points_at(o: &Observation) -> Vec<&str> {
    o.tags.iter().filter_map(|t| ["re:", "reply:", "in_reply_to:", "closes:", "annotates:"].iter().find_map(|p| t.strip_prefix(p))).collect()
}

/// The names an @mention may address (None: any @word, for pure fixtures).
type Seats<'a> = Option<&'a std::collections::BTreeSet<String>>;

/// Whom a message addresses: its `to:` list, then its @mentions of real seats, each once.
fn addressed(o: &Observation, seats: Seats) -> Vec<String> {
    let mut out: Vec<String> = o.tags.iter().filter_map(|t| t.strip_prefix("to:")).flat_map(|v| v.split(',')).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()).collect();
    for m in crate::store::mentions(&o.content).into_iter().filter_map(|t| t.strip_prefix("mention:").map(str::to_owned)) {
        let real = seats.is_none_or(|set| set.contains(&m));
        if real && m != o.author && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

fn obligation_kind(o: &Observation) -> Option<&'static str> {
    match tag(o, "ask:") {
        Some("act") => Some("act"),
        Some("review") => Some("review"),
        Some("decide") => Some("decide"),
        _ => None,
    }
}

/// An unaddressed thing that asks for an answer: a proposal or question, or an obligation-kind ask, that is not a reply.
fn routable_kind(o: &Observation) -> Option<&'static str> {
    obligation_kind(o).or(if has(o, "intent:proposal") { Some("decide") } else if has(o, "intent:question") { Some("reply") } else { None })
}

/// Evidence for a revision: a result or receipt at a `rev:`, or an ask that names the revision it asks about. Whether it
/// moves the current revision is [`fold`]'s judgement (the implementer's, never backwards, `prev:` past an approval).
fn is_evidence(o: &Observation) -> bool {
    tag(o, "rev:").is_some() && (has(o, "intent:result") || has(o, "intent:receipt") || obligation_kind(o).is_some()) && tag(o, "verdict:").is_none()
        && !TRUSTED.iter().any(|(p, _)| tag(o, &format!("{p}:")).is_some())
}

struct Raw<'a> {
    room: String,
    title: String,
    msgs: Vec<&'a Observation>,
}

/// [`derive`] where an @mention addresses someone only if it names a real seat or an alias of one.
pub fn derive_with_seats(msgs: &[Observation], deciders: &[String], aliases: &Aliases, seats: &std::collections::BTreeSet<String>) -> Vec<Task> {
    derive_in(msgs, deciders, aliases, Some(seats))
}

/// Fold a room's messages (any order; folded by seq) into its tasks. `deciders` are the room's roots.
pub fn derive(msgs: &[Observation], deciders: &[String], aliases: &Aliases) -> Vec<Task> {
    derive_in(msgs, deciders, aliases, None)
}

fn derive_in(msgs: &[Observation], deciders: &[String], aliases: &Aliases, seats: Seats) -> Vec<Task> {
    let mut sorted: Vec<&Observation> = msgs.iter().collect();
    sorted.sort_by_key(|o| o.seq);
    let mut subject_of: BTreeMap<&str, String> = BTreeMap::new();
    let mut parent_of: BTreeMap<&str, &str> = BTreeMap::new();
    let mut raw: BTreeMap<String, Raw> = BTreeMap::new();
    let mut cycles: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for o in &sorted {
        // Speech only (a State head's `subject:` is another namespace's key); annotations, because they route and reassign.
        if o.kind == "state" || (!crate::projection::is_speech(&o.tags) && tag(o, "annotates:").is_none()) {
            continue;
        }
        let parents = points_at(o);
        if let Some(p) = parents.first() {
            parent_of.insert(&o.id, p);
        }
        let subject = tag(o, "subject:").map(str::to_owned)
            .or_else(|| parents.iter().find_map(|p| subject_of.get(p).cloned()))
            .or_else(|| (obligation_kind(o).is_some() || (routable_kind(o).is_some() && parents.is_empty())).then(|| {
                // An ask with no subject is its own thread's task: the topmost message its links reach. Links can loop
                // (A points at B, B at A): a message seen twice stops the walk, and the ask is its own root, noted.
                let mut root: &str = &o.id;
                let mut seen: std::collections::BTreeSet<&str> = [root].into();
                while let Some(p) = parent_of.get(root).copied() {
                    if !seen.insert(p) {
                        cycles.entry(format!("thread:{}", o.id)).or_default().push(format!("link cycle at {p}: {} is its own thread's root", o.id));
                        root = &o.id;
                        break;
                    }
                    root = p;
                }
                format!("thread:{root}")
            }));
        let Some(subject) = subject else { continue };
        subject_of.insert(&o.id, subject.clone());
        raw.entry(subject).or_insert_with(|| Raw { room: o.channel.clone(), title: o.content.lines().next().unwrap_or("").to_owned(), msgs: vec![] }).msgs.push(o);
    }
    // A subject with nothing asked, nothing to route and no revision is talk, not a task.
    raw.into_iter()
        .map(|(subject, r)| {
            let mut t = fold(subject, r, deciders, aliases, seats);
            t.notes.extend(cycles.remove(&t.subject).unwrap_or_default());
            t
        })
        .filter(|t| !t.obligations.is_empty() || !t.unrouted.is_empty() || !t.revisions.is_empty()).collect()
}

fn fold(subject: String, r: Raw, deciders: &[String], aliases: &Aliases, seats: Seats) -> Task {
    let is_decider = |who: &str| deciders.iter().any(|d| same(aliases, d, who));
    let mut revisions: Vec<Revision> = vec![];
    let rev_at = |revisions: &mut Vec<Revision>, rev: &str| -> usize {
        match revisions.iter().position(|x| x.rev == rev) {
            Some(i) => i,
            None => {
                revisions.push(Revision { rev: rev.to_owned(), ..Default::default() });
                revisions.len() - 1
            }
        }
    };
    let canon = |who: &str| crate::alias::canon(aliases, who).to_owned();
    let mut notes: Vec<String> = vec![];
    // Who may move the subject's revision, seeded by the first message that starts work on it: an act ask (its holders),
    // a review ask (its asker) or evidence (its author). Later asks add authority only when the seed's author or a room
    // decider writes them, so nobody grants themselves authority by asking for a review (Emmy's mallory case).
    let mut implementers: std::collections::BTreeSet<String> = Default::default();
    let mut seed_author: Option<String> = None;
    let mut current_rev: Option<String> = None;
    let mut been_current: std::collections::BTreeSet<String> = Default::default();
    let mut obligations: Vec<Obligation> = vec![];
    let mut unrouted: Vec<(String, &'static str, String)> = vec![]; // (id, kind, asker)
    let per_holder = |id: &str, kind: &str, asker: &str, holders: &[String], evidence: Vec<String>| -> Vec<Obligation> {
        holders.iter().map(|h| Obligation { id: id.to_owned(), kind: kind.to_owned(), asker: asker.to_owned(), holder: h.clone(),
            state: "open".into(), rev: None, evidence: evidence.clone() }).collect()
    };
    for o in &r.msgs {
        let kind = obligation_kind(o).filter(|_| !addressed(o, seats).is_empty());
        let starts = kind.is_some() || is_evidence(o);
        let authorised = starts && match &seed_author {
            None => {
                seed_author = Some(canon(&o.author));
                true
            }
            Some(seed) => *seed == canon(&o.author) || is_decider(&o.author),
        };
        match kind {
            Some("act") if authorised => implementers.extend(addressed(o, seats).iter().map(|h| canon(h))),
            _ => {}
        }
        if authorised && (kind == Some("review") || (kind.is_none() && implementers.is_empty())) {
            implementers.insert(canon(&o.author));
        }
        // Revisions: evidence, checks and verdicts, each on the revision it names.
        if let Some(rev) = tag(o, "rev:") {
            let i = rev_at(&mut revisions, rev);
            if is_evidence(o) {
                revisions[i].evidence.push(o.id.clone());
                // After the first revision every move names its predecessor (prev:<current>): arrival order is not causality,
                // so evidence without prev: is recorded on its revision and moves nothing.
                let prev = tag(o, "prev:");
                let why_not = if !implementers.contains(&canon(&o.author)) {
                    Some(format!("{} at rev {rev}: {} is not an implementer of this subject", o.id, o.author))
                } else if current_rev.as_deref() == Some(rev) {
                    None
                } else if been_current.contains(rev) {
                    Some(format!("{} at rev {rev}: that revision was already superseded; a revision never comes back", o.id))
                } else if prev.is_some_and(|p| current_rev.as_deref() != Some(p)) {
                    Some(format!("{} at rev {rev}: names prev:{} but the current revision is {}", o.id, prev.unwrap_or(""), current_rev.as_deref().unwrap_or("none")))
                } else if current_rev.is_some() && prev.is_none() {
                    Some(format!("{} at rev {rev}: no prev:; after the first revision a new one names its predecessor ({})", o.id, current_rev.as_deref().unwrap_or("")))
                } else {
                    None
                };
                match why_not {
                    Some(n) => notes.push(n),
                    None => {
                        if let Some(old) = current_rev.replace(rev.to_owned()) {
                            if old != rev {
                                been_current.insert(old);
                            }
                        }
                    }
                }
            }
            for (prefix, issuer) in TRUSTED {
                if let Some(v) = tag(o, &format!("{prefix}:")) {
                    let c = Check { name: (*prefix).to_owned(), value: v.to_owned(), by: o.author.clone(), id: o.id.clone() };
                    if same(aliases, &o.author, issuer) { revisions[i].checks.push(c) } else { revisions[i].claims.push(c) }
                }
            }
            if let Some(v) = tag(o, "verdict:").filter(|v| matches!(*v, "approve" | "changes")) {
                revisions[i].verdicts.push(Verdict { by: o.author.clone(), verdict: v.to_owned(), id: o.id.clone() });
            }
        }
        // Obligations: an addressed ask, one per holder; unrouted: one addressed to nobody, until its asker or a decider routes it.
        let to = addressed(o, seats);
        if let Some(kind) = obligation_kind(o).filter(|_| !to.is_empty()) {
            obligations.extend(per_holder(&o.id, kind, &o.author, &to, vec![]));
        } else if let Some(kind) = routable_kind(o).filter(|_| to.is_empty() && points_at(o).is_empty()) {
            unrouted.push((o.id.clone(), kind, o.author.clone()));
        }
        // Routing and reassignment: the asker's or a decider's annotation with set:to= on an ask replaces its holders.
        if let (Some(target), Some(holders)) = (tag(o, "annotates:"), tag(o, "set:to=")) {
            if !has(o, "set:by=model") {
                let holders: Vec<String> = holders.split(',').map(|h| h.trim().to_owned()).filter(|h| !h.is_empty()).collect();
                if let Some(first) = obligations.iter().find(|b| b.id == target).cloned() {
                    if same(aliases, &first.asker, &o.author) || is_decider(&o.author) {
                        obligations.retain(|b| b.id != target);
                        obligations.extend(per_holder(&first.id, &first.kind, &first.asker, &holders, vec![o.id.clone()]));
                    }
                } else if let Some(i) = unrouted.iter().position(|u| u.0 == target) {
                    if same(aliases, &unrouted[i].2, &o.author) || is_decider(&o.author) {
                        let (id, kind, asker) = unrouted.remove(i);
                        obligations.extend(per_holder(&id, kind, &asker, &holders, vec![o.id.clone()]));
                    }
                }
            }
        }
    }
    // Each obligation's state, from what its own holder said, judged at the subject's current revision.
    for ob in &mut obligations {
        let holds = |who: &str| same(aliases, &ob.holder, who);
        let mut state = "open";
        let mut rev = None;
        let mut by: Vec<String> = vec![];
        for o in r.msgs.iter().filter(|o| points_at(o).contains(&ob.id.as_str())) {
            if has(o, "intent:commit") && holds(&o.author) && state == "open" {
                state = "owned";
                by = vec![o.id.clone()];
            }
            if (has(o, "intent:result") || has(o, "intent:receipt")) && holds(&o.author) && matches!(state, "open" | "owned" | "answered") {
                state = "answered";
                rev = tag(o, "rev:").map(str::to_owned);
                by = vec![o.id.clone()];
            }
        }
        // A review is settled by its holder's newest verdict on the current revision. With no current revision a verdict
        // stays recorded on the revision it names and settles nothing: a reviewer cannot establish the revision by approving it.
        if ob.kind == "review" {
            if let Some(cur) = &current_rev {
                if let Some(v) = revisions.iter().filter(|x| &x.rev == cur).flat_map(|x| x.verdicts.iter()).rfind(|v| holds(&v.by)) {
                    state = if v.verdict == "changes" { "changes" } else { "done" };
                    rev = Some(cur.clone());
                    by = vec![v.id.clone()];
                }
            }
        }
        // Closing is the asker's or a decider's: closes:<ask>, done:cancelled to cancel; it closes every holder's part.
        if let Some(c) = r.msgs.iter().rev().find(|o| has(o, &format!("closes:{}", ob.id)) && (same(aliases, &o.author, &ob.asker) || is_decider(&o.author))) {
            state = if has(c, "done:cancelled") { "cancelled" } else { "done" };
            by = vec![c.id.clone()];
        }
        ob.state = state.into();
        ob.rev = rev;
        ob.evidence.extend(by);
    }
    let live: Vec<&Obligation> = obligations.iter().filter(|o| !matches!(o.state.as_str(), "done" | "cancelled")).collect();
    let (state, summary) = if !live.is_empty() {
        // Who the task waits on: the holders, or the asker once the holders have answered. Repeats are counted, not listed.
        let mut parts: Vec<(String, usize)> = vec![];
        for o in &live {
            let part = match o.state.as_str() {
                "answered" => format!("{} (accept {}'s {})", canon(&o.asker), canon(&o.holder), o.kind),
                "changes" => format!("{} ({}, changes requested)", canon(&o.holder), o.kind),
                "owned" => format!("{} ({}, owned)", canon(&o.holder), o.kind),
                _ => format!("{} ({})", canon(&o.holder), o.kind),
            };
            match parts.iter_mut().find(|(p, _)| *p == part) {
                Some(e) => e.1 += 1,
                None => parts.push((part, 1)),
            }
        }
        let parts: Vec<String> = parts.into_iter().map(|(p, n)| if n > 1 { format!("{p} ×{n}") } else { p }).collect();
        ("open", format!("waiting on {}", parts.join(", ")))
    } else if !obligations.is_empty() {
        if obligations.iter().all(|o| o.state == "cancelled") { ("cancelled", "cancelled".to_owned()) } else { ("done", "done".to_owned()) }
    } else if !unrouted.is_empty() {
        ("unrouted", "needs routing".to_owned())
    } else {
        ("open", "nothing asked".to_owned())
    };
    Task {
        subject,
        room: r.room,
        title: r.title,
        current_rev,
        state: state.into(),
        summary,
        obligations,
        revisions,
        unrouted: unrouted.into_iter().map(|u| u.0).collect(),
        messages: r.msgs.iter().map(|o| o.id.clone()).collect(),
        last_seq: r.msgs.last().map(|o| o.seq).unwrap_or(0),
        notes,
    }
}

/// Messages read per room, newest first. A room with more says `capped`: its oldest tasks may be partial.
pub const WINDOW: usize = 5000;

#[derive(Debug, Serialize)]
pub struct Tasks {
    pub room: String,
    pub tasks: Vec<Task>,
    /// Messages folded.
    pub scanned: usize,
    /// True when the room has more than [`WINDOW`] messages: a task begun before the window may be partial.
    pub capped: bool,
    /// Always true until /tasks replaces the cards: nothing reads this as authority yet.
    pub shadow: bool,
    /// The exclusive cursor this answer was derived at: every message folded has `seq < as_of_seq`. The same
    /// cursor as `/rooms/:ch/state`, so a board, a State and a task list can be read at one point (Emmy, 2026-10-11).
    pub as_of_seq: i64,
    /// True when every message of the room below the cursor was folded; false when `capped` cut the window.
    pub complete: bool,
}

/// One room's tasks, newest activity first, filtered by `state` (open | done | cancelled | unrouted | all) and by a seat that
/// holds or asked one of a task's obligations (aliases count). `as_of_seq` is an exclusive cursor (the newest seq + 1 when
/// absent): paging is by seq below it, so a message written while the pages are read cannot enter the answer.
pub fn tasks_in(s: &Store, room: &str, seat: Option<&str>, state: &str, as_of_seq: Option<i64>) -> Result<Tasks> {
    let as_of = match as_of_seq {
        Some(n) if n >= 1 => n,
        Some(_) => return Err(StoreError::Rejected("as_of_seq must be a positive exclusive seq cursor".into())),
        None => s.newest_seq()? + 1,
    };
    let mut msgs: Vec<Observation> = Vec::new();
    let mut before = Some(as_of);
    let mut capped = false;
    loop {
        let page = s.recent(Some(room), 500, before)?;
        let n = page.len();
        before = page.last().map(|o| o.seq);
        msgs.extend(page);
        if n < 500 {
            break;
        }
        if msgs.len() >= WINDOW {
            capped = true;
            break;
        }
    }
    let aliases = s.seat_aliases();
    let seats = s.seat_names()?;
    let mut tasks = derive_with_seats(&msgs, &s.room_roots(Some(room)), &aliases, &seats);
    tasks.retain(|t| state == "all" || t.state == state);
    if let Some(seat) = seat {
        tasks.retain(|t| t.obligations.iter().any(|o| same(&aliases, &o.asker, seat) || same(&aliases, &o.holder, seat)));
    }
    tasks.sort_by_key(|t| std::cmp::Reverse(t.last_seq));
    Ok(Tasks { room: room.to_owned(), tasks, scanned: msgs.len(), capped, shadow: true, as_of_seq: as_of, complete: !capped })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(seq: i64, author: &str, text: &str, tags: &[&str]) -> Observation {
        Observation {
            seq, id: format!("OBS-{seq}"), channel: "oracle".into(), author: author.into(), kind: "say".into(), content: text.into(), cargo: None,
            tags: tags.iter().map(|t| t.to_string()).collect(), created_ms: seq * 1000, authored_ms: None, pubkey: String::new(), sig: String::new(), envelope: 2,
        }
    }

    fn one(msgs: &[Observation]) -> Task {
        let t = derive(msgs, &["blaze".to_string()], &Aliases::new());
        assert_eq!(t.len(), 1, "{t:#?}");
        t.into_iter().next().unwrap()
    }

    #[test]
    fn f1_new_evidence_is_the_same_task_at_a_new_revision() {
        let t = one(&[
            m(1, "rocky", "fix the fold", &["subject:x", "rev:a", "intent:result"]),
            m(2, "rocky", "fixed the review", &["subject:x", "rev:b", "intent:result", "prev:a"]),
        ]);
        assert_eq!((t.subject.as_str(), t.current_rev.as_deref(), t.revisions.len()), ("x", Some("b"), 2));
        assert_eq!(t.title, "fix the fold");
    }

    #[test]
    fn f2_a_late_check_attaches_to_its_revision_and_erases_no_approval() {
        let t = one(&[
            m(1, "rocky", "review x", &["subject:x", "ask:review", "to:emmy"]),
            m(2, "rocky", "evidence a", &["subject:x", "rev:a", "intent:result"]),
            m(3, "rocky", "evidence b", &["subject:x", "rev:b", "intent:result", "prev:a"]),
            m(4, "emmy", "approve", &["subject:x", "rev:b", "verdict:approve"]),
            m(5, "core-ci", "failed", &["subject:x", "rev:a", "ci:failed"]),
        ]);
        assert_eq!(t.obligations[0].state, "done");
        assert_eq!(t.state, "done");
        let a = t.revisions.iter().find(|r| r.rev == "a").unwrap();
        assert_eq!(a.checks[0].value, "failed");
        assert!(t.revisions.iter().find(|r| r.rev == "b").unwrap().checks.is_empty());
    }

    #[test]
    fn f3_a_verdict_counts_for_the_revision_it_names_not_its_arrival() {
        let t = one(&[
            m(1, "rocky", "review x", &["subject:x", "ask:review", "to:emmy"]),
            m(2, "rocky", "evidence a", &["subject:x", "rev:a", "intent:result"]),
            m(3, "rocky", "evidence b", &["subject:x", "rev:b", "intent:result", "prev:a"]),
            m(4, "emmy", "changes on a", &["subject:x", "rev:a", "verdict:changes"]),
        ]);
        assert_eq!(t.obligations[0].state, "open", "the review of b is still open");
        assert_eq!(t.revisions.iter().find(|r| r.rev == "a").unwrap().verdicts[0].verdict, "changes");
        // A newer revision reopens an approval of an older one.
        let t = one(&[
            m(1, "rocky", "review x", &["subject:x", "ask:review", "to:emmy"]),
            m(2, "rocky", "evidence a", &["subject:x", "rev:a", "intent:result"]),
            m(3, "emmy", "approve a", &["subject:x", "rev:a", "verdict:approve"]),
            m(4, "rocky", "evidence b", &["subject:x", "rev:b", "intent:result", "prev:a"]),
        ]);
        assert_eq!(t.obligations[0].state, "open");
        assert_eq!(t.revisions[0].verdicts[0].verdict, "approve", "the approval stays recorded on a");
    }

    #[test]
    fn f4_a_task_is_several_obligations_and_done_only_when_all_are() {
        let msgs = vec![
            m(1, "blaze", "ship x", &["subject:x", "ask:act", "to:rocky"]),
            m(2, "rocky", "review x", &["subject:x", "ask:review", "to:emmy"]),
            m(3, "rocky", "go for x?", &["subject:x", "ask:decide", "to:blaze"]),
            m(4, "rocky", "done at b", &["subject:x", "rev:b", "intent:result", "re:OBS-1"]),
        ];
        let t = one(&msgs);
        assert_eq!(t.obligations.len(), 3);
        assert_eq!(t.summary, "waiting on blaze (accept rocky's act), emmy (review), blaze (decide)");
        let mut all = msgs.clone();
        all.push(m(5, "emmy", "approve", &["subject:x", "rev:b", "verdict:approve"]));
        all.push(m(6, "blaze", "go", &["closes:OBS-3"]));
        all.push(m(7, "blaze", "accepted", &["closes:OBS-1"]));
        let t = one(&all);
        assert_eq!((t.state.as_str(), t.summary.as_str()), ("done", "done"), "{t:#?}");
        let mut not_yet = msgs;
        not_yet.push(m(5, "emmy", "approve", &["subject:x", "rev:b", "verdict:approve"]));
        assert_eq!(one(&not_yet).summary, "waiting on blaze (accept rocky's act), blaze (decide)");
    }

    #[test]
    fn f5_checks_count_only_from_trusted_issuers() {
        let t = one(&[
            m(1, "rocky", "evidence", &["subject:x", "rev:b", "intent:result"]),
            m(2, "rocky", "ci passed", &["subject:x", "rev:b", "ci:passed"]),
            m(3, "core-ci", "ci passed", &["subject:x", "rev:b", "ci:passed"]),
        ]);
        let b = &t.revisions[0];
        assert_eq!((b.claims.len(), b.claims[0].by.as_str()), (1, "rocky"));
        assert_eq!((b.checks.len(), b.checks[0].by.as_str()), (1, "core-ci"));
        assert_eq!(b.evidence, vec!["OBS-1"], "a check is not evidence");
    }

    #[test]
    fn f6_unrouted_rings_nobody_until_its_asker_or_a_decider_routes_it() {
        let p = m(1, "rocky", "proposal: shard the index", &["intent:proposal"]);
        let t = one(std::slice::from_ref(&p));
        assert_eq!((t.state.as_str(), t.summary.as_str(), t.obligations.len()), ("unrouted", "needs routing", 0));
        assert_eq!(t.unrouted, vec!["OBS-1"]);
        // A model's suggestion moves nothing; a decider's annotation routes it.
        let model = m(2, "dinesh", "", &["annotates:OBS-1", "set:to=emmy", "set:by=model"]);
        assert_eq!(one(&[p.clone(), model.clone()]).state, "unrouted");
        let t = one(&[p, model, m(3, "blaze", "", &["annotates:OBS-1", "set:to=emmy"])]);
        assert_eq!((t.state.as_str(), t.summary.as_str()), ("open", "waiting on emmy (decide)"));
    }

    #[test]
    fn a_reply_resolves_nothing_by_being_a_reply_and_owning_is_not_done() {
        let t = one(&[
            m(1, "blaze", "do x", &["ask:act", "to:rocky"]),
            m(2, "rocky", "looking", &["re:OBS-1"]),
            m(3, "emmy", "done it", &["re:OBS-1", "intent:result"]),
        ]);
        assert_eq!(t.subject, "thread:OBS-1");
        assert_eq!(t.obligations[0].state, "open", "a reply, and someone else's result, move nothing");
        let t = one(&[m(1, "blaze", "do x", &["ask:act", "to:rocky"]), m(2, "rocky", "mine", &["re:OBS-1", "intent:commit"])]);
        assert_eq!(t.obligations[0].state, "owned");
        // Only the asker or a decider closes; the holder cannot close its own obligation.
        let t = one(&[m(1, "emmy", "do x", &["ask:act", "to:rocky"]), m(2, "rocky", "closing", &["closes:OBS-1"])]);
        assert_eq!(t.obligations[0].state, "open");
        let t = one(&[m(1, "emmy", "do x", &["ask:act", "to:rocky"]), m(2, "emmy", "never mind", &["closes:OBS-1", "done:cancelled"])]);
        assert_eq!((t.obligations[0].state.as_str(), t.state.as_str()), ("cancelled", "cancelled"));
    }

    #[test]
    fn conflicting_verdicts_on_one_revision_stay_changes_for_whoever_asked_for_them() {
        let t = one(&[
            m(1, "rocky", "review x", &["subject:x", "ask:review", "to:emmy,dinesh"]),
            m(2, "rocky", "evidence", &["subject:x", "rev:b", "intent:result"]),
            m(3, "emmy", "approve", &["subject:x", "rev:b", "verdict:approve"]),
            m(4, "dinesh", "changes", &["subject:x", "rev:b", "verdict:changes"]),
        ]);
        let by = |h: &str| t.obligations.iter().find(|o| o.holder == h).unwrap().state.clone();
        assert_eq!((by("emmy").as_str(), by("dinesh").as_str(), t.state.as_str()), ("done", "changes", "open"));
        assert_eq!(t.revisions[0].verdicts.len(), 2, "both shown");
    }

    #[test]
    fn state_heads_are_not_tasks_and_talk_without_an_ask_is_not_a_task() {
        // A State write that would otherwise be evidence: kind state, a rev and a result.
        let mut head = m(1, "estate", "running", &["subject:ui.flicker", "state:running", "rev:a", "intent:result"]);
        head.kind = "state".into();
        let heads = derive(&[head], &[], &Aliases::new());
        assert!(heads.is_empty(), "{heads:#?}");
        let talk = derive(&[m(1, "rocky", "fyi", &["subject:x"]), m(2, "emmy", "ok", &["re:OBS-1"])], &[], &Aliases::new());
        assert!(talk.is_empty());
    }

    #[test]
    fn a_review_ask_that_names_a_revision_makes_it_current_and_repeats_are_counted() {
        let t = one(&[
            m(1, "rocky", "review a", &["ask:review", "to:emmy", "rev:a"]),
            m(2, "emmy", "approve a", &["re:OBS-1", "rev:a", "verdict:approve"]),
        ]);
        assert_eq!((t.current_rev.as_deref(), t.obligations[0].state.as_str()), (Some("a"), "done"));
        let t = one(&[
            m(1, "rocky", "review x", &["subject:x", "ask:review", "to:emmy"]),
            m(2, "rocky", "again", &["subject:x", "ask:review", "to:emmy"]),
            m(3, "rocky", "and again", &["subject:x", "ask:review", "to:emmy"]),
        ]);
        assert_eq!(t.summary, "waiting on emmy (review) ×3");
    }

    #[test]
    fn with_no_known_revision_a_verdict_is_kept_but_settles_nothing_and_old_names_read_as_their_seat() {
        // Emmy's ruling: a reviewer cannot establish the subject's revision by approving it.
        let t = one(&[m(1, "emmy", "review my patch", &["ask:review", "to:rocky"]), m(2, "rocky", "approve", &["re:OBS-1", "rev:44ffaed", "verdict:approve"])]);
        assert_eq!((t.obligations[0].state.as_str(), t.current_rev.as_deref()), ("open", None));
        assert_eq!(t.revisions[0].verdicts[0].verdict, "approve", "kept on the revision it names");
        let aliases: Aliases = [("emmy-core".to_string(), "emmy".to_string())].into();
        let t = derive(&[m(1, "rocky", "do x", &["ask:act", "to:emmy-core"])], &[], &aliases);
        assert_eq!(t[0].summary, "waiting on emmy (act)");
        assert_eq!(t[0].obligations[0].holder, "emmy-core", "attribution kept as written");
    }

    #[test]
    fn a_link_cycle_ends_the_walk_and_is_noted() {
        // A points at B, B points at A: the walk must end, and say so.
        let t = derive(&[m(1, "rocky", "a", &["re:OBS-2"]), m(2, "emmy", "b", &["re:OBS-1"]), m(3, "rocky", "do x", &["ask:act", "to:emmy", "re:OBS-1"])],
            &[], &Aliases::new());
        assert_eq!(t.len(), 1, "{t:#?}");
        assert_eq!(t[0].subject, "thread:OBS-3", "the ask is its own root");
        assert!(t[0].notes.iter().any(|n| n.contains("link cycle")), "{t:#?}");
        // A self-link too.
        let t = derive(&[m(1, "rocky", "a", &["ask:act", "to:emmy", "re:OBS-1"])], &[], &Aliases::new());
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn neither_a_delayed_old_revision_nor_an_unrelated_author_replaces_the_accepted_one() {
        let base = vec![
            m(1, "rocky", "review x", &["subject:x", "ask:review", "to:emmy"]),
            m(2, "rocky", "evidence a", &["subject:x", "rev:a", "intent:result"]),
            m(3, "rocky", "evidence b", &["subject:x", "rev:b", "intent:result", "prev:a"]),
            m(4, "emmy", "approve b", &["subject:x", "rev:b", "verdict:approve"]),
        ];
        let with = |extra: Observation| { let mut v = base.clone(); v.push(extra); one(&v) };
        // The old revision arriving late, from the implementer.
        let t = with(m(5, "rocky", "evidence a again", &["subject:x", "rev:a", "intent:result"]));
        assert_eq!((t.current_rev.as_deref(), t.state.as_str()), (Some("b"), "done"));
        assert!(t.notes.iter().any(|n| n.contains("superseded")), "{:?}", t.notes);
        // A revision never seen, delayed and without prev:.
        let t = with(m(5, "rocky", "evidence z", &["subject:x", "rev:z", "intent:result"]));
        assert_eq!(t.current_rev.as_deref(), Some("b"));
        assert!(t.notes.iter().any(|n| n.contains("no prev:")), "{:?}", t.notes);
        // Someone who is not an implementer.
        let t = with(m(5, "dinesh", "evidence c", &["subject:x", "rev:c", "intent:result", "prev:b"]));
        assert_eq!(t.current_rev.as_deref(), Some("b"));
        assert!(t.notes.iter().any(|n| n.contains("not an implementer")));
        // The implementer's next revision, naming b as its predecessor, moves on and reopens the review.
        let t = with(m(5, "rocky", "evidence c", &["subject:x", "rev:c", "intent:result", "prev:b"]));
        assert_eq!((t.current_rev.as_deref(), t.obligations[0].state.as_str()), (Some("c"), "open"));
    }

    #[test]
    fn a_review_to_two_holders_is_two_obligations_and_one_approval_is_not_done() {
        let t = one(&[
            m(1, "rocky", "review x", &["subject:x", "ask:review", "to:emmy,dinesh"]),
            m(2, "rocky", "evidence", &["subject:x", "rev:b", "intent:result"]),
            m(3, "emmy", "approve", &["subject:x", "rev:b", "verdict:approve"]),
        ]);
        assert_eq!(t.obligations.len(), 2);
        assert_eq!((t.state.as_str(), t.summary.as_str()), ("open", "waiting on dinesh (review)"));
    }

    #[test]
    fn an_unapproved_current_revision_is_not_replaced_by_a_delayed_one_without_prev() {
        // Emmy (a): b is current, nobody approved it; a delayed, never-seen a arrives without prev:.
        let t = one(&[
            m(1, "rocky", "evidence b", &["subject:x", "rev:b", "intent:result"]),
            m(2, "rocky", "evidence a", &["subject:x", "rev:a", "intent:result"]),
        ]);
        assert_eq!(t.current_rev.as_deref(), Some("b"));
        assert_eq!(t.revisions.iter().find(|r| r.rev == "a").unwrap().evidence, vec!["OBS-2"], "recorded on its revision");
    }

    #[test]
    fn a_later_review_ask_grants_its_author_no_authority() {
        // Emmy (b): mallory asks for a review of x at c, prev:b; that must not make her an implementer.
        let base = vec![
            m(1, "blaze", "build x", &["subject:x", "ask:act", "to:rocky"]),
            m(2, "rocky", "evidence b", &["subject:x", "rev:b", "intent:result", "re:OBS-1"]),
        ];
        let mut v = base.clone();
        v.push(m(3, "mallory", "please review c", &["subject:x", "ask:review", "to:emmy", "rev:c", "prev:b"]));
        let t = one(&v);
        assert_eq!(t.current_rev.as_deref(), Some("b"), "{:?}", t.notes);
        assert!(t.notes.iter().any(|n| n.contains("mallory is not an implementer")));
        // The seed's author (blaze) or a decider can widen it; rocky, the implementer, moves it with prev:.
        let mut v = base;
        v.push(m(3, "rocky", "review c", &["subject:x", "ask:review", "to:emmy", "rev:c", "prev:b"]));
        assert_eq!(one(&v).current_rev.as_deref(), Some("c"));
    }
    #[test]
    fn future_authority_does_not_authorize_past_evidence() {
        let t = one(&[
            m(1, "blaze", "build", &["subject:x", "ask:act", "to:rocky"]),
            m(2, "rocky", "b", &["subject:x", "rev:b", "intent:result"]),
            m(3, "mallory", "c", &["subject:x", "rev:c", "prev:b", "intent:result"]),
            m(4, "blaze", "help going forward", &["subject:x", "ask:act", "to:mallory"]),
        ]);
        assert_eq!(t.current_rev.as_deref(), Some("b"));
    }

}
