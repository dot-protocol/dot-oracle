//! What a message *is*, and whether it still matters — computed when you read, never stored.
//!
//! The rule this follows was settled long before this file (OBS-core-20260813): a room has no
//! lifecycle enum, there are no transitions, and nobody fires them. An author declares INTENT once,
//! in a signed tag. Everything else — closed, superseded, archived, expired — is a projection over
//! later messages and a clock, recomputed on every read. Nothing here mutates the ledger, so a
//! wrong rule is a bad afternoon, not a lost message, and improving the rule re-judges all history.
//!
//! Tags an author may write (all signed, all optional):
//!   kind:question | kind:proposal | kind:decision | kind:receipt | kind:status | kind:note
//!   ttl:<hours>            how long this should stay open; otherwise the intent's default
//!   re:<id>                about that message, without resolving it
//!   closes:<id>            answers or resolves it
//!   supersedes:<id>        replaces it
//!   archives:<id>          take it out of the view (the ledger keeps it; only a human deletes)
//!   ask:<none|read|reply|act|decide|review>  the outcome the author needs (Emmy, 2026-09-22); decide
//!                          carries cargo `options` and is answered with `choice:<n>`, review points
//!                          at a `ref:`; without it a
//!                          question or proposal asks for a reply and everything else asks nothing
//!   by:<unix ms>           when that outcome is due
//!   ref:<id|sha|url>       what the message changed or points at; the delta's evidence
//!
//! Everything about the outcome is derived: `answered_by` is the first later message that replies
//! to or closes this one — an authored CLAIM that it was answered, never a verified completion —
//! and `overdue` is an unmet ask past its clock. Neither is ever written by an author.
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const HOUR_MS: i64 = 3_600_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Intent {
    /// Asks for an answer. Holds the room until answered or aged out.
    Question,
    /// Asks for a yes or no. Same, with a longer fuse.
    Proposal,
    /// A call that was made. Stays until something supersedes it.
    Decision,
    /// Something was found, with its evidence (Jobs and Bobby, 2026-09-27: "result = found with
    /// evidence, receipt = shipped with id"). Ambient, and it moves an asked thread to researched.
    Result,
    /// Proof that something shipped: an id, a hash, a deploy. Ambient.
    Receipt,
    /// Progress. Ambient, and shorter-lived than a receipt.
    Status,
    /// Anything else, including every message written before this vocabulary existed.
    Note,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Still wants attention. This is what a room shows by default.
    Open,
    /// Something answered it.
    Closed,
    /// Something replaced it.
    Superseded,
    /// Taken out of the view on purpose.
    Archived,
    /// Its time ran out and nothing answered it.
    Expired,
    /// Never competed for the view: receipts and status lines.
    Ambient,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Closed => "closed",
            State::Superseded => "superseded",
            State::Archived => "archived",
            State::Expired => "expired",
            State::Ambient => "ambient",
        }
    }
    pub fn parse(s: &str) -> Option<State> {
        Some(match s {
            "open" => State::Open,
            "closed" => State::Closed,
            "superseded" => State::Superseded,
            "archived" => State::Archived,
            "expired" => State::Expired,
            "ambient" => State::Ambient,
            _ => return None,
        })
    }
}

impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Question => "question",
            Intent::Proposal => "proposal",
            Intent::Decision => "decision",
            Intent::Result => "result",
            Intent::Receipt => "receipt",
            Intent::Status => "status",
            Intent::Note => "note",
        }
    }
    /// Names seen in the live ledger, mapped to the six that matter.
    ///
    /// `kind:` is overloaded: 71k of its 80k uses are `kind:say`, the storage rail copied onto
    /// tags at import, not an author's intent. Rail names deliberately fall through to `Note`.
    fn parse(s: &str) -> Option<Intent> {
        Some(match s {
            "question" | "ask" => Intent::Question,
            "proposal" | "rip" => Intent::Proposal,
            "decision" | "ruling" | "verdict" => Intent::Decision,
            // `result` was folded into receipt until 2026-09-27, so 78 findings read as "shipped" and
            // never moved their thread. A finding is not a shipment.
            "result" => Intent::Result,
            "receipt" | "ack" => Intent::Receipt,
            "status" | "progress" | "sysevent" | "bubble" | "heartbeat" => Intent::Status,
            "note" | "claim" | "report" | "wall" => Intent::Note,
            _ => return None,
        })
    }
    /// How long this kind of message stays open with nothing answering it.
    ///
    /// A decision has no fuse: it stands until something supersedes it. Receipts and status lines
    /// are ambient from the start, so their window never matters.
    pub fn default_ttl_ms(self) -> Option<i64> {
        Some(match self {
            Intent::Question => 72 * HOUR_MS,
            Intent::Proposal => 168 * HOUR_MS,
            Intent::Note => 24 * HOUR_MS,
            Intent::Decision => return None,
            Intent::Result | Intent::Receipt | Intent::Status => return None,
        })
    }
}

/// What the author needs back. Declared once with `ask:`; the default follows the intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ask {
    /// Nothing: a receipt, a note, something to know.
    None,
    /// Read it; no answer needed.
    Read,
    /// Answer it.
    Reply,
    /// Do something, then say so.
    Act,
    /// Pick one of the message's `options` (cargo). Any reply answers it; `choice:<n>` says which.
    Decide,
    /// Look at the message's `ref:` and approve it or not.
    Review,
}

impl Ask {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Ask::None,
            "read" => Ask::Read,
            "reply" => Ask::Reply,
            "act" => Ask::Act,
            "decide" => Ask::Decide,
            "review" => Ask::Review,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Ask::None => "none",
            Ask::Read => "read",
            Ask::Reply => "reply",
            Ask::Act => "act",
            Ask::Decide => "decide",
            Ask::Review => "review",
        }
    }
}

/// The outcome the author needs: the `ask:` tag, else what the intent implies.
pub fn ask_of(tags: &[String], intent: Intent) -> Ask {
    tag_value(tags, "ask:")
        .and_then(Ask::parse)
        .unwrap_or(match intent {
            Intent::Question | Intent::Proposal => Ask::Reply,
            _ => Ask::None,
        })
}

/// `choice:<n>` — which option (1 to [`MAX_OPTIONS`]) a reply picked.
pub fn choice_of(tags: &[String]) -> Option<u32> {
    tag_value(tags, "choice:")?.parse::<u32>().ok().filter(|n| (1..=MAX_OPTIONS as u32).contains(n))
}
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 6;
pub const MAX_OPTION_LEN: usize = 80;

/// `by:<unix ms>` — when the ask is due. Seconds are accepted and promoted to milliseconds.
pub fn by_of(tags: &[String]) -> Option<i64> {
    let n = tag_value(tags, "by:")?.parse::<i64>().ok()?;
    Some(if n < 100_000_000_000 { n * 1000 } else { n })
}

/// The part of a message this projection reads. Deliberately not the storage row: this module
/// knows nothing about SQL, and can judge a message that has not been stored yet.
#[derive(Debug, Clone)]
pub struct Item<'a> {
    pub id: &'a str,
    pub author: &'a str,
    pub created_ms: i64,
    pub tags: &'a [String],
}

#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub state: State,
    pub intent: Intent,
    /// The message that closed, superseded or archived this one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// How many later messages point at this one with `re:`.
    #[serde(skip_serializing_if = "is_zero")]
    pub replies: u32,
    /// What the author asked for.
    pub ask: Ask,
    /// When it is due, if a clock was set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_ms: Option<i64>,
    /// The first later message that replied to or closed this one: a claim, not a verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<String>,
    /// An ask (reply, act, decide or review) with a clock, past it, with no answer claimed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub overdue: bool,
    /// Whose attention it asks: the author's own `to:` tags, else what a person's annotation set (`set:to=a,b`); a model's never addresses.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
    /// For `ask:decide`: the option (1-based) the earliest reply that picked one chose, by `choice:<n>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choice: Option<u32>,
    /// Where intent/ask came from: the author's own tags, or an annotation (its id). A person's
    /// annotation outranks a model's (`set:by=model`); the author always outranks both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classified_by: Option<String>,
    /// The classifier looked and could not say: stated, never silent.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unclassified: bool,
    /// The message's items (Blaze, 2026-09-22: "a reply is a state change, not a receipt"). Declared
    /// by the author's own `item:<k>=<kind>` tags or by an annotation's; moved by a later reply's or
    /// annotation's `item:<k>=<state>` tag. `accepted` counts only from the message's author.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ItemState>,
    /// The thread word folded from the items: needs_you · in_progress · built · live · closed.
    /// Absent when the message has no items (the ask/answer fields say what there is to say).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fold: Option<&'static str>,
    /// Who marked it, per mark, oldest first; a seat counts once per mark, and a mark that was
    /// withdrawn (`archives:<mark id>`) is gone. Counted and named only: settling is the lifecycle's.
    #[serde(skip_serializing_if = "Marks::is_empty")]
    pub marks: Marks,
    /// What a decider's go/no made of it: accepted · answered · confirmed · declined · dropped ·
    /// withdrawn. Absent while nothing settled it. Never "closed" (that is an explicit close).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled: Option<&'static str>,
    /// The mark that settled it, and when.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<i64>,
    /// What the message's own words, a person's annotation or a model's say became of it: `settled`, `stale`, `superseded` or `acked` (`set:done=` on an
    /// annotation, `done:` on the message; the author, then a person, then a model, as for `ask`). `answered` is never taken from here: it stays derived
    /// from real replies. Absent when nobody said.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<&'static str>,
    /// The annotation (or the message) that said it, and when.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_at: Option<i64>,
}

/// The one seat whose model annotations may say whom a message is for (`set:to=`). Any other model's `set:to=` addresses nobody: a classifier that tagged
/// nearly every message `to=blaze` put other seats' notes on his list (2026-09-29). The thread observer reads whole threads before it says whom one is for
/// (Blaze, Rocky, 2026-09-30). The list is code, not configuration, so every reader and every reindex agrees.
pub const OBSERVER_SEATS: [&str; 1] = ["dinesh"];

/// What a `done` word means to the fold. `answered` and `open` are true or false by the replies, so they are not verdicts here.
pub const DONE_WORDS: [&str; 7] = ["open", "answered", "settled", "stale", "superseded", "acked", "none"];
fn done_named(v: &str) -> Option<&'static str> {
    match v {
        "settled" => Some("settled"),
        "stale" => Some("stale"),
        "superseded" => Some("superseded"),
        "acked" => Some("acked"),
        // a notice that asks nothing: recorded on the verdict, and it changes no state and closes no ask
        "none" => Some("none"),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Marks {
    pub seen: Vec<String>,
    pub go: Vec<String>,
    pub like: Vec<String>,
    pub no: Vec<String>,
}
impl Marks {
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty() && self.go.is_empty() && self.like.is_empty() && self.no.is_empty()
    }
    fn add(&mut self, which: &str, seat: &str) {
        let list = match which {
            "seen" => &mut self.seen,
            "go" => &mut self.go,
            "like" => &mut self.like,
            "no" => &mut self.no,
            _ => return,
        };
        if !list.iter().any(|s| s == seat) {
            list.push(seat.to_owned());
        }
    }
}

/// One item of a message and where it stands. `by` is the message that last moved it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ItemState {
    pub k: String,
    pub kind: &'static str,
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// The annotation that declared this item, when it is not the message itself: where its words live
    /// (a seat other than the classifier can add items, and the surface needs their text).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Items this one waits on (`needs:K=K1,K2` on the message that declares K).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// Where the item stands on the map: `frontier` (can be worked now), `fog` (waits on `blocked_by`), `done`, or
    /// `destination`. Derived, never written.
    pub place: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    /// The reply that resolved it (`resolves:K`): the primary source a summary must link to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

/// Each item's place on its map, from the items' own states and needs. A need naming no item of the map is fog:
/// it is unknown, and an unknown is never treated as met.
pub fn place_items(items: &mut [ItemState]) {
    let state: HashMap<String, &'static str> = items.iter().map(|i| (i.k.clone(), i.state)).collect();
    for it in items.iter_mut() {
        it.blocked_by = it.needs.iter().filter(|n| !state.get(*n).is_some_and(|s| NEED_MET.contains(s))).cloned().collect();
        it.place = if it.kind == "destination" {
            "destination"
        } else if matches!(it.state, "accepted" | "dropped") {
            "done"
        } else if it.state == "blocked" || !it.blocked_by.is_empty() {
            "fog"
        } else {
            "frontier"
        };
    }
}
/// Maps (Blaze, 2026-10-02: "maps are integral to our enterprise"): beside what was asked, the four kinds of session a
/// decision needs (research, prototype, grill, task) and the map's own destination.
pub const ITEM_KINDS: [&str; 10] =
    ["requirement", "question", "ask", "decision", "complaint", "research", "prototype", "grill", "task", "destination"];
/// A need is met once the item it names stops being in the way: delivered, live, accepted or dropped.
const NEED_MET: [&str; 4] = ["built", "live", "accepted", "dropped"];
pub const ITEM_STATES: [&str; 7] = ["open", "building", "built", "live", "accepted", "blocked", "dropped"];
fn item_kind(v: &str) -> Option<&'static str> {
    ITEM_KINDS.iter().find(|k| **k == v).copied()
}
fn item_state(v: &str) -> Option<&'static str> {
    ITEM_STATES.iter().find(|k| **k == v).copied()
}
/// `item:R1=requirement` → ("R1", "requirement"). Keys are short: letters, digits, dashes.
fn item_tag(t: &str) -> Option<(&str, &str)> {
    let rest = t.strip_prefix("item:")?;
    let (k, v) = rest.split_once('=')?;
    (!k.is_empty() && k.len() <= 24 && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then_some((k, v))
}
/// The thread word from its items. Closed only when every item is accepted or dropped.
pub fn fold_items(items: &[ItemState]) -> Option<&'static str> {
    if items.is_empty() {
        return None;
    }
    let live = items.iter().filter(|i| !matches!(i.state, "accepted" | "dropped"));
    let live: Vec<&ItemState> = live.collect();
    if live.is_empty() {
        return Some("closed");
    }
    // built or live waits on the asker's acceptance; a blocked item waits on a decision: both need a person.
    if live.iter().any(|i| matches!(i.state, "built" | "live" | "blocked")) {
        return Some("needs_you");
    }
    if live.iter().any(|i| i.state == "building") {
        return Some("in_progress");
    }
    Some("open")
}
fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn tag_value<'a>(tags: &'a [String], prefix: &str) -> Option<&'a str> {
    tags.iter().find_map(|t| t.strip_prefix(prefix))
}

/// A STATE UPDATE, not a message (Blaze, 2026-09-27: "a state update on a message is not a
/// message"). Written as envelope kind `annotate`, which the store only accepts with an
/// `annotates:<id>` tag; says written before that kind existed carry the same tag, so the tag is
/// the whole test and the ledger is never rewritten. A state update is never a root, a thread
/// member, a reply, an answer or news: it is read back as the derived fields of the message it
/// names, and as that message's history.
pub fn is_state_update(tags: &[String]) -> bool {
    tags.iter().any(|t| t.starts_with("annotates:"))
}

/// THE MARKS (MESSAGE-UNIVERSE §3, Nadia 2026-09-27): seen · go · like · no; and what a seat did with an ask
/// addressed to it (Blaze, 2026-10-10: "you can mark it read, mark it acknowledged, archive it, mark done … every
/// time you pull inbox you should not get the same ask"): ack (I have it, stop asking) · stuck (I cannot move it) ·
/// done (I did it) · later (not now; back by itself at `until:<ms>`, Blaze 2026-10-23). A mark is one seat's: it moves
/// the ask on that seat's list, not on anyone else's.
pub const MARKS: [&str; 8] = ["seen", "go", "like", "no", "ack", "stuck", "done", "later"];

/// When a `later` mark brings its ask back (`until:<epoch ms>`).
pub fn until_of(tags: &[String]) -> Option<i64> {
    tag_value(tags, "until:").and_then(|v| v.parse().ok())
}

fn mark_named(v: &str) -> Option<&'static str> {
    MARKS.iter().find(|k| **k == v).copied()
}

/// `Some("go")` for a mark written the legacy way: a say replying with `react:<mark>`
/// (AXXIS dev f95684b0). A mark is a reply in the ledger and nothing in a thread: it is not
/// counted in `replies`, never answers an ask, and folds onto its target as `marks`.
pub fn reaction_of(tags: &[String]) -> Option<&'static str> {
    tags.iter().filter_map(|t| t.strip_prefix("react:")).find_map(mark_named)
}

/// The mark an item records, whichever way it was written: an `annotate` carrying
/// `set:mark=<mark>`, or a legacy `react:<mark>` reply.
pub fn mark_of(tags: &[String]) -> Option<&'static str> {
    tag_value(tags, "set:mark=").and_then(mark_named).or_else(|| reaction_of(tags))
}

/// Speech a person or agent can answer: neither a state update nor a reaction.
pub fn is_speech(tags: &[String]) -> bool {
    !is_state_update(tags) && reaction_of(tags).is_none()
}

/// The author's declared intent. `intent:` wins over `kind:` because it can only ever have been
/// written on purpose, while `kind:` also carries the storage rail.
pub fn intent_of(tags: &[String]) -> Intent {
    tag_value(tags, "intent:")
        .and_then(Intent::parse)
        .or_else(|| tag_value(tags, "kind:").and_then(Intent::parse))
        .unwrap_or(Intent::Note)
}

/// The message's fuse in milliseconds, for readers outside this module.
pub fn ttl_ms(tags: &[String], intent: Intent) -> Option<i64> {
    ttl_of(tags, intent)
}

/// An explicit `ttl:<hours>`, clamped so one bad tag cannot pin a message open for a century
/// or blink it out of the room before anyone reads it. `ttl:0` means "no fuse".
fn ttl_of(tags: &[String], intent: Intent) -> Option<i64> {
    match tag_value(tags, "ttl:").and_then(|h| h.parse::<i64>().ok()) {
        Some(0) => None,
        Some(h) => Some(h.clamp(1, 24 * 365) * HOUR_MS),
        None => intent.default_ttl_ms(),
    }
}

/// Judge every item against the others and a clock. `items` may be in any order.
///
/// Only a *later* message can resolve an earlier one, and a message can never resolve itself:
/// without that, two messages pointing at each other would flicker between states depending on
/// how they were sorted.
pub fn judge<'a>(items: &'a [Item<'a>], now_ms: i64) -> HashMap<String, Verdict> {
    judge_in(items, now_ms, &[])
}

/// What a decider's mark settles, by what the message is (MESSAGE-UNIVERSE §3, Nadia's rows of
/// 2026-09-27). A mark never yields "closed": that word belongs to an explicit close or archive.
/// `own`: the author marking their own message. `decided_by_decider`: a decision whose author is a
/// decider is decided already. `has_items`: a root carrying items settles through them.
pub fn settles(intent: Intent, mark: &str, own: bool, decided_by_decider: bool, has_items: bool) -> Option<&'static str> {
    match (mark, intent) {
        ("no", _) if own => Some("withdrawn"),
        ("go", Intent::Proposal) => Some("accepted"),
        ("no", Intent::Proposal) | ("no", Intent::Question) => Some("declined"),
        ("go", Intent::Question) => Some("answered"),
        ("go", Intent::Decision) if !decided_by_decider => Some("accepted"),
        ("no", Intent::Decision) if !decided_by_decider => Some("dropped"),
        ("go", Intent::Status | Intent::Result | Intent::Receipt) => Some("confirmed"),
        ("go", _) if has_items => Some("accepted"),
        ("no", _) if has_items => Some("dropped"),
        _ => None,
    }
}

/// [`judge`], knowing the room's deciders: its roots (`room-members/1`); the seat a message names
/// with `to:` is a decider for that message too. A decider's newest go/no SETTLES (`settled`,
/// `settled_by`); anyone else's mark is an opinion, counted and named in `marks` and nothing more.
pub fn judge_in<'a>(items: &'a [Item<'a>], now_ms: i64, roots: &[String]) -> HashMap<String, Verdict> {
    judge_in_with(items, now_ms, roots, &crate::alias::Aliases::new())
}

/// [`judge_in`] where a name in a `to:` list or in `roots` also stands for the seat it is an alias of (docs: seat aliases): her go settles what was addressed to her
/// old label.
pub fn judge_in_with<'a>(items: &'a [Item<'a>], now_ms: i64, roots: &[String], aliases: &crate::alias::Aliases) -> HashMap<String, Verdict> {
    let at: HashMap<&str, i64> = items.iter().map(|i| (i.id, i.created_ms)).collect();
    let by_id: HashMap<&str, &Item<'a>> = items.iter().map(|i| (i.id, i)).collect();
    let (mut resolved, mut replies) = (HashMap::new(), HashMap::<&str, u32>::new());
    // Annotations (`annotates:<id>` + `set:k=v` tags) fill what the author left blank. Rank: a
    // person's annotation over a model's, then the newest. Emmy, 2026-09-22: never an edit, always
    // a message with a why; the author's own tags cannot be overridden by anyone.
    let mut notes: HashMap<&str, Vec<&Item<'a>>> = HashMap::new();
    for item in items {
        for target in item.tags.iter().filter_map(|t| t.strip_prefix("annotates:")) {
            if target != item.id {
                notes.entry(target).or_default().push(item);
            }
        }
    }
    // People first; among models the observer seat's, whatever the age (Rocky, 2026-09-30: another classifier running in the same threads would otherwise
    // overwrite its verdicts, newest wins); then the newest.
    let rank = |a: &Item<'a>| {
        let model = a.tags.iter().any(|t| t == "set:by=model");
        (!model, model && OBSERVER_SEATS.contains(&a.author), a.created_ms)
    };
    let pick = |target: &str, key: &str| -> Option<(String, String)> {
        let mut best: Option<(&Item<'a>, String)> = None;
        for a in notes.get(target)? {
            // A model may classify what a message is; it may not say whom it is for. A classifier that tags nearly every
            // message `set:to=blaze` put an author's own note to another seat on his list (2026-09-29): a message is
            // addressed by its own `to:` or an @mention, or by a person's annotation.
            if key == "to" && a.tags.iter().any(|t| t == "set:by=model") && !OBSERVER_SEATS.contains(&a.author) {
                continue;
            }
            let prefix = format!("set:{key}=");
            if let Some(v) = a.tags.iter().find_map(|t| t.strip_prefix(prefix.as_str())) {
                if best.as_ref().is_none_or(|(b, _)| rank(a) > rank(b)) {
                    best = Some((a, v.to_owned()));
                }
            }
        }
        best.map(|(a, v)| (a.id.to_owned(), v))
    };
    // The author changing their own mind (Bobby, 2026-09-30: `set:ask=none` on his own `ask:reply` signed twice and
    // changed nothing). "The author's own tags win" guards the author against everyone ELSE; the author's own later
    // annotation, newest first, outranks the author's original tag.
    let pick_own = |target: &str, key: &str| -> Option<(String, String)> {
        let author = by_id.get(target)?.author;
        let prefix = format!("set:{key}=");
        notes.get(target)?.iter().filter(|a| a.author == author)
            .filter_map(|a| a.tags.iter().find_map(|t| t.strip_prefix(prefix.as_str())).map(|v| (a, v.to_owned())))
            .max_by_key(|(a, _)| a.created_ms)
            .map(|(a, v)| (a.id.to_owned(), v))
    };
    // The earliest later message pointing at a target, by reply or close: who answered it first.
    let mut first_answer: HashMap<&str, (i64, &str)> = HashMap::new();
    // An author's own follow-up does not answer its own ask (2026-10-10: Noor's Round 47 "answered" Round 46 to Rocky
    // and Emmy, and the room read it as answered while neither had replied). It is still a reply in the thread.
    let author_of: HashMap<&str, &str> = items.iter().map(|i| (i.id, i.author)).collect();
    let mut note_answer = |target: &'a str, item: &Item<'a>| {
        if target == item.id || at.get(target).is_some_and(|t| *t > item.created_ms) {
            return;
        }
        if author_of.get(target).is_some_and(|a| crate::alias::same(aliases, a, item.author)) {
            return;
        }
        let e = first_answer.entry(target).or_insert((item.created_ms, item.id));
        if item.created_ms < e.0 {
            *e = (item.created_ms, item.id);
        }
    };

    // Strongest verdict wins regardless of arrival order: archive over supersede over close.
    for (prefix, state) in [
        ("closes:", State::Closed),
        ("supersedes:", State::Superseded),
        ("archives:", State::Archived),
    ] {
        for item in items {
            for target in item.tags.iter().filter_map(|t| t.strip_prefix(prefix)) {
                if target == item.id {
                    continue;
                }
                if at.get(target).is_some_and(|t| *t > item.created_ms) {
                    continue; // points at a message from the future; ignore
                }
                resolved.insert(target.to_owned(), (state, item.id.to_owned()));
                if state == State::Closed && is_speech(item.tags) {
                    note_answer(target, item);
                }
            }
        }
    }
    // `in_reply_to:` is the ledger's existing spelling (26k uses); `re:` and `reply:` are the
    // short ones the CLI writes. All three mean the same thing and none of them resolve anything.
    // A mark or a state update points at a message without being a reply to it. A mark that was
    // withdrawn (archived) no longer counts.
    let mut marks: HashMap<&str, Vec<(i64, &'static str, &str, &str)>> = HashMap::new();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new(); // speech replies, by target
    let withdrawn = |id: &str| matches!(resolved.get(id), Some((State::Archived, _)));
    for item in items {
        let reaction = reaction_of(item.tags);
        let speech = reaction.is_none() && !is_state_update(item.tags);
        if let (Some(m), false) = (tag_value(item.tags, "set:mark=").and_then(mark_named), withdrawn(item.id)) {
            for target in item.tags.iter().filter_map(|t| t.strip_prefix("annotates:")) {
                if target != item.id && at.get(target).is_none_or(|t| *t <= item.created_ms) {
                    marks.entry(target).or_default().push((item.created_ms, m, item.author, item.id));
                }
            }
        }
        for target in item.tags.iter().filter_map(|t| {
            t.strip_prefix("re:")
                .or_else(|| t.strip_prefix("reply:"))
                .or_else(|| t.strip_prefix("in_reply_to:"))
        }) {
            if target == item.id {
                continue;
            }
            if speech {
                children.entry(target).or_default().push(item.id);
                *replies.entry(target).or_default() += 1;
                note_answer(target, item);
            } else if let Some(r) = reaction {
                if !withdrawn(item.id) && at.get(target).is_none_or(|t| *t <= item.created_ms) {
                    marks.entry(target).or_default().push((item.created_ms, r, item.author, item.id));
                }
            }
        }
    }

    // Items: declarations from the message itself or its annotations; moves from later replies,
    // annotations, or the message's own later tags never (a message cannot move its own items after
    // the fact — that is a reply). Latest move wins; `accepted` only from the author.
    type Decl<'a> = (String, &'static str, Option<&'a str>); // (k, kind, declaring annotation)
    let mut declared: HashMap<&str, Vec<Decl>> = HashMap::new(); // target → its declared items
    type Move<'a> = (i64, String, &'static str, &'a str, &'a str); // (at, k, state, mover author, mover id)
    let mut moves: HashMap<&str, Vec<Move>> = HashMap::new(); // target → moves
    // A seat that classifies a message again REPLACES its items (Blaze, 2026-09-27: his message carried R1–R4 from
    // Wags's second pass beside Q1, C1, Q2 from the first). Per (message, seat), only the newest annotation that
    // declares items declares them.
    let mut newest_decl: HashMap<(&str, &str), (i64, &str)> = HashMap::new();
    for item in items {
        if !item.tags.iter().filter_map(|t| item_tag(t)).any(|(_, v)| item_kind(v).is_some()) {
            continue;
        }
        for target in item.tags.iter().filter_map(|t| t.strip_prefix("annotates:")) {
            let e = newest_decl.entry((target, item.author)).or_insert((item.created_ms, item.id));
            if (item.created_ms, item.id) > *e {
                *e = (item.created_ms, item.id);
            }
        }
    }
    for item in items {
        let targets: Vec<&str> = item
            .tags
            .iter()
            .filter_map(|t| {
                t.strip_prefix("annotates:")
                    .or_else(|| t.strip_prefix("re:"))
                    .or_else(|| t.strip_prefix("reply:"))
                    .or_else(|| t.strip_prefix("in_reply_to:"))
            })
            .filter(|target| *target != item.id && at.get(target).is_none_or(|t| *t <= item.created_ms))
            .collect();
        for (k, v) in item.tags.iter().filter_map(|t| item_tag(t)) {
            if let Some(kind) = item_kind(v) {
                declared.entry(item.id).or_default().push((k.to_owned(), kind, None));
                for target in item.tags.iter().filter_map(|t| t.strip_prefix("annotates:")) {
                    let newest = newest_decl.get(&(target, item.author)).is_none_or(|(_, id)| *id == item.id);
                    if target != item.id && newest {
                        declared.entry(target).or_default().push((k.to_owned(), kind, Some(item.id)));
                    }
                }
            } else if let Some(state) = item_state(v) {
                for target in &targets {
                    moves.entry(target).or_default().push((item.created_ms, k.to_owned(), state, item.author, item.id));
                }
            }
        }
    }
    // The items each mark names (`item:D5=dropped`). A mark naming only SOME of a message's items moves those and
    // settles nothing else; one naming them all (the rail's ✓ on a root) settles the message too.
    let mark_items: HashMap<&str, HashSet<&str>> = items
        .iter()
        .map(|i| (i.id, i.tags.iter().filter_map(|t| item_tag(t)).filter(|(_, v)| item_state(v).is_some()).map(|(k, _)| k).collect::<HashSet<&str>>()))
        .filter(|(_, ks)| !ks.is_empty())
        .collect();
    let items_of = |item: &Item<'a>, decider: &dyn Fn(&str) -> bool, bulk: Option<(&'static str, String, i64)>| -> Vec<ItemState> {
        let Some(decl) = declared.get(item.id) else { return Vec::new() };
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (k, kind, from) in decl {
            if !seen.insert(k.clone()) {
                continue;
            }
            let mut state = "open";
            let mut by = None;
            let mut when = i64::MIN;
            for (ms, mk, st, who, id) in moves.get(item.id).map(|v| v.as_slice()).unwrap_or(&[]) {
                if mk != k || *ms < when {
                    continue;
                }
                // Accepted counts from the author (self-close), a seat the message names, or a root.
                // Anyone else's accepted is an opinion, not a move.
                if *st == "accepted" && *who != item.author && !decider(who) {
                    continue;
                }
                state = st;
                by = Some((*id).to_owned());
                when = *ms;
            }
            if let Some((st, id, ms)) = &bulk {
                if *ms >= when && !matches!(state, "accepted" | "dropped") {
                    state = st;
                    by = Some(id.clone());
                }
            }
            // Edges belong to the selected declaration, not every historical annotation of the root.
            let source_id = from.unwrap_or(item.id);
            let mut needs = Vec::new();
            if let Some(source) = items.iter().find(|i| i.id == source_id) {
                for (key, list) in source.tags.iter().filter_map(|t| t.strip_prefix("needs:")?.split_once('=')) {
                    if key != k { continue; }
                    for need in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                        if !needs.iter().any(|n| n == need) { needs.push(need.to_owned()); }
                    }
                }
            }
            // A claimed resolution is authoritative only when attached to the effective valid move.
            // A later rejected acceptance or a bare resolves tag cannot replace its provenance.
            let resolved_by = by.as_ref().filter(|id| NEED_MET.contains(&state) && items.iter().any(|i|
                i.id == id.as_str() && i.tags.iter().any(|t| t.strip_prefix("resolves:") == Some(k.as_str()))
            )).cloned();
            out.push(ItemState {
                k: k.clone(),
                kind,
                state,
                by,
                from: from.map(str::to_owned),
                needs,
                place: "frontier",
                blocked_by: Vec::new(),
                resolved_by,
            });
        }
        place_items(&mut out);
        out
    };

    items
        .iter()
        .map(|item| {
            let intent = intent_of(item.tags);
            let replies = replies.get(item.id).copied().unwrap_or(0);
            // An ask its author declared (reply, act, decide, review) is never ambient, whatever the intent, and has no
            // fuse unless the author set one with `ttl:` (Blaze, 2026-10-10: "it should not be a cutoff … if it's remaining
            // open, it should stab you every time"). Noor's rounds are results that ask for a reply; judged ambient, they
            // reached no list. Notes and status lines keep their short life.
            let declared_ask = tag_value(item.tags, "ask:").and_then(Ask::parse).is_some_and(|a| a != Ask::None);
            let (state, by) = match resolved.get(item.id) {
                Some((state, by)) => (*state, Some(by.clone())),
                None if !declared_ask && matches!(intent, Intent::Result | Intent::Receipt | Intent::Status) => {
                    (State::Ambient, None)
                }
                None if declared_ask && tag_value(item.tags, "ttl:").is_none() => (State::Open, None),
                None => match ttl_of(item.tags, intent) {
                    Some(ttl) if now_ms.saturating_sub(item.created_ms) > ttl => {
                        (State::Expired, None)
                    }
                    _ => (State::Open, None),
                },
            };
            let own_intent = tag_value(item.tags, "intent:").and_then(Intent::parse).is_some();
            let own_ask = tag_value(item.tags, "ask:").and_then(Ask::parse).is_some();
            let noted_intent = if own_intent { pick_own(item.id, "intent") } else { pick(item.id, "intent") }.and_then(|(id, v)| Intent::parse(&v).map(|i| (id, i)));
            let intent = noted_intent.as_ref().map(|(_, i)| *i).unwrap_or(intent);
            let noted_ask = if own_ask { pick_own(item.id, "ask") } else { pick(item.id, "ask") }.and_then(|(id, v)| Ask::parse(&v).map(|a| (id, a)));
            let ask = noted_ask.as_ref().map(|(_, a)| *a).unwrap_or_else(|| ask_of(item.tags, intent));
            let classified_by = noted_intent.map(|(id, _)| id).or(noted_ask.map(|(id, _)| id));
            let unclassified = !own_intent && classified_by.is_none() && pick(item.id, "unclassified").is_some();
            // `done`: the author's own `done:` tag, unless the author annotated it again; else a person's annotation; else a model's.
            let own_done = tag_value(item.tags, "done:").and_then(done_named);
            let noted_done = if own_done.is_some() { pick_own(item.id, "done") } else { pick(item.id, "done") }.and_then(|(id, v)| done_named(&v).map(|d| (id, d)));
            let done: Option<(String, &'static str)> = noted_done.or_else(|| own_done.map(|d| (item.id.to_owned(), d)));
            let by_ms = by_of(item.tags);
            let answered_by = first_answer.get(item.id).map(|(_, id)| (*id).to_owned());
            // Deciders for THIS message: the room's roots and any seat it names with `to:`.
            // `to:` may be a list (`to:blaze,kepler`): each name is a seat.
            let named: Vec<&str> = item
                .tags
                .iter()
                .filter_map(|t| t.strip_prefix("to:"))
                .flat_map(|v| v.split(','))
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .collect();
            // Whose attention: the author's own `to:` tags; only when there are none, a person's annotation's `set:to=`.
            let to: Vec<String> = if named.is_empty() {
                pick(item.id, "to").map(|(_, v)| v.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).collect()).unwrap_or_default()
            } else {
                named.iter().map(|x| (*x).to_owned()).collect()
            };
            // The option a decide-ask was answered with: the earliest direct reply carrying `choice:<n>`.
            let choice = if ask == Ask::Decide {
                children
                    .get(item.id)
                    .into_iter()
                    .flatten()
                    .filter_map(|c| by_id.get(c))
                    .filter_map(|c| choice_of(c.tags).map(|n| (c.created_ms, n)))
                    .min_by_key(|(ms, _)| *ms)
                    .map(|(_, n)| n)
            } else {
                None
            };
            let decider = |who: &str| roots.iter().any(|r| crate::alias::same(aliases, r, who)) || named.iter().any(|n| crate::alias::same(aliases, n, who));
            let mut reacted = Marks::default();
            let mut settled: Option<(&'static str, String, i64)> = None;
            let has_items = declared.contains_key(item.id);
            if let Some(list) = marks.get(item.id) {
                let mut list = list.clone();
                list.sort_by_key(|(ms, _, _, _)| *ms);
                for (ms, r, who, id) in &list {
                    reacted.add(r, who);
                    // A mark that names items (`item:D5=dropped`) settles those items only, never the message
                    // (Blaze's ✓ D1–D4 and ✕ D5 read "declined" as a whole, 2026-09-27).
                    if let Some(ks) = mark_items.get(id) {
                        let all = declared.get(item.id).is_some_and(|d| d.iter().all(|(k, _, _)| ks.contains(k.as_str())));
                        if !all {
                            continue;
                        }
                    }
                    // Oldest first, so the newest decider verdict wins: a later no undoes a go.
                    let own_no = *r == "no" && *who == item.author;
                    if matches!(*r, "go" | "no") && (decider(who) || own_no) {
                        settled = settles(intent, r, *who == item.author, decider(item.author), has_items)
                            .map(|w| (w, (*id).to_owned(), *ms));
                    }
                }
            }
            // A decider's go on a REPLY answers the question it replies to.
            if settled.is_none() && intent == Intent::Question {
                let mut best: Option<(i64, &str)> = None;
                for child in children.get(item.id).map(|v| v.as_slice()).unwrap_or(&[]) {
                    for (ms, r, who, id) in marks.get(child).map(|v| v.as_slice()).unwrap_or(&[]) {
                        if *r == "go" && decider(who) && best.is_none_or(|(b, _)| *ms > b) {
                            best = Some((*ms, id));
                        }
                    }
                }
                settled = best.map(|(ms, id)| ("answered", id.to_owned(), ms));
            }
            let overdue = matches!(ask, Ask::Reply | Ask::Act | Ask::Decide | Ask::Review)
                && answered_by.is_none()
                && settled.is_none()
                && !matches!(state, State::Closed | State::Superseded | State::Archived)
                && by_ms.is_some_and(|b| b < now_ms);
            // A decider's ✓/✕ on a root moves every item still open with it, in that one record.
            let bulk = settled.as_ref().and_then(|(w, id, ms)| match *w {
                "accepted" => Some(("accepted", id.clone(), *ms)),
                "dropped" | "declined" => Some(("dropped", id.clone(), *ms)),
                _ => None,
            });
            let item_states = items_of(item, &decider, bulk);
            let fold = fold_items(&item_states);
            (
                item.id.to_owned(),
                Verdict {
                    state,
                    intent,
                    by,
                    replies,
                    ask,
                    by_ms,
                    answered_by,
                    overdue,
                    to,
                    choice,
                    classified_by,
                    unclassified,
                    items: item_states,
                    fold,
                    marks: reacted,
                    settled: settled.as_ref().map(|(w, _, _)| *w),
                    settled_at: settled.as_ref().map(|(_, _, ms)| *ms),
                    done_at: done.as_ref().and_then(|(id, _)| at.get(id.as_str()).copied()),
                    done_by: done.as_ref().map(|(id, _)| id.clone()),
                    done: done.as_ref().map(|(_, d)| *d),
                    settled_by: settled.map(|(_, id, _)| id),
                },
            )
        })
        .collect()
}

/// Which floor of a room a message belongs on. The author's `floor:` tag is the truth; without
/// one this guesses from a few plain words and SAYS it guessed, so a reader never mistakes a
/// keyword match for a decision someone made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Floor {
    pub name: String,
    pub guessed: bool,
}

pub fn floor_of(tags: &[String], content: &str) -> Floor {
    if let Some(name) = tag_value(tags, "floor:") {
        let clean: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(24)
            .collect();
        if !clean.is_empty() {
            return Floor {
                name: clean.to_ascii_lowercase(),
                guessed: false,
            };
        }
    }
    let text = content.to_ascii_lowercase();
    let hit = |words: &[&str]| words.iter().filter(|w| text.contains(*w)).count();
    let scores = [
        (
            "trading",
            hit(&[
                "trade",
                "position",
                "pnl",
                "futures",
                "market",
                "portfolio",
                "liquidat",
                "leverage",
                "entry",
                "stop loss",
                "btc",
                "eth",
            ]),
        ),
        (
            "engineering",
            hit(&[
                "deploy", "build", "bug", "commit", " pr ", "merge", "api", "ui ", "frontend",
                "backend", "refactor", "test",
            ]),
        ),
        (
            "ops",
            hit(&[
                "alert", "outage", "disk", "restart", "vps", "service", "down", "latency", "token",
                "auth",
            ]),
        ),
    ];
    let best = scores
        .iter()
        .max_by_key(|(_, n)| *n)
        .copied()
        .unwrap_or(("general", 0));
    Floor {
        name: if best.1 == 0 {
            "general".into()
        } else {
            best.0.into()
        },
        guessed: true,
    }
}

/// Which line of a thread's card a reply feeds. Finer than [`Intent`]: a card distinguishes work
/// that found something from work that did something, which the room view has no need to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Facet {
    Research,
    Action,
    Pending,
    Idea,
    Recommendation,
    Talk,
}

/// A reply's facet, from the most deliberate tag it carries. An open question is `Pending`
/// whoever asked it; once something closes it, it is just talk.
pub fn facet_of(tags: &[String], state: State) -> Facet {
    let named = tag_value(tags, "intent:")
        .or_else(|| tag_value(tags, "kind:"))
        .unwrap_or("");
    match named {
        "result" | "research" | "learning" | "finding" | "evidence" | "report" => Facet::Research,
        "action" | "receipt" | "ack" | "shipped" | "done" => Facet::Action,
        "question" | "ask" | "request" if state == State::Open => Facet::Pending,
        "proposal" | "idea" | "rip" | "experiment" => Facet::Idea,
        "decision" | "ruling" | "verdict" | "recommendation" | "direction" => Facet::Recommendation,
        _ => Facet::Talk,
    }
}

/// One root message and what happened under it, as a card that is never typed by hand.
#[derive(Debug, Clone, Serialize)]
pub struct Card {
    pub replies: u32,
    pub research: u32,
    pub actions: u32,
    pub pending: u32,
    pub ideas: u32,
    /// The newest recommendation under the root, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    pub participants: Vec<String>,
    pub last_activity_ms: i64,
    /// How far the thread has got: `asked` → `researched` → `proposed` → `decided` → `done`. The
    /// furthest step reached, so five hundred replies that are all research still read `researched`:
    /// volume is not progress, a new KIND of reply is.
    pub stage: &'static str,
    /// Questions raised inside the thread, and how many are still open — the honest progress count.
    pub questions: u32,
    pub questions_open: u32,
    /// The newest `intent:summary` reply: where a long thread is read from, instead of message one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// `waiting` while anything under it is pending, `answered` once the root is resolved or has a
    /// recommendation, `quiet` when nobody replied, otherwise `active`.
    pub status: &'static str,
}

/// Roll a thread up. `members` are the replies (any depth), already judged.
pub fn card(root: &Item<'_>, root_state: State, members: &[(Item<'_>, State)]) -> Card {
    let mut c = Card {
        replies: members.len() as u32,
        research: 0,
        actions: 0,
        pending: 0,
        ideas: 0,
        recommendation: None,
        participants: vec![],
        last_activity_ms: root.created_ms,
        stage: "asked",
        questions: 0,
        questions_open: 0,
        summary: None,
        status: "quiet",
    };
    let (mut newest_rec, mut newest_sum) = (i64::MIN, i64::MIN);
    let members: Vec<&(Item<'_>, State)> = members.iter().filter(|(m, _)| is_speech(m.tags)).collect();
    c.replies = members.len() as u32;
    for (m, state) in members.iter().map(|x| (&x.0, &x.1)) {
        let named = tag_value(m.tags, "intent:")
            .or_else(|| tag_value(m.tags, "kind:"))
            .unwrap_or("");
        if matches!(named, "question" | "ask" | "request") {
            c.questions += 1;
            c.questions_open += u32::from(*state == State::Open);
        }
        if matches!(named, "summary" | "recap") && m.created_ms > newest_sum {
            newest_sum = m.created_ms;
            c.summary = Some(m.id.to_owned());
        }
        match facet_of(m.tags, *state) {
            Facet::Research => c.research += 1,
            Facet::Action => c.actions += 1,
            Facet::Pending => c.pending += 1,
            Facet::Idea => c.ideas += 1,
            Facet::Recommendation => {
                if m.created_ms > newest_rec {
                    newest_rec = m.created_ms;
                    c.recommendation = Some(m.id.to_owned());
                }
            }
            Facet::Talk => {}
        }
        if !c.participants.iter().any(|p| p == m.author) {
            c.participants.push(m.author.to_owned());
        }
        c.last_activity_ms = c.last_activity_ms.max(m.created_ms);
    }
    c.status = if c.pending > 0 {
        "waiting"
    } else if matches!(root_state, State::Closed | State::Superseded) || c.recommendation.is_some()
    {
        "answered"
    } else if members.is_empty() {
        "quiet"
    } else {
        "active"
    };
    c.stage = if matches!(root_state, State::Closed) {
        "done"
    } else if c.recommendation.is_some() {
        "decided"
    } else if c.ideas > 0 {
        "proposed"
    } else if c.research > 0 {
        // Only a finding researches. A receipt says something shipped; it is counted in `actions`
        // and does not pretend the question was looked into (Jobs, 2026-09-27).
        "researched"
    } else {
        "asked"
    };
    c
}

/// What a room shows by default: anything still open, plus every voice that is never filtered.
///
/// The second half is the whole point. A room where the owner's own words age out is a room
/// nobody trusts, so named voices stay visible whatever the clock says.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// `None` means the caller asked for no filter. `Some(set)` is exactly what they asked for,
    /// and an empty set matches nothing — a misspelled filter must show an empty room, never the
    /// whole room, or a typo silently turns the feature off and everything looks fine.
    pub states: Option<HashSet<State>>,
    pub keep_authors: HashSet<String>,
}

impl View {
    /// `states` is a comma list; empty or "all" means no state filter.
    pub fn parse(states: Option<&str>, keep: Option<&str>) -> View {
        let keep_authors = keep
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let states = match states.map(str::trim) {
            None | Some("") | Some("all") => None,
            Some(list) => Some(
                list.split(',')
                    .map(str::trim)
                    .filter_map(State::parse)
                    .collect(),
            ),
        };
        View {
            states,
            keep_authors,
        }
    }
    pub fn filters(&self) -> bool {
        self.states.is_some()
    }
    pub fn shows(&self, author: &str, state: State) -> bool {
        match &self.states {
            None => true,
            Some(wanted) => self.keep_authors.contains(author) || wanted.contains(&state),
        }
    }
}

/// What a room thread IS, so one server answers every view (Blaze, 2026-09-21: "all filters have
/// separate, separate things you are hand rolling randomly; that is causing all the chaos").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A person is in it: a person started it, replied in it, or an agent addressed one.
    People,
    /// Agents talking to each other.
    Agents,
    /// A feed or other machine output (`origin:automated`, a `[Room book]` snapshot).
    Automated,
    /// The room's own records (task cards, member configs, message-state records): never a thread.
    Bookkeeping,
}

/// Classify a thread by its root and the seats taking part. `people` are the room's humans.
pub fn kind_of(
    root_author: &str,
    root_tags: &[String],
    root_content: &str,
    participants: &[String],
    people: &[String],
) -> Kind {
    let face = root_content.trim_start();
    if face.starts_with('[') {
        let head = face.split(']').next().unwrap_or("");
        if head.contains(" task:")
            || head.contains(" problem:")
            || head.ends_with(" members")
            || head.starts_with("[Message state:")
        {
            return Kind::Bookkeeping;
        }
        if head.ends_with(" book") {
            return Kind::Automated;
        }
    }
    if root_tags.iter().any(|t| t == "origin:automated") {
        return Kind::Automated;
    }
    let is_person = |who: &str| people.iter().any(|p| p == who);
    let addressed = root_tags
        .iter()
        .filter_map(|t| t.strip_prefix("to:"))
        .any(is_person)
        || people.iter().any(|p| mentions(root_content, p));
    if is_person(root_author) || participants.iter().any(|p| is_person(p)) || addressed {
        Kind::People
    } else {
        Kind::Agents
    }
}

/// `@name` as a whole word (so `@hustler-book` does not mention `@hustler`).
fn mentions(text: &str, name: &str) -> bool {
    let needle = format!("@{name}");
    text.match_indices(&needle).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + needle.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '@')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_')
    })
}

/// Where a thread stands, as one word a filter can match. Archived and closed come from the ledger
/// (`archives:` / `closes:` markers); the rest from the card and from what the root ASKED.
/// `needs_answer` is earned by an ask (a question, a proposal, an `ask:` of reply/act) that nobody
/// has answered, or by a thread whose replies are still pending — whoever wrote it. `quiet` is
/// simply "no replies". A person's note that asks nothing is quiet, not owed an answer
/// (Blaze, 2026-09-22: the room said 38 needed answers and he could not see why).
pub fn bucket_of(root_state: State, card: &Card, root: Option<&Verdict>) -> &'static str {
    let unanswered_ask = root.is_some_and(|v| {
        matches!(v.ask, Ask::Reply | Ask::Act) && v.answered_by.is_none() && v.settled.is_none()
    });
    // A decider's verdict outranks the card: settled is settled, whatever is still under it.
    let settled = root.and_then(|v| v.settled);
    match root_state {
        State::Archived => "archived",
        State::Closed | State::Superseded => "closed",
        _ if settled.is_some() => "answered",
        _ if card.status == "waiting" => "needs_answer",
        _ if unanswered_ask && card.status != "answered" => "needs_answer",
        _ if card.status == "answered" => "answered",
        _ if card.status == "quiet" => "quiet",
        _ => "in_progress",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_thread_moves_by_kind_of_reply_not_by_volume() {
        let t = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let root_tags = t(&[]);
        let root = Item {
            id: "r",
            author: "blaze",
            created_ms: 0,
            tags: &root_tags,
        };
        let mk = |tags: &'static [&'static str]| t(tags);
        let (research, question, idea, decision, summary) = (
            mk(&["intent:result"]),
            mk(&["intent:question"]),
            mk(&["intent:proposal"]),
            mk(&["intent:decision"]),
            mk(&["intent:summary"]),
        );
        let item = |id: &'static str, at: i64, tags: &'static Vec<String>| Item {
            id,
            author: "a",
            created_ms: at,
            tags,
        };
        let research: &'static Vec<String> = Box::leak(Box::new(research));
        let question: &'static Vec<String> = Box::leak(Box::new(question));
        let idea: &'static Vec<String> = Box::leak(Box::new(idea));
        let decision: &'static Vec<String> = Box::leak(Box::new(decision));
        let summary: &'static Vec<String> = Box::leak(Box::new(summary));

        assert_eq!(card(&root, State::Open, &[]).stage, "asked");
        // Two hundred research replies are still only "researched".
        let many: Vec<(Item, State)> = (0..200)
            .map(|i| (item("x", i, research), State::Ambient))
            .collect();
        assert_eq!(card(&root, State::Open, &many).stage, "researched");
        let mut m = vec![
            (item("a", 1, research), State::Ambient),
            (item("q1", 2, question), State::Open),
            (item("q2", 3, question), State::Closed),
        ];
        let c = card(&root, State::Open, &m);
        assert_eq!(
            (c.stage, c.questions, c.questions_open),
            ("researched", 2, 1)
        );
        m.push((item("p", 4, idea), State::Open));
        assert_eq!(card(&root, State::Open, &m).stage, "proposed");
        m.push((item("s1", 5, summary), State::Ambient));
        m.push((item("s2", 9, summary), State::Ambient));
        m.push((item("d", 6, decision), State::Open));
        let c = card(&root, State::Open, &m);
        assert_eq!(
            (c.stage, c.summary.as_deref(), c.recommendation.as_deref()),
            ("decided", Some("s2"), Some("d"))
        );
        assert_eq!(card(&root, State::Closed, &m).stage, "done");
    }

    use super::*;

    const T0: i64 = 1_700_000_000_000;
    fn item<'a>(id: &'a str, author: &'a str, hours_ago: i64, tags: &'a [String]) -> Item<'a> {
        Item {
            id,
            author,
            created_ms: T0 - hours_ago * HOUR_MS,
            tags,
        }
    }
    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }
    fn state_of(v: &HashMap<String, Verdict>, id: &str) -> State {
        v[id].state
    }

    #[test]
    fn a_result_is_its_own_intent_and_researches_a_thread_where_a_receipt_does_not() {
        let (result, receipt) = (tags(&["intent:result"]), tags(&["intent:receipt"]));
        assert_eq!(intent_of(&result), Intent::Result);
        assert_eq!(intent_of(&result).as_str(), "result");
        assert_eq!(intent_of(&receipt), Intent::Receipt);
        let root_tags = tags(&["intent:question"]);
        let root = item("root", "blaze", 5, &root_tags);
        // Negative control: a receipt alone leaves the thread asked.
        let shipped = vec![(item("s", "rocky", 4, &receipt), State::Ambient)];
        let c = card(&root, State::Open, &shipped);
        assert_eq!((c.stage, c.actions, c.research), ("asked", 1, 0));
        let found = vec![(item("f", "taylor-core", 4, &result), State::Ambient)];
        let c = card(&root, State::Open, &found);
        assert_eq!((c.stage, c.research), ("researched", 1));
        // Both are ambient: neither holds the room open.
        let v = judge(&[item("f", "taylor-core", 400, &result)], T0);
        assert_eq!(v["f"].state, State::Ambient);
    }

    #[test]
    fn a_reaction_is_counted_on_its_message_and_is_never_a_reply_or_an_answer() {
        let root_tags = tags(&["intent:question"]);
        let (go, go_again, seen, speech) = (
            tags(&["reply:root", "react:go"]),
            tags(&["reply:root", "react:go"]),
            tags(&["re:root", "react:seen"]),
            tags(&["reply:root"]),
        );
        let only_reactions = [
            item("root", "blaze", 5, &root_tags),
            item("g1", "bobby-core", 4, &go),
            item("g2", "bobby-core", 3, &go_again),
            item("s1", "taylor-core", 2, &seen),
        ];
        let v = judge(&only_reactions, T0);
        let r = &v["root"];
        assert_eq!(r.replies, 0, "a reaction is not a reply");
        assert!(r.answered_by.is_none(), "a reaction answers nothing");
        assert_eq!(r.marks.go, ["bobby-core"], "a seat counts once per reaction");
        assert_eq!(r.marks.seen, ["taylor-core"]);
        assert!(r.marks.like.is_empty());
        let wire = serde_json::to_value(r).unwrap();
        assert_eq!(wire["marks"]["go"], serde_json::json!(["bobby-core"]));
        // Negative control: the same link without react: is a reply and an answer.
        let answered = [item("root", "blaze", 5, &root_tags), item("a", "bobby-core", 4, &speech)];
        let v = judge(&answered, T0);
        assert_eq!((v["root"].replies, v["root"].answered_by.as_deref()), (1, Some("a")));
        assert!(serde_json::to_value(&v["root"]).unwrap().get("marks").is_none(), "none is absent");
        // A card does not count a reaction as a reply or a participant.
        let members = vec![(item("g1", "bobby-core", 4, &go), State::Open)];
        let c = card(&only_reactions[0], State::Open, &members);
        assert_eq!((c.replies, c.status, c.participants.len()), (0, "quiet", 0));
    }

    #[test]
    fn the_four_marks_come_from_either_door_and_a_withdrawn_mark_is_gone() {
        let root_tags = tags(&["intent:proposal"]);
        let (legacy_no, update_go, update_like, withdraw) = (
            tags(&["reply:root", "react:no"]),
            tags(&["annotates:root", "set:mark=go"]),
            tags(&["annotates:root", "set:mark=like"]),
            tags(&["archives:l1"]),
        );
        let bogus = tags(&["annotates:root", "set:mark=maybe"]);
        let items = [
            item("root", "blaze", 6, &root_tags),
            item("n1", "jobs-core", 5, &legacy_no),
            item("g1", "blaze", 4, &update_go),
            item("l1", "taylor-core", 3, &update_like),
            item("w1", "taylor-core", 2, &withdraw),
            item("x1", "bobby-core", 2, &bogus),
        ];
        let v = judge(&items, T0);
        let m = &v["root"].marks;
        assert_eq!((m.no.as_slice(), m.go.as_slice()), (&["jobs-core".to_string()][..], &["blaze".to_string()][..]));
        assert!(m.like.is_empty(), "a withdrawn mark is gone");
        assert!(m.seen.is_empty(), "an unknown mark is no mark");
        assert_eq!((v["root"].replies, v["root"].answered_by.as_deref()), (0, None), "marks are never replies");
        // Negative control: without the withdrawal the like counts.
        let v = judge(&items[..4], T0);
        assert_eq!(v["root"].marks.like, ["taylor-core"]);
    }

    fn roots() -> Vec<String> {
        vec!["blaze".into(), "hustler".into()]
    }

    #[test]
    fn a_deciders_go_settles_and_anyone_elses_is_only_counted() {
        let prop = tags(&["intent:proposal"]);
        let (go, no) = (tags(&["annotates:p", "set:mark=go"]), tags(&["annotates:p", "set:mark=no"]));
        let opinion = [item("p", "rocky", 6, &prop), item("o", "taylor-core", 5, &go)];
        let v = judge_in(&opinion, T0, &roots());
        assert_eq!(v["p"].marks.go, ["taylor-core"], "counted and named");
        assert_eq!(v["p"].settled, None, "an agent's go is an opinion");
        let accepted = [item("p", "rocky", 6, &prop), item("o", "taylor-core", 5, &go), item("g", "blaze", 4, &go)];
        let v = judge_in(&accepted, T0, &roots());
        assert_eq!((v["p"].settled, v["p"].settled_by.as_deref()), (Some("accepted"), Some("g")));
        // Negative control: the same marks with no roots settle nothing.
        assert_eq!(judge_in(&accepted, T0, &[])["p"].settled, None);
        // The newest decider verdict wins: a later no declines it.
        let reversed = [item("p", "rocky", 6, &prop), item("g", "blaze", 4, &go), item("n", "hustler", 3, &no)];
        assert_eq!(judge_in(&reversed, T0, &roots())["p"].settled, Some("declined"));
    }

    #[test]
    fn a_question_is_settled_by_the_seat_it_asks_or_a_root_and_answered_through_a_reply() {
        let q = tags(&["intent:question", "to:bobby-core"]);
        let reply = tags(&["reply:q"]);
        let (go_q, go_r) = (tags(&["annotates:q", "set:mark=go"]), tags(&["annotates:r", "set:mark=go"]));
        let asked = [item("q", "rocky", 6, &q), item("m", "bobby-core", 5, &go_q)];
        assert_eq!(judge_in(&asked, T0, &roots())["q"].settled, Some("answered"), "the asked seat decides");
        let answered = [item("q", "rocky", 6, &q), item("r", "wags-core", 5, &reply), item("m", "blaze", 4, &go_r)];
        let v = judge_in(&answered, T0, &roots());
        assert_eq!((v["q"].settled, v["q"].settled_by.as_deref()), (Some("answered"), Some("m")));
        // Negative control: a go on that reply from a seat that is not a decider answers nothing.
        let opinion = [item("q", "rocky", 6, &q), item("r", "wags-core", 5, &reply), item("m", "taylor-core", 4, &go_r)];
        assert_eq!(judge_in(&opinion, T0, &roots())["q"].settled, None);
    }

    #[test]
    fn each_kind_of_message_settles_its_own_way_and_like_never_does() {
        let (res, rec, dec, note) = (tags(&["intent:result"]), tags(&["intent:receipt"]), tags(&["intent:decision"]), tags(&[]));
        let go = tags(&["annotates:x", "set:mark=go"]);
        let no = tags(&["annotates:x", "set:mark=no"]);
        let like = tags(&["annotates:x", "set:mark=like"]);
        let one = |t: &Vec<String>, author: &'static str, mark: &Vec<String>, by: &'static str| {
            let items = [item("x", author, 6, t), item("m", by, 5, mark)];
            judge_in(&items, T0, &roots())["x"].settled
        };
        assert_eq!(one(&res, "taylor-core", &go, "blaze"), Some("confirmed"));
        assert_eq!(one(&rec, "rocky", &go, "blaze"), Some("confirmed"), "verified is retired");
        assert_eq!(one(&dec, "rocky", &go, "blaze"), Some("accepted"), "a decision by someone else");
        assert_eq!(one(&dec, "rocky", &no, "blaze"), Some("dropped"));
        assert_eq!(one(&dec, "hustler", &go, "blaze"), None, "a root's decision is decided already");
        assert_eq!(one(&note, "rocky", &no, "rocky"), Some("withdrawn"), "the author's own no");
        assert_eq!(one(&res, "taylor-core", &like, "blaze"), None, "like is never a state");
    }

    #[test]
    fn a_deciders_mark_on_a_root_moves_every_open_item_and_an_agents_moves_nothing() {
        let root = tags(&["item:R1=requirement", "item:R2=requirement", "item:R3=question"]);
        let r3_dropped = tags(&["reply:m", "item:R3=dropped"]);
        let (go, no) = (tags(&["annotates:m", "set:mark=go"]), tags(&["annotates:m", "set:mark=no"]));
        let states = |v: &HashMap<String, Verdict>| v["m"].items.iter().map(|i| (i.k.clone(), i.state)).collect::<Vec<_>>();
        let accepted = [item("m", "rocky", 6, &root), item("d", "wags-core", 5, &r3_dropped), item("g", "blaze", 4, &go)];
        let v = judge_in(&accepted, T0, &roots());
        assert_eq!(states(&v), [("R1".into(), "accepted"), ("R2".into(), "accepted"), ("R3".into(), "dropped")]);
        assert_eq!((v["m"].settled, v["m"].items[0].by.as_deref(), v["m"].settled_at), (Some("accepted"), Some("g"), Some(T0 - 4 * HOUR_MS)));
        let dropped = [item("m", "rocky", 6, &root), item("n", "hustler", 4, &no)];
        let v = judge_in(&dropped, T0, &roots());
        assert!(v["m"].items.iter().all(|i| i.state == "dropped"));
        // Negative control: an agent's ✓ moves nothing.
        let agent = [item("m", "rocky", 6, &root), item("g", "taylor-core", 4, &go)];
        let v = judge_in(&agent, T0, &roots());
        assert!(v["m"].items.iter().all(|i| i.state == "open") && v["m"].settled.is_none());
    }

    #[test]
    fn an_item_is_accepted_by_its_author_a_named_seat_or_a_root_and_by_nobody_else() {
        let root = tags(&["item:R1=requirement", "to:jobs-core"]);
        let accept = tags(&["reply:m", "item:R1=accepted"]);
        let by = |who: &'static str| {
            let items = [item("m", "rocky", 6, &root), item("a", who, 5, &accept)];
            judge_in(&items, T0, &roots())["m"].items[0].state
        };
        assert_eq!(by("blaze"), "accepted", "a root, on a message Rocky wrote");
        assert_eq!(by("jobs-core"), "accepted", "the seat it names");
        assert_eq!(by("rocky"), "accepted", "the author closing their own");
        assert_eq!(by("taylor-core"), "open", "an unrelated seat's accepted is an opinion");
    }

    #[test]
    fn a_mark_on_some_items_settles_those_items_not_the_message() {
        // Blaze on the DOT slices (2026-09-27): ✓ D1, ✓ D2, ✕ D3, one mark each. The message read "declined".
        let q = tags(&["intent:question", "ask:act", "to:blaze", "item:D1=decision", "item:D2=decision", "item:D3=decision"]);
        let (g1, g2, n3) = (tags(&["reply:q", "react:go", "item:D1=accepted"]), tags(&["reply:q", "react:go", "item:D2=accepted"]), tags(&["reply:q", "react:no", "item:D3=dropped"]));
        let v = judge_in(&[item("q", "rocky", 6, &q), item("a", "blaze", 5, &g1), item("b", "blaze", 4, &g2), item("c", "blaze", 3, &n3)], T0, &roots());
        assert_eq!(v["q"].settled, None, "no single item mark speaks for the message");
        assert_eq!(v["q"].items.iter().map(|i| i.state).collect::<Vec<_>>(), vec!["accepted", "accepted", "dropped"]);
        assert_eq!(v["q"].fold, Some("closed"), "every item settled: the thread word is closed");
        // Negative control: the rail's ✓ on the root names every open item, and that settles the message.
        let all = tags(&["reply:q", "react:go", "item:D1=accepted", "item:D2=accepted", "item:D3=accepted"]);
        let v = judge_in(&[item("q", "rocky", 6, &q), item("a", "blaze", 5, &all)], T0, &roots());
        assert_eq!(v["q"].settled, Some("answered"));
    }

    #[test]
    fn a_seat_that_classifies_again_replaces_its_items() {
        // Wags classified Blaze's 21:14 message twice: Q1/C1/Q2, then R1–R4. The message carried all seven.
        let m = tags(&[]);
        let first = tags(&["annotates:m", "set:by=model", "item:Q1=question", "item:C1=complaint"]);
        let second = tags(&["annotates:m", "set:by=model", "item:R1=question", "item:R2=complaint"]);
        let other = tags(&["annotates:m", "item:P1=ask"]);
        let v = judge_in(&[item("m", "blaze", 6, &m), item("w1", "wags-core", 5, &first), item("w2", "wags-core", 4, &second), item("o", "jobs-core", 3, &other)], T0, &roots());
        let keys: Vec<&str> = v["m"].items.iter().map(|i| i.k.as_str()).collect();
        assert_eq!(keys, vec!["R1", "R2", "P1"], "Wags' newest pass replaces its first; another seat's items stay");
        let from: Vec<Option<&str>> = v["m"].items.iter().map(|i| i.from.as_deref()).collect();
        assert_eq!(from, vec![Some("w2"), Some("w2"), Some("o")], "each item names the annotation that declared it");
    }

    #[test]
    fn a_settled_root_is_never_owed_an_answer() {
        let q = tags(&["intent:question"]);
        let no = tags(&["annotates:q", "set:mark=no"]);
        let items = [item("q", "rocky", 6, &q), item("m", "blaze", 5, &no)];
        let v = judge_in(&items, T0, &roots());
        let c = card(&items[0], State::Open, &[]);
        assert_eq!(v["q"].settled, Some("declined"), "a mark never says closed");
        assert_eq!(bucket_of(State::Open, &c, v.get("q")), "answered");
        // Negative control: unsettled, the same question needs an answer.
        let v = judge(&items[..1], T0);
        assert_eq!(bucket_of(State::Open, &c, v.get("q")), "needs_answer");
    }

    #[test]
    fn a_state_update_is_never_a_reply_an_answer_or_a_thread_member() {
        let root_tags = tags(&["intent:question"]);
        // wags-core's shape: it names the message, and some carry a reply link as well.
        let update = tags(&["annotates:root", "reply:root", "set:ask=none", "set:by=model"]);
        let v = judge(&[item("root", "blaze", 5, &root_tags), item("u", "wags-core", 4, &update)], T0);
        assert!(is_state_update(&update) && !is_speech(&update));
        assert_eq!(v["root"].replies, 0, "a state update is not a reply");
        assert!(v["root"].answered_by.is_none(), "a state update answers nothing");
        assert_eq!(v["root"].ask, Ask::None, "but it is read back as the message's derived fields");
        assert_eq!(v["root"].classified_by.as_deref(), Some("u"));
        let c = card(&item("root", "blaze", 5, &root_tags), State::Open, &[(item("u", "wags-core", 4, &update), State::Open)]);
        assert_eq!((c.replies, c.status), (0, "quiet"));
        // Negative control: without annotates: it is speech, and it answers.
        let reply = tags(&["reply:root", "set:ask=none"]);
        assert!(is_speech(&reply));
        let v = judge(&[item("root", "blaze", 5, &root_tags), item("u", "wags-core", 4, &reply)], T0);
        assert_eq!((v["root"].replies, v["root"].answered_by.as_deref()), (1, Some("u")));
    }

    #[test]
    fn a_message_with_no_tags_is_a_note_and_ages_out_by_itself() {
        let (fresh, old) = (tags(&[]), tags(&[]));
        let items = [item("a", "bobby", 1, &fresh), item("b", "bobby", 48, &old)];
        let v = judge(&items, T0);
        assert_eq!(state_of(&v, "a"), State::Open);
        assert_eq!(
            state_of(&v, "b"),
            State::Expired,
            "the backlog collapses without anyone touching it"
        );
        assert_eq!(v["b"].intent, Intent::Note);
    }

    #[test]
    fn a_question_holds_the_room_until_answered_and_an_answer_beats_the_clock() {
        let q = tags(&["kind:question"]);
        let answer = tags(&["closes:q1"]);
        let items = [item("q1", "rocky", 96, &q), item("a1", "blaze", 1, &answer)];
        assert_eq!(
            state_of(&judge(&items[..1], T0), "q1"),
            State::Expired,
            "96h with no answer"
        );
        let v = judge(&items, T0);
        assert_eq!(state_of(&v, "q1"), State::Closed);
        assert_eq!(v["q1"].by.as_deref(), Some("a1"));
    }

    #[test]
    fn archive_beats_supersede_beats_close_whatever_order_they_arrive_in() {
        let (t, c, s, a) = (
            tags(&["kind:question"]),
            tags(&["closes:t"]),
            tags(&["supersedes:t"]),
            tags(&["archives:t"]),
        );
        let all = [
            item("t", "x", 5, &t),
            item("c", "x", 4, &c),
            item("s", "x", 3, &s),
            item("a", "x", 2, &a),
        ];
        assert_eq!(state_of(&judge(&all, T0), "t"), State::Archived);
        let reversed: Vec<_> = all.iter().rev().cloned().collect();
        assert_eq!(
            state_of(&judge(&reversed, T0), "t"),
            State::Archived,
            "order must not change a verdict"
        );
        assert_eq!(state_of(&judge(&all[..3], T0), "t"), State::Superseded);
        assert_eq!(state_of(&judge(&all[..2], T0), "t"), State::Closed);
    }

    #[test]
    fn nothing_can_resolve_itself_or_be_resolved_from_the_future() {
        let own = tags(&["kind:question", "closes:loop"]);
        assert_eq!(
            state_of(&judge(&[item("loop", "x", 1, &own)], T0), "loop"),
            State::Open
        );
        let (q, late) = (tags(&["kind:question"]), tags(&["closes:q"]));
        let items = [item("q", "x", 1, &q), item("later", "x", 5, &late)];
        assert_eq!(
            state_of(&judge(&items, T0), "q"),
            State::Open,
            "a message older than the question cannot answer it"
        );
    }

    #[test]
    fn receipts_and_status_never_compete_for_the_view_however_old_they_are() {
        let (r, s) = (tags(&["kind:receipt"]), tags(&["kind:status"]));
        let items = [item("r", "rocky", 9000, &r), item("s", "rocky", 1, &s)];
        let v = judge(&items, T0);
        assert_eq!(
            (state_of(&v, "r"), state_of(&v, "s")),
            (State::Ambient, State::Ambient)
        );
    }

    #[test]
    fn a_decision_stands_until_something_replaces_it() {
        let (d, s) = (tags(&["kind:decision"]), tags(&["supersedes:d"]));
        assert_eq!(
            state_of(&judge(&[item("d", "blaze", 9000, &d)], T0), "d"),
            State::Open
        );
        let items = [item("d", "blaze", 9000, &d), item("d2", "blaze", 1, &s)];
        assert_eq!(state_of(&judge(&items, T0), "d"), State::Superseded);
    }

    #[test]
    fn an_explicit_ttl_is_honoured_but_clamped_and_zero_means_no_fuse() {
        let (short, forever, silly) = (
            tags(&["kind:note", "ttl:1"]),
            tags(&["kind:note", "ttl:0"]),
            tags(&["kind:note", "ttl:99999999"]),
        );
        let items = [
            item("s", "x", 2, &short),
            item("f", "x", 9000, &forever),
            item("l", "x", 5000, &silly),
        ];
        let v = judge(&items, T0);
        assert_eq!(state_of(&v, "s"), State::Expired);
        assert_eq!(state_of(&v, "f"), State::Open);
        assert_eq!(
            state_of(&v, "l"),
            State::Open,
            "clamped to a year, so still open at 5000h"
        );
        let past_clamp = [item("p", "x", 9000, &silly)];
        assert_eq!(
            state_of(&judge(&past_clamp, T0), "p"),
            State::Expired,
            "the clamp is real: 9000h is past a year"
        );
    }

    #[test]
    fn the_storage_rail_is_not_mistaken_for_intent() {
        // 71k messages carry kind:say. If that read as an intent the whole ledger would be mislabelled.
        for rail in ["say", "state", "memory"] {
            assert_eq!(
                intent_of(&tags(&[&format!("kind:{rail}")])),
                Intent::Note,
                "{rail}"
            );
        }
        assert_eq!(intent_of(&tags(&["kind:proposal"])), Intent::Proposal);
        assert_eq!(intent_of(&tags(&["kind:verdict"])), Intent::Decision);
        assert_eq!(intent_of(&tags(&["kind:ack"])), Intent::Receipt);
        // A deliberate intent: tag outranks the rail carried in kind:.
        assert_eq!(
            intent_of(&tags(&["kind:say", "intent:question"])),
            Intent::Question
        );
    }

    #[test]
    fn every_spelling_of_a_reply_is_counted_and_none_of_them_resolve() {
        let q = tags(&["kind:question"]);
        let (a, b, c) = (
            tags(&["re:q"]),
            tags(&["reply:q"]),
            tags(&["in_reply_to:q"]),
        );
        let items = [
            item("q", "x", 1, &q),
            item("a", "y", 1, &a),
            item("b", "y", 1, &b),
            item("c", "y", 1, &c),
        ];
        let v = judge(&items, T0);
        assert_eq!((v["q"].replies, state_of(&v, "q")), (3, State::Open));
    }

    #[test]
    fn replies_are_counted_without_resolving_anything() {
        let (q, r1, r2) = (tags(&["kind:question"]), tags(&["re:q"]), tags(&["re:q"]));
        let items = [
            item("q", "x", 1, &q),
            item("a", "y", 1, &r1),
            item("b", "z", 1, &r2),
        ];
        let v = judge(&items, T0);
        assert_eq!((v["q"].replies, state_of(&v, "q")), (2, State::Open));
    }

    #[test]
    fn a_floor_tag_is_the_truth_and_a_guess_always_says_it_is_one() {
        let tagged = tags(&["floor:Trading"]);
        assert_eq!(
            floor_of(&tagged, "deploy the api"),
            Floor {
                name: "trading".into(),
                guessed: false
            }
        );
        let none = tags(&[]);
        assert_eq!(
            floor_of(&none, "open positions and pnl on the futures book"),
            Floor {
                name: "trading".into(),
                guessed: true
            }
        );
        assert_eq!(
            floor_of(&none, "deploy the build, the commit has a bug"),
            Floor {
                name: "engineering".into(),
                guessed: true
            }
        );
        assert_eq!(
            floor_of(&none, "disk alert, the vps service is down"),
            Floor {
                name: "ops".into(),
                guessed: true
            }
        );
        assert_eq!(
            floor_of(&none, "hello everyone"),
            Floor {
                name: "general".into(),
                guessed: true
            }
        );
        let hostile = tags(&["floor:<script>../etc"]);
        assert_eq!(
            floor_of(&hostile, "x").name,
            "scriptetc",
            "only letters, digits and dashes survive"
        );
    }

    #[test]
    fn a_card_is_rolled_up_from_the_replies_and_never_typed() {
        let root_tags = tags(&[]);
        let root = item("root", "blaze", 10, &root_tags);
        let (r, a, q, i, d1, d2, chat) = (
            tags(&["intent:result"]),
            tags(&["kind:receipt"]),
            tags(&["intent:question"]),
            tags(&["kind:proposal"]),
            tags(&["intent:decision"]),
            tags(&["intent:recommendation"]),
            tags(&[]),
        );
        let members = vec![
            (item("r", "taylor-core", 9, &r), State::Open),
            (item("a", "bobby-core", 8, &a), State::Ambient),
            (item("q", "wags-core", 7, &q), State::Open),
            (item("i", "rocky", 6, &i), State::Open),
            (item("d1", "rocky", 5, &d1), State::Open),
            (item("d2", "emmy", 2, &d2), State::Open),
            (item("c", "taylor-core", 1, &chat), State::Open),
        ];
        let c = card(&root, State::Expired, &members);
        assert_eq!(
            (c.replies, c.research, c.actions, c.pending, c.ideas),
            (7, 1, 1, 1, 1)
        );
        assert_eq!(
            c.recommendation.as_deref(),
            Some("d2"),
            "the newest recommendation wins"
        );
        assert_eq!(
            c.status, "waiting",
            "an open question under it outranks a recommendation"
        );
        assert_eq!(
            c.participants,
            ["taylor-core", "bobby-core", "wags-core", "rocky", "emmy"]
        );
        assert_eq!(c.last_activity_ms, T0 - HOUR_MS);
    }

    #[test]
    fn a_card_status_follows_what_is_under_the_root() {
        let none = tags(&[]);
        let root = item("root", "blaze", 10, &none);
        assert_eq!(card(&root, State::Open, &[]).status, "quiet");
        let q = tags(&["intent:question"]);
        let answered_q = vec![(item("q", "x", 5, &q), State::Closed)];
        assert_eq!(
            card(&root, State::Open, &answered_q).status,
            "active",
            "a closed question is no longer pending"
        );
        assert_eq!(card(&root, State::Closed, &answered_q).status, "answered");
        let d = tags(&["intent:decision"]);
        assert_eq!(
            card(&root, State::Open, &[(item("d", "x", 5, &d), State::Open)]).status,
            "answered"
        );
    }

    #[test]
    fn the_default_room_view_keeps_named_voices_however_old_they_are() {
        let view = View::parse(Some("open"), Some("blaze,hustler"));
        assert!(
            view.shows("blaze", State::Expired),
            "the owner's words never vanish"
        );
        assert!(view.shows("hustler", State::Ambient));
        assert!(!view.shows("bobby-core", State::Expired));
        assert!(view.shows("bobby-core", State::Open));
    }

    #[test]
    fn no_state_filter_and_all_show_everything() {
        for spec in [None, Some(""), Some("all")] {
            let view = View::parse(spec, None);
            assert!(!view.filters());
            for s in [State::Open, State::Expired, State::Archived, State::Ambient] {
                assert!(view.shows("anyone", s), "{spec:?}");
            }
        }
        let two = View::parse(Some("open,closed"), None);
        assert!(two.shows("x", State::Closed) && !two.shows("x", State::Expired));
        assert!(
            !View::parse(Some("nonsense"), None).shows("x", State::Open),
            "an unknown state is not a wildcard"
        );
    }

    #[test]
    fn a_real_room_backlog_collapses_to_the_few_things_that_still_matter() {
        let (noise, mine, ask) = (tags(&[]), tags(&[]), tags(&["kind:question"]));
        let mut items: Vec<Item> = (0..200)
            .map(|i| item("n", "bobby-core", 100 + i, &noise))
            .collect();
        items.push(item("blaze-old", "blaze", 500, &mine));
        items.push(item("q-live", "rocky", 2, &ask));
        let v = judge(&items, T0);
        let view = View::parse(Some("open"), Some("blaze,hustler"));
        let shown = items
            .iter()
            .filter(|i| view.shows(i.author, v[i.id].state))
            .count();
        assert_eq!(shown, 2, "202 messages, two worth the room's attention");
    }
    #[test]
    fn a_thread_is_classified_once_by_who_is_in_it_and_what_it_is() {
        let people = vec!["blaze".to_string(), "hustler".to_string()];
        let none: Vec<String> = vec![];
        let t = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            kind_of("hustler", &none, "buy?", &none, &people),
            Kind::People
        );
        assert_eq!(
            kind_of(
                "rocky",
                &none,
                "@bobby-core look",
                &t(&["bobby-core"]),
                &people
            ),
            Kind::Agents
        );
        assert_eq!(
            kind_of(
                "rocky",
                &none,
                "look",
                &t(&["bobby-core", "blaze"]),
                &people
            ),
            Kind::People,
            "a person replied"
        );
        assert_eq!(
            kind_of("nexus", &none, "@blaze DASH is down", &none, &people),
            Kind::People,
            "addressed a person"
        );
        assert_eq!(
            kind_of("nexus", &t(&["to:hustler"]), "update", &none, &people),
            Kind::People
        );
        assert_eq!(
            kind_of("rocky", &none, "@hustler-book is fine", &none, &people),
            Kind::Agents,
            "whole-word mention"
        );
        assert_eq!(
            kind_of(
                "hustler-book",
                &none,
                "[Hustler book] opened X",
                &none,
                &people
            ),
            Kind::Automated
        );
        assert_eq!(
            kind_of("x", &t(&["origin:automated"]), "ping", &none, &people),
            Kind::Automated
        );
        assert_eq!(
            kind_of(
                "rocky",
                &none,
                "[Hustler task:q1] review: t",
                &none,
                &people
            ),
            Kind::Bookkeeping
        );
        assert_eq!(
            kind_of(
                "blaze",
                &none,
                "[Hustler members] 2 members",
                &none,
                &people
            ),
            Kind::Bookkeeping
        );
        assert_eq!(
            kind_of(
                "blaze",
                &none,
                "[Message state:OBS-x] q · open",
                &none,
                &people
            ),
            Kind::Bookkeeping
        );
    }

    #[test]
    fn the_ledger_decides_closed_and_archived_before_the_card_does() {
        let tags: Vec<String> = vec![];
        let root = Item {
            id: "r",
            author: "blaze",
            created_ms: 1,
            tags: &tags,
        };
        let mut c = card(&root, State::Open, &[]);
        let asked = Verdict { state: State::Open, intent: Intent::Question, by: None, replies: 0, ask: Ask::Reply, by_ms: None, answered_by: None, overdue: false, to: Vec::new(), choice: None, classified_by: None, unclassified: false, items: Vec::new(), fold: None, marks: Marks::default(), settled: None, settled_by: None, settled_at: None, done: None, done_by: None, done_at: None };
        let answered = Verdict { answered_by: Some("x".into()), ..asked.clone() };
        let note = Verdict { intent: Intent::Note, ask: Ask::None, ..asked.clone() };
        c.status = "waiting";
        assert_eq!(bucket_of(State::Archived, &c, Some(&note)), "archived");
        assert_eq!(bucket_of(State::Closed, &c, Some(&note)), "closed");
        assert_eq!(bucket_of(State::Open, &c, Some(&note)), "needs_answer");
        c.status = "quiet";
        assert_eq!(bucket_of(State::Open, &c, Some(&asked)), "needs_answer", "an unanswered ask, from anyone");
        assert_eq!(bucket_of(State::Open, &c, Some(&answered)), "quiet", "answered elsewhere: no longer owed");
        assert_eq!(bucket_of(State::Open, &c, Some(&note)), "quiet", "a note asks nothing");
        assert_eq!(bucket_of(State::Open, &c, None), "quiet");
        c.status = "answered";
        assert_eq!(bucket_of(State::Open, &c, Some(&asked)), "answered");
        c.status = "active";
        assert_eq!(bucket_of(State::Open, &c, Some(&note)), "in_progress");
    }

    #[test]
    fn decide_and_review_are_asks_and_a_choice_is_read_from_the_earliest_reply_that_picked_one() {
        assert_eq!(Ask::parse("decide"), Some(Ask::Decide));
        assert_eq!(Ask::parse("review"), Some(Ask::Review));
        assert_eq!((Ask::Decide.as_str(), Ask::Review.as_str()), ("decide", "review"));
        assert_eq!(ask_of(&tags(&["ask:decide"]), Intent::Note), Ask::Decide);
        let root = tags(&["ask:decide", "to:blaze"]);
        let chat = tags(&["reply:d"]);
        let late = tags(&["reply:d", "choice:2"]);
        let early = tags(&["reply:d", "choice:1"]);
        let bogus = tags(&["reply:d", "choice:9"]);
        let items = vec![
            item("d", "rocky", 6, &root),
            item("c", "blaze", 5, &chat),
            item("l", "blaze", 3, &late),
            item("e", "blaze", 4, &early),
            item("b", "blaze", 6, &bogus),
        ];
        let v = judge(&items, T0);
        assert_eq!(v["d"].ask, Ask::Decide);
        assert_eq!(v["d"].choice, Some(1), "the earliest reply that picks a valid option; choice:9 is no option");
        assert_eq!(v["d"].answered_by.as_deref(), Some("b"));
        // A choice on a message that is not a decide-ask is not reported.
        let plain = tags(&["ask:reply"]);
        let v = judge(&[item("p", "rocky", 6, &plain), item("e", "blaze", 4, &early)], T0);
        assert_eq!(v["p"].choice, None);
    }

    #[test]
    fn an_unanswered_decide_or_review_past_its_clock_is_overdue_and_an_answered_one_is_not() {
        let past = format!("by:{}", T0 - HOUR_MS);
        let d = tags(&["ask:decide", past.as_str()]);
        let r = tags(&["ask:review", past.as_str()]);
        let reply = tags(&["reply:r"]);
        let v = judge(&[item("d", "a", 3, &d), item("r", "a", 3, &r), item("x", "b", 2, &reply)], T0);
        assert!(v["d"].overdue);
        assert!(!v["r"].overdue, "answered");
    }

    #[test]
    fn to_is_the_authors_own_tags_else_a_persons_annotation_and_never_a_models() {
        let bare = tags(&["ask:act"]);
        let own = tags(&["ask:act", "to:rocky"]);
        let model = tags(&["annotates:bare", "set:to=dinesh,bobby-core", "set:by=model"]);
        let person = tags(&["annotates:bare", "set:to=blaze"]);
        let model2 = tags(&["annotates:own", "set:to=dinesh"]);
        let v = judge(&[item("bare", "a", 5, &bare), item("own", "a", 5, &own), item("m", "k", 4, &model), item("p", "k", 3, &person), item("m2", "k", 3, &model2)], T0);
        assert_eq!(v["bare"].to, vec!["blaze".to_string()], "a person's annotation addresses it; the model's set:to is ignored");
        assert_eq!(v["own"].to, vec!["rocky".to_string()], "the author's own to: beats every annotation");
        let v = judge(&[item("bare", "a", 5, &bare), item("m", "k", 4, &model)], T0);
        assert!(v["bare"].to.is_empty(), "a model's guess at an addressee addresses no one");
    }
    #[test]
    fn a_to_list_names_every_seat_in_it_and_each_may_settle() {
        let root = tags(&["intent:proposal", "ask:reply", "to:blaze, kepler", "to:rocky", "to:"]);
        let go = tags(&["annotates:q", "set:mark=go"]);
        let items = vec![item("q", "dinesh", 5, &root), item("g", "kepler", 4, &go)];
        let v = judge_in(&items, T0, &[]);
        assert_eq!(v["q"].to, vec!["blaze".to_string(), "kepler".to_string(), "rocky".to_string()]);
        assert_eq!(v["q"].settled, Some("accepted"), "kepler is named inside a list, so his go settles it");
        let lonely = tags(&["intent:proposal", "ask:reply", "to:blaze,kepler"]);
        let v = judge_in(&[item("q", "dinesh", 5, &lonely), item("g", "stranger", 4, &go)], T0, &[]);
        assert_eq!(v["q"].settled, None, "a seat that is not named cannot settle");
    }

}

#[cfg(test)]
mod item_tests {
    use super::*;
    fn it<'a>(id: &'a str, author: &'a str, ms: i64, tags: &'a [String]) -> Item<'a> {
        Item { id, author, created_ms: ms, tags }
    }
    #[test]
    fn items_move_by_replies_and_only_the_author_accepts() {
        let root_tags: Vec<String> = vec!["item:R1=requirement".into(), "item:R2=question".into(), "ask:act".into()];
        let note_tags: Vec<String> = vec!["annotates:root".into(), "item:R3=complaint".into(), "set:by=model".into()];
        let built_tags: Vec<String> = vec!["reply:root".into(), "item:R1=built".into()];
        let fake_accept: Vec<String> = vec!["reply:root".into(), "item:R1=accepted".into()];
        let real_accept: Vec<String> = vec!["reply:root".into(), "item:R1=accepted".into(), "item:R2=dropped".into(), "item:R3=dropped".into()];
        let items = [
            it("root", "blaze", 1, &root_tags),
            it("note", "wags", 2, &note_tags),
            it("built", "rocky", 3, &built_tags),
            it("fake", "rocky", 4, &fake_accept),
        ];
        let v = judge(&items, 10);
        let r = &v["root"];
        assert_eq!(r.items.len(), 3, "author's items plus the annotation's");
        assert_eq!(r.items[0].state, "built");
        assert_eq!(r.items[0].by.as_deref(), Some("built"), "rocky's acceptance did not count");
        assert_eq!(r.fold, Some("needs_you"));
        let items = [
            it("root", "blaze", 1, &root_tags),
            it("note", "wags", 2, &note_tags),
            it("built", "rocky", 3, &built_tags),
            it("ok", "blaze", 5, &real_accept),
        ];
        let v = judge(&items, 10);
        assert_eq!(v["root"].fold, Some("closed"));
        assert!(v["built"].items.is_empty(), "a reply has no items of its own");
        assert_eq!(v["root"].items[0].state, "accepted");
    }
    #[test]
    fn a_map_derives_its_frontier_fog_and_done_from_needs_and_resolutions() {
        // Blaze's DOT map in miniature: a destination, two first steps, one that waits on both, one on an unknown.
        let root: Vec<String> = vec![
            "item:D=destination".into(), "item:R1=research".into(), "item:G1=grill".into(), "item:P1=prototype".into(),
            "item:T1=task".into(), "needs:P1=R1,G1".into(), "needs:T1=X9".into(), "ask:act".into(),
        ];
        let solved: Vec<String> = vec!["reply:root".into(), "item:R1=built".into(), "resolves:R1".into()];
        let items = [it("root", "blaze", 1, &root), it("found", "rocky", 2, &solved)];
        let v = judge(&items, 10);
        let by: HashMap<&str, &ItemState> = v["root"].items.iter().map(|i| (i.k.as_str(), i)).collect();
        assert_eq!(by["D"].place, "destination");
        assert_eq!((by["R1"].place, by["R1"].resolved_by.as_deref()), ("frontier", Some("found")), "built, not yet accepted");
        assert_eq!(by["G1"].place, "frontier");
        assert_eq!((by["P1"].place, by["P1"].blocked_by.clone()), ("fog", vec!["G1".to_string()]), "R1 delivered; G1 still open");
        assert_eq!((by["T1"].place, by["T1"].blocked_by.clone()), ("fog", vec!["X9".to_string()]), "an unknown need is fog");
        let done: Vec<String> = vec!["reply:root".into(), "item:G1=accepted".into(), "resolves:G1".into()];
        let items = [it("root", "blaze", 1, &root), it("found", "rocky", 2, &solved), it("decided", "blaze", 3, &done)];
        let v = judge(&items, 10);
        let p1 = v["root"].items.iter().find(|i| i.k == "P1").unwrap();
        assert_eq!(p1.place, "frontier", "both needs met: the frontier moved");
        assert_eq!(v["root"].items.iter().find(|i| i.k == "G1").unwrap().place, "done");
    }

    #[test]
    fn map_edges_follow_the_winning_declaration_and_keep_self_cycles() {
        let root = vec!["ask:act".into()];
        let old = vec!["annotates:root".into(), "item:T=task".into(), "needs:T=OLD".into()];
        let new = vec!["annotates:root".into(), "item:T=task".into(), "needs:T=T".into()];
        let rows = [it("root", "blaze", 1, &root), it("old", "rocky", 2, &old), it("new", "rocky", 3, &new)];
        let v = judge(&rows, 10);
        let item = &v["root"].items[0];
        assert_eq!(item.needs, vec!["T"]);
        assert_eq!(item.blocked_by, vec!["T"]);
        assert_eq!(item.place, "fog");
        let clear = vec!["annotates:root".into(), "item:T=task".into()];
        let rows = [it("root", "blaze", 1, &root), it("old", "rocky", 2, &old), it("new", "rocky", 3, &clear)];
        assert!(judge(&rows, 10)["root"].items[0].needs.is_empty());
    }

    #[test]
    fn map_resolution_uses_the_effective_move_not_a_rejected_or_stale_claim() {
        let root = vec!["item:T=task".into(), "ask:act".into()];
        let built = vec!["reply:root".into(), "item:T=built".into(), "resolves:T".into()];
        let fake = vec!["reply:root".into(), "item:T=accepted".into(), "resolves:T".into()];
        let rows = [it("root", "blaze", 1, &root), it("built", "rocky", 2, &built), it("fake", "rocky", 3, &fake)];
        assert_eq!(judge(&rows, 10)["root"].items[0].resolved_by.as_deref(), Some("built"));
        let reopened = vec!["reply:root".into(), "item:T=open".into()];
        let rows = [it("root", "blaze", 1, &root), it("built", "rocky", 2, &built), it("again", "blaze", 4, &reopened)];
        assert_eq!(judge(&rows, 10)["root"].items[0].resolved_by, None);
    }

    #[test]
    fn a_message_without_items_has_no_fold() {
        let tags: Vec<String> = vec!["ask:reply".into()];
        let items = [it("a", "kin", 1, &tags)];
        assert_eq!(judge(&items, 2)["a"].fold, None);
        assert_eq!(fold_items(&[ItemState { k: "R1".into(), kind: "requirement", state: "building", by: None, from: None, needs: vec![], place: "frontier", blocked_by: vec![], resolved_by: None }]), Some("in_progress"));
    }
}
