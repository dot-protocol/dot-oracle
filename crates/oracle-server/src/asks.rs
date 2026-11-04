//! Asks, read out of the messages (docs/ASKS-AND-EVENTS.md, Layer 1). Nothing is written and nothing is
//! stored as status: a person's State is three lists derived from what was said, with the ids of the
//! messages they came from.
//!
//!   waiting on me   open asks addressed to the seat (`to:`, an annotation's `set:to=`, or an @mention)
//!   I'm waiting on  the seat's own open asks, in every room, with who holds each
//!   done today     asks the seat asked or was asked that were settled since 00:00 UTC
//!
//! What counts as an ask is `projection`'s verdict (the author's own tags, else an annotation, by the
//! precedence it already has): reply, act, decide or review. A message that declares nothing is a note
//! and never appears here, with one exception: a recent message that names the seat and asks a question
//! in its own words is a reply ask for that seat (`implies_ask`). `answered_by` stays a claim; for act and review, DONE needs the addressee's
//! own receipt, result or decision pointing at it (anyone but the author when nobody was named).
use crate::alias::{canon, same, Aliases};
use crate::projection::{self, Ask, Intent, Item, State};
use crate::store::{Observation, Result, Store, StoreError};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Candidates read per page of a tag, and how many pages of a tag are read: a tag's whole history up to
/// CANDIDATE_CAP × CANDIDATE_PAGES messages (Emmy's Room contract, 2026-10-04: no cut by count). Only a tag
/// longer than that reports `capped`.
pub const CANDIDATE_CAP: i64 = 500;
pub const CANDIDATE_PAGES: usize = 40;

/// Every message with `tag`, newest first, page by page; true when the bound stopped the walk.
fn all_tagged(s: &Store, tag: &str) -> Result<(Vec<Observation>, bool)> {
    let mut out = s.by_tag(tag, CANDIDATE_CAP)?;
    let mut last = out.len() as i64;
    for _ in 1..CANDIDATE_PAGES {
        if last < CANDIDATE_CAP {
            return Ok((out, false));
        }
        let before = out.last().map(|o| o.seq).unwrap_or(i64::MAX);
        let page = s.by_tag_before(tag, before, CANDIDATE_CAP)?;
        last = page.len() as i64;
        out.extend(page);
    }
    Ok((out, last >= CANDIDATE_CAP))
}
pub const LIST_CAP: usize = 200;
const DAY_MS: i64 = 86_400_000;
/// How long an implied ask (`implies_ask`) waits: a week, then it is history, not a list item.
pub const IMPLIED_MS: i64 = 7 * DAY_MS;

/// A question mark that ends a sentence: followed by a space, a line end, closing punctuation or nothing.
/// One inside a URL (`/search?q=`) asks nothing.
pub fn asks_a_question(text: &str) -> bool {
    let b = text.as_bytes();
    b.iter().enumerate().any(|(i, &c)| c == b'?' && b.get(i + 1).is_none_or(|n| n.is_ascii_whitespace() || b"\"')*_]".contains(n)))
}

/// An ask nobody tagged (Blaze, 2026-10-08: Noor's questions to Rocky, written through a claude.ai connector
/// with `to:rocky` and no `ask:`, sat unanswered because no list showed them). A message from someone else,
/// in the last [`IMPLIED_MS`], that declares no ask and is not a note, status or receipt, and asks a question
/// in its words, asks the seats it names for a reply. Read the same way whoever wrote it: connector, ox, web, kernel.
pub fn implies_ask(o: &Observation, aliases: &Aliases, me: &str, now_ms: i64) -> bool {
    now_ms - o.created_ms <= IMPLIED_MS
        && !same(aliases, &o.author, me)
        && projection::is_speech(&o.tags)
        && !o.tags.iter().any(|t| t.starts_with("ask:") || t.starts_with("annotates:") || matches!(t.as_str(), "intent:note" | "intent:status" | "intent:receipt"))
        && asks_a_question(&o.content)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateFilter {
    All,
    Open,
    Done,
}
impl StateFilter {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "all" => StateFilter::All,
            "open" => StateFilter::Open,
            "done" => StateFilter::Done,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AskRow {
    pub id: String,
    pub room: String,
    pub author: String,
    /// reply · act · decide · review
    pub ask: String,
    pub intent: String,
    /// open · answered · done. `answered` on an act or review means a reply arrived and the work is not done.
    pub state: &'static str,
    /// Whose attention it asks: the names as they were written.
    pub held_by: Vec<String>,
    /// A name in `held_by` that is a seat alias, and the seat it stands for (`alias.rs`): the original addressee stays visible beside it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub addressee_aliases: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// The gap of a decide (docs/GAP.md): who it affects, who was absent and why, what is owed; signed in cargo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choice: Option<u32>,
    /// The decide as surfaces draw it: options, the asker's pick, the choice (`decide_view`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decide: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_ms: Option<i64>,
    pub overdue: bool,
    /// Past its time-to-live with no deadline and still unanswered: set aside with the overdue ones (`aside`).
    pub aged: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_by: Option<String>,
    /// Each seat's newest mark on the ask (seen · ack · stuck · done · go · like · no): what each one did with it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub marks: BTreeMap<String, &'static str>,
    /// For a seat whose newest mark is `later`: when the ask comes back to it (epoch ms).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub later_until: BTreeMap<String, i64>,
    /// When it was settled: the settling message's time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_ms: Option<i64>,
    /// The annotation that classified it, when the author did not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classified_by: Option<String>,
    pub created_ms: i64,
    /// The first line of the message, at most 160 characters.
    pub card: String,
}

#[derive(Debug, Serialize)]
pub struct Asks {
    pub seat: String,
    /// When `seat` is an alias, the seat it stands for: the lists are that seat's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    pub waiting_on_me: Vec<AskRow>,
    pub im_waiting_on: Vec<AskRow>,
    pub done_today: Vec<AskRow>,
    /// Asks to me I marked ack (or go): they stop asking; still open until done.
    pub acked: Vec<AskRow>,
    /// Asks to me I marked stuck: listed apart, with what I said blocks them.
    pub stuck: Vec<AskRow>,
    /// Asks to me I marked later, whose time has not come: they come back to waiting_on_me by themselves.
    pub later: Vec<AskRow>,
    /// Asks to me or by me whose time has passed (its `by:` expiry, or the fuse of an undeclared question), still
    /// unanswered: set aside, readable here, never on an open list (Blaze, 4 Nov 2026: "anything that is being asked
    /// … is due right now … only a project or outcome might have an expiry. After that, it should not stay clogged in
    /// the open things … kept aside or archived"; Rocky's record).
    pub aside: Vec<AskRow>,
    /// True when a candidate read hit its cap: older asks may be missing.
    pub capped: bool,
    /// Each list's full length before it was cut to LIST_CAP rows, so a reader can say "200 of 312".
    pub totals: Totals,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub waiting_on_me: usize,
    pub im_waiting_on: usize,
    pub done_today: usize,
    /// Of `im_waiting_on`, the asks someone already answered: they wait for their author to close them. Counted before
    /// the cut, for the footer's count answer (`counts`).
    pub im_waiting_on_answered: usize,
    /// When the oldest ask waiting on me was written (epoch ms), before the cut; None when nothing waits on me.
    pub oldest_waiting_ms: Option<i64>,
    /// Asks set aside because their time passed (`aside`), before the cut.
    pub aside: usize,
}

/// `options` from a cargo string: the labels, when it carries a list of strings.
pub fn options_of(cargo: Option<&str>) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(cargo?).ok()?;
    v.get("options")?.as_array()?.iter().map(|o| o.as_str().map(str::to_owned)).collect()
}

/// A decide as every surface draws it (Blaze, 27 Oct 2026: "select options like on the bridge", then "build it properly to
/// be oracle native"): the options, the asker's pick with why (cargo.recommendation, b00ddf5), and the option the earliest
/// choosing reply picked (`choice:<n>`, judged by the projection), with its label. One derivation for the Bridge, the inbox
/// and the room; a pick or a choice outside the options is not reported.
pub fn decide_view(cargo: Option<&str>, choice: Option<u32>) -> Option<serde_json::Value> {
    let options = options_of(cargo)?;
    if options.len() < 2 {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(cargo?).ok()?;
    let in_range = |n: u64| n >= 1 && n as usize <= options.len();
    let recommendation = v.get("recommendation").and_then(|r| {
        let n = r.get("option")?.as_u64().filter(|n| in_range(*n))?;
        Some(serde_json::json!({"option": n, "why": r.get("why").and_then(|w| w.as_str()).unwrap_or("")}))
    });
    let mut out = serde_json::json!({"options": options});
    if let Some(r) = recommendation {
        out["recommendation"] = r;
    }
    if let Some(c) = choice.filter(|c| in_range(u64::from(*c))) {
        out["choice"] = serde_json::json!(c);
        out["chosen"] = serde_json::json!(options[c as usize - 1]);
    }
    Some(out)
}

/// `gap` from a cargo string (docs/GAP.md), when it carries one as an object.
pub fn gap_of(cargo: Option<&str>) -> Option<serde_json::Value> {
    let mut v: serde_json::Value = serde_json::from_str(cargo?).ok()?;
    v.get_mut("gap").map(serde_json::Value::take).filter(serde_json::Value::is_object)
}

/// The seats a message names with an @mention in its text (the store indexes them as `mention:<seat>`,
/// but they are not among the signed tags, so they are read from the words).
///
/// A handle in quotes or code is a reference, not an address (Emmy, 2026-10-04):
/// Rocky's trace said "'@blaze' match is case-sensitive" about a regex and that put the trace on Blaze's list. Text in
/// backticks, and a short quoted span ('…', "…", ‘…’, “…”) that opens after a space or bracket and closes before
/// a space or punctuation, is set aside before the @handles are read. An apostrophe inside a word opens nothing.
fn mentioned(o: &Observation, seats: &BTreeSet<String>) -> Vec<String> {
    // No ASCII '@' anywhere: no mention, whatever the quotes and code around (`mentions` starts only at '@', and
    // `addressing_text` only keeps or blanks characters, never adds one), so the scan over the whole content is
    // skipped. Measured 21 Oct (asks-phases): the scan over every candidate was 30 ms of a 246 ms view.
    if !o.content.as_bytes().contains(&b'@') {
        return Vec::new();
    }
    crate::store::mentions(&addressing_text(&o.content)).into_iter().filter_map(|t| t.strip_prefix("mention:").map(str::to_owned))
        .filter(|h| *h != o.author && seats.contains(h)).collect()
}

/// `content` with code and short quoted spans blanked out, so only the words that address someone are left.
pub fn addressing_text(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let mut keep = vec![true; chars.len()];
    let closes = |c: char| match c {
        '\'' => Some('\''),
        '"' => Some('"'),
        '‘' => Some('’'),
        '“' => Some('”'),
        _ => None,
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            // Code: to the next backtick (or the end of the line, for an unclosed one).
            let end = (i + 1..chars.len()).find(|&j| chars[j] == '`' || chars[j] == '\n').unwrap_or(chars.len());
            for k in keep.iter_mut().take(end.min(chars.len() - 1) + 1).skip(i) {
                *k = false;
            }
            i = end + 1;
            continue;
        }
        let opens = i == 0 || chars[i - 1].is_whitespace() || "([{".contains(chars[i - 1]);
        if let (true, Some(close)) = (opens, closes(c)) {
            let limit = (i + 1 + 120).min(chars.len());
            let end = (i + 1..limit).take_while(|&j| chars[j] != '\n').find(|&j| {
                chars[j] == close && (j + 1 == chars.len() || chars[j + 1].is_whitespace() || ".,;:!?)]}".contains(chars[j + 1]))
            });
            if let Some(end) = end {
                for k in keep.iter_mut().take(end + 1).skip(i) {
                    *k = false;
                }
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    chars.iter().zip(keep).map(|(c, k)| if k { *c } else { ' ' }).collect()
}

/// Whom a message addresses: its `to` (tags, else annotation) and whoever it @mentions, each once.
fn addressed(to: &[String], o: &Observation, seats: &BTreeSet<String>) -> Vec<String> {
    let mut out: Vec<String> = to.to_vec();
    for m in mentioned(o, seats) {
        if !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

fn is_ask(a: Ask) -> bool {
    matches!(a, Ask::Reply | Ask::Act | Ask::Decide | Ask::Review)
}

fn points_at(tags: &[String]) -> impl Iterator<Item = &str> {
    tags.iter().filter_map(|t| {
        ["re:", "reply:", "in_reply_to:", "closes:"].iter().find_map(|p| t.strip_prefix(p))
    })
}

/// The seat's three lists as of `now_ms`, optionally in one room.
/// `GET /asks`, answered once per change (Blaze's go, 17 Oct 2026). Measured that day:
/// the clients on one Mac asked /asks 72 times in three minutes, every answer walking every ask's thread to its end, and
/// the server sat near 150% CPU. Now a question's answer is kept with the newest seq it was computed at: asked again while
/// nothing new was written, it costs one indexed lookup (for up to `MAX_AGE_MS`, so the time-dependent parts move on).
/// Anything written makes the next answer a walk: a seat reads its own ask, or the reply that closed one, at once (a
/// 2 s reuse window after writes was tried and failed that in e2e: `ox say` then `ox asks` within 2 s missed the ask).
/// The same question asked concurrently waits for the walk already running (single flight) instead of starting another.
#[derive(Default)]
pub struct AsksCache {
    slots: Mutex<HashMap<String, Arc<Mutex<Option<Kept>>>>>,
    /// Answers computed (a walk) and answers reused, for /metrics and the tests.
    pub walks: AtomicU64,
    pub hits: AtomicU64,
}

struct Kept {
    seq: i64,
    at_ms: i64,
    value: Arc<serde_json::Value>,
}

pub const MAX_AGE_MS: i64 = 60_000;
const MAX_SLOTS: usize = 4096;

impl AsksCache {
    /// The answer `asks_for` would give, as JSON, computed at most once per change of the log.
    pub fn get(&self, s: &Store, seat: &str, room: Option<&str>, filter: StateFilter, now_ms: i64) -> Result<Arc<serde_json::Value>> {
        let key = format!("{seat}\u{1f}{}\u{1f}{filter:?}", room.unwrap_or(""));
        let slot = {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            if slots.len() >= MAX_SLOTS && !slots.contains_key(&key) {
                slots.clear();
            }
            slots.entry(key).or_default().clone()
        };
        // One walk per question at a time: a concurrent request waits on this lock, then finds the answer just kept.
        let mut kept = slot.lock().unwrap_or_else(|e| e.into_inner());
        let newest = s.recent(None, 1, None)?.first().map_or(0, |o| o.seq);
        if let Some(k) = kept.as_ref() {
            let age = now_ms - k.at_ms;
            if k.seq == newest && (0..MAX_AGE_MS).contains(&age) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(k.value.clone());
            }
        }
        let out = asks_for(s, seat, room, filter, now_ms)?;
        let value = Arc::new(serde_json::to_value(&out).map_err(|e| StoreError::Internal(e.to_string()))?);
        self.walks.fetch_add(1, Ordering::Relaxed);
        *kept = Some(Kept { seq: newest, at_ms: now_ms, value: value.clone() });
        Ok(value)
    }
}

/// How the view walks its threads. `Batched` is the view; `PerRoot` is the reference it must equal, kept for
/// the equivalence check (`asks-equivalence`, tests/asks.rs) and nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Walk {
    Batched,
    PerRoot,
}

/// Where a view's time goes, phase by phase: wall milliseconds, row loads (`Store::loads`) and rows fetched
/// (`Store::rows_loaded`) since the previous lap, under the name of the phase that just ended. The view laps
/// at every boundary; the measurement binary (`asks-phases`) reads the ledger. `asks_for` keeps none.
#[derive(Debug, Clone, Serialize)]
pub struct Phases {
    pub ms: BTreeMap<&'static str, f64>,
    pub loads: BTreeMap<&'static str, u64>,
    pub rows: BTreeMap<&'static str, u64>,
    #[serde(skip)]
    at: std::time::Instant,
    #[serde(skip)]
    cursor: (u64, u64),
}

impl Default for Phases {
    fn default() -> Self {
        Phases { ms: BTreeMap::new(), loads: BTreeMap::new(), rows: BTreeMap::new(), at: std::time::Instant::now(), cursor: (Store::loads(), Store::rows_loaded()) }
    }
}

impl Phases {
    /// Charge everything since the previous lap to `name`.
    pub fn lap(&mut self, name: &'static str) {
        let now = std::time::Instant::now();
        let (l, r) = (Store::loads(), Store::rows_loaded());
        *self.ms.entry(name).or_default() += now.duration_since(self.at).as_secs_f64() * 1000.0;
        *self.loads.entry(name).or_default() += l - self.cursor.0;
        *self.rows.entry(name).or_default() += r - self.cursor.1;
        self.at = now;
        self.cursor = (l, r);
    }

    /// Add another ledger's phases to this one.
    pub fn add(&mut self, other: &Phases) {
        for (k, v) in &other.ms {
            *self.ms.entry(k).or_default() += v;
        }
        for (k, v) in &other.loads {
            *self.loads.entry(k).or_default() += v;
        }
        for (k, v) in &other.rows {
            *self.rows.entry(k).or_default() += v;
        }
    }

    pub fn total_ms(&self) -> f64 {
        self.ms.values().sum()
    }
}

/// The ids an observation resolves: the targets of its relation tags.
fn resolved_ids(o: &Observation) -> impl Iterator<Item = &str> {
    o.tags.iter().filter_map(|t| crate::store::RESOLVER_PREFIXES.iter().find_map(|p| t.strip_prefix(p)))
}

/// A batched fetch indexed by the id each row resolves (a row under every id it resolves, once each), so a
/// root's selection is a lookup per frontier id and not a scan of every row for every root.
pub fn by_target(rows: &[Observation]) -> HashMap<&str, Vec<&Observation>> {
    let mut index: HashMap<&str, Vec<&Observation>> = HashMap::new();
    for o in rows {
        let mut once: BTreeSet<&str> = BTreeSet::new();
        for t in resolved_ids(o) {
            if once.insert(t) {
                index.entry(t).or_default().push(o);
            }
        }
    }
    index
}

/// What `Store::resolvers_of(ids)` returns for one frontier, taken from an indexed batched fetch: the rows
/// resolving any of `ids`, newest first, each once (the query's GROUP BY), at most 2,000 (its LIMIT).
pub fn per_frontier(index: &HashMap<&str, Vec<&Observation>>, ids: &BTreeSet<String>) -> Vec<Observation> {
    let mut v: Vec<&Observation> = ids.iter().filter_map(|id| index.get(id.as_str())).flatten().copied().collect();
    v.sort_by_key(|o| std::cmp::Reverse(o.seq));
    v.dedup_by(|a, b| a.id == b.id);
    v.truncate(2_000);
    v.into_iter().cloned().collect()
}

/// `Store::thread_members(root, max)` for every root at once. Each root keeps its own seen set, frontier, depth
/// (`THREAD_HOPS`), scan cap (`THREAD_SCAN`) and newest-`max` truncation, in the same order as the per-root walk;
/// only the fetch is shared: per level one chunked, unlimited resolvers query over every root's frontier, each
/// root then taking what its own query would have returned (`per_frontier`). Same members per root (tests/asks.rs
/// proves it on Emmy's two-roots-of-2,000, a 70-deep chain, a cycle, a shared descendant and an over-cap thread);
/// one query per level instead of one per root per level (21 Oct: ~1,000 roots a call, 1.9 s p50 on /asks).
pub fn thread_members_batched(s: &Store, roots: &[String], max: usize) -> Result<BTreeMap<String, Vec<Observation>>> {
    thread_members_batched_timed(s, roots, max, &mut Phases::default())
}

/// `thread_members_batched`, lapping `ph` at each phase: `members.query` (the resolvers fetch, decoded),
/// `members.index` (`by_target`, each root's `per_frontier` selection and its seen/frontier bookkeeping),
/// `members.cap` (the final sort and newest-`max` truncation).
pub fn thread_members_batched_timed(s: &Store, roots: &[String], max: usize, ph: &mut Phases) -> Result<BTreeMap<String, Vec<Observation>>> {
    struct Walk1 {
        seen: std::collections::HashSet<String>,
        frontier: BTreeSet<String>,
        out: Vec<Observation>,
        done: bool,
    }
    let mut walks: BTreeMap<String, Walk1> = roots
        .iter()
        .map(|r| (r.clone(), Walk1 { seen: [r.clone()].into_iter().collect(), frontier: [r.clone()].into_iter().collect(), out: Vec::new(), done: false }))
        .collect();
    for _depth in 0..crate::store::THREAD_HOPS {
        let ids: Vec<String> = walks.values().filter(|w| !w.done).flat_map(|w| w.frontier.iter().cloned()).collect::<BTreeSet<_>>().into_iter().collect();
        if ids.is_empty() {
            break;
        }
        let rows = s.resolvers_of_all(&ids)?;
        ph.lap("members.query");
        let index = by_target(&rows);
        for w in walks.values_mut() {
            if w.done {
                continue;
            }
            if w.frontier.is_empty() {
                w.done = true;
                continue;
            }
            let found = per_frontier(&index, &w.frontier);
            w.frontier = BTreeSet::new();
            for o in found {
                if w.seen.insert(o.id.clone()) {
                    w.frontier.insert(o.id.clone());
                    w.out.push(o);
                    if w.out.len() >= crate::store::THREAD_SCAN {
                        w.done = true;
                        break;
                    }
                }
            }
        }
        ph.lap("members.index");
    }
    let out = walks
        .into_iter()
        .map(|(r, mut w)| {
            w.out.sort_by_key(|o| o.seq);
            if w.out.len() > max {
                w.out.drain(..w.out.len() - max);
            }
            (r, w.out)
        })
        .collect();
    ph.lap("members.cap");
    Ok(out)
}

/// `Store::thread_root` for every candidate at once, hop for hop: each walk starts at its own record, keeps
/// its own seen set and the `THREAD_HOPS` cap, stops at a missing or repeated parent where the per-record walk
/// stops; only the parent lookups are shared, one chunked query per hop over every walk's next parent
/// (`Store::get_many`). Same root per candidate; one query per level instead of one per candidate per hop
/// (21 Oct: rocky's view made 6,498 parent loads).
pub fn thread_roots_batched(s: &Store, candidates: &BTreeMap<String, Observation>) -> Result<BTreeMap<String, Observation>> {
    thread_roots_batched_timed(s, candidates, &mut Phases::default())
}

/// `thread_roots_batched`, lapping `ph`: `roots.query` (the parent fetch per hop, decoded) and `roots.walk`
/// (the per-candidate seen sets, parent assignment and the clones of the current record).
pub fn thread_roots_batched_timed(s: &Store, candidates: &BTreeMap<String, Observation>, ph: &mut Phases) -> Result<BTreeMap<String, Observation>> {
    struct Up {
        cur: Observation,
        seen: std::collections::HashSet<String>,
        done: bool,
    }
    let mut walks: BTreeMap<String, Up> = candidates
        .iter()
        .map(|(id, o)| (id.clone(), Up { cur: o.clone(), seen: [id.clone()].into_iter().collect(), done: false }))
        .collect();
    for _hop in 0..crate::store::THREAD_HOPS {
        let mut wanted: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (id, w) in walks.iter_mut() {
            if w.done {
                continue;
            }
            match crate::store::parent_of(&w.cur) {
                Some(parent) if w.seen.insert(parent.clone()) => wanted.entry(parent).or_default().push(id.clone()),
                _ => w.done = true,
            }
        }
        ph.lap("roots.walk");
        if wanted.is_empty() {
            break;
        }
        let ids: Vec<String> = wanted.keys().cloned().collect();
        let found: BTreeMap<String, Observation> = s.get_many(&ids)?.into_iter().map(|o| (o.id.clone(), o)).collect();
        ph.lap("roots.query");
        for (parent, walkers) in wanted {
            for id in walkers {
                let w = walks.get_mut(&id).expect("a walker");
                match found.get(&parent) {
                    Some(o) => w.cur = o.clone(),
                    None => w.done = true,
                }
            }
        }
    }
    let out = walks.into_iter().map(|(id, w)| (id, w.cur)).collect();
    ph.lap("roots.walk");
    Ok(out)
}

/// The count answer (`GET /asks?count=1`): the lists' lengths from an /asks answer, and no rows. Every `ox` command ends
/// with a footer that fetched the open asks whole only to count them: 168 KB for seat rocky, about 2.5 s over the
/// tailnet, longer than the footer's budget, so every command printed "footer unavailable" (Rocky, 1 Nov 2026). The
/// numbers are the totals counted before the lists were cut to LIST_CAP, so they hold for a seat with more asks than that.
pub fn counts(asks: &serde_json::Value) -> serde_json::Value {
    let t = &asks["totals"];
    let n = |k: &str| t[k].as_u64().unwrap_or(0);
    let len = |k: &str| asks[k].as_array().map_or(0, Vec::len);
    serde_json::json!({
        "schema": "asks-count/1", "seat": asks["seat"], "canonical": asks["canonical"],
        "waiting_on_me": n("waiting_on_me"), "oldest_ms": t["oldest_waiting_ms"],
        "im_waiting_on": n("im_waiting_on"), "im_waiting_on_answered": n("im_waiting_on_answered"),
        "acked": len("acked"), "stuck": len("stuck"), "later": len("later"), "aside": n("aside"), "done_today": n("done_today"),
        "capped": asks["capped"] == true,
    })
}

/// Cut a list to its `cap` newest rows by creation time, keeping their order.
pub fn keep_newest(l: &mut Vec<AskRow>, cap: usize) {
    if l.len() <= cap {
        return;
    }
    let mut times: Vec<i64> = l.iter().map(|r| r.created_ms).collect();
    times.sort_unstable_by(|a, b| b.cmp(a));
    let cut = times[cap - 1];
    let newer = l.iter().filter(|r| r.created_ms > cut).count();
    let mut ties = cap - newer;
    l.retain(|r| r.created_ms > cut || (r.created_ms == cut && ties > 0 && { ties -= 1; true }));
}

pub fn asks_for(s: &Store, seat: &str, room: Option<&str>, filter: StateFilter, now_ms: i64) -> Result<Asks> {
    asks_for_with(s, seat, room, filter, now_ms, Walk::Batched)
}

pub fn asks_for_with(s: &Store, seat: &str, room: Option<&str>, filter: StateFilter, now_ms: i64, walk: Walk) -> Result<Asks> {
    asks_for_timed(s, seat, room, filter, now_ms, walk, &mut Phases::default())
}

/// `asks_for_with`, lapping `ph` at every phase boundary (names in `Phases`' order of appearance):
/// `setup` (aliases, seat names), `candidates.tagged` (the declared asks, six tags), `candidates.classified`
/// (`set:` annotations and their targets), `candidates.implied` (`to:`/`mention:` reads and `implies_ask`),
/// `roots.*` (the walk to each thread root), `members.*` and `resolvers.*` (what each thread holds),
/// `judge` (each thread's judgement, marks, answers and rows), `lists` (the five lists, sorting, caps).
pub fn asks_for_timed(s: &Store, seat: &str, room: Option<&str>, filter: StateFilter, now_ms: i64, walk: Walk, ph: &mut Phases) -> Result<Asks> {
    asks_settled_from(s, seat, room, filter, now_ms, walk, ph, None)
}

/// The lists with `done_today` widened to every ask settled at or after `since_ms` (for `delta`: what was closed since a
/// seat's last RESUME, which can be days back, not only since midnight). Everything else is `asks_for`'s answer.
pub fn asks_since(s: &Store, seat: &str, since_ms: i64, now_ms: i64) -> Result<Asks> {
    asks_settled_from(s, seat, None, StateFilter::All, now_ms, Walk::Batched, &mut Phases::default(), Some(since_ms))
}

/// The view itself; `done_from` is where done_today starts (None: 00:00 UTC today).
#[allow(clippy::too_many_arguments)]
fn asks_settled_from(s: &Store, seat: &str, room: Option<&str>, filter: StateFilter, now_ms: i64, walk: Walk, ph: &mut Phases, done_from: Option<i64>) -> Result<Asks> {
    // A name a seat once used stands for that seat (card seat-alias-migration): the lists are the seat's, and every name is kept as it was written.
    let aliases: Aliases = s.seat_aliases();
    let me = canon(&aliases, seat).to_owned();
    let seats = s.seat_names()?;
    ph.lap("setup");
    let mut candidates: BTreeMap<String, Observation> = BTreeMap::new();
    let mut capped = false;
    let mut take = |list: Vec<Observation>, _capped: &mut bool| {
        for o in list {
            if room.is_none_or(|r| o.channel == r) {
                candidates.entry(o.id.clone()).or_insert(o);
            }
        }
    };
    // Asks the author declared, and the questions and proposals that ask for a reply by themselves.
    for tag in ["ask:reply", "ask:act", "ask:decide", "ask:review", "intent:question", "intent:proposal"] {
        let (all, hit) = all_tagged(s, tag)?;
        ph.lap("candidates.tagged.query");
        if hit {
            capped = true;
        }
        let list: Vec<Observation> = all.into_iter().filter(|o| projection::is_speech(&o.tags)).collect();
        // Cheap pre-filter: asks addressed elsewhere (by `to:` or an @mention) and not written by `seat` go;
        // unaddressed asks stay, since an annotation may still address them with `set:to=`.
        let list = list.into_iter().filter(|o| {
            let to: Vec<String> = o.tags.iter().filter_map(|t| t.strip_prefix("to:")).flat_map(|v| v.split(',')).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()).collect();
            let named = addressed(&to, o, &seats);
            same(&aliases, &o.author, &me) || named.is_empty() || named.iter().any(|n| same(&aliases, n, &me))
        }).collect();
        take(list, &mut capped);
        ph.lap("candidates.tagged.filter");
    }
    // Asks a reader classified: the annotation's target is the ask. The batched view reads every target in one
    // chunked query (`Store::get_many`); the reference reads each target by itself, as the view did until 21 Oct
    // (2,400 point reads per view on the profile copy, the largest single cost measured: asks-phases).
    let mut classified: BTreeSet<String> = BTreeSet::new();
    for tag in ["set:ask=reply", "set:ask=act", "set:ask=decide", "set:ask=review", "set:intent=question", "set:intent=proposal"] {
        let (notes, hit) = all_tagged(s, tag)?;
        ph.lap("candidates.classified.query");
        if hit {
            capped = true;
        }
        let targets: BTreeSet<String> = notes
            .iter()
            .flat_map(|n| n.tags.iter().filter_map(|t| t.strip_prefix("annotates:").map(str::to_owned)))
            .collect();
        match walk {
            Walk::Batched => classified.extend(targets),
            Walk::PerRoot => {
                for t in targets {
                    if let Ok(o) = s.get(&t) {
                        if projection::is_speech(&o.tags) && room.is_none_or(|r| o.channel == r) {
                            candidates.entry(o.id.clone()).or_insert(o);
                        }
                    }
                }
                ph.lap("candidates.classified.get");
            }
        }
    }
    if walk == Walk::Batched {
        let ids: Vec<String> = classified.into_iter().collect();
        for o in s.get_many(&ids)? {
            if projection::is_speech(&o.tags) && room.is_none_or(|r| o.channel == r) {
                candidates.entry(o.id.clone()).or_insert(o);
            }
        }
        ph.lap("candidates.classified.get");
    }

    // Asks nobody tagged: messages that name the seat and ask a question in their words (`implies_ask`).
    let mut implied: BTreeSet<String> = BTreeSet::new();
    for tag in [format!("to:{me}"), format!("mention:{me}")] {
        let list = s.by_tag(&tag, CANDIDATE_CAP)?;
        ph.lap("candidates.implied.query");
        if list.len() as i64 >= CANDIDATE_CAP {
            capped = true;
        }
        for o in list {
            if room.is_none_or(|r| o.channel == r) && !candidates.contains_key(&o.id) && implies_ask(&o, &aliases, &me, now_ms) {
                implied.insert(o.id.clone());
                candidates.insert(o.id.clone(), o);
            }
        }
    }
    // A question to several seats is each one's: another addressee's answer settles it for its author,
    // not for the seat that has not answered (Blaze, 2026-10-08: Emmy answered Noor's question to Emmy and
    // Rocky, and Rocky's part of it left his list).
    let mut still_mine: BTreeSet<String> = BTreeSet::new();
    ph.lap("candidates.implied.filter");

    // One judgement per thread, with the room's deciders, so answered_by and overdue are the reader's own.
    let today = done_from.unwrap_or(now_ms - now_ms.rem_euclid(DAY_MS));
    let mut rows: Vec<AskRow> = Vec::new();
    // A room's deciders are read once per room, not once per thread: that lookup was 90% of the time.
    let mut deciders: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Each candidate belongs to exactly one thread: the one its own links lead back to. A thread can
    // contain another thread's ask (a `supersedes:` reaches across), and it must not be listed twice.
    let mut by_root: BTreeMap<String, (Observation, BTreeSet<String>)> = BTreeMap::new();
    let roots_of = match walk {
        Walk::Batched => thread_roots_batched_timed(s, &candidates, ph)?,
        Walk::PerRoot => BTreeMap::new(),
    };
    for id in candidates.keys() {
        let root = match walk {
            Walk::Batched => match roots_of.get(id) {
                Some(r) => r.clone(),
                None => continue,
            },
            Walk::PerRoot => {
                let Ok(root) = s.thread_root(id) else { continue };
                root
            }
        };
        by_root.entry(root.id.clone()).or_insert_with(|| (root, BTreeSet::new())).1.insert(id.clone());
    }
    ph.lap(match walk {
        Walk::Batched => "roots.walk",
        Walk::PerRoot => "roots.query",
    });
    let (mut members_by_root, ask_resolvers) = match walk {
        Walk::Batched => {
            let roots: Vec<String> = by_root.keys().cloned().collect();
            let all_asks: Vec<String> = by_root.values().flat_map(|(_, m)| m.iter().cloned()).collect();
            let members = thread_members_batched_timed(s, &roots, 300, ph)?;
            let resolvers = s.resolvers_of_all(&all_asks)?;
            ph.lap("resolvers.query");
            (members, resolvers)
        }
        Walk::PerRoot => (BTreeMap::new(), Vec::new()),
    };
    let ask_index = by_target(&ask_resolvers);
    ph.lap("resolvers.index");
    for (_, (root, mine_here)) in by_root {
        let mut members = match walk {
            Walk::Batched => members_by_root.remove(&root.id).unwrap_or_default(),
            Walk::PerRoot => s.thread_members(&root.id, 300)?,
        };
        ph.lap(match walk {
            Walk::Batched => "members.take",
            Walk::PerRoot => "members.query",
        });
        // What points straight at an ask belongs to its judgement however deep the ask sits. Until 2026-10-16 the walk
        // stopped at four levels, so an ask at depth 4 never saw its own closes, answers or its author's annotation (Bobby's,
        // closed twice and still listed, 2026-09-30), and an ask deeper than that was
        // never judged at all (tests/asks.rs, an_ask_deep_in_a_long_exchange_is_on_both_lists). The walk now reads to the
        // end; this stays for a thread longer than the 300 members it keeps.
        let have: BTreeSet<String> = members.iter().map(|o| o.id.clone()).collect();
        let asks_here: Vec<String> = mine_here.iter().cloned().collect();
        let resolvers = match walk {
            Walk::Batched => per_frontier(&ask_index, &mine_here),
            Walk::PerRoot => s.resolvers_of(&asks_here)?,
        };
        ph.lap(match walk {
            Walk::Batched => "resolvers.index",
            Walk::PerRoot => "resolvers.query",
        });
        members.extend(resolvers.into_iter().filter(|o| !have.contains(&o.id) && o.id != root.id));
        members.sort_by_key(|o| o.seq);
        members.dedup_by(|a, b| a.id == b.id);
        let all: Vec<&Observation> = std::iter::once(&root).chain(members.iter()).collect();
        // An implied ask is judged as the question it asks (a result is ambient and would never be open).
        let judged: Vec<Vec<String>> = all
            .iter()
            .map(|o| {
                let mut t = o.tags.clone();
                if implied.contains(&o.id) {
                    t.retain(|x| !x.starts_with("intent:") && !x.starts_with("kind:"));
                    t.extend(["intent:question".to_owned(), "ask:reply".to_owned()]);
                }
                t
            })
            .collect();
        let items: Vec<Item> = all.iter().zip(&judged).map(|(o, t)| Item { id: &o.id, author: &o.author, created_ms: o.created_ms, tags: t }).collect();
        ph.lap("judge.prepare");
        let roots = deciders.entry(root.channel.clone()).or_insert_with(|| s.room_roots(Some(root.channel.as_str())));
        let verdicts = projection::judge_in_with(&items, now_ms, roots, &aliases);
        ph.lap("judge.verdict");
        for o in all.iter().filter(|o| mine_here.contains(&o.id)) {
            let Some(v) = verdicts.get(&o.id) else { continue };
            if !is_ask(v.ask) || !projection::is_speech(&o.tags) {
                continue;
            }
            // Withdrawn, replaced, or aged out with no clock on it: not on anyone's list.
            // An unanswered ask does not stop needing someone because a clock passed (Blaze 2026-09-30; Emmy's Room
            // contract 2026-10-04: independent of age). Expired by its intent's default fuse is listed and marked `aged`;
            // withdrawn or replaced is not, and neither is an ask whose author set its own `ttl:` (Blaze 2026-10-10: the
            // author's fuse ends it; tests/inbox_marks.rs).
            let author_fuse = o.tags.iter().any(|t| t.starts_with("ttl:"));
            let live = matches!(v.state, State::Open | State::Closed) || (v.state == State::Expired && !author_fuse);
            // A thread said to be stale, or replaced by a later message (`set:done=`), waits on nobody: it is not on any list.
            if !live || matches!(v.done, Some("stale" | "superseded")) {
                continue;
            }
            // Each seat's newest mark on this ask, and when; every named seat marking done (or no) settles it.
            let mut marks: BTreeMap<String, (&'static str, i64)> = BTreeMap::new();
            let mut later_until: BTreeMap<String, i64> = BTreeMap::new();
            for a in all.iter().filter(|a| a.created_ms >= o.created_ms && a.id != o.id) {
                let at = a.tags.iter().any(|t| ["re:", "reply:", "annotates:"].iter().any(|p| t.strip_prefix(p) == Some(o.id.as_str())));
                if let (true, Some(m)) = (at, projection::mark_of(&a.tags)) {
                    let who = canon(&aliases, &a.author).to_owned();
                    // newest mark wins, and a later without a time is no later at all
                    match (m, projection::until_of(&a.tags)) {
                        ("later", Some(u)) => { later_until.insert(who.clone(), u); }
                        ("later", None) => continue,
                        _ => { later_until.remove(&who); }
                    }
                    marks.insert(who, (m, a.created_ms));
                }
            }
            let named_now = addressed(&v.to, o, &seats);
            let marked_done = (!named_now.is_empty())
                .then(|| named_now.iter().map(|n| marks.get(canon(&aliases, n)).filter(|(m, _)| matches!(*m, "done" | "no")).map(|(_, ms)| *ms)).collect::<Option<Vec<i64>>>())
                .flatten()
                .and_then(|v| v.into_iter().max());
            let answers: Vec<&&Observation> = all
                .iter()
                .filter(|a| a.created_ms >= o.created_ms && a.id != o.id && projection::is_speech(&a.tags) && points_at(&a.tags).any(|p| p == o.id))
                .collect();
            let done_by = if matches!(v.ask, Ask::Act | Ask::Review) {
                answers
                    .iter()
                    .filter(|a| {
                        let complete = matches!(projection::intent_of(&a.tags), Intent::Receipt | Intent::Result | Intent::Decision);
                        let named = addressed(&v.to, o, &seats);
                        let who = if named.is_empty() { !same(&aliases, &a.author, &o.author) } else { named.iter().any(|n| same(&aliases, n, &a.author)) };
                        complete && who
                    })
                    .min_by_key(|a| a.created_ms)
                    .map(|a| (a.id.clone(), a.created_ms))
            } else {
                None
            };
            let first = v.answered_by.as_ref().and_then(|id| all.iter().find(|a| &a.id == id)).map(|a| (a.id.clone(), a.created_ms));
            let closed = (v.state == State::Closed).then(|| v.by.as_ref().and_then(|id| all.iter().find(|a| &a.id == id)).map(|a| a.created_ms)).flatten();
            let (resolved, settled_ms) = match v.ask {
                Ask::Act | Ask::Review => match (&done_by, closed) {
                    (Some((_, ms)), _) => (true, Some(*ms)),
                    (None, Some(ms)) => (true, Some(ms)),
                    _ => (false, None),
                },
                _ => match (&first, closed) {
                    (Some((_, ms)), _) => (true, Some(*ms)),
                    (None, Some(ms)) => (true, Some(ms)),
                    _ => (false, None),
                },
            };
            // Said settled, or acknowledged (`set:done=`): done, at the moment it was said.
            let said_done = matches!(v.done, Some("settled" | "acked")) && !resolved;
            let (resolved, settled_ms) = if said_done { (true, v.done_at) } else { (resolved, settled_ms) };
            let said_done = said_done || (marked_done.is_some() && !resolved);
            let (resolved, settled_ms) = match marked_done { Some(ms) if !resolved => (true, Some(ms)), _ => (resolved, settled_ms) };
            let state = if said_done || done_by.is_some() { "done" } else if v.answered_by.is_some() || resolved { "answered" } else { "open" };
            let named = addressed(&v.to, o, &seats);
            // A seat named with others keeps its own part open until it answers (reply) or delivers (act, review):
            // Jobs' result on Round 51, an act ask to Jobs and Rocky, took it off Rocky's list too (2026-10-10).
            let mine_answered = match v.ask {
                Ask::Reply => all.iter().any(|a| a.created_ms > o.created_ms && same(&aliases, &a.author, &me) && projection::is_speech(&a.tags)),
                Ask::Act | Ask::Review => answers.iter().any(|a| {
                    same(&aliases, &a.author, &me) && matches!(projection::intent_of(&a.tags), Intent::Receipt | Intent::Result | Intent::Decision)
                }),
                _ => true,
            };
            if resolved
                && closed.is_none()
                && !said_done
                && named.len() > 1
                && named.iter().any(|n| same(&aliases, n, &me))
                && !same(&aliases, &o.author, &me)
                && !mine_answered
            {
                still_mine.insert(o.id.clone());
            }
            rows.push(AskRow {
                id: o.id.clone(),
                room: o.channel.clone(),
                author: o.author.clone(),
                ask: v.ask.as_str().to_owned(),
                intent: v.intent.as_str().to_owned(),
                state: if resolved && state == "open" { "answered" } else { state },
                held_by: addressed(&v.to, o, &seats),
                addressee_aliases: addressed(&v.to, o, &seats).into_iter().filter(|n| canon(&aliases, n) != n.as_str()).map(|n| { let c = canon(&aliases, &n).to_owned(); (n, c) }).collect(),
                options: options_of(o.cargo.as_deref()).unwrap_or_default(),
                gap: gap_of(o.cargo.as_deref()),
                choice: v.choice,
                decide: if v.ask.as_str() == "decide" { decide_view(o.cargo.as_deref(), v.choice) } else { None },
                by_ms: v.by_ms,
                overdue: v.overdue,
                aged: v.state == State::Expired && v.by_ms.is_none(),
                answered_by: v.answered_by.clone(),
                done_by: done_by.map(|(id, _)| id),
                marks: marks.into_iter().map(|(k, (m, _))| (k, m)).collect(),
                later_until,
                settled_ms: if resolved { settled_ms } else { None },
                classified_by: if implied.contains(&o.id) { Some("implied: names you and asks a question".into()) } else { v.classified_by.clone() },
                created_ms: o.created_ms,
                card: o.content.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(160).collect(),
            });
        }
        ph.lap("judge.rows");
    }

    let mine = |r: &AskRow| r.held_by.iter().any(|t| same(&aliases, t, &me));
    let wrote = |r: &AskRow| same(&aliases, &r.author, &me);
    let open = |r: &AskRow| r.settled_ms.is_none();
    // My own mark moves an ask to me: none or seen keeps asking on every pull; ack/go and stuck move it to their
    // lists; done or no takes it off mine (for a seat named with others, the ask stays open for them).
    let my_mark = |r: &AskRow| r.marks.get(&me).copied();
    let to_me = |r: &AskRow| (open(r) || still_mine.contains(&r.id)) && mine(r) && !wrote(r) && !matches!(my_mark(r), Some("done" | "no"));
    let mut acked: Vec<AskRow> = rows.iter().filter(|r| to_me(r) && matches!(my_mark(r), Some("ack" | "go"))).cloned().collect();
    let mut stuck: Vec<AskRow> = rows.iter().filter(|r| to_me(r) && my_mark(r) == Some("stuck")).cloned().collect();
    // later: off my waiting list until its time, then back on it by itself (a time passing is not a new record, so the
    // asks cache's own age bounds how late it shows).
    let deferred = |r: &AskRow| my_mark(r) == Some("later") && r.later_until.get(&me).is_some_and(|u| *u > now_ms);
    // An ask is due the moment it is asked; its `by:` is when it expires, not when it is due. Past it (or past the fuse
    // of an undeclared question) and still open, it is set aside: off every open list, kept readable (Blaze, 4 Nov 2026).
    let expired = |r: &AskRow| r.overdue || r.aged;
    let mut aside: Vec<AskRow> = rows.iter().filter(|r| expired(r) && ((to_me(r) && !deferred(r)) || (open(r) && wrote(r)))).cloned().collect();
    let mut later: Vec<AskRow> = rows.iter().filter(|r| to_me(r) && deferred(r) && !expired(r)).cloned().collect();
    let mut waiting_on_me: Vec<AskRow> = rows
        .iter()
        .filter(|r| to_me(r) && !matches!(my_mark(r), Some("ack" | "go" | "stuck")) && !deferred(r) && !expired(r))
        .cloned()
        .map(|mut r| {
            // Another addressee answered; this seat has not: open for this seat, with the other answer beside it.
            if still_mine.contains(&r.id) {
                r.state = "open";
                r.settled_ms = None;
            }
            r
        })
        .collect();
    let mut im_waiting_on: Vec<AskRow> = rows.iter().filter(|r| open(r) && wrote(r) && !expired(r)).cloned().collect();
    let mut done_today: Vec<AskRow> = rows.iter().filter(|r| r.settled_ms.is_some_and(|ms| ms >= today) && (wrote(r) || (mine(r) && !still_mine.contains(&r.id)))).cloned().collect();
    // The oldest first (every open ask is due now, so the one waiting longest leads); what was settled most recently first.
    let by_age = |a: &AskRow, b: &AskRow| a.created_ms.cmp(&b.created_ms);
    waiting_on_me.sort_by(by_age);
    im_waiting_on.sort_by(by_age);
    done_today.sort_by_key(|r| std::cmp::Reverse(r.settled_ms));
    acked.sort_by(by_age);
    stuck.sort_by(by_age);
    later.sort_by_key(|r| r.later_until.get(&me).copied().unwrap_or(i64::MAX));
    aside.sort_by_key(|r| std::cmp::Reverse(r.created_ms));
    let mut totals = Totals {
        waiting_on_me: waiting_on_me.len(),
        im_waiting_on: im_waiting_on.len(),
        done_today: done_today.len(),
        im_waiting_on_answered: im_waiting_on.iter().filter(|r| r.state == "answered").count(),
        oldest_waiting_ms: waiting_on_me.iter().map(|r| r.created_ms).min(),
        aside: aside.len(),
    };
    // A list over the cap keeps its NEWEST rows, in the order above (23 Oct: with 200 old asks open, `ox close` refused
    // a seat's own ask of five minutes ago, because the cut kept the 200 oldest and the closer reads this list; Emmy's
    // repro). done_today is newest first already.
    for l in [&mut waiting_on_me, &mut im_waiting_on, &mut acked, &mut stuck, &mut later, &mut aside] {
        keep_newest(l, LIST_CAP);
    }
    done_today.truncate(LIST_CAP);
    if filter == StateFilter::Open {
        done_today.clear();
        totals.done_today = 0;
    }
    if filter == StateFilter::Done {
        waiting_on_me.clear();
        im_waiting_on.clear();
        acked.clear();
        stuck.clear();
        later.clear();
        aside.clear();
        totals.aside = 0;
        totals.waiting_on_me = 0;
        totals.im_waiting_on = 0;
        totals.im_waiting_on_answered = 0;
        totals.oldest_waiting_ms = None;
    }
    ph.lap("lists");
    Ok(Asks { seat: seat.to_owned(), canonical: (me != seat).then_some(me), waiting_on_me, im_waiting_on, done_today, acked, stuck, later, aside, capped, totals })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(author: &str, content: &str) -> Observation {
        Observation {
            seq: 1,
            id: "OBS-core-20001021-000000000001".into(),
            channel: "core".into(),
            author: author.into(),
            kind: "say".into(),
            content: content.into(),
            cargo: None,
            tags: vec![],
            created_ms: 0,
            authored_ms: None,
            pubkey: String::new(),
            sig: String::new(),
            envelope: 2,
        }
    }

    fn seats() -> BTreeSet<String> {
        ["blaze", "rocky", "dinesh", "bobby-core"].into_iter().map(str::to_owned).collect()
    }

    /// The scan `mentioned` skips, computed as it was before the fast path: the reference for the equivalence below.
    fn mentioned_by_scan(o: &Observation, seats: &BTreeSet<String>) -> Vec<String> {
        crate::store::mentions(&addressing_text(&o.content)).into_iter().filter_map(|t| t.strip_prefix("mention:").map(str::to_owned))
            .filter(|h| *h != o.author && seats.contains(h)).collect()
    }

    /// Content without an ASCII '@' mentions nobody, by the fast path and by the scan alike: Unicode quotes,
    /// backticks, apostrophes, a full-width ＠, an e-mail-like "at", an empty text, a handle written plain.
    #[test]
    fn content_without_an_at_sign_mentions_nobody_on_both_paths() {
        let seats = seats();
        for content in [
            "",
            "blaze, which port?",
            "he wrote \u{201c}blaze ship it\u{201d} yesterday",
            "the fix in `main.rs` is merged; 'blaze' is a name here",
            "Dinesh's fix isn't merged: go?",
            "full-width ＠blaze is not a mention",
            "write to rocky at axxis dot world",
            "日本語のテキストと “引用” と `コード`",
        ] {
            let o = obs("rocky", content);
            assert!(mentioned(&o, &seats).is_empty(), "{content:?}");
            assert_eq!(mentioned(&o, &seats), mentioned_by_scan(&o, &seats), "{content:?}");
        }
    }

    /// Content with an '@' goes through the scan as before: a plain handle addresses, a quoted or coded one does not,
    /// the author's own handle and a stranger's never do; the fast path changes nothing there.
    #[test]
    fn content_with_an_at_sign_is_scanned_as_before() {
        let seats = seats();
        let cases: [(&str, &[&str]); 7] = [
            ("@dinesh and @blaze, which port?", &["dinesh", "blaze"]),
            ("@dinesh the bug: '@blaze' match is case-sensitive; also `@bobby-core` in code", &["dinesh"]),
            ("@dinesh he wrote \u{201c}@blaze ship it\u{201d} yesterday", &["dinesh"]),
            ("Dinesh's fix, @blaze, isn't merged: go?", &["blaze"]),
            ("`@blaze` only in code", &[]),
            ("@rocky talking to myself and @nobody-here", &[]),
            ("mail me at rocky@axxis.world", &[]),
        ];
        for (content, want) in cases {
            let o = obs("rocky", content);
            let mut got = mentioned(&o, &seats);
            got.sort();
            let mut want = want.to_vec();
            want.sort();
            assert_eq!(got, want, "{content:?}");
            assert_eq!(mentioned(&o, &seats), mentioned_by_scan(&o, &seats), "{content:?}");
        }
    }

    /// An explicit recipient (`to:`) stands without any '@' in the text, and beside a mention each once.
    #[test]
    fn explicit_recipients_stand_without_an_at_sign_and_beside_a_mention() {
        let seats = seats();
        let o = obs("rocky", "the port question, no handle in the text: which?");
        assert_eq!(addressed(&["dinesh".to_owned()], &o, &seats), vec!["dinesh".to_owned()]);
        let o = obs("rocky", "@dinesh which port? (blaze decides)");
        assert_eq!(addressed(&["blaze".to_owned()], &o, &seats), vec!["blaze".to_owned(), "dinesh".to_owned()]);
        let o = obs("rocky", "@dinesh which port?");
        assert_eq!(addressed(&["dinesh".to_owned()], &o, &seats), vec!["dinesh".to_owned()], "a recipient named twice is listed once");
    }

    /// docs/GAP.md: an ask row carries the gap its cargo signed, and only an object.
    #[test]
    fn an_ask_row_carries_the_gap_of_its_cargo() {
        let gap = serde_json::json!({"affects": [{"who": "A", "how": "b", "asked": null}], "absent": [], "owed": "x"});
        let cargo = serde_json::json!({"options": ["a", "b"], "gap": gap}).to_string();
        assert_eq!(gap_of(Some(&cargo)), Some(gap));
        assert_eq!(gap_of(Some(r#"{"options":["a","b"]}"#)), None);
        // decide_view: options, the asker's pick, the choice with its label; out-of-range picks and choices are dropped.
        let c = r#"{"options":["Ship now","Wait for CI"],"recommendation":{"option":2,"why":"CI first"}}"#;
        assert_eq!(decide_view(Some(c), None), Some(serde_json::json!({"options":["Ship now","Wait for CI"],"recommendation":{"option":2,"why":"CI first"}})));
        assert_eq!(decide_view(Some(c), Some(1)).unwrap()["chosen"], "Ship now");
        assert!(decide_view(Some(c), Some(3)).unwrap().get("choice").is_none(), "a choice the ask does not offer is not reported");
        assert!(decide_view(Some(r#"{"options":["a","b"],"recommendation":{"option":9}}"#), None).unwrap().get("recommendation").is_none());
        assert_eq!(decide_view(Some(r#"{"options":["only"]}"#), None), None, "one option is not a decide");
        assert_eq!(gap_of(Some(r#"{"gap":"who?"}"#)), None);
        assert_eq!(gap_of(None), None);
    }
}
