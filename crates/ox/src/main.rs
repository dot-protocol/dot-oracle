//! ox — the Oracle CLI.
//!
//! One rule above the rest: ox never reports success for a write the server did not
//! accept. A write that cannot be delivered is queued durably and `ox flush` delivers it;
//! because the `client_op` is preserved, a retry can never double-post.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use ed25519_dalek::SigningKey;
use oracle_mcp::{brief_with, cards_text, items_text, one_line, Written};
use oracle_proto::{keyfile, pubkey_hex, Envelope, Kind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod manual;
mod authority;
mod board;
mod capture;
mod edit;
mod plan;
mod close;
mod doorbell;
mod eval;
mod join;
mod mcp;
mod service;
mod views;

/// ox recall's page size when none is asked; the other verbs folded into recall read it as "none asked".
const RECALL_LIMIT: i64 = 10;

#[derive(Parser)]
#[command(
    name = "ox",
    version,
    about = "The Oracle CLI: sign, speak, fold, search."
)]
struct Cli {
    /// Machine-readable JSON output.
    #[arg(long, global = true)]
    json: bool,
    /// Config directory (default ~/.oracle).
    #[arg(long, env = "OX_HOME", global = true)]
    home: Option<PathBuf>,
    /// The operation id of a write, to retry one whose outcome was unknown as the same write (it is then one event).
    #[arg(long, global = true)]
    op_id: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
// The parsed command line, made once per run: a large Say (its 30-odd flags) costs nothing, and boxing it would only make every
// match on it longer (clippy large_enum_variant, crossed when Say gained --valid-until, 3 Nov 2026).
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Read the reference or agent workflow embedded in this binary, offline.
    Manual {
        #[arg(value_enum, default_value = "reference")]
        topic: manual::Topic,
        /// Check command-row coverage in a manual file (for CI).
        #[arg(long, hide = true)]
        check: Option<PathBuf>,
    },
    /// Connect this machine to an Oracle as a seat.
    Setup {
        /// Oracle base URL, e.g. http://127.0.0.1:8900
        #[arg(long)]
        url: String,
        #[arg(long)]
        handle: String,
        /// Bearer token for this seat. Defaults to ORACLE_TOKEN only when neither token option is given.
        // Resolve the environment after explicit options: clap's env binding conflicts with --token-file.
        // Never include the environment value in help or errors.
        #[arg(long, conflicts_with = "token_file")]
        token: Option<String>,
        /// A file holding the token (what `oracle seat add` and `oracle token new` write), so it never passes through a
        /// command line, a terminal or a log.
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Path to this seat's key file (hex seed or PKCS8 PEM, mode 0600).
        #[arg(long)]
        key: PathBuf,
        /// Another address for the same Oracle, tried when `--url` does not answer (repeatable).
        /// A seat should never depend on one network path: a VPN that is off, a DNS name that
        /// does not resolve, a proxy that is down.
        #[arg(long = "fallback-url")]
        fallback_urls: Vec<String>,
    },
    /// Join an Oracle as a public seat: a key is made on this machine, the server's challenge is signed with it, and
    /// the seat and write token it returns are saved, so every ox command and `ox mcp` work as that seat.
    Join {
        /// Your name in rooms (1 to 60 characters).
        #[arg(long)]
        name: String,
        /// The Oracle to join: its base URL (no default; the page that invites you names it).
        #[arg(long)]
        url: String,
        /// Join with this key file (hex seed or PKCS8 PEM) instead of a new one.
        #[arg(long)]
        key: Option<PathBuf>,
    },
    /// Connect an agent on this machine to this seat: writes the `oracle` skill where that agent reads skills
    /// (skills/oracle: when to search, how to tell the owner, what to write back), then adds `ox mcp` as its MCP server,
    /// through the agent's own `mcp add` where it has one (Claude Code, Codex, Gemini CLI, OpenClaw), or its config
    /// file (Hermes, Cursor).
    Connect {
        #[arg(value_enum)]
        agent: join::Agent,
        /// Show what would be done; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Check everything end to end: config, key, server, registry, a signed round trip.
    Doctor,
    /// Who am I, and does the server agree?
    Whoami,
    /// Speak in a room. Multi-line text is fine.
    Say {
        room: String,
        /// The text; with --from-dot it may be left out (the dot's own text is used).
        #[arg(default_value = "")]
        text: String,
        /// Import a microdot made elsewhere (docs/MICRODOT.md §8): read the picture, write its text as this record, the
        /// picture as its file, and tags dot_id, dot_prev, dot_who, dot_when and dot_seal (ok or failed: a seal is reported,
        /// never read as this seat's signature); a dot whose prev is already in Oracle becomes its reply.
        #[arg(long, conflicts_with = "dot")]
        from_dot: Option<PathBuf>,
        /// Also mint the message as a microdot sealed by this seat (docs/MICRODOT.md §6): chained to the seat's
        /// Oracle chain head, its face put in the room as a file, the message tagged dot:<id> and file:<sha256>.
        /// A reply to a message that carries a dot becomes a v4 composite naming it (rel 1, reply).
        #[arg(long)]
        dot: bool,
        /// The record's type address, domain.form.topic (adds type:<addr>; plan.oracle.search T7): its name, judged by
        /// the writer, routed by prefix (`ox types oracle.decision`), renamed later with `ox retype`.
        #[arg(long = "type")]
        type_addr: Option<String>,
        /// Address a seat (adds to:<handle>); @owner resolves the registry owner. @mentions work too.
        #[arg(long)]
        to: Vec<String>,
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// The observation this answers (adds reply:<id>).
        #[arg(long)]
        reply: Option<String>,
        /// What kind of message this is: result, question, decision, … (adds intent:<x>).
        #[arg(long)]
        intent: Option<String>,
        /// The observation this answers or resolves (adds closes:<id>); it leaves the room's
        /// open list, so use it only when the question is actually settled.
        #[arg(long)]
        closes: Option<String>,
        /// The observation this replaces (adds supersedes:<id>).
        #[arg(long)]
        supersedes: Option<String>,
        /// The thing being worked on, the task's key: `oracle-core#62`, a card id (adds subject:<x>). Replies inherit it.
        #[arg(long)]
        subject: Option<String>,
        /// The exact revision this is about: a commit or version (adds rev:<x>). Evidence at a rev moves the task's
        /// current revision; a verdict binds the rev it names.
        #[arg(long)]
        rev: Option<String>,
        /// The revision this one follows (adds prev:<x>): after the first, a new revision names its predecessor.
        #[arg(long)]
        prev: Option<String>,
        /// A review's verdict on --rev: approve or changes (adds verdict:<x>).
        #[arg(long)]
        verdict: Option<String>,
        /// Take an observation out of the room view (adds archives:<id>). The ledger keeps it.
        #[arg(long)]
        archives: Option<String>,
        /// Hours this should stay open before it ages out of the view; 0 means never.
        #[arg(long)]
        ttl: Option<i64>,
        /// When this record leaves the view (docs/ARCHIVE.md): a span (48h, 7d, 90m) or a unix ms. For alerts and feeds; a person's message never expires unless they say so.
        #[arg(long)]
        expires: Option<String>,
        /// What you need back: none, read, reply, act, decide or review (adds ask:<x>). A question or
        /// proposal asks for a reply by itself; say `--ask act` when someone must do something,
        /// `--ask decide` with 2 to 6 `--option`s to have one picked, `--ask review` with a `--ref`.
        #[arg(long)]
        ask: Option<String>,
        /// An option to pick from, for `--ask decide` (2 to 6, each up to 80 characters, distinct).
        /// Rides in the signed cargo as `options`. The reader adds "Something else…" last.
        #[arg(long = "option")]
        options: Vec<String>,
        /// With --ask decide (required): the option you would pick, numbered from 1 in --option order. It rides in cargo as
        /// `recommendation` and is written into the words, so the Bridge marks it and voice reads it (docs/BRIDGE-CONTRACT.md).
        #[arg(long)]
        recommend: Option<usize>,
        /// With --ask decide (required): one plain sentence, at most 240 characters, on why you recommend that option.
        #[arg(long)]
        why: Option<String>,
        /// Which option (1-based) this reply picks; needs --reply to a message that offers options.
        /// Adds choice:<n>. Any reply answers an ask; this says which one you chose.
        #[arg(long)]
        choice: Option<u32>,
        /// When the ask is due: `90m`, `2h`, `3d`, or `2026-10-01[T15:30]` UTC (adds by:<unix ms>).
        #[arg(long)]
        by: Option<String>,
        /// What this changed or points at: an observation id, a commit, a URL (adds ref:<x>).
        #[arg(long = "ref")]
        refs: Vec<String>,
        /// Ids this is blocked on (cargo `needs`, not prose).
        #[arg(long = "needs")]
        needs: Vec<String>,
        /// How the outcome will be checked, and by whom (cargo `verify`).
        #[arg(long)]
        verify: Option<String>,
        /// Extra facets (adds facet:<x>).
        #[arg(long = "facet")]
        facets: Vec<String>,
        /// Declare an item of this message: `R1=<kind>[:text]`, kind one of requirement question ask
        /// decision complaint. With --reply, `R1=<state>` moves the parent's item (open building built
        /// live accepted blocked dropped); without, a state is refused — or use `ox move`.
        #[arg(long = "item")]
        items: Vec<String>,
        /// Structured data signed with the message: JSON text, or `@path` to read it from a file.
        #[arg(long)]
        cargo: Option<String>,
        /// Until when this holds: a date (through its end), a date and time, or a span (30d, 12h) (adds valid-until:<unix ms>;
        /// docs/SEARCH.md "Valid in time").
        #[arg(long)]
        valid_until: Option<String>,
    },
    /// Mark rooms read without listing them: name the rooms, or `--all` for every room (nothing named refuses).
    #[command(hide = true)]
    Ack {
        rooms: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Seen: move this seat's read mark in a room up to one message, by id — what the eye on the
    /// surface does. Nothing is posted; the mark only moves forward, so seeing an old message never
    /// un-reads the newer ones.
    Seen {
        /// Message ids (the mark moves up to each), or room names (the whole room is marked read; was `ox ack`).
        targets: Vec<String>,
        /// Every room marked read.
        #[arg(long)]
        all: bool,
    },
    /// A room's recent messages, newest last.
    Read {
        /// The room. With none: every room, how many messages, when it last spoke (was `ox rooms`).
        room: Option<String>,
        /// With no room: only rooms that spoke within this span: 7d, 24h, 30m.
        #[arg(long)]
        since: Option<String>,
        /// Follow the Oracle live: one line per new message as it is stored, until stopped (was `ox watch`; `ox read --follow a,b` narrows it to rooms).
        #[arg(long)]
        follow: bool,
        #[arg(long, default_value_t = 15)]
        limit: i64,
        /// Whole messages, with tags and cargo, instead of one line each.
        #[arg(long)]
        full: bool,
        /// Which derived states to show: `open` (the default room view), `open,closed`, or `all`.
        #[arg(long, default_value = "all")]
        state: String,
        /// Voices that are never filtered out, comma separated.
        #[arg(long, default_value = "")]
        keep: String,
        /// Older than this seq: the next page is the previous one's `next_before` (null there means the end).
        /// A page of the past is not presence: the read mark does not move.
        #[arg(long)]
        before: Option<i64>,
        /// Only messages whose text starts with this (`--starts '[Message state:'`). The mark does not move.
        #[arg(long)]
        starts: Option<String>,
        /// Only messages carrying this tag (`--tag intent:decision`, `--tag to:rocky`). The mark does not move.
        #[arg(long)]
        tag: Option<String>,
    },
    /// A room's signed State: the head of its `room-context/1` revisions (purpose, goal, current,
    /// scope, constraints, questions, next, acceptance), resolved exactly as the AXXIS State tab does.
    /// A seat reads its room's State before working in it; Rocky's boot reads `ox state rocky`.
    State {
        room: String,
        /// With --map: a message id, for where that piece of work stands (frontier, fog, done).
        id: Option<String>,
        /// The room's map (docs/ROOM-MAP.md) instead of its State; `--map --set "…"` writes it (was `ox map`).
        #[arg(long)]
        map: bool,
        /// Write a revision: field=text (purpose goal current scope constraints questions next
        /// acceptance; a public room also about question why outcomes latest plan take_part, docs/PUBLIC-STATE.md; a seat's own
        /// room also kind persona boot tools rooms rules). The current head's other fields are kept and the parents filled in.
        #[arg(long = "set")]
        sets: Vec<String>,
        /// The one line people read; defaults to "State updated: <current>".
        #[arg(long)]
        note: Option<String>,
        /// With --set: the id of a proposal of this room that a decider accepted. Refused unless the server shows it settled as accepted by a decider; the revision
        /// records the proposal, the settling mark, who decided and the mark's seq. Provenance, not enforcement: a revision without it is still valid.
        #[arg(long)]
        adopts: Option<String>,
        /// Read the State as it stood below this seq (an exclusive cursor). Read only.
        #[arg(long = "as-of-seq", alias = "as-of")]
        as_of: Option<i64>,
        /// The whole room in one read (= oracle_room_state_full): its State, its board, where its threads stand, its last rounds.
        #[arg(long)]
        full: bool,
        /// With --full: how many recent rounds.
        #[arg(long, default_value_t = 5)]
        rounds: i64,
    },
    /// Where a room stands in a few lines: floors, what is waiting, what needs someone.
    Digest {
        /// The room. With none (or --all): every active room's current and next lines and its oldest open ask (was `ox hallway`).
        room: Option<String>,
        /// Whose messages start a thread.
        #[arg(long, default_value = "blaze,hustler")]
        roots: String,
        /// The digest for a seat arriving (= oracle_room_digest_agent): goal, current, next, blocked and overdue heads,
        /// threads needing someone, the last rounds, in under 500 tokens.
        #[arg(long)]
        agent: bool,
        /// The room's threads, newest activity first: root, state, replies, who took part (was `ox threads`).
        #[arg(long)]
        threads: bool,
        /// With --threads: the page before this cursor.
        #[arg(long)]
        before: Option<i64>,
        /// With --threads: how many.
        #[arg(long, default_value_t = 20)]
        limit: i64,
        /// Every active room, one read.
        #[arg(long)]
        all: bool,
        /// With --all: rooms active in the last N days.
        #[arg(long, default_value_t = 14)]
        days: i64,
    },
    /// End an ask you opened (docs/ASKS-AND-EVENTS.md): `ox close <id> --answered-in chat --why "…"`, `--done`,
    /// `--drop "<reason>"`, `--superseded-by <id>`; or `ox close --batch <file.json>` with entries
    /// `{id, end: closed|done|superseded|dropped, answered_in?, why?, by?}`, validated as a whole before anything is written.
    Close {
        id: Option<String>,
        /// Where the answer came: chat, bridge, call or room:<name>.
        #[arg(long = "answered-in")]
        answered_in: Option<String>,
        /// The answer's words (needed with --answered-in chat), or what was done.
        #[arg(long)]
        why: Option<String>,
        /// The work arrived (for act or review).
        #[arg(long)]
        done: bool,
        /// Drop it, with the reason.
        #[arg(long)]
        drop: Option<String>,
        /// A newer ask on the same matter replaces it.
        #[arg(long = "superseded-by")]
        superseded_by: Option<String>,
        /// A JSON file with many entries.
        #[arg(long)]
        batch: Option<PathBuf>,
    },
    /// Your asks, read out of the messages: waiting on you, what you are waiting on, done today.
    /// `ox asks` (yours), `ox asks --seat rocky`, `--room core`, `--state open|done`.
    #[command(hide = true)]
    Asks {
        /// Only the asks I opened that haven't ended: answered (close them), open past 24 h, open.
        #[arg(long)]
        mine: bool,
        /// Whose lists (default: this seat).
        #[arg(long)]
        seat: Option<String>,
        /// Only this room (default: every room).
        #[arg(long)]
        room: Option<String>,
        /// open, done or all.
        #[arg(long, default_value = "all")]
        state: String,
    },
    /// The global task board, every room: `ox cards` (all open), `--mine` (I own, open), `--waiting` (review or blocked, naming me),
    /// `--room core`, `--owner rocky`, `--status review,blocked|open|done`.
    #[command(hide = true)]
    Cards {
        /// Cards I own that are not done.
        #[arg(long)]
        mine: bool,
        /// Cards in review or blocked whose unblock line names me.
        #[arg(long)]
        waiting: bool,
        #[arg(long)]
        room: Option<String>,
        #[arg(long)]
        owner: Option<String>,
        /// Comma list of queue, in_progress, review, blocked, done; `open` is all but done.
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Start here: every open ask to me from every room, on every pull until I mark it (`ox react <room> <id> ack|stuck|done|no`);
    /// then what is new since I last looked (`--ack` marks it read), the rest counted per room.
    Inbox {
        /// The asks read out of the messages: waiting on me, what I am waiting on, done today (was `ox asks`).
        #[arg(long)]
        asks: bool,
        /// With --asks: another seat's asks.
        #[arg(long)]
        seat: Option<String>,
        /// With --asks: one room.
        #[arg(long)]
        room: Option<String>,
        /// With --asks: open, done or all.
        #[arg(long, default_value = "all")]
        state: String,
        #[arg(long)]
        ack: bool,
        /// Also list the open asks from before the inbox began (8 Oct 2026 08:00 UTC); they are counted either way.
        #[arg(long)]
        older: bool,
        /// Only items addressed to me.
        #[arg(long)]
        mine: bool,
        #[arg(long, default_value_t = 50)]
        limit: i64,
        /// Whole messages, with tags and cargo, instead of one line each.
        #[arg(long)]
        full: bool,
    },
    /// The morning recitation: who I am, my last RESUME note, what is addressed to me, and my work counted.
    Brief {
        /// List every open head and card instead of counting them.
        #[arg(long)]
        full: bool,
        /// The first call for a seat that is new here: who you are, how the rooms work, which rooms exist, what to do next (was `ox hello`).
        #[arg(long)]
        hello: bool,
        /// me (default), or world: WORLD STATE, the page the server composes from signed heads (GET /world): north star,
        /// now, active projects, key decisions, active seats, open questions; the same text oracle_brief where=world prints.
        #[arg(long = "where", value_name = "me|world")]
        place: Option<String>,
        /// Printed for an agent's session-start hook (docs/CAPTURE.md): cursor wants JSON (`additional_context`); Claude Code
        /// and Codex take the text as it is.
        #[arg(long, value_name = "claude-code|codex|cursor")]
        hook: Option<String>,
    },
    /// The waking read (Blaze, 1 Nov 2026: food for an agent is the diff between the world's last known state and its
    /// current state): what changed since my last RESUME, or --since an OBS id, a time (2026-10-31T08:00, UTC) or a span
    /// (6h, 2d): the rooms I follow, the asks that involve me, Blaze's words, the machine. The page oracle_delta prints.
    Delta {
        #[arg(long)]
        since: Option<String>,
    },
    /// Who I am (docs/IDENTITY.md): print my identity head whole, or --seat's; --write <file.json> writes a new head
    /// (identity-head/1) after checking the persona record it names, and records that record's hash.
    Identity {
        #[arg(long)]
        seat: Option<String>,
        #[arg(long)]
        write: Option<PathBuf>,
    },
    /// How a thing is done (docs/PROCEDURES.md): list every procedure, print one whole by name, or --write a new
    /// version (procedure/1) from a JSON file in my own room.
    Procedure {
        name: Option<String>,
        #[arg(long)]
        write: Option<PathBuf>,
    },
    /// The author chain (docs/CHAIN.md): fetch my chain (or --seat's, as admin) and check it: every signature over its
    /// rebuilt bytes, every id, every prev. --out writes the face (one JSON line per record); --check verifies a face
    /// file with no server at all.
    Chain {
        #[arg(long)]
        seat: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        check: Option<PathBuf>,
    },
    /// The rule canon (docs/CANON.md): print my canon whole (grouped by kind, each rule with its sources), one --kind,
    /// another --seat's, or --write a new version (canon/1) from a JSON file in my own room.
    Canon {
        #[arg(long)]
        seat: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        write: Option<PathBuf>,
    },
    /// Tell the owner (docs/BRIDGE-CONTRACT.md): the one way to write to Blaze. A head of at most 40 words; --ask decide
    /// with 2 to 6 --option, --recommend <n>, --why and --by, or --ask read, or none (a result); detail in --detail
    /// page.json (doc/1 or plan/1).
    Tell {
        head: String,
        /// Also mint it as a microdot sealed by this seat (docs/MICRODOT.md §6), like `ox say --dot`.
        #[arg(long)]
        dot: bool,
        #[arg(long)]
        room: Option<String>,
        #[arg(long, default_value = "none")]
        ask: String,
        #[arg(long = "option")]
        options: Vec<String>,
        /// A decide's recommended option, numbered from 1 in --option order (required for --ask decide).
        #[arg(long)]
        recommend: Option<usize>,
        /// One plain sentence: why that option (required for --ask decide; at most 240 characters).
        #[arg(long)]
        why: Option<String>,
        #[arg(long)]
        by: Option<String>,
        #[arg(long = "ref")]
        refs: Vec<String>,
        #[arg(long)]
        detail: Option<PathBuf>,
        #[arg(long)]
        intent: Option<String>,
        #[arg(long)]
        reply: Option<String>,
        /// The gap of a decide (docs/GAP.md): who it affects and how, "<who>: <how>"; `@OBS-…` at the end cites where they spoke.
        #[arg(long = "affects")]
        affects: Vec<String>,
        /// Who is affected but not at this table, and why: "<who>: <why>", or `none`.
        #[arg(long = "absent")]
        absent: Vec<String>,
        /// What the decision owes those it affects who had no voice.
        #[arg(long)]
        owed: Option<String>,
    },
    /// Record durable knowledge.
    Remember {
        text: String,
        /// decision | lesson | preference | commitment
        #[arg(long)]
        kind: String,
        #[arg(long, default_value = "memory")]
        room: String,
        /// Until when this holds: a date (through its end), a date and time, or a span (30d, 12h). After it, recall ranks it
        /// as a replaced fact and says since when (adds valid-until:<unix ms>; docs/SEARCH.md "Valid in time").
        #[arg(long)]
        valid_until: Option<String>,
    },
    /// Search everything.
    Recall {
        #[arg(required_unless_present_any = ["terms", "eval"])]
        query: Option<String>,
        /// How many hits (10 since 26 Oct: at 5 a seat read the newest layer and missed the older records).
        #[arg(long, default_value_t = RECALL_LIMIT)]
        limit: i64,
        #[arg(long)]
        room: Option<String>,
        /// With --room: what the room has said about it, newest first, who claimed what and what was corrected (was `ox about`).
        #[arg(long)]
        newest: bool,
        /// Which rooms say it: how often, first and last date, most first (was `ox where`).
        #[arg(long = "where")]
        which_rooms: bool,
        /// With --where: any of the words instead of all.
        #[arg(long)]
        any: bool,
        /// The words that matter across the Oracle, ranked, refreshed hourly (was `ox terms`).
        #[arg(long)]
        terms: bool,
        /// Instead of hits, a cheap check before recall: can Oracle answer this? yes, partial or no, the words on record and
        /// not, and what is approved (GET /answerable).
        #[arg(long)]
        answerable: bool,
        /// Run a frozen query set (JSONL: q, expect, none) against search and score it (was `ox eval`).
        #[arg(long)]
        eval: Option<PathBuf>,
        /// With --eval: extra search parameters, key=value, repeatable.
        #[arg(long = "param")]
        params: Vec<String>,
        /// With --eval: also check each case's brief words against the brief this seat wakes with.
        #[arg(long)]
        brief: bool,
        /// Search the archived rooms and expired records too (docs/ARCHIVE.md); off, the answer says what it left out.
        #[arg(long = "include-archived")]
        include_archived: bool,
        /// As of a moment: only what was written by then, and what still held then (a date through its end, or a date and
        /// time; docs/SEARCH.md "Valid in time").
        #[arg(long = "as-of")]
        as_of: Option<String>,
    },
    /// Run a frozen query set (JSONL: q, expect, none) against search: per query the rank of the right observation and
    /// how the answer reads (bytes, duplicates, synthetic and noisy hits, edges, "no prior"), then hit@k, MRR and latency.
    #[command(hide = true)]
    Eval {
        set: PathBuf,
        #[arg(long, default_value_t = 10)]
        limit: i64,
        /// Extra search parameters for an arm being measured, key=value, repeatable (e.g. --param ranker=and-first).
        #[arg(long = "param")]
        params: Vec<String>,
        /// Also check each case's `brief` words against the brief this seat wakes with (docs/RECALL-SCOREBOARD.md 2f).
        #[arg(long)]
        brief: bool,
    },
    /// The exact time, to the millisecond, from the Oracle's clock. Ground "now" here — never in
    /// what you remember — before you search the web or say "as of". With --head: what I am doing this minute
    /// (docs/NOW.md), mine or --seat's; --set field=text writes a new head with those fields changed and the rest kept.
    Now {
        /// Print the Now head instead of the clock.
        #[arg(long)]
        head: bool,
        /// Another seat's head (default: mine).
        #[arg(long)]
        seat: Option<String>,
        /// field=text, for task why room repo checkout branch head last next waiting; writes a new head.
        #[arg(long = "set")]
        sets: Vec<String>,
        /// A field to remove; writes a new head.
        #[arg(long)]
        clear: Vec<String>,
    },
    /// First call for a seat that is new here: who you are, how the rooms work, which rooms exist, what to do next (= oracle_hello).
    #[command(hide = true)]
    Hello,
    /// The server: version, build commit, sizes, the room index, search latency, cross-seat mode (`GET /health`).
    Health,
    /// Every route since the server started: calls, errors, p50/p95/max ms, the top callers (`GET /metrics/routes`).
    Metrics {
        /// Only the routes whose path contains this (`--route /asks`).
        #[arg(long)]
        route: Option<String>,
        /// Order by calls, p95, max or errors.
        #[arg(long, default_value = "calls")]
        sort: String,
        /// Also the routes nobody has called.
        #[arg(long)]
        all: bool,
    },
    /// Every seat: live, idle, gone or retired; last seen, calls, last route, since the server started (`GET /presence`).
    #[command(hide = true)]
    Presence,
    /// The key registry: each seat, its role, its key(s), when added, revoked (`GET /keys`).
    Seats {
        /// The signed key events instead, oldest first (`GET /keys?history=true`).
        #[arg(long)]
        history: bool,
        /// Who is live, idle, gone or retired: last seen, calls, last route (was `ox presence`).
        #[arg(long)]
        live: bool,
        /// Write the key file a reader needs to check records with no server (`oracle-keys/1`, docs/VERIFY.md): the
        /// owner key and every signed key event. Publish it where readers can fetch it.
        #[arg(long, value_name = "FILE", conflicts_with_all = ["history", "live"])]
        export: Option<PathBuf>,
    },
    /// Check records with no server at all (docs/VERIFY.md): replay the key file's log from the owner key, then check
    /// each record of a face (`ox chain --out`) against the key its author held when it signed. Pin the owner key with
    /// --owner-key (from the owner's published page); without it, the key inside the file is trusted and the report says so.
    Verify {
        face: PathBuf,
        #[arg(long)]
        keys: PathBuf,
        #[arg(long, value_name = "HEX")]
        owner_key: Option<String>,
    },
    /// Every room: how many messages, when it last spoke, newest first (`GET /channels`).
    #[command(hide = true)]
    Rooms {
        /// Only rooms that spoke within this span: 7d, 24h, 30m.
        #[arg(long)]
        since: Option<String>,
    },
    /// A room's threads, newest activity first: root, state, replies, who took part (`GET /rooms/<room>/threads`).
    #[command(hide = true)]
    Threads {
        room: String,
        #[arg(long, default_value_t = 20)]
        limit: i64,
        /// Older than this: the previous page's `next_before`.
        #[arg(long)]
        before: Option<i64>,
    },
    /// State heads: mine, another owner's, or everyone's (= oracle_tasks).
    #[command(hide = true)]
    Heads {
        /// Another seat's heads.
        #[arg(long, conflicts_with = "all")]
        owner: Option<String>,
        /// Everyone's heads.
        #[arg(long)]
        all: bool,
    },
    /// Which rooms say it: how often, first and last date, most first (= oracle_where).
    #[command(hide = true)]
    Where {
        #[arg(required = true)]
        words: Vec<String>,
        /// Any of the words, not all of them.
        #[arg(long)]
        any: bool,
    },
    /// Can Oracle answer this, before recall (= oracle_recall answerable): yes, partial or no.
    #[command(hide = true)]
    Answerable {
        #[arg(required = true)]
        words: Vec<String>,
        #[arg(long)]
        room: Option<String>,
    },
    /// The words that matter across the Oracle and the Time Machine, ranked, refreshed hourly (= oracle_terms).
    #[command(hide = true)]
    Terms {
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// The experiment registry, newest first: stage, room, title, result (= oracle_experiments).
    #[command(hide = true)]
    Experiments {
        #[arg(long)]
        track: Option<String>,
        #[arg(long)]
        stage: Option<String>,
        /// Words to narrow by.
        #[arg(long)]
        q: Option<String>,
        #[arg(long, default_value_t = 60)]
        limit: i64,
    },
    /// Follow the Oracle live: one line per new message as it is stored (`GET /stream`). Runs until stopped.
    #[command(hide = true)]
    Watch {
        /// Only these rooms, comma separated.
        #[arg(long, default_value = "")]
        room: String,
    },
    /// Everything to read before answering: the time, the message, its whole thread, where the room
    /// stands, and the room's shared book if its owner has published one. A few hundred tokens.
    Context {
        room: String,
        /// The message you were rung about (any message in the thread works).
        #[arg(long)]
        re: Option<String>,
        /// Whose messages start a thread in this room.
        #[arg(long, default_value = "blaze,hustler")]
        roots: String,
    },
    /// A map: where a big piece of work stands. The destination, the FRONTIER (items that can be worked now), the
    /// FOG (items waiting on others, and on what), and what is DONE with the reply that resolved it. Declare items
    /// with `--item K=research|prototype|grill|task|destination:text` and edges with `--tag needs:K=K1,K2`; resolve
    /// with a reply carrying `--item K=<state> --tag resolves:K`. `ox map dot OBS-…`
    ///
    /// Without an id: the room's map (docs/ROOM-MAP.md) — the written map from its State and what the index
    /// computes; `ox brief` prints one line per room my State names. `--set "…"` writes it (one State revision).
    #[command(hide = true)]
    Map {
        room: String,
        /// A work map's root message; without it, the room's map.
        id: Option<String>,
        /// Write the room's map (one State revision that keeps every other field): a few lines, the first says the most; at most 4,000 characters.
        #[arg(long)]
        set: Option<String>,
    },
    /// What this room has said about one thing, newest first: who claimed what, in which thread, and
    /// what was later corrected. Start here before you say anything new about it:
    /// `ox about hustler AAVE AAVEUSDT`
    #[command(hide = true)]
    About {
        room: String,
        /// Names for the thing, matched as whole words (give the ticker and the venue symbol).
        #[arg(required = true)]
        terms: Vec<String>,
        #[arg(long, default_value_t = 25)]
        limit: i64,
    },
    /// One message in full, by id — what a doorbell or a `--reply` points at. With `--thread`, the thread it is in:
    /// the root, then its replies newest first, one page at a time (`next_before` null means nothing older).
    Get {
        id: String,
        #[arg(long)]
        thread: bool,
        /// With --thread: replies older than this seq, the previous page's `next_before`.
        #[arg(long, requires = "thread")]
        before: Option<i64>,
        /// With --thread: replies per page, 1 to 200 (the server's default is 50).
        #[arg(long, requires = "thread")]
        limit: Option<i64>,
        /// With --thread: each message's cargo too.
        #[arg(long, requires = "thread")]
        cargo: bool,
        /// The record's dot (docs/MICRODOT.md §6): fetch its face, read it, check the seal, the body against the
        /// record's content and `who` against the author's key; print the fields and the hex (or the PNG to --out).
        #[arg(long, conflicts_with = "thread")]
        dot: bool,
        /// With --dot: write the face PNG here instead of printing the hex.
        #[arg(long, requires = "dot")]
        out: Option<PathBuf>,
    },
    /// Records by type address (plan.oracle.search T7): `ox types oracle.decision` lists the records whose type is that
    /// address or under it, newest first, with the count per type; with none, every type.
    Types {
        prefix: Option<String>,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Rename a record's type address: writes retype:<id> and type:<addr> in the record's room. Only the record's own
    /// author renames it; anyone else's retype is kept as a proposal, and every one stays in the ledger.
    Retype { id: String, addr: String },
    /// Edit a record in place (Blaze, 3 Nov 2026: the competitor map's editable-memory row; docs/EDIT.md): a new record in
    /// the same room, of the same kind, that supersedes it; the old one is kept, read whole by id, and ranks under the new one
    /// in recall. A seat edits its own records; the owner edits any. The new text is --text, --file, or $EDITOR on the old.
    /// Capture a coding agent's session as memories (docs/CAPTURE.md; the competitor map's agent-adapters and session-extraction
    /// rows, 4 Nov 2026). `--hook <agent>` is what an agent's hook runs: it spools the session and returns at once. `--drain`
    /// reads each spooled transcript from where the last capture stopped, asks the Oracle's /extract for the decisions,
    /// lessons, preferences and commitments in it, and signs each as this seat's memory. `--file` captures one transcript by
    /// hand; `--dry-run` prints what would be kept and writes nothing.
    Capture {
        #[arg(long, value_name = "claude-code|codex|cursor", conflicts_with_all = ["drain", "file"])]
        hook: Option<String>,
        #[arg(long, conflicts_with = "file")]
        drain: bool,
        #[arg(long)]
        file: Option<PathBuf>,
        /// With --file: the agent that wrote the transcript.
        #[arg(long, default_value = "claude-code")]
        agent: String,
        /// The room the memories go to.
        #[arg(long, default_value = "memory")]
        room: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Reflect (docs/REFLECT.md): an Oracle has one reflector, not one per agent (Blaze, 4 Nov 2026: 'there should only be
    /// one, Pythia, which would be doing it for all of oracle'). Every seat's memories in each room that came since the last
    /// reflection, oldest first, go to the Oracle's /reflect with the reflector's current observations. Each observation that
    /// comes back is signed as the reflector's memory in that same room (`reflection`, `observation:<subject>`, a `ref:` for
    /// each memory it rests on); one that refines an earlier observation supersedes it, and the earlier one is kept. Run it
    /// nightly as the reflector, or by hand; --dry-run prints what it would write and writes nothing.
    Reflect {
        /// The rooms to reflect on, each on its own: observations stay in the room their memories came from. `all` is every
        /// room in one pass.
        #[arg(long, default_value = "memory", value_delimiter = ',')]
        room: Vec<String>,
        /// Authors whose memories are left out: automated feeds that write memories but are not minds.
        #[arg(long, value_delimiter = ',')]
        except: Vec<String>,
        /// Memories read in one call to /reflect.
        #[arg(long, default_value_t = 24)]
        limit: usize,
        /// Calls a room gets in one run; what is left waits for the next run.
        #[arg(long, default_value_t = 4)]
        batches: usize,
        #[arg(long)]
        dry_run: bool,
    },
    Edit {
        id: String,
        /// The new text.
        #[arg(long, conflicts_with = "file")]
        text: Option<String>,
        /// The new text, read from a file.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Why it changed, in one line (cargo `edit.why`).
        #[arg(long)]
        why: Option<String>,
    },
    /// One subject's state, read from the record instead of remembered (Blaze, 2026-09-30: "nothing should be
    /// guessed"): who holds it now, in what state, and every revision by every seat, oldest first — who created it,
    /// who moved it, who closed it and why. `ox head policy.p1`
    Head {
        /// The subject. With none: my heads, `--owner s`'s, or everyone's with `--all` (was `ox heads`).
        subject: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
        #[arg(long, conflicts_with = "all")]
        owner: Option<String>,
        #[arg(long)]
        all: bool,
        /// The experiment registry, newest first: stage, room, title, result (was `ox experiments`).
        #[arg(long)]
        exp: bool,
        /// With --exp: a track (qsb, fhe, oracle-memory).
        #[arg(long)]
        track: Option<String>,
        /// With --exp: a stage (proposed, running, measured, killed).
        #[arg(long)]
        stage: Option<String>,
        /// With --exp: words that must all appear.
        #[arg(long)]
        q: Option<String>,
    },
    /// Put a task on a room's board, or move one that is there. One invocation posts ONE signed
    /// `room-task-event/1` message (a status, not speech) whose moves carry the `room-task/1` revisions:
    /// `ox card hustler q3-kernel-read in_progress "Kernel reads oracle-core" --summary … --acceptance …`
    /// A card already on the board keeps what you leave out: `ox card hustler q3-kernel-read review --handoff …`.
    /// Many cards at once, still one message: `ox card hustler --batch moves.json` (or `-` for stdin),
    /// a JSON array of {id, status, title?, summary?, acceptance?, evidence?, handoff?, unblock?, owner?, …}.
    Card {
        /// The room. With none: the board, every room (was `ox cards`): --mine, --waiting, --owner, --state, --limit.
        room: Option<String>,
        /// Stable id, lowercase letters, digits and dashes. Reusing it edits the card.
        id: Option<String>,
        /// queue | in_progress | review | blocked | done; `take` takes the card (was `ox take`), `done` with --evidence and --handoff finishes it (was `ox done`).
        status: Option<String>,
        /// Needed for a new card; an existing card keeps its title.
        title: Option<String>,
        /// Board: cards I own, open.
        #[arg(long)]
        mine: bool,
        /// Board: cards in review or blocked, naming me.
        #[arg(long)]
        waiting: bool,
        /// Board: one status (open, done, …).
        #[arg(long)]
        state: Option<String>,
        /// Board: at most this many.
        #[arg(long)]
        limit: Option<usize>,
        /// A JSON array of moves (path, or `-` for stdin), posted as one message.
        #[arg(long, conflicts_with_all = ["id", "status", "title"])]
        batch: Option<String>,
        #[arg(long)]
        summary: Option<String>,
        #[arg(long)]
        acceptance: Option<String>,
        /// Defaults to the card's owner, or this seat for a new card.
        #[arg(long)]
        owner: Option<String>,
        /// Defaults to the card's, or "Run reliably".
        #[arg(long)]
        group: Option<String>,
        /// Defaults to the card's, or "high".
        #[arg(long)]
        impact: Option<String>,
        /// Defaults to the card's, or "not_live".
        #[arg(long)]
        delivery: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        /// Proof: a SHA, an obs id, a URL. `done` needs at least one.
        #[arg(long = "evidence")]
        evidence: Vec<String>,
        #[arg(long = "depends")]
        depends: Vec<String>,
        /// The other rooms this card touches (`--rooms axxis,hustler`): it stays on this board and is
        /// counted on theirs. Defaults to the card's; `--rooms ''` clears it.
        #[arg(long, value_delimiter = ',')]
        rooms: Option<Vec<String>>,
        /// The handoff note (how-we-work.md: "handing a task on is a responsibility, not a status flip"):
        /// what changed · how to test it · the gotcha. `review` and `done` need one.
        #[arg(long)]
        handoff: Option<String>,
        /// Who can unblock it. `blocked` needs one, and the summary says why.
        #[arg(long)]
        unblock: Option<String>,
        /// The message this card carries out (OBS-…): a linked card moves its thread.
        #[arg(long = "ref")]
        link: Option<String>,
    },
    /// Update a message's state without a new message: `ox annotate hustler OBS-… --set intent=decision
    /// --set ask=act --set item:R1=requirement --why "the words that decided it"`. The room folds it
    /// onto the message (the author's own tags win, a person beats a model, newest wins).
    Annotate {
        room: String,
        id: String,
        /// key=value, repeatable: intent=… ask=… to=<seat> ref=… by=model|person, item:R1=<kind>
        #[arg(long = "set", required = true)]
        sets: Vec<String>,
        /// What decided it — the annotation's face.
        #[arg(long)]
        why: String,
        #[arg(long)]
        confidence: Option<f64>,
        /// Structured payload (e.g. items/1), JSON text.
        #[arg(long)]
        cargo: Option<String>,
    },
    /// Move an item: `ox move hustler OBS-… R2=built --note "…"` (= ox annotate … --set item:R2=built).
    /// To decline or drop the whole message: `ox react <room> <id> no --why …`.
    #[command(hide = true)]
    Move {
        room: String,
        id: String,
        /// R2=<open|building|built|live|accepted|blocked|dropped>
        what: String,
        #[arg(long)]
        note: String,
    },
    /// Claim an item of a message: `ox claim hustler OBS-… Q2 --note "…"`. A reply tagged owner:Q2=<you> (or --seat
    /// for another seat); the room's State tab shows the newest claim as the item's WHO.
    #[command(hide = true)]
    Claim {
        room: String,
        id: String,
        /// The item: Q1, R2, A1 …
        item: String,
        /// Claim it for another seat (default: you).
        #[arg(long)]
        seat: Option<String>,
        #[arg(long)]
        note: String,
    },
    /// Vote on an item: `ox vote hustler OBS-… Q1=for --note "why"` (for, against or abstain). A reply tagged
    /// vote:Q1=for; a seat's newest vote counts, and the reply's words are the reason.
    #[command(hide = true)]
    Vote {
        room: String,
        id: String,
        /// Q1=<for|against|abstain>
        what: String,
        #[arg(long)]
        note: String,
    },
    /// Archive a thread out of the view: `ox archive hustler OBS-…` (the room's Archive button).
    Archive {
        room: String,
        id: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// WORKING: you are on one task and check the room yourself as you go (ox ends every call with
    /// "N new for you"), so the doorbell stops ringing you. On a card: `ox working q3 --room hustler`
    /// (= ox take: the card moves to in_progress with you as owner); free text: `ox working "the
    /// ledger replay"`. Still rung for your card, a decider's ✓/✕ on your work, and an ask:act to you.
    Working {
        what: String,
        /// The card's room: with it, <what> is a card id and this is `ox take`.
        #[arg(long)]
        room: Option<String>,
    },
    /// WAITING: you sent your done, you have no next action — rings back ON:
    /// `ox waiting "Blaze's go on the deploy"`.
    Waiting {
        what: Option<String>,
    },
    /// Same as `ox waiting`.
    #[command(hide = true)]
    Idle {
        #[arg(long)]
        why: Option<String>,
    },
    /// Take a card: it moves to in_progress with you as owner, and you are WORKING on it from now
    /// (rings off). `ox take hustler q3`
    #[command(hide = true)]
    Take {
        room: String,
        card: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Finish the card you took: it moves to done (with proof and a handoff) and you are WAITING
    /// again (rings on).
    /// `ox done hustler q3 --evidence <sha|OBS> --handoff "what changed · how to test · the gotcha"`
    #[command(hide = true)]
    Done {
        room: String,
        card: String,
        #[arg(long = "evidence", required = true)]
        evidence: Vec<String>,
        #[arg(long)]
        handoff: String,
    },
    /// The hallway: every active room's State current and next lines and its oldest open ask, one read.
    #[command(hide = true)]
    Hallway {
        #[arg(long, default_value_t = 14)]
        days: i64,
    },
    /// Files: bytes whose sha256 is their id, in a room (Round 68: "everything should happen through Oracle").
    /// `ox file put <room> <path> [--name n] [--type image/png]` → sha256:<hex>; `ox file get <id> [--out path]` (stdout
    /// when no --out); `ox file meta <id>` (size, type, rooms, the messages citing it); `ox file list <room>`. A message
    /// cites a file with the tag `file:<hex>`.
    File {
        /// put | get | meta | list
        op: String,
        args: Vec<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "type")]
        ctype: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// The graph of Oracle (docs/GRAPH.md): `ox graph [--view core|cortex|seo] [--min 3] [--marks approved|all] [--top 120]`; `--json` for the nodes and edges.
    Graph {
        #[arg(long, default_value = "cortex")]
        view: String,
        #[arg(long)]
        min: Option<i64>,
        /// cortex: approved (Blaze's verdict, the default) or all (drafts too)
        #[arg(long)]
        marks: Option<String>,
        /// seo: how many ranked words (default 120, at most 500)
        #[arg(long)]
        top: Option<i64>,
    },
    /// The cortex (claims): `ox cortex mint <room> --statement … --type … --status … [--source …] [--row B12]`,
    /// `ox cortex edge <room> <from> <supports|contradicts|supersedes|is-same-as|cites> <to>`,
    /// `ox cortex status <room> <cid> <verified|refuted|open> [--why …]`, `ox cortex list [--room …] [--status …] [--cid …]`.
    /// The widened cortex (docs/CORTEX.md): `ox cortex nodes [--room …] [--type question|entity|concept] [--q …]`,
    /// `ox cortex node <nid>`, `ox cortex marks [--room …] [--source <message>]`.
    /// Mark the exact words of an observation as a Brain-cases mark held for Blaze's approval (docs/CORTEX.md): `ox mark OBS-… --quote "…"
    /// --label Vision --note "why, then and now" --link about:OBS-… --link refines:m1830`. One label of the fourteen tests, at least one
    /// lineage link found with ox recall. Written to #brain. Never personal history, tool picks or one-session logistics.
    Mark {
        observation: String,
        #[arg(long)]
        quote: String,
        /// Which occurrence of the quote (default 1).
        #[arg(long, default_value_t = 1)]
        nth: usize,
        #[arg(long = "label", required = true)]
        labels: Vec<String>,
        #[arg(long = "intent")]
        intents: Vec<String>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long = "link", required = true)]
        links: Vec<String>,
    },
    /// Blaze's answer on a mark or node: `ox verdict OBS-… approve|reject|note [--text …]`; approve lets it into the Cortex.
    Verdict {
        target: String,
        verdict: String,
        #[arg(long)]
        text: Option<String>,
    },
    /// Brain cases through the cortex (docs/BRAIN-CASES.md, for allowed seats): `ox case list [--which drafted]`, `ox case read 146`,
    /// `ox case draft --case <prompt uuid> [--msg <reply uuid>] --quote "…" --label Vision --intent "…" --frame "…" --state "…"
    /// --method "…" --lineage "…" --link part-of:m1875 [--supersedes m1910] [--about "hub"]`. Approval is Blaze's, in his page.
    Case {
        /// list | read | draft
        op: String,
        args: Vec<String>,
        #[arg(long)]
        which: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u64,
        #[arg(long)]
        case: Option<String>,
        #[arg(long)]
        msg: Option<String>,
        #[arg(long)]
        quote: Option<String>,
        #[arg(long, default_value_t = 1)]
        nth: u64,
        #[arg(long = "label")]
        labels: Vec<String>,
        #[arg(long)]
        intent: Option<String>,
        #[arg(long)]
        frame: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        method: Option<String>,
        #[arg(long)]
        lineage: Option<String>,
        #[arg(long = "link")]
        links: Vec<String>,
        #[arg(long)]
        supersedes: Option<String>,
        #[arg(long = "about")]
        about: Vec<String>,
        #[arg(long = "entity")]
        entities: Vec<String>,
        #[arg(long = "why")]
        intents: Vec<String>,
        #[arg(long)]
        project: Option<String>,
    },
    Cortex {
        /// mint | edge | status | source | list (claims) · nodes | node | marks (docs/CORTEX.md)
        op: String,
        args: Vec<String>,
        #[arg(long)]
        statement: Option<String>,
        #[arg(long = "type")]
        kind: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long = "source")]
        sources: Vec<String>,
        #[arg(long)]
        row: Option<String>,
        #[arg(long)]
        why: Option<String>,
        #[arg(long)]
        room: Option<String>,
        #[arg(long)]
        cid: Option<String>,
        /// nodes: titles and aliases containing this
        #[arg(long)]
        q: Option<String>,
        /// marks: only those with no verdict yet (waiting for Blaze)
        #[arg(long)]
        pending: bool,
        /// nodes: every node, drafts included, each with its approval class (default: the Cortex, what Blaze approved)
        #[arg(long)]
        all: bool,
    },
    /// Mark a message or a card: `ox react hustler OBS-… go` — 👁 seen · ✓ go · ♥ like · ✕ no
    /// (`--why` says why). A decider's go or no rings the author; an agent's no rings the decider.
    /// An agent's seen is its read mark: `ox seen <id>`.
    React {
        room: String,
        /// OBS-… or a card id
        id: String,
        /// seen | go | like | no | ack | stuck | done | later (with --until); `claim <item>` claims an item (was `ox claim`), `vote <item>=for|against|abstain` votes on one (was `ox vote`)
        kind: String,
        /// With claim or vote: the item (Q2, or Q1=for).
        item: Option<String>,
        #[arg(long)]
        why: Option<String>,
        /// With claim or vote: the reply's text.
        #[arg(long)]
        note: Option<String>,
        /// With claim: claim it for another seat.
        #[arg(long)]
        seat: Option<String>,
        /// With later: when it comes back (30m, 4h, 2d, 1w, or epoch milliseconds).
        #[arg(long)]
        until: Option<String>,
    },
    /// A machine-readable progress update (`status/1`): live stage, n of total, since when.
    /// `ox status hustler "replaying fills" --stage replay --task q3-ledger --n 3 --of 7`
    /// The newest status with the same key replaces the older one in the room's view.
    Status {
        room: String,
        /// The one line people read.
        text: String,
        #[arg(long)]
        stage: String,
        /// The card this is progress on.
        #[arg(long)]
        task: Option<String>,
        /// What a newer status replaces; defaults to --task, else run:<seat>.
        #[arg(long)]
        key: Option<String>,
        /// running | stalled | failed | done. Left out: running, or done/failed when that is the stage.
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        n: Option<u64>,
        #[arg(long)]
        of: Option<u64>,
        #[arg(long = "eta-ms")]
        eta_ms: Option<i64>,
        /// A SHA, an obs id, a URL.
        #[arg(long = "ref")]
        refs: Vec<String>,
    },
    /// Write an owned state head: `ox task ship-v1 running "cutting the release" --next 2026-10-01`
    Task {
        subject: String,
        /// noted | open | running | blocked | done
        state: String,
        note: String,
        #[arg(long)]
        next: Option<String>,
        #[arg(long, default_value = "board")]
        room: String,
        /// Machine state carried with the head (JSON text, or `@path`): what a feed publishes as its
        /// current value, e.g. a book, instead of posting it into a room over and over.
        #[arg(long)]
        data: Option<String>,
        /// blocked only: who or what must act (a handle, a subject, an obs id); repeatable.
        #[arg(long)]
        needs: Vec<String>,
        /// blocked only: the one step that cannot proceed; everything else continues.
        #[arg(long)]
        blocks: Option<String>,
        /// blocked only: how anyone can verify the blocker still holds.
        #[arg(long)]
        check: Option<String>,
    },
    /// A plan head (docs/PLANS.md): a State head, subject `plan.<room>.<name>`, whose data is a `plan/1` page (the
    /// problems and the steps that fix them). `ox plan <room> <name> --write page.json` posts a revision from a file;
    /// `ox plan <room> <name> --step S2=doing` moves one step (proposed | doing | done | dropped) in a new revision,
    /// history kept; `ox plan <room> <name>` prints the newest revision and its history. The head's state follows
    /// the steps: every step done or dropped = done, any step doing = running, else open.
    Plan {
        #[arg(required_unless_present = "pipeline")]
        room: Option<String>,
        #[arg(required_unless_present = "pipeline")]
        name: Option<String>,
        #[arg(long)]
        write: Option<PathBuf>,
        #[arg(long)]
        step: Option<String>,
        /// A step's place in the one pipeline across every plan, 1 first: `--rank S2=3`; `S2=` clears it.
        #[arg(long)]
        rank: Option<String>,
        /// What a step waits on: `--after S2=S1,plan.oracle.search:T14`; `S2=` clears it. A circle is refused.
        #[arg(long)]
        after: Option<String>,
        /// Every open step of every plan, ranked steps first, each with its owner and what it still waits on.
        #[arg(long)]
        pipeline: bool,
        /// The revision's note (default: the plan's title and its counts)
        #[arg(long)]
        note: Option<String>,
        /// When this plan is next touched (a date); an open or running head needs one or it rots. A step move keeps
        /// the previous revision's date unless --next says otherwise.
        #[arg(long)]
        next: Option<String>,
    },
    /// Serve this seat to an MCP client (Claude Code, Codex, Cursor, …) over stdio.
    Mcp,
    /// One doorbell look for the Claude Code session in this environment (`ox mcp` runs it every 15 s as its own
    /// process, so an updated ox takes effect on the next look, with no MCP reconnect).
    #[command(hide = true)]
    DoorbellTick,
    /// Run the Oracle service for this machine: holds the seats' keys and outboxes, retries queued
    /// writes, keeps the Oracle clock, and answers local clients on a Unix socket (`oracle-local/1`).
    Serve {
        /// A seat's home (repeat for several seats). Default: --home.
        #[arg(long = "seat")]
        seats: Vec<PathBuf>,
        /// Where the socket lives. Default: <home>/run.
        #[arg(long)]
        run: Option<PathBuf>,
        /// Do not serve: ask the running service for this seat who it is, its clock and its queue.
        #[arg(long)]
        status: bool,
    },
    /// The tip anchor for a DOT authority journal (dot-terminal CommandAnchor). One signed
    /// dot-ledger-tip/1 line per spend in #dot-ledger, written as this seat; on read, a tip is trusted
    /// only after its stored envelope verifies under this seat's own key. Exit 0 only with a verified
    /// answer or a durable write; anything else is non-zero, and a journal then releases no new spend.
    #[command(hide = true)]
    Anchor {
        #[command(subcommand)]
        op: AnchorOp,
    },
    /// Sign one DOT authority Request v1 (dot-terminal crates/authority) as this seat, for a DOT broker to verify
    /// against an owner's grant (the Iris grant contract). Only that message type, decoded and re-encoded, never
    /// arbitrary bytes. Prints {"subject":"<public key hex>","signature":"<hex>"}.
    #[command(hide = true)]
    Authority {
        #[command(subcommand)]
        op: AuthorityOp,
    },
    /// Deliver queued writes.
    Flush,
    /// The DOT signers: `ox dot anchor …` (was `ox anchor`) and `ox dot authority …` (was `ox authority`).
    Dot {
        #[command(subcommand)]
        op: DotOp,
    },
}

#[derive(clap::Subcommand)]
enum DotOp {
    /// The tip anchor for a DOT authority journal (dot-terminal CommandAnchor).
    Anchor {
        #[command(subcommand)]
        op: AnchorOp,
    },
    /// Sign one DOT authority Request v1 as this seat.
    Authority {
        #[command(subcommand)]
        op: AuthorityOp,
    },
}

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    url: String,
    /// Other addresses of the same Oracle, tried in order when the one in use does not answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    fallback_urls: Vec<String>,
    handle: String,
    token: String,
    key_path: String,
}

/// What `get --dot` found about a v5 dot's predecessor claim (`Ox::chain_check`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChainCheck {
    /// Read and equal: the record's own `prev`, verified against the record before it (or genesis).
    Ok(String),
    /// The chain could not be read here, or the record is not in it: nothing verified, nothing refused.
    Unknown(String),
    /// Read and different: refused.
    Mismatch(String),
    /// The dot claims no Oracle predecessor (v3, v4, prevkind 0 or 1).
    NotClaimed(String),
}

impl ChainCheck {
    fn word(&self) -> &'static str {
        match self {
            ChainCheck::Ok(_) => "ok",
            ChainCheck::Unknown(_) => "unknown",
            ChainCheck::Mismatch(_) => "MISMATCH",
            ChainCheck::NotClaimed(_) => "not claimed",
        }
    }
    fn why(&self) -> &str {
        match self {
            ChainCheck::Ok(w) | ChainCheck::Unknown(w) | ChainCheck::Mismatch(w) | ChainCheck::NotClaimed(w) => w,
        }
    }
}

#[derive(Clone)]
struct Ox {
    home: PathBuf,
    cfg: Config,
    json: bool,
    /// Index into [`Ox::urls`] of the address that last answered; tried first next time.
    live: std::cell::Cell<usize>,
    /// The Oracle service's run directory, when one serves this seat: reads, writes, read marks and flushes go
    /// through it, and this process never touches the network for them. None = speak to the Oracle directly.
    via: Option<PathBuf>,
    /// Set when a service serves this seat in another protocol: every call is refused rather than bypassing it.
    incompatible: Option<String>,
    /// `--op-id`: the operation id for this invocation's write, so a retry is the same write.
    op_id: Option<String>,
}

/// Does the registry hold this key for my seat? A seat may have several; older servers
/// report only the first.
fn registered(whoami: &Value, local: &str) -> bool {
    whoami["pubkeys"]
        .as_array()
        .is_some_and(|a| a.iter().any(|k| k.as_str() == Some(local)))
        || whoami["pubkey"].as_str() == Some(local)
}

/// Resolve with IPv4 addresses first. Public hosts behind Cloudflare publish IPv6 too, and on a network whose IPv6 has
/// no route (29 Oct 2026, the Mac: `No route to host`, and reads that timed out) the client did not reach the IPv4
/// address curl falls back to. IPv6 addresses stay in the list, after.
fn ipv4_first(netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    let mut addrs: Vec<std::net::SocketAddr> = netloc.to_socket_addrs()?.collect();
    addrs.sort_by_key(|a| a.is_ipv6());
    Ok(addrs)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn home_dir(given: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(h) = given {
        return Ok(h);
    }
    Ok(user_home()?.join(".oracle"))
}

/// The user's home directory: `HOME` on Unix (the password database when it is unset), the profile folder on
/// Windows, where `HOME` is usually not set (`%USERPROFILE%`).
fn user_home() -> Result<PathBuf> {
    std::env::home_dir().filter(|h| !h.as_os_str().is_empty()).ok_or_else(|| anyhow!("cannot find your home directory: set HOME (USERPROFILE on Windows), or pass --home"))
}

fn write_private(path: &Path, body: &str) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(body.as_bytes())?;
    Ok(())
}

/// Write a file so that a crash leaves either the old content or the new, never a torn mix: a private temporary
/// file in the same directory, synced, renamed over the target, then the directory synced so the rename itself
/// survives power loss. For the outbox and the service's op journal, where a half-written record could lose the
/// one client_op that keeps a retried write a single event.
fn write_durable(path: &Path, body: &str) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| anyhow!("{} has no directory", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("f"), oracle_proto::random_hex(6)));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    // The directory is synced on Unix so the rename survives power loss. Windows cannot open a directory as a file
    // ("Access is denied", os error 5: every signed write failed there, found by the release smoke on 1 Nov 2026);
    // its rename (MoveFileEx, replacing) is the durable step.
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

enum Sent {
    Ok(Value),
    /// The server answered and said no. Retrying will not help.
    Refused(u16, String),
    /// No answer. The write may be retried.
    Unreachable(String),
}


impl Ox {
    /// This seat as a client: through the machine's Oracle service when one is running for it, else directly.
    fn load(home: PathBuf, json: bool) -> Result<Ox> {
        let mut ox = Ox::direct(home, json)?;
        match service::serving(&ox.home) {
            service::Serving::No | service::Serving::Unavailable => {}
            service::Serving::Yes(run) => ox.via = Some(run),
            service::Serving::Incompatible(why) => ox.incompatible = Some(why),
        }
        Ok(ox)
    }

    /// This seat speaking to the Oracle itself: the service's own view of a seat, and the fallback.
    fn direct(home: PathBuf, json: bool) -> Result<Ox> {
        let path = home.join("config.json");
        let raw = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "not set up yet — run `ox join --name <you>` (or `ox setup` for a registry seat) ({} missing)",
                path.display()
            )
        })?;
        Ok(Ox {
            home,
            cfg: serde_json::from_str(&raw).context("config.json is not valid")?,
            json,
            live: std::cell::Cell::new(0),
            via: None,
            incompatible: None,
            op_id: None,
        })
    }

    /// Ask the service, if one serves this seat. None when none does: the caller speaks to the Oracle directly, so
    /// Oracle works without the service. Otherwise the answer, or why there was none (`service::Unanswered`).
    fn ask_service(&self, req: Value) -> Option<std::result::Result<Value, service::Unanswered>> {
        if let Some(why) = &self.incompatible {
            return Some(Err(service::Unanswered::Incompatible(why.clone())));
        }
        Some(service::request(self.via.as_ref()?, &self.home, req))
    }

    /// For a request with no side effect (a read, a forward-only read mark): the service's answer, or None to do it
    /// directly. Never None for an incompatible service.
    fn ask_service_idempotent(&self, req: Value) -> Option<Result<Value>> {
        match self.ask_service(req)? {
            Ok(v) => Some(Ok(v)),
            Err(service::Unanswered::Incompatible(why)) => Some(Err(anyhow!(why))),
            Err(_) => None,
        }
    }

    /// The service hop for an advisory read: one request, `wait` for its reply, and no answer is an error here
    /// (never a fall-through to the network, which would be the second copy the budget forbids).
    fn ask_service_within(&self, req: Value, wait: Duration) -> Option<Result<Value>> {
        if let Some(why) = &self.incompatible {
            return Some(Err(anyhow!(why.clone())));
        }
        let via = self.via.as_ref()?;
        let req = json!({"op": "advisory_get", "path": req["path"], "budget_ms": wait.as_millis() as u64});
        Some(match service::request_within(via, &self.home, req, wait) {
            Ok(v) => Ok(v),
            Err(service::Unanswered::Incompatible(why)) => Err(anyhow!(why)),
            Err(_) => Err(anyhow!("the Oracle service gave no answer within the footer budget")),
        })
    }

    /// An advisory read (the footer: the inbox summary and the open asks after a write): one logical attempt
    /// within `budget`, on the live address only, never retried and never fanned out to a fallback address. A
    /// slow answer is the server still working on it, and a second copy queues behind the first on the seat's
    /// lane (21 Oct: a 40 s fold asked four times, 131 s per `ox say`). Explicit reads (`get`) keep their retry
    /// and their fallback; writes are untouched.
    fn advisory_get(&self, path: &str, budget: Duration) -> Result<Value> {
        if budget.is_zero() {
            bail!("no time left in the footer budget");
        }
        if let Some(v) = self.ask_service_within(json!({"op": "get", "path": path}), budget) {
            return service::value(v?, "value");
        }
        let agent = ureq::AgentBuilder::new().timeout_connect(budget.min(Duration::from_secs(5))).timeout(budget).build();
        match Self::call_with(&agent, &self.cfg.token, self.json, self.base(), "GET", path, None) {
            Sent::Ok(v) => Ok(v),
            Sent::Refused(c, d) => bail!("server refused ({c}): {d}"),
            Sent::Unreachable(e) => bail!("no answer within the footer budget ({e})"),
        }
    }

    fn key(&self) -> Result<SigningKey> {
        Ok(keyfile::load(&PathBuf::from(&self.cfg.key_path))?)
    }

    /// One HTTP client for the process: its connections are kept and reused. A new client per request paid a TLS
    /// handshake every time (about half a second from the Mac).
    fn agent() -> ureq::Agent {
        static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
        AGENT
            .get_or_init(|| {
                ureq::AgentBuilder::new()
                    .resolver(ipv4_first)
                    .timeout_connect(Duration::from_secs(5))
                    .timeout(Duration::from_secs(30))
                    .build()
            })
            .clone()
    }

    /// Every address of this Oracle: the configured one first, then the fallbacks.
    fn urls(&self) -> Vec<&str> {
        std::iter::once(self.cfg.url.as_str())
            .chain(self.cfg.fallback_urls.iter().map(String::as_str))
            .collect()
    }

    /// The address in use: the one that last answered.
    fn base(&self) -> &str {
        let urls = self.urls();
        urls[self.live.get().min(urls.len() - 1)]
    }

    /// One request, to whichever address answers. Only NO ANSWER moves on to the next address:
    /// a refusal is the Oracle's verdict and would be the same everywhere, so it is returned as is
    /// and a refused write is never replayed against another door.
    ///
    /// A write can fail AFTER it reached the server (the connection drops before the reply). Sending
    /// it on to the next door is still safe: a write is a signed envelope with its own id, and the
    /// store answers a repeat with `duplicate: true` instead of storing it twice — the same property
    /// the outbox already relies on.
    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Sent {
        let urls = self.urls();
        let start = self.live.get().min(urls.len() - 1);
        let mut misses: Vec<String> = Vec::new();
        for step in 0..urls.len() {
            let i = (start + step) % urls.len();
            match self.call_at(urls[i], method, path, body) {
                Sent::Unreachable(e) => misses.push(format!("{}: {e}", urls[i])),
                answered => {
                    self.live.set(i);
                    return answered;
                }
            }
        }
        Sent::Unreachable(misses.join("; "))
    }

    fn call_at(&self, base: &str, method: &str, path: &str, body: Option<&Value>) -> Sent {
        Self::call_with(&Self::agent(), &self.cfg.token, self.json, base, method, path, body)
    }

    /// One HTTP call with the given agent (its deadline is the agent's): the shared transport for `call_at` and
    /// for an advisory read on its own budget.
    fn call_with(agent: &ureq::Agent, token: &str, json: bool, base: &str, method: &str, path: &str, body: Option<&Value>) -> Sent {
        let url = format!("{}{}", base.trim_end_matches('/'), path);
        let req = agent
            .request(method, &url)
            .set("Authorization", &format!("Bearer {token}"));
        // Presence from reads (oracle-core): a program's read, and the fold's count, move no cursor.
        let req = if board::peek(json) || path.starts_with("/fold") { req.set("X-Ox-Peek", "1") } else { req };
        let res = match body {
            Some(b) => req.send_json(b),
            None => req.call(),
        };
        match res {
            Ok(r) => Sent::Ok(r.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(code, r)) => Sent::Refused(code, refusal_detail(&r.into_string().unwrap_or_default())),
            Err(e) => Sent::Unreachable(e.to_string()),
        }
    }

    /// Raw bytes to or from this Oracle (files), at whichever address answers; a refusal is returned as is.
    fn bytes(&self, method: &str, path: &str, ctype: Option<&str>, body: Option<&[u8]>) -> Result<(Vec<u8>, String)> {
        let mut misses: Vec<String> = Vec::new();
        for base in self.urls() {
            let url = format!("{}{}", base.trim_end_matches('/'), path);
            let req = Self::agent().request(method, &url).set("Authorization", &format!("Bearer {}", self.cfg.token));
            let req = match ctype {
                Some(t) => req.set("Content-Type", t),
                None => req,
            };
            let res = match body {
                Some(b) => req.send_bytes(b),
                None => req.call(),
            };
            match res {
                Ok(r) => {
                    let t = r.content_type().to_string();
                    let mut buf = Vec::new();
                    std::io::Read::read_to_end(&mut std::io::Read::take(r.into_reader(), 64 * 1024 * 1024), &mut buf)?;
                    return Ok((buf, t));
                }
                Err(ureq::Error::Status(code, r)) => bail!("server refused ({code}): {}", refusal_detail(&r.into_string().unwrap_or_default())),
                Err(e) => misses.push(format!("{base}: {e}")),
            }
        }
        bail!("Oracle unreachable ({})", misses.join("; "))
    }

    /// `GET /stream` (server-sent events, docs/STREAM.md) until stopped: each `observation` event as one line, or as its
    /// JSON with `--json`. `rooms` is sent as `room=`, so the server filters. Every event's id is kept and sent back as
    /// `Last-Event-ID` when the stream ends (a server restart, a dropped connection, `lagged`), so a reconnect replays
    /// what was missed and nothing twice; after `reset` (a gap over the replay bound) it continues from now and says so.
    /// No read timeout: a quiet Oracle is not a dead one (keep-alive every 20 s). Read only: it moves no read mark.
    fn watch(&self, rooms: &[String]) -> Result<()> {
        use std::io::BufRead;
        if let Some(why) = &self.incompatible {
            bail!("{why}");
        }
        let query = if rooms.is_empty() { String::new() } else { format!("?room={}", rooms.join(",")) };
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).build();
        let (mut last, mut announced, mut wait) = (None::<i64>, false, 1u64);
        loop {
            let mut misses: Vec<String> = Vec::new();
            for base in self.urls() {
                let mut req = agent
                    .get(&format!("{}/stream{query}", base.trim_end_matches('/')))
                    .set("Authorization", &format!("Bearer {}", self.cfg.token))
                    .set("Accept", "text/event-stream");
                if let Some(id) = last {
                    req = req.set("Last-Event-ID", &id.to_string());
                }
                let r = match req.call() {
                    Ok(r) => r,
                    Err(ureq::Error::Status(code, r)) => bail!("server refused ({code}): {}", refusal_detail(&r.into_string().unwrap_or_default())),
                    Err(e) => {
                        misses.push(format!("{base}: {e}"));
                        continue;
                    }
                };
                wait = 1;
                let (mut event, mut id, mut data) = (String::new(), None::<i64>, String::new());
                for line in std::io::BufReader::new(r.into_reader()).lines() {
                    let Ok(line) = line else { break };
                    if line.is_empty() {
                        match event.as_str() {
                            "connected" if !self.json => {
                                if !announced {
                                    eprintln!("watching {base}{} — Ctrl-C stops", if rooms.is_empty() { String::new() } else { format!(" · #{}", rooms.join(", #")) });
                                } else if let Some(l) = last {
                                    eprintln!("reconnected to {base}; replaying from seq {l}");
                                }
                                announced = true;
                            }
                            "observation" => {
                                if let Some(text) = views::watch_line(&data, rooms) {
                                    println!("{}", if self.json { data.clone() } else { text });
                                }
                                last = id.or(last);
                            }
                            "lagged" => eprintln!("fell behind the live feed; resuming from seq {}", last.map_or("?".into(), |l| l.to_string())),
                            "reset" => {
                                eprintln!("more than the server replays was missed; continuing from now (read the rooms for the gap)");
                                last = None;
                            }
                            _ => {}
                        }
                        event.clear();
                        data.clear();
                        id = None;
                    } else if let Some(x) = line.strip_prefix("event:") {
                        event = x.trim().to_string();
                    } else if let Some(x) = line.strip_prefix("id:") {
                        id = x.trim().parse().ok();
                    } else if let Some(x) = line.strip_prefix("data:") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(x.strip_prefix(' ').unwrap_or(x));
                    }
                }
                misses.clear();
                break;
            }
            if !misses.is_empty() {
                eprintln!("Oracle unreachable ({}); retrying in {wait} s", misses.join("; "));
            }
            std::thread::sleep(Duration::from_secs(wait));
            wait = (wait * 2).min(30);
        }
    }

    /// The full id a caller named: an `OBS-...` id as it is; a record's code (six to sixty-four hex digits) resolved by
    /// the Oracle, which refuses a code that names more than one record.
    fn full_id(&self, named: &str) -> Result<String> {
        if !((6..=64).contains(&named.len()) && named.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Ok(named.to_string());
        }
        let v = self.get(&format!("/search?id={}", urlencode(named)))?;
        v["id"].as_str().map(str::to_string).with_context(|| format!("no record has the code {named}"))
    }

    fn get(&self, path: &str) -> Result<Value> {
        if let Some(v) = self.ask_service_idempotent(json!({"op": "get", "path": path})) {
            return service::value(v?, "value");
        }
        // A read changes nothing, so one retry on no-answer is free and hides a cold connection.
        let first = match self.call("GET", path, None) {
            Sent::Unreachable(_) => self.call("GET", path, None),
            answered => answered,
        };
        match first {
            Sent::Ok(v) => Ok(v),
            Sent::Refused(c, d) => bail!("server refused ({c}): {d}"),
            Sent::Unreachable(e) => bail!("Oracle unreachable ({e})"),
        }
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        if let Some(v) = self.ask_service_idempotent(json!({"op": "post", "path": path, "body": body})) {
            return service::value(v?, "value");
        }
        match self.call("POST", path, Some(body)) {
            Sent::Ok(v) => Ok(v),
            Sent::Refused(c, d) => bail!("server refused ({c}): {d}"),
            Sent::Unreachable(e) => bail!("Oracle unreachable ({e})"),
        }
    }

    // ── writes ──────────────────────────────────────────────────────────────────

    /// One write door for every kind: the kind travels inside the signed envelope.
    fn route(_kind: Kind) -> &'static str {
        "/observations"
    }

    fn outbox(&self) -> PathBuf {
        self.home.join("outbox")
    }

    /// Sign and deliver. On no-answer, queue durably and exit non-zero: never a false ok.
    fn write(
        &self,
        kind: Kind,
        room: &str,
        text: &str,
        cargo: Option<String>,
        tags: Vec<String>,
    ) -> Result<Value> {
        match self.try_write(kind, room, text, cargo, tags, None)? {
            Written::Stored(v) => Ok(v),
            Written::Queued { file, reason } => {
                eprintln!("queued   Oracle unreachable ({reason})");
                eprintln!(
                    "         {}  — run `ox flush` when it is back",
                    file.display()
                );
                std::process::exit(3);
            }
        }
    }

    /// The same, for a caller that must outlive an unreachable Oracle (the MCP server).
    fn try_write(
        &self,
        kind: Kind,
        room: &str,
        text: &str,
        cargo: Option<String>,
        tags: Vec<String>,
        op_id: Option<String>,
    ) -> Result<Written> {
        // Through the service: it signs as this seat, journals the op, and queues or delivers. The op id is the
        // caller's (--op-id, or an MCP tool's op_id) when given, so a retry after an unknown outcome is the same write.
        let op = op_id.or_else(|| self.op_id.clone()).unwrap_or_else(|| oracle_proto::random_hex(16));
        let req = json!({"op": "write", "op_id": op, "kind": kind.as_str(), "room": room, "text": text, "cargo": cargo, "tags": tags});
        // Once a request may have reached the service, this write is never re-signed: it ends in the recorded outcome
        // or in "outcome unknown" with the op id to retry it by.
        let mut maybe_sent = false;
        for attempt in 0..3u64 {
            match self.ask_service(req.clone()) {
                // No service: this write was never submitted to one, so the Oracle directly is safe.
                None => break,
                Some(Err(service::Unanswered::NotSent(_))) if !maybe_sent => break,
                Some(Err(service::Unanswered::Incompatible(why))) => bail!(why),
                // Sent with no usable reply (or the service went away after that): ask again as the SAME op, which
                // answers with the recorded outcome.
                Some(Err(_)) => {
                    maybe_sent = true;
                    std::thread::sleep(Duration::from_millis(300 * (attempt + 1)));
                }
                Some(Ok(v)) => {
                    if v["ok"] != true {
                        bail!("{}", v["error"].as_str().unwrap_or("the service refused the write"));
                    }
                    return Ok(match v.get("stored") {
                        Some(s) => Written::Stored(s.clone()),
                        None => Written::Queued { file: PathBuf::from(v["queued"].as_str().unwrap_or("")), reason: v["reason"].as_str().unwrap_or("").to_string() },
                    });
                }
            }
        }
        if maybe_sent && self.ask_service(json!({"op": "hello"})).is_some_and(|r| r.is_ok()) {
            // The service is still there and still not answering this write: it is not bypassed.
            bail!("outcome unknown: the Oracle service may have stored this write (op {op}); nothing was sent again as a new write. Retry it as the same write: ox --op-id {op} <the same command>, or the MCP tool with op_id \"{op}\"");
        }
        // No service (or it has gone away): the same journal the service keeps. A known op resolves to its recorded
        // outcome or its recorded envelope (same client_op, one event); only a new op signs a new envelope.
        let out = self.journaled_write(&op, kind, room, text, cargo, tags, false)?;
        Ok(match out.get("stored") {
            Some(s) => Written::Stored(s.clone()),
            None => Written::Queued { file: PathBuf::from(out["queued"].as_str().unwrap_or("")), reason: out["reason"].as_str().unwrap_or("").to_string() },
        })
    }

    /// A write, exactly once per caller `op_id`, shared by the service and by `ox` speaking directly. Under an
    /// exclusive lock on this seat's op journal (across processes), the op record (the payload's digest and the
    /// signed envelope) is durable before anything is sent. A known op returns its recorded outcome (`replayed`),
    /// replays its refusal as a refusal, or re-delivers its recorded envelope (same client_op: the Oracle stores it
    /// once). The same op with another payload is a conflict. `known_down`: queue without touching the network.
    #[allow(clippy::too_many_arguments)]
    fn journaled_write(&self, op_id: &str, kind: Kind, room: &str, text: &str, cargo: Option<String>, tags: Vec<String>, known_down: bool) -> Result<Value> {
        // Both IPC and direct callers reach this path; validate before deriving any filename.
        if !service::op_id_ok(op_id) {
            bail!("write needs an op_id: 8-64 of [A-Za-z0-9_-], the same on every retry");
        }
        let digest = oracle_proto::sha_hex(json!([kind.as_str(), room, text, cargo, tags]).to_string().as_bytes());
        let ops = self.home.join("ops");
        std::fs::create_dir_all(&ops)?;
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(ops.join(".lock"))?;
        lock.lock()?;
        let record_path = ops.join(format!("{op_id}.json"));
        // Missing is "never recorded"; anything else unreadable fails closed: guessing "never recorded" for a torn
        // or corrupt record could mint a second write after the first was accepted.
        let record: Option<Value> = match std::fs::read_to_string(&record_path) {
            Ok(r) => Some(serde_json::from_str(&r).map_err(|e| anyhow!("op record {} is unreadable ({e}); refusing rather than risking a second write", record_path.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => bail!("op record {} cannot be read ({e}); refusing rather than risking a second write", record_path.display()),
        };
        let save = |env: &Envelope, outcome: Value| write_durable(&record_path, &json!({"op_id": op_id, "digest": digest, "envelope": env, "outcome": outcome}).to_string());
        let env: Envelope = match &record {
            Some(rec) if rec["digest"] != json!(digest) => bail!("conflict: op_id {op_id} was already used for a different write"),
            // A refusal replays as the same refusal: never as success, never as queued. Except a busy server's 503, which
            // said "nothing was stored, retry": that op is still unsettled, so it re-delivers the same envelope (journals
            // written before 28 Oct 2026 recorded it as a refusal, and every retry replayed it).
            Some(rec) if rec["outcome"]["ok"] == false && !retryable_refusal(rec["outcome"]["refused"].as_str().unwrap_or("")) => bail!("{}", rec["outcome"]["refused"].as_str().unwrap_or("NOT written — refused earlier")),
            Some(rec) if rec["outcome"]["ok"] == false => self.refresh(serde_json::from_value(rec["envelope"].clone())?, self.takes_authored())?,
            Some(rec) if !rec["outcome"].is_null() => {
                let mut out = rec["outcome"].clone();
                out["replayed"] = json!(true);
                return Ok(out);
            }
            // Recorded but never settled (queued, or the process died): the same envelope again, same client_op.
            Some(rec) => self.refresh(serde_json::from_value(rec["envelope"].clone())?, self.takes_authored())?,
            None => {
                let env = self.sign_env(kind, room, text, cargo, tags)?;
                save(&env, Value::Null)?;
                env
            }
        };
        if known_down {
            return Ok(json!({"queued": self.enqueue(&env)?.display().to_string(), "reason": "Oracle known unreachable"}));
        }
        match self.deliver(&env) {
            Ok(Written::Stored(v)) => {
                // Fault injection, debug builds only: die between the Oracle's acceptance and the journal update.
                if service::fault("crash-after-accept") {
                    std::process::abort();
                }
                let outcome = json!({"stored": v});
                save(&env, outcome.clone())?;
                Ok(outcome)
            }
            Ok(Written::Queued { file, reason }) => Ok(json!({"queued": file.display().to_string(), "reason": reason})),
            // Brain cases, 28 Oct 2026: m1905 and m1907 stayed unpublished for hours because one busy answer was saved
            // as this op's final outcome. A 503 stored nothing: keep the op unsettled so the same op retries the write.
            Err(e) if retryable_refusal(&format!("{e:#}")) => Err(e.context(format!("not stored; retry it as the same write: ox --op-id {op_id} <the same command>"))),
            Err(e) => {
                save(&env, json!({"ok": false, "refused": format!("{e:#}")}))?;
                Err(e)
            }
        }
    }


    /// This seat's journal record for `op_id` (the signed envelope and its outcome), read without taking the lock or sending anything. `None` when the op was never
    /// recorded; a record that cannot be read is an error (never guessed as "never recorded").
    fn recorded_op(&self, op_id: &str) -> Result<Option<Value>> {
        if !service::op_id_ok(op_id) {
            bail!("op_id must be 8-64 of [A-Za-z0-9_-]");
        }
        match std::fs::read_to_string(self.home.join("ops").join(format!("{op_id}.json"))) {
            Ok(r) => Ok(Some(serde_json::from_str(&r).map_err(|e| anyhow!("op record for {op_id} is unreadable ({e})"))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => bail!("op record for {op_id} cannot be read ({e})"),
        }
    }

    /// Sign a write as this seat, now. Nothing is sent or stored.
    fn sign_env(&self, kind: Kind, room: &str, text: &str, cargo: Option<String>, tags: Vec<String>) -> Result<Envelope> {
        let text = text.trim_end();
        if text.trim().is_empty() {
            bail!("nothing to say");
        }
        let env = Envelope::sign(&self.key()?, room, &self.cfg.handle, kind, text, cargo, tags, now_ms());
        env.validate().map_err(|e| anyhow!("{e}"))?;
        Ok(env)
    }

    /// Deliver a signed envelope. Write-ahead: it is on disk before it is sent, so a crash mid-send leaves it
    /// queued, and its client_op makes any later delivery one event even if this send did land. A refusal
    /// removes it (it would be refused again); no answer leaves it queued.
    fn deliver(&self, env: &Envelope) -> Result<Written> {
        // One outbox file per client_op: an older copy of the same write (an earlier signing) is replaced.
        if let Ok(rd) = std::fs::read_dir(self.outbox()) {
            for f in rd.filter_map(|e| e.ok().map(|e| e.path())) {
                if f.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&format!("-{}.json", env.client_op))) {
                    let _ = std::fs::remove_file(f);
                }
            }
        }
        let file = self.enqueue(env)?;
        match self.post_write(env)? {
            Sent::Ok(v) => {
                std::fs::remove_file(&file)?;
                Ok(Written::Stored(v))
            }
            Sent::Refused(c, d) => {
                std::fs::remove_file(&file)?;
                bail!("NOT written — server refused ({c}): {d}")
            }
            Sent::Unreachable(reason) => Ok(Written::Queued { file, reason }),
        }
    }

    /// docs/MICRODOT.md §6 and §7: the text as a v5 dot sealed by this seat. `prevkind` 2 with the seat's Oracle
    /// chain head (`GET /chain/{author}/head`: the `prev` the envelope will carry) when the Oracle keeps chains and
    /// the head names a record; `prevkind` 0 with a zero `prev` at genesis (a kind-2 dot never names a zero prev:
    /// the codec refuses it), when the Oracle keeps no chains, or when the head cannot be read (then the dot claims
    /// no predecessor, and `get --dot` says what the record's own prev is). A reply to a record that carries a
    /// `dot:` tag is a composite naming the parent
    /// (rel 1). The face is put in the room as a file; the tags the message then carries. The head is read once,
    /// here: a head that moves before the write lands (another device of this seat writing in between) leaves the
    /// dot naming a predecessor the record does not, which `get --dot` reports as `chain: mismatch`; a journaled
    /// write is never re-minted (one op, one envelope).
    /// A microdot made elsewhere, for `ox say --from-dot` (docs/MICRODOT.md §8): its text, and the tags that keep what it
    /// is: dot_id, dot_prev, dot_who, dot_when, dot_seal (ok or failed, reported, never read as this seat's signature), the
    /// picture as the record's file, and reply:<id> when a record already in Oracle carries the dot it names as prev.
    fn import_dot(&self, room: &str, path: &Path) -> Result<(String, Vec<String>)> {
        use oracle_proto::microdot as md;
        let png = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let (_, _, rows) = md::png_read(&png)?;
        let (d, _, sealed) = md::tiles_in(&rows).iter().find_map(md::read_tile_reporting)
            .context("no MICRODOT tile in this picture reads (a body must match its hash)")?;
        let text = d.markdown.clone().unwrap_or_else(|| String::from_utf8_lossy(&d.body).into_owned()).trim().to_string();
        anyhow::ensure!(!text.is_empty(), "the dot holds no text");
        let (id, prev) = (d.id_hex(), hex::encode(d.prev));
        let q = format!("/files?room={}&name={}.png", urlencode(room), &id[..16]);
        let (body, _) = self.bytes("PUT", &q, Some("image/png"), Some(&png))?;
        let v: Value = serde_json::from_slice(&body).context("the Oracle's answer to the picture was not JSON")?;
        let sha = v["sha256"].as_str().context("the Oracle's answer to the picture names no sha256")?.to_string();
        let mut tags = vec![format!("dot_id:{id}"), format!("dot_prev:{prev}"), format!("dot_who:{}", hex::encode(d.who)),
            format!("dot_when:{}", d.when_ms), format!("dot_seal:{}", if sealed { "ok" } else { "failed" }), format!("file:{sha}")];
        if d.prev != [0u8; 32] {
            for t in [format!("dot_id:{prev}"), format!("dot:{prev}")] {
                if let Ok(p) = self.get(&format!("/search?tag={}&limit=1", urlencode(&t))) {
                    if let Some(rid) = p["items"].as_array().and_then(|a| a.first()).and_then(|i| i["id"].as_str()) {
                        tags.push(format!("reply:{rid}"));
                        break;
                    }
                }
            }
        }
        Ok((text, tags))
    }

    fn mint_dot(&self, room: &str, text: &str, parent: Option<&str>) -> Result<Vec<String>> {
        use oracle_proto::microdot as md;
        let seed = self.key()?.to_bytes();
        let head: Option<[u8; 32]> = if self.takes_prev() {
            match self.call("GET", &format!("/chain/{}/head", urlencode(&self.cfg.handle)), None) {
                Sent::Ok(v) => v["head"].as_str().and_then(|h| hex::decode(h).ok()).and_then(|b| b.try_into().ok()),
                _ => None,
            }
        } else {
            None
        };
        let (prevkind, prev) = match head {
            Some(h) if h != [0u8; 32] => (2u8, h),
            _ => (0u8, [0u8; 32]),
        };
        let parent_dot = match parent {
            Some(id) => record_tag(&self.get(&format!("/search?id={}", urlencode(id)))?, "dot:")
                .and_then(|h| hex::decode(h).ok())
                .filter(|b| b.len() == 32),
            None => None,
        };
        let when = now_ms().max(0) as u64;
        // the body is the text as the record will hold it (the Oracle stores a message trimmed)
        let text = text.trim();
        let (body, composite) = match &parent_dot {
            Some(p) => {
                let mut id = [0u8; 32];
                id.copy_from_slice(p);
                (md::layout(&[(1, id)], text), true)
            }
            None => (text.as_bytes().to_vec(), false),
        };
        let canon = md::pack_v5(&seed, &body, when, prev, prevkind, [0u8; 32], composite, None, 6, false)?;
        // what was sealed reads back before it is published: a dot the codec refuses is never put in a room
        md::parse_any(&canon).map_err(|e| anyhow!("the minted dot does not read back ({e}); nothing published"))?;
        let dot_id = hex::encode(md::dot_id_v5(&canon));
        let png = md::face_png(&canon, Some(&format!("MICRODOT v5 {dot_id}")));
        let q = format!("/files?room={}&name={}.png", urlencode(room), &dot_id[..16]);
        let (body, _) = self.bytes("PUT", &q, Some("image/png"), Some(&png))?;
        let v: Value = serde_json::from_slice(&body).context("the Oracle's answer to the face was not JSON")?;
        let sha = v["sha256"].as_str().context("the Oracle's answer to the face names no sha256")?.to_string();
        Ok(vec![format!("dot:{dot_id}"), format!("file:{sha}")])
    }

    /// A v5 dot's predecessor claim against the stored record (docs/MICRODOT.md §7; Emmy).
    /// For `prevkind` 2: the record's own envelope `prev`, read from the author's
    /// chain (`GET /chain/{author}?after=seq-1&limit=1`), must equal the dot's `prev`; the chain's key for the record
    /// must be the dot's `who`; and that `prev` must be the link of the record before it in the chain (`link_of` its
    /// signature), or genesis (all zero) when the author has no earlier native record. `Unknown` when the chain is
    /// not readable here (another seat's chain, no network) or the record is not in it; `Mismatch` when read and
    /// different: a freshly sealed dot naming the wrong predecessor fails here, not at the seal. Never the head at
    /// `when`: a clock proves nothing about order.
    fn chain_check(&self, record: &Value, d: &oracle_proto::microdot::Dot) -> ChainCheck {
        use oracle_proto::{link_of, ChainRecord, GENESIS_PREV};
        if d.ver != 5 {
            return ChainCheck::NotClaimed(format!("v{}: no predecessor kind", d.ver));
        }
        match d.prevkind {
            0 | 2 => {}
            1 => return ChainCheck::NotClaimed("prevkind 1: a microdot chain, walked only by a reader that holds the dots".into()),
            k => return ChainCheck::NotClaimed(format!("prevkind {k}")),
        }
        let rec = record_object(record);
        let Some(seq) = rec.and_then(|r| r["seq"].as_i64()) else { return ChainCheck::Unknown("the record carries no seq".into()) };
        let author = rec.and_then(|r| r["author"].as_str()).unwrap_or("").to_string();
        let page = |after: i64, limit: i64| -> std::result::Result<Vec<ChainRecord>, String> {
            match self.call("GET", &format!("/chain/{}?after={after}&limit={limit}", urlencode(&author)), None) {
                Sent::Ok(v) => serde_json::from_value(v["records"].clone()).map_err(|e| format!("the chain's records are unreadable ({e})")),
                Sent::Refused(c, w) => Err(format!("the chain is not readable here ({c}: {w})")),
                Sent::Unreachable(w) => Err(format!("the chain is unreachable ({w})")),
            }
        };
        let own = match page(seq - 1, 1) {
            Ok(r) => r,
            Err(w) if d.prevkind == 0 => return ChainCheck::NotClaimed(format!("prevkind 0: no predecessor claimed; {w}")),
            Err(w) => return ChainCheck::Unknown(w),
        };
        let Some(own) = own.into_iter().find(|r| r.seq == seq) else {
            return ChainCheck::Unknown("the record is not in its author's chain (not a native write?)".into());
        };
        // A chain response is evidence to verify, including on the genesis path.
        let report = oracle_proto::check_chain(std::slice::from_ref(&own));
        if !report.breaks.is_empty() {
            return ChainCheck::Mismatch(format!("the stored record does not verify: {:?}", report.breaks));
        }
        if rec.and_then(|r| r["id"].as_str()) != Some(own.id.as_str())
            || own.envelope.author != author
            || rec.and_then(|r| r["content"].as_str()) != Some(own.envelope.content.as_str())
            || !own.pubkey.eq_ignore_ascii_case(&hex::encode(d.who))
            || !own.envelope.tags.iter().any(|t| t == &format!("dot:{}", d.id_hex()))
        {
            return ChainCheck::Mismatch("the chain record is not the requested record and sealed dot".into());
        }
        if d.prevkind == 0 {
            // Genesis, explicitly: the dot names no predecessor, and the record's own prev says whether there was one.
            return match own.envelope.prev.as_deref() {
                None => ChainCheck::NotClaimed("prevkind 0: no predecessor claimed; the record is unchained".into()),
                Some(GENESIS_PREV) => ChainCheck::Ok("genesis: the dot names no predecessor and the record is the author's first native record (prev all zero)".into()),
                Some(p) => ChainCheck::NotClaimed(format!("prevkind 0: no predecessor claimed; the record's own prev is {}… (the head was not read at minting)", &p[..12])),
            };
        }
        let dot_prev = hex::encode(d.prev);
        let Some(env_prev) = own.envelope.prev.clone() else {
            return ChainCheck::Mismatch("the record's envelope carries no prev; the dot names a predecessor".into());
        };
        if env_prev != dot_prev {
            return ChainCheck::Mismatch(format!("the dot's prev {}… is not the record's prev {}…", &dot_prev[..12], &env_prev[..12]));
        }
        if !own.pubkey.eq_ignore_ascii_case(&hex::encode(d.who)) {
            return ChainCheck::Mismatch("the key the chain stored for the record is not the dot's who".into());
        }
        // The record before it in the chain: pages walked forward from a window before `seq`, widened until one holds
        // an earlier record or reaches the start.
        let mut window = 256i64;
        let pred: Option<ChainRecord> = loop {
            let start = (seq - window).max(0);
            let mut after = start;
            let mut pred: Option<ChainRecord> = None;
            let mut reached = false;
            loop {
                let recs = match page(after, 2000) {
                    Ok(r) => r,
                    Err(w) => return ChainCheck::Unknown(w),
                };
                if recs.is_empty() {
                    break;
                }
                let n = recs.len();
                for r in recs {
                    if r.seq >= seq {
                        reached = true;
                        break;
                    }
                    after = r.seq;
                    pred = Some(r);
                }
                if reached || n < 2000 {
                    break;
                }
            }
            if pred.is_some() || start == 0 {
                break pred;
            }
            window *= 4;
        };
        match pred {
            None if env_prev == GENESIS_PREV => ChainCheck::Ok("genesis: the author's first native record, prev all zero".into()),
            None => ChainCheck::Mismatch("prev names a predecessor; the author has no earlier native record".into()),
            Some(p) if !oracle_proto::check_chain(&[p.clone(), own.clone()]).breaks.is_empty() => ChainCheck::Mismatch("the predecessor/record signatures, identities or adjacency do not verify".into()),
            Some(p) if link_of(&p.envelope.sig) == env_prev => ChainCheck::Ok(format!("prev = the link of {}, the record before it", p.id)),
            Some(p) => ChainCheck::Mismatch(format!("prev is not the link of {}, the record before it", p.id)),
        }
    }

    /// `ox get <id> --dot`: the record's face fetched and read; the seal, the body against the content, `who`
    /// against the author's key, a v5 dot's predecessor against the author's chain (`chain_check`); the fields
    /// printed; the PNG to `out` or the canonical hex to stdout. A mismatching predecessor is refused; an unknown
    /// one is said, not refused.
    fn get_dot(&self, id: &str, record: &Value, out: Option<&Path>) -> Result<()> {
        use oracle_proto::microdot as md;
        let dot_id = record_tag(record, "dot:").context("this record carries no dot: tag; it was not said as a dot")?;
        let sha = record_tag(record, "file:").context("this record names a dot but no file: tag for its face")?;
        let (png, _) = self.bytes("GET", &format!("/files/{}", urlencode(&sha)), None, None)?;
        let (_, _, rows) = md::png_read(&png)?;
        let found = md::tiles_in(&rows).iter().find_map(md::read_tile).context("the face holds no tile that verifies in any orientation")?;
        let (d, sym) = found;
        anyhow::ensure!(d.id_hex() == dot_id, "the face holds dot {} but the record names {dot_id}", d.id_hex());
        let rec = record_object(record);
        let content = rec.and_then(|r| r["content"].as_str()).unwrap_or("");
        let body_text = d.markdown.clone().unwrap_or_else(|| String::from_utf8_lossy(&d.body).into_owned());
        let body_ok = body_text.trim() == content.trim();
        let key = rec.and_then(|r| r["pubkey"].as_str().or(r["key"].as_str())).map(str::to_string);
        let who_ok = key.as_deref().map(|k| k.eq_ignore_ascii_case(&hex::encode(d.who)));
        let chain = self.chain_check(record, &d);
        if self.json {
            println!("{}", json!({"id": id, "dot_id": dot_id, "ver": d.ver, "flags": d.flags, "prevkind": d.prevkind, "enc": d.enc, "when_ms": d.when_ms, "who": hex::encode(d.who), "prev": hex::encode(d.prev), "next": hex::encode(d.next), "read_in": [sym.0, sym.1], "body_matches_content": body_ok, "who_matches_author": who_ok, "chain": chain.word(), "chain_why": chain.why(), "layout": d.layout.as_ref().map(|l| l.iter().map(|(r, i)| json!({"rel": md::REL_NAMES[*r as usize], "dot_id": hex::encode(i)})).collect::<Vec<_>>()), "file": sha}));
        } else {
            println!("dot      {dot_id}  (ver {}, read in {} turn(s){})", d.ver, sym.0, if sym.1 { ", mirrored" } else { "" });
            println!("who      {}  {}", hex::encode(d.who), match who_ok { Some(true) => "= the author's key", Some(false) => "NOT the author's key", None => "(record carries no key)" });
            println!("when     {}", d.when_ms);
            println!("prev     {}{}", hex::encode(d.prev), if d.ver == 5 { format!("  (prevkind {})", d.prevkind) } else { String::new() });
            println!("chain    {}: {}", chain.word(), chain.why());
            println!("body     {}", if body_ok { "= the record's content" } else { "DIFFERS from the record's content" });
            if let Some(l) = &d.layout {
                for (r, i) in l {
                    println!("names    {}  {}", md::REL_NAMES[*r as usize], hex::encode(i));
                }
            }
        }
        anyhow::ensure!(body_ok, "the dot's body is not the record's content");
        anyhow::ensure!(who_ok != Some(false), "the dot's who is not the author's key");
        anyhow::ensure!(!matches!(chain, ChainCheck::Mismatch(_)), "the dot's predecessor is not the record's: {}", chain.why());
        match out {
            Some(p) => {
                std::fs::write(p, &png).with_context(|| format!("cannot write {}", p.display()))?;
                if !self.json {
                    println!("face     {}", p.display());
                }
            }
            None if !self.json => println!("{}", hex::encode(&d.canon)),
            None => {}
        }
        Ok(())
    }

    /// POST a write, on its author's chain where the Oracle keeps one (docs/CHAIN.md): signed over the head the Oracle
    /// reports, and signed again over the new head when another device wrote first (the same client_op, so one write).
    /// An Oracle without chains, or a head that cannot be read, gets the write without `prev`, as before.
    fn post_write(&self, env: &Envelope) -> Result<Sent> {
        let mut env = env.clone();
        let chains = self.takes_prev();
        for _ in 0..4 {
            let head = if chains {
                match self.call("GET", &format!("/chain/{}/head", urlencode(&env.author)), None) {
                    Sent::Ok(v) => v["head"].as_str().map(str::to_string),
                    _ => None,
                }
            } else {
                None
            };
            env = match head {
                Some(h) => env.chain_to(&h, &self.key()?, now_ms()),
                None if env.prev.is_some() => Envelope { prev: None, ..env }.resign(&self.key()?, now_ms()),
                None => env,
            };
            match self.call("POST", Self::route(env.kind), Some(&serde_json::to_value(&env)?)) {
                Sent::Refused(409, d) if chains && d.contains("chain head") => continue,
                other => return Ok(other),
            }
        }
        Ok(Sent::Refused(409, "the chain head kept moving under four tries; NOT written".into()))
    }

    /// Does the Oracle keep author chains (`prev`)? Remembered once it says yes, for the life of the process.
    fn takes_prev(&self) -> bool {
        static YES: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if YES.get().is_some() {
            return true;
        }
        let yes = matches!(self.call("GET", "/whoami", None), Sent::Ok(v) if v["features"].as_array().is_some_and(|f| f.iter().any(|x| x == "prev")));
        if yes {
            let _ = YES.set(());
        }
        yes
    }

    /// Does the Oracle accept a signed `authored_ms`? (Older servers would refuse the extra field.)
    fn takes_authored(&self) -> bool {
        match self.call("GET", "/whoami", None) {
            Sent::Ok(v) => v["features"].as_array().is_some_and(|f| f.iter().any(|x| x == "authored_ms")),
            _ => false,
        }
    }

    /// An envelope signed earlier, re-signed for delivery now: the same client_op (so it is one event), a fresh
    /// clock and nonce, and, where the Oracle takes it, the first signing time kept as `authored_ms`.
    fn refresh(&self, env: Envelope, authored: bool) -> Result<Envelope> {
        let sk = self.key()?;
        Ok(if authored { env.resign_authored(&sk, now_ms()) } else { env.resign(&sk, now_ms()) })
    }

    /// The outbox file is written before anyone is told "queued", and named by client_op, so a retry is one event.
    fn enqueue(&self, env: &Envelope) -> Result<PathBuf> {
        let file = self.outbox().join(format!("{}-{}.json", env.ts_ms, env.client_op));
        write_durable(&file, &serde_json::to_string_pretty(env)?)?;
        Ok(file)
    }

    fn flush(&self) -> Result<(usize, usize)> {
        let asked = match self.ask_service(json!({"op": "flush"})) {
            None | Some(Err(service::Unanswered::NotSent(_))) => None,
            // A flush may have delivered before its reply was lost: not redone directly from here.
            Some(Err(e)) => bail!("{e}"),
            Some(Ok(v)) => Some(v),
        };
        if let Some(v) = asked {
            let v = service::value(v, "")?;
            return Ok((v["sent"].as_u64().unwrap_or(0) as usize, v["left"].as_u64().unwrap_or(0) as usize));
        }
        let dir = self.outbox();
        let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect(),
            Err(_) => return Ok((0, 0)),
        };
        files.sort();
        if files.is_empty() {
            return Ok((0, 0));
        }
        // A server that announces `authored_ms` keeps each queued write's own time; an older one would refuse
        // the extra signed field, so it gets the envelope re-signed exactly as before.
        let authored = self.takes_authored();
        let (mut sent, mut left) = (0, 0);
        for f in files {
            let queued: Envelope = serde_json::from_str(&std::fs::read_to_string(&f)?)?;
            // A queued envelope is stale by now: fresh clock and nonce, the SAME client_op,
            // so if the first attempt did land this is recognised and nothing doubles.
            let env = self.refresh(queued, authored)?;
            match self.post_write(&env)? {
                Sent::Ok(_) => {
                    std::fs::remove_file(&f)?;
                    sent += 1;
                }
                Sent::Refused(c, d) => {
                    let dead = f.with_extension("rejected");
                    std::fs::rename(&f, &dead)?;
                    eprintln!("rejected ({c}) {d} → {}", dead.display());
                }
                Sent::Unreachable(_) => left += 1,
            }
        }
        Ok((sent, left))
    }
}




fn show(items: &[Value], full: bool) {
    if full {
        print!(
            "{}",
            items.iter().map(oracle_mcp::full).collect::<Vec<_>>().join("\n")
        );
    } else {
        print_items(items);
    }
}

fn print_items(items: &[Value]) {
    print!("{}", items_text(items, 96));
}


/// Enough escaping for the few values that reach a query string here.
fn print_written(ox: &Ox, v: &Value, what: &str) {
    if ox.json {
        println!("{v}");
    } else {
        println!("signed:ok  {}  {what}", v["id"].as_str().unwrap_or("?"));
    }
}

/// The three lists as text: one line per ask, its id last so it can be replied to.
fn asks_text(v: &Value, state: &str) -> String {
    let age = |ms: i64| {
        let now = v["now_ms"].as_i64().unwrap_or(ms);
        let m = (now - ms).max(0) / 60_000;
        if m < 60 { format!("{m}m") } else if m < 48 * 60 { format!("{}h", m / 60) } else { format!("{}d", m / 1440) }
    };
    let line = |r: &Value, who: &str| {
        let mut extra = Vec::new();
        if r["overdue"] == true { extra.push("OVERDUE".to_string()); }
        if let Some(c) = r["choice"].as_u64() {
            let label = r["options"].get(c as usize - 1).and_then(|o| o.as_str()).unwrap_or("?");
            extra.push(format!("chose {c}: {label}"));
        } else if let Some(o) = r["options"].as_array().filter(|o| !o.is_empty()) {
            extra.push(format!("options: {}", o.iter().filter_map(|x| x.as_str()).enumerate().map(|(i, x)| format!("{}) {x}", i + 1)).collect::<Vec<_>>().join(" · ")));
        }
        format!("  {:<7} #{:<8} {:<10} {}  {}  {}{}",
            r["ask"].as_str().unwrap_or("?"), r["room"].as_str().unwrap_or("?"), who,
            age(r["created_ms"].as_i64().unwrap_or(0)), r["card"].as_str().unwrap_or(""),
            r["id"].as_str().unwrap_or("?"), if extra.is_empty() { String::new() } else { format!("  [{}]", extra.join("; ")) })
    };
    let mut out = String::new();
    let mut section = |title: &str, key: &str, who: &dyn Fn(&Value) -> String, empty: &str| {
        let rows = v[key].as_array().cloned().unwrap_or_default();
        out.push_str(&format!("{title} · {}\n", rows.len()));
        if rows.is_empty() { out.push_str(&format!("  {empty}\n")); }
        for r in &rows { out.push_str(&line(r, &who(r))); out.push('\n'); }
    };
    if state != "done" {
        section("WAITING ON ME", "waiting_on_me", &|r| format!("from {}", r["author"].as_str().unwrap_or("?")), "nothing is waiting on you");
        section("I'M WAITING ON", "im_waiting_on", &|r| format!("held by {}", r["held_by"].as_array().map(|h| h.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(",")).filter(|h| !h.is_empty()).unwrap_or_else(|| "nobody named".into())), "you are not waiting on anyone");
    }
    if state != "open" {
        section("DONE TODAY", "done_today", &|r| r["author"].as_str().unwrap_or("?").to_string(), "nothing settled today");
    }
    if v["capped"] == true { out.push_str("(a read hit its cap: older asks may be missing)\n"); }
    out.trim_end().to_string()
}

/// What a refusal says, whatever answered: Oracle's JSON `detail`, else the body itself. A proxy in front of Oracle
/// answers in its own words (Cloudflare 1010 is its bot check refusing the client, not Oracle refusing the seat;
/// 2026-09-30 a bare "403" was read as a permission refusal).
fn refusal_detail(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(d) = v["detail"].as_str().filter(|d| !d.is_empty()) {
            return d.to_string();
        }
        if let Some(e) = v["error"].as_str().filter(|e| !e.is_empty()) {
            return e.to_string();
        }
    }
    let text = oracle_mcp::one_line(body.trim(), 160);
    if let Some(code) = text.strip_prefix("error code: ") {
        return format!("Cloudflare {code} — the proxy refused this client, not Oracle (text: {text})");
    }
    if text.is_empty() { "no body".into() } else { format!("not from Oracle: {text}") }
}

/// The subject a state record is about: its `subject:` tag, else its cargo's `subject`.
fn subject_of(h: &Value) -> Option<String> {
    let tagged = h["tags"].as_array().and_then(|t| {
        t.iter().filter_map(Value::as_str).find_map(|t| t.strip_prefix("subject:").map(str::to_string))
    });
    tagged.or_else(|| {
        let c = h["cargo"].as_str().and_then(|c| serde_json::from_str::<Value>(c).ok())?;
        c["subject"].as_str().map(str::to_string)
    })
}

/// Every state record of one subject, by any seat, oldest first, as {at, id, author, owner, state, note}.
fn head_revisions(v: &Value) -> Vec<Value> {
    let rows = v["hits"].as_array().or_else(|| v["items"].as_array()).cloned().unwrap_or_default();
    let mut out: Vec<Value> = rows
        .iter()
        .filter(|h| h["kind"].as_str().is_none_or(|k| k == "state"))
        .map(|h| {
            let c = h["cargo"].as_str().and_then(|c| serde_json::from_str::<Value>(c).ok()).unwrap_or(Value::Null);
            json!({
                "at": h["created_ms"], "id": h["id"], "author": h["author"],
                "owner": c["owner"].as_str().or(h["author"].as_str()),
                "state": c["state"].as_str().unwrap_or("?"),
                "note": h["content"].as_str().unwrap_or(""),
            })
        })
        .collect();
    out.sort_by_key(|r| r["at"].as_i64().unwrap_or(0));
    out
}

fn rev_line(r: &Value) -> String {
    format!(
        "{}  {:<7} {:<12} {}  {}",
        oracle_proto::iso_ms(r["at"].as_i64().unwrap_or(0)).get(..16).unwrap_or(""),
        r["state"].as_str().unwrap_or("?"),
        r["author"].as_str().unwrap_or("?"),
        r["id"].as_str().unwrap_or(""),
        oracle_mcp::one_line(r["note"].as_str().unwrap_or(""), 100)
    )
}

/// One line: what the subject is now and who made it so.
/// A current head on one line, for recall: subject, state, who moved it last and when, its note, how to read it whole.
/// Who made the head is said only when someone else moved it last (a seat closing another's head is the case to see).
fn state_line(subject: &str, revs: &[Value]) -> String {
    let now = revs.last().cloned().unwrap_or(Value::Null);
    let first = revs.first().cloned().unwrap_or(Value::Null);
    let by = if first["author"] != now["author"] { format!(", made by {}, {} revisions", first["author"].as_str().unwrap_or("?"), revs.len()) } else { String::new() };
    format!(
        "STATE {subject} [{}] {} {} — {} (ox head {subject}{by})",
        now["state"].as_str().unwrap_or("?"),
        now["author"].as_str().unwrap_or("?"),
        oracle_proto::iso_ms(now["at"].as_i64().unwrap_or(0)).get(..10).unwrap_or(""),
        oracle_mcp::one_line(now["note"].as_str().unwrap_or(""), 140),
    )
}

fn head_text(subject: &str, revs: &[Value]) -> String {
    let now = &revs[revs.len() - 1];
    let mut out = format!(
        "{subject}\nnow      {} · by {} · {}\n         {}\nhistory  (oldest first, every seat)\n",
        now["state"].as_str().unwrap_or("?"),
        now["author"].as_str().unwrap_or("?"),
        oracle_proto::iso_ms(now["at"].as_i64().unwrap_or(0)),
        now["note"].as_str().unwrap_or("")
    );
    for r in revs {
        out.push_str("  ");
        out.push_str(&rev_line(r));
        out.push('\n');
    }
    out
}

/// A reply that claims or votes on one item of a message: reply:<id> and owner:K=<seat> or vote:K=<stance>, the tags
/// the Oracle room index projects from a message's replies (crates/oracle-server/src/roomindex.rs).
fn item_reply_tags(id: &str, kind: &str, item: &str, value: &str) -> Result<Vec<String>> {
    // Keep this in sync with board::item_tag: claims and votes must work on every
    // item key that `ox say --item` can declare, not just the common Q1/R12 form.
    let key_ok = !item.is_empty()
        && item.len() <= 24
        && item
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    anyhow::ensure!(key_ok, "invalid item key {item}; use letters, digits, dashes or underscores (at most 24 characters)");
    anyhow::ensure!(!value.is_empty() && value.len() <= 40 && value.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && value.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'), "{value} is not a seat handle");
    anyhow::ensure!(id.starts_with("OBS-"), "{id} is not a message id");
    Ok(vec![format!("reply:{id}"), format!("{kind}:{item}={value}")])
}

/// The words of item `k`, from the annotation that declared it: an items/1 cargo, else a "K kind: text" line.
fn item_text(ox: &Ox, from: &str, k: &str, cache: &mut std::collections::HashMap<String, Value>) -> String {
    let o = cache
        .entry(from.to_owned())
        .or_insert_with(|| ox.get(&format!("/search?id={}", urlencode(from))).unwrap_or(Value::Null));
    if let Some(c) = o["cargo"].as_str().and_then(|c| serde_json::from_str::<Value>(c).ok()) {
        if let Some(t) = c["items"].as_array().into_iter().flatten().find(|i| i["k"].as_str() == Some(k)).and_then(|i| i["text"].as_str()) {
            return t.to_owned();
        }
    }
    o["content"]
        .as_str()
        .unwrap_or("")
        .lines()
        .find_map(|l| Some(l.trim().strip_prefix(k)?.split_once(':')?.1.trim().to_owned()))
        .unwrap_or_default()
}

/// The root's items as an agent must see them before answering (Blaze, 2026-10-01: 23 replies answered his message
/// and not one row of its State moved, because nobody was shown the rows): key, kind, state, owner, words, and the
/// flags that move one.
fn items_block(ox: &Ox, t: &Value, root_id: &str) -> String {
    let items: Vec<&Value> = t["items"].as_array().into_iter().flatten().collect();
    if items.is_empty() {
        return String::new();
    }
    // Owners ride on replies as owner:K=<seat>; the newest claim wins.
    let mut owner: std::collections::HashMap<String, String> = Default::default();
    let mut replies: Vec<&Value> = t["replies"].as_array().into_iter().flatten().collect();
    replies.sort_by_key(|r| r["created_ms"].as_i64().unwrap_or(0));
    for r in replies {
        for tag in r["tags"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
            if let Some((k, seat)) = tag.strip_prefix("owner:").and_then(|x| x.split_once('=')) {
                owner.insert(k.to_owned(), seat.to_owned());
            }
        }
    }
    let open = items.iter().filter(|i| !matches!(i["state"].as_str(), Some("accepted" | "dropped"))).count();
    let mut out = format!(
        "ITEMS    {open} of {} not accepted. A reply that answers or moves one says so: --reply {root_id} --item <K>=<state> (open building built live blocked dropped; only the asker accepts) --tag owner:<K>=<seat>\n",
        items.len()
    );
    let mut cache = Default::default();
    for i in items {
        let k = i["k"].as_str().unwrap_or("?");
        let text = item_text(ox, i["from"].as_str().unwrap_or(root_id), k, &mut cache);
        out += &format!(
            "  {k:<4} {:<11} {:<9} {:<14} {}\n",
            i["kind"].as_str().unwrap_or("?"),
            i["state"].as_str().unwrap_or("?"),
            owner.get(k).map(String::as_str).unwrap_or("nobody named"),
            one_line(&text, 200)
        );
    }
    out
}

/// One map as lines: destination, frontier, fog (and what each waits on), done (and what resolved it).
fn map_text(ox: &Ox, t: &Value, room: &str, root_id: &str) -> String {
    let items: Vec<&Value> = t["items"].as_array().into_iter().flatten().collect();
    if items.is_empty() {
        return format!("{root_id}: no items. Declare them: ox annotate {room} {root_id} --set item:D=destination --set item:R1=research … (text in an items/1 cargo)\n");
    }
    let mut cache = Default::default();
    let mut owner: std::collections::HashMap<String, String> = Default::default();
    let mut replies: Vec<&Value> = t["replies"].as_array().into_iter().flatten().collect();
    replies.sort_by_key(|r| r["created_ms"].as_i64().unwrap_or(0));
    for r in replies {
        for tag in r["tags"].as_array().into_iter().flatten().filter_map(|x| x.as_str()) {
            if let Some((k, seat)) = tag.strip_prefix("owner:").and_then(|x| x.split_once('=')) {
                owner.insert(k.to_owned(), seat.to_owned());
            }
        }
    }
    let line = |i: &Value, cache: &mut std::collections::HashMap<String, Value>| {
        let k = i["k"].as_str().unwrap_or("?");
        let text = item_text(ox, i["from"].as_str().unwrap_or(root_id), k, cache);
        format!("  {k:<4} {:<11} {:<9} {:<14} {}", i["kind"].as_str().unwrap_or("?"), i["state"].as_str().unwrap_or("?"),
                owner.get(k).map(String::as_str).unwrap_or("nobody named"), one_line(&text, 160))
    };
    let of = |p: &str| items.iter().copied().filter(|i| i["place"].as_str() == Some(p)).collect::<Vec<_>>();
    let (dest, front, fog, done) = (of("destination"), of("frontier"), of("fog"), of("done"));
    let mut out = format!("MAP {root_id}  ·  {} frontier · {} in fog · {} done\n", front.len(), fog.len(), done.len());
    for d in &dest {
        let k = d["k"].as_str().unwrap_or("?");
        out += &format!("DESTINATION {}\n", one_line(&item_text(ox, d["from"].as_str().unwrap_or(root_id), k, &mut cache), 240));
    }
    out += "FRONTIER  (can be worked now: take one with --tag owner:K=<seat>)\n";
    for i in &front {
        out += &line(i, &mut cache);
        out += "\n";
    }
    out += "FOG  (waits on)\n";
    for i in &fog {
        let waits: Vec<&str> = i["blocked_by"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
        out += &format!("{}  ← {}\n", line(i, &mut cache), waits.join(", "));
    }
    out += "DONE\n";
    for i in &done {
        out += &line(i, &mut cache);
        if let Some(r) = i["resolved_by"].as_str() {
            let o = ox.get(&format!("/search?id={}", urlencode(r))).unwrap_or(Value::Null);
            out += &format!("\n       resolved by {r}: {}", one_line(o["content"].as_str().unwrap_or(""), 160));
        }
        out += "\n";
    }
    out
}

/// A refusal that stored nothing and asks to be retried: the server was busy or briefly unavailable (503). Such an op
/// is not settled; every other refusal (401, 403, 409, 422 ...) is the write's final answer.
fn retryable_refusal(refused: &str) -> bool {
    refused.contains("server refused (503)")
}

#[cfg(test)]
mod item_reply_tests {
    use super::item_reply_tags;
    #[test]
    fn claims_and_votes_are_the_tags_the_state_tab_reads_or_refused() {
        assert_eq!(item_reply_tags("OBS-a", "owner", "Q2", "bobby-core").unwrap(), ["reply:OBS-a", "owner:Q2=bobby-core"]);
        assert_eq!(item_reply_tags("OBS-a", "vote", "R12", "for").unwrap(), ["reply:OBS-a", "vote:R12=for"]);
        assert_eq!(item_reply_tags("OBS-a", "owner", "milestone_2026", "rocky").unwrap(), ["reply:OBS-a", "owner:milestone_2026=rocky"]);
        for bad in ["", "Q 1", "Q.1", "abcdefghijklmnopqrstuvwxy"] {
            assert!(item_reply_tags("OBS-a", "owner", bad, "rocky").is_err(), "{bad}");
        }
        assert!(item_reply_tags("OBS-a", "owner", "Q1", "Rocky Core").is_err());
        assert!(item_reply_tags("not-an-id", "owner", "Q1", "rocky").is_err());
    }
}

/// The task vocabulary of `ox say` (docs/ASKS-AND-EVENTS.md, "Tasks"): subject, rev, prev, verdict. A verdict or a prev
/// means nothing without the revision it is about, so both need --rev.
fn task_tags(subject: Option<String>, rev: Option<String>, prev: Option<String>, verdict: Option<String>) -> Result<Vec<String>> {
    if let Some(v) = &verdict {
        anyhow::ensure!(matches!(v.as_str(), "approve" | "changes"), "--verdict is approve or changes, not {v}");
        anyhow::ensure!(rev.is_some(), "--verdict {v} binds a revision: add --rev <commit>");
    }
    anyhow::ensure!(prev.is_none() || rev.is_some(), "--prev names the revision --rev follows: add --rev <commit>");
    for (flag, x) in [("--subject", &subject), ("--rev", &rev), ("--prev", &prev)] {
        if let Some(x) = x {
            anyhow::ensure!(!x.is_empty() && !x.contains(char::is_whitespace), "{flag} is one word, not {x:?}");
        }
    }
    Ok([subject.map(|x| format!("subject:{x}")), rev.map(|x| format!("rev:{x}")), prev.map(|x| format!("prev:{x}")), verdict.map(|x| format!("verdict:{x}"))]
        .into_iter().flatten().collect())
}

#[cfg(test)]
mod task_tag_tests {
    use super::task_tags;
    fn s(x: &str) -> Option<String> {
        Some(x.into())
    }
    #[test]
    fn the_task_vocabulary_is_written_whole_or_refused() {
        assert_eq!(task_tags(s("oracle-core#62"), s("4940bb0"), s("2778ecf"), s("approve")).unwrap(),
            ["subject:oracle-core#62", "rev:4940bb0", "prev:2778ecf", "verdict:approve"]);
        assert!(task_tags(None, None, None, s("approve")).unwrap_err().to_string().contains("add --rev"));
        assert!(task_tags(None, s("a"), None, s("lgtm")).unwrap_err().to_string().contains("approve or changes"));
        assert!(task_tags(None, None, s("a"), None).unwrap_err().to_string().contains("add --rev"));
        assert!(task_tags(s("two words"), None, None, None).is_err());
        assert!(task_tags(None, None, None, None).unwrap().is_empty());
    }
}

/// A content type from a file's extension, for `ox file put` without --type.
fn guess_type(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("md") => "text/markdown",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("jsonl") => "application/x-ndjson",
        Some("csv") => "text/csv",
        Some("html") | Some("htm") => "text/html",
        Some("py") => "text/x-python",
        Some("rs") => "text/x-rust",
        Some("dot") => "application/x-dot",
        _ => "application/octet-stream",
    }
}

/// The record object inside a `/search?id=` answer (its first hit), or the value itself when it is the record.
fn record_object(v: &Value) -> Option<&Value> {
    if v.get("tags").is_some() {
        return Some(v);
    }
    for k in ["results", "hits", "messages", "items"] {
        if let Some(first) = v[k].as_array().and_then(|a| a.first()) {
            return Some(first);
        }
    }
    None
}

/// The value of the record's first tag with this prefix.
fn record_tag(v: &Value, prefix: &str) -> Option<String> {
    record_object(v)?["tags"].as_array()?.iter().filter_map(|t| t.as_str()).find_map(|t| t.strip_prefix(prefix)).map(str::to_string)
}

/// `valid-until:<unix ms>` from a date (through its end), a date and time, or a span from now (30d, 12h, 90m): when a fact stops
/// holding (docs/SEARCH.md "Valid in time").
fn valid_until_tag(s: &str) -> Result<String> {
    let ms = oracle_proto::parse_when(s)
        .filter(|_| !s.trim().ends_with(['d', 'h', 'm']))
        .map(Ok)
        .unwrap_or_else(|| oracle_mcp::parse_by(s))
        .with_context(|| format!("--valid-until {s:?}: a date (2026-12-31), a date and time (2026-12-31T18:00) or a span (30d, 12h)"))?;
    Ok(format!("valid-until:{ms}"))
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b',' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(clap::Subcommand)]
enum AuthorityOp {
    /// Sign the Request v1 JSON in <file> ("-" reads stdin).
    SignRequest { file: String },
}

#[derive(clap::Subcommand)]
enum AnchorOp {
    /// Print the verified tip of <journal> as {"seq":N,"hash":"<hex>"}, or null for a fresh journal.
    Tip { journal: String },
    /// Record <seq> <hash> as the tip of <journal>. Refuses a lower seq, or the same seq with another
    /// hash; re-announcing the current tip succeeds without writing.
    Advance {
        journal: String,
        seq: u64,
        hash: String,
    },
}
const LEDGER_CHANNEL: &str = "dot-ledger";
const LEDGER_SCHEMA: &str = "dot-ledger-tip/1";
/// `ox read`: a room's page, newest first, as Core's `/search` serves it (`count`, `items`, `scanned`, `next_before`).
/// An MCP tool run through this seat's door, printed as the tool prints it (docs/ox.md, the parity rule). With `--json`, a
/// tool that composes several reads prints `{"tool": …, "text": …}`; single-route twins print the route's JSON instead.
fn twin(ox: &Ox, tool: &str, args: Value) -> Result<()> {
    let text = oracle_mcp::call(ox, tool, &args)?;
    if ox.json {
        println!("{}", json!({"tool": tool, "text": text}));
    } else {
        println!("{}", text.trim_end());
    }
    Ok(())
}

fn read_path(room: &str, limit: i64, state: &str, keep: &str, before: Option<i64>, starts: Option<&str>, tag: Option<&str>) -> String {
    let mut q = format!("/search?channel={}&limit={limit}&state={}&keep={}", urlencode(room), urlencode(state), urlencode(keep));
    if let Some(t) = tag {
        q.push_str(&format!("&tag={}", urlencode(t)));
    }
    if let Some(b) = before {
        q.push_str(&format!("&before={b}"));
    }
    if let Some(p) = starts {
        q.push_str(&format!("&starts={}", urlencode(p)));
    }
    q
}

/// `ox get --thread`: one page of the thread any message is in, as Core's `/threads/{root}` serves it.
fn thread_path(id: &str, before: Option<i64>, limit: Option<i64>, cargo: bool) -> String {
    let mut q = vec![];
    if let Some(b) = before {
        q.push(format!("before={b}"));
    }
    if let Some(l) = limit {
        q.push(format!("limit={l}"));
    }
    if cargo {
        q.push("include_cargo=true".to_string());
    }
    format!("/threads/{}?{}", urlencode(id), q.join("&"))
}

/// Pages of 200 read looking for this seat's newest tip line before giving up (fail closed).
const ANCHOR_PAGES: usize = 50;
fn ledger_journal_ok(j: &str) -> bool {
    !j.is_empty()
        && j.len() <= 64
        && j.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn ledger_hash_ok(h: &str) -> bool {
    h.len() == 64
        && h.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
/// A listing row as (seq, hash, id, cargo) when it is a well-formed tip line of `journal`.
fn tip_line(r: &Value, journal: &str) -> Option<(u64, String, String, String)> {
    let cargo = r["cargo"].as_str()?;
    let c: Value = serde_json::from_str(cargo).ok()?;
    if c["schema"] != LEDGER_SCHEMA || c["journal"] != journal {
        return None;
    }
    let hash = c["hash"].as_str().filter(|h| ledger_hash_ok(h))?;
    Some((
        c["seq"].as_u64()?,
        hash.to_string(),
        r["id"].as_str()?.to_string(),
        cargo.to_string(),
    ))
}
impl Ox {
    /// Seen: the seat's read mark in the message's room moves up to that message. Mirrors the
    /// surface's `/api/room-read`: read the observation for its room and seq, then fold that one
    /// mark; the server keeps the higher of the old and new mark, so this never rewinds.
    fn seen(&self, id: &str) -> Result<(String, i64)> {
        let v = self.get(&format!("/search?id={}", urlencode(id)))?;
        let o = v.get("observation").unwrap_or(&v);
        let room = o["channel"]
            .as_str()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| anyhow!("{id}: that message names no room"))?
            .to_string();
        let seq = o["seq"]
            .as_i64()
            .ok_or_else(|| anyhow!("{id}: that message has no seq"))?;
        self.post("/fold", &json!({"marks": {room.as_str(): seq}}))?;
        Ok((room, seq))
    }

    /// The verified tip of `journal`: the highest seq among this seat's lines, trusted only if its stored
    /// envelope verifies under this seat's key and matches what the server serves. The newest line not
    /// verifying is an error, never a reason to fall back to an older one (that is how a rollback would
    /// look). Two verified lines with the same seq and different hashes is an error too.
    fn anchor_tip(&self, journal: &str) -> Result<Option<(u64, String)>> {
        let me = pubkey_hex(&self.key()?);
        // Newest first, one journal's tag, paged until this seat's newest line: the anchor only moves
        // forward, so that line is the tip. Running out of pages means a fresh journal; running out of
        // the page budget is an error, never "fresh" (lines from others must not hide the tip).
        let tag = format!("dot-ledger:{journal}");
        let mut before: Option<i64> = None;
        let mut mine: Vec<(u64, String, String, String)> = Vec::new(); // seq, hash, id, cargo
        for page in 0.. {
            if page == ANCHOR_PAGES {
                bail!("no tip line of this seat within {ANCHOR_PAGES} pages of #{LEDGER_CHANNEL}");
            }
            let mut path = format!(
                "/search?channel={LEDGER_CHANNEL}&tag={}&limit=200",
                urlencode(&tag)
            );
            if let Some(b) = before {
                path.push_str(&format!("&before={b}"));
            }
            let v = self.get(&path)?;
            // Anything but a listing (a portal page, an error body) is no answer, never "no lines".
            let rows = v["items"]
                .as_array()
                .cloned()
                .ok_or_else(|| anyhow!("#{LEDGER_CHANNEL} did not answer with a listing"))?;
            for r in rows
                .iter()
                .filter(|r| r["author"].as_str() == Some(self.cfg.handle.as_str()))
            {
                // This seat's first line under the journal's tag is the candidate. If it is not a tip
                // line for this journal, tip fails: skipping it would make an older line the tip, which
                // is the rollback this anchor exists to catch. Later lines only ever add refusals.
                let line = tip_line(r, journal);
                if mine.is_empty() {
                    let id = r["id"].as_str().unwrap_or("?");
                    mine.push(line.ok_or_else(|| {
                        anyhow!("the newest line of {journal} ({id}) is not a {LEDGER_SCHEMA} line")
                    })?);
                } else if let Some(l) = line {
                    mine.push(l);
                }
            }
            // Every line of this page at the newest seq found is in hand; a same-seq twin on a later
            // page would be older than a line already read, and is caught when that seq is re-announced.
            if !mine.is_empty() {
                break;
            }
            match v["next_before"].as_i64() {
                Some(b) if !rows.is_empty() => before = Some(b),
                _ => return Ok(None),
            }
        }
        let top = mine.iter().map(|m| m.0).max().unwrap_or(0);
        if mine[0].0 != top {
            bail!("the newest tip line of {journal} is not its highest seq: the anchor moved backwards");
        }
        let mut verified: Option<String> = None;
        for (seq, hash, id, cargo) in mine.iter().filter(|m| m.0 == top) {
            let e = self.get(&format!("/search?id={}&include=envelope", urlencode(id)))?;
            let env: Envelope = serde_json::from_value(e["envelope"].clone())
                .map_err(|_| anyhow!("tip line {id}: no stored envelope"))?;
            if env.verify_stored(&me).is_err()
                || env.channel != LEDGER_CHANNEL
                || env.author != self.cfg.handle
                || env.cargo.as_deref() != Some(cargo.as_str())
            {
                bail!("tip line {id} at seq {seq} does not verify under this seat's key");
            }
            match &verified {
                Some(h) if h != hash => bail!("two different verified tips at seq {seq}"),
                _ => verified = Some(hash.clone()),
            }
        }
        Ok(verified.map(|h| (top, h)))
    }
    fn anchor_advance(&self, journal: &str, seq: u64, hash: &str) -> Result<()> {
        if let Some((t, h)) = self.anchor_tip(journal)? {
            if seq < t {
                bail!("refused: seq {seq} is below the anchored tip {t}");
            }
            if seq == t {
                if h == hash {
                    return Ok(()); // re-announcing the current tip
                }
                bail!("refused: seq {seq} is already anchored with another hash");
            }
        }
        let cargo = json!({"schema": LEDGER_SCHEMA, "journal": journal, "seq": seq, "hash": hash})
            .to_string();
        let env = Envelope::sign(
            &self.key()?,
            LEDGER_CHANNEL,
            &self.cfg.handle,
            Kind::Say,
            &format!("tip {journal} {seq}"),
            Some(cargo),
            vec!["dot-ledger-tip:v1".into(), format!("dot-ledger:{journal}")],
            now_ms(),
        );
        env.validate().map_err(|e| anyhow!("{e}"))?;
        // Never the outbox: a queued tip is not durable, and the journal must not release the spend. Through the
        // service it is submitted as signed here, at once, or refused; never queued.
        let asked = match self.ask_service(json!({"op": "submit", "envelope": env})) {
            None | Some(Err(service::Unanswered::NotSent(_))) => None,
            Some(Err(e)) => bail!("tip outcome unknown or refused: {e}"),
            Some(Ok(v)) => Some(v),
        };
        if let Some(v) = asked {
            return match service::value(v, "stored") {
                Ok(r) if r["id"].as_str().is_some_and(|i| !i.is_empty()) => Ok(()),
                Ok(_) => bail!("tip not written: the answer carried no receipt"),
                Err(e) => bail!("tip not written: {e:#}"),
            };
        }
        match self.call(
            "POST",
            Self::route(Kind::Say),
            Some(&serde_json::to_value(&env)?),
        ) {
            Sent::Ok(r) if r["id"].as_str().is_some_and(|i| !i.is_empty()) => Ok(()),
            Sent::Ok(_) => bail!("tip not written: the answer carried no receipt"),
            Sent::Refused(c, d) => bail!("tip not written: server refused ({c}): {d}"),
            Sent::Unreachable(r) => bail!("tip not written: Oracle unreachable: {r}"),
        }
    }
}

impl Ox {
    /// Read the server's one authoritative snapshot and verify its revision evidence.
    /// No client lineage fold: archive/head selection is shared with the server write guard.
    fn room_state(&self, room: &str) -> Result<Value> {
        self.room_state_at(room, None)
    }

    fn room_state_at(&self, room: &str, as_of_seq: Option<i64>) -> Result<Value> {
        oracle_mcp::state::read(self, room, as_of_seq)
    }
}

/// Print a chain check (docs/CHAIN.md); a chain with a break exits non-zero.
/// `ox verify` (docs/VERIFY.md): records checked with the key file alone; no seat, no config, no server.
fn verify_cmd(face: &Path, keys: &Path, owner_key: Option<&str>, json: bool) -> Result<()> {
    let file: oracle_proto::offline::KeysFile = serde_json::from_str(&std::fs::read_to_string(keys)?).with_context(|| format!("{} is not an oracle-keys/1 file", keys.display()))?;
    if file.schema != oracle_proto::offline::KEYS_SCHEMA {
        bail!("{} is {}, not {}", keys.display(), file.schema, oracle_proto::offline::KEYS_SCHEMA);
    }
    let pinned = owner_key.map(str::trim).map(str::to_lowercase);
    if let Some(pk) = &pinned {
        if *pk != file.owner_pubkey {
            bail!("the key file hangs from {}, not the owner key you pinned ({pk}): it is not this owner's log", file.owner_pubkey);
        }
    }
    let book = oracle_proto::offline::replay(&file.owner, &file.owner_pubkey, &file.events).map_err(|e| anyhow!("the key log does not replay: {e}"))?;
    let mut recs = Vec::new();
    for (n, line) in std::fs::read_to_string(face)?.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        recs.push(serde_json::from_str::<oracle_proto::ChainRecord>(line).map_err(|e| anyhow!("{} line {}: {e}", face.display(), n + 1))?);
    }
    let r = oracle_proto::offline::verify_records(&book, &recs);
    if json {
        println!("{}", json!({"report": r, "owner": file.owner, "owner_pubkey": file.owner_pubkey, "owner_pinned": pinned.is_some(), "key_events": file.events.len()}));
    } else {
        println!("verify  {} records · {} verified · {} failed · key log {} events from owner {}", r.records, r.verified, r.failures.len(), file.events.len(), file.owner);
        if r.withheld > 0 {
            println!("note    {} withheld link(s): records in rooms this face's reader may not read; linked, not signature-checked (docs/CHAIN.md)", r.withheld);
        }
        for f in &r.failures {
            println!("FAIL    {}  {}", f.id, f.why);
        }
        if pinned.is_none() {
            println!("note    the owner key was taken from the key file itself; pin it with --owner-key <hex> from the owner's published page");
        }
        if r.failures.is_empty() {
            println!("whole: every signature verifies, every id matches, and every key was its author's when it signed; no server was asked");
        }
    }
    if !r.failures.is_empty() {
        bail!("{} record(s) did not verify", r.failures.len());
    }
    Ok(())
}

/// One face file (`ox chain --out`, docs/CHAIN.md), read whole.
fn read_face(path: &Path) -> Result<Vec<oracle_proto::ChainRecord>> {
    let mut recs = Vec::new();
    for (n, line) in std::fs::read_to_string(path)?.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        recs.push(serde_json::from_str::<oracle_proto::ChainRecord>(line).map_err(|e| anyhow!("{} line {}: {e}", path.display(), n + 1))?);
    }
    Ok(recs)
}

fn chain_report(r: &oracle_proto::ChainReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(r)?);
    } else {
        println!(
            "chain  {} records · {} chained · {} unchained{}{}",
            r.records,
            r.chained,
            r.unchained,
            r.first_chained.as_deref().map(|f| format!(" · chained from {f}")).unwrap_or_default(),
            if r.from_genesis { " · starts at genesis" } else { "" }
        );
        for b in &r.breaks {
            println!("BREAK  {}  {}", b.id, b.why);
        }
        if r.breaks.is_empty() && r.withheld == 0 {
            println!("whole: every signature verifies over its rebuilt bytes, every id matches, every prev names the record before it");
        } else if r.breaks.is_empty() {
            println!(
                "whole: every prev names the record before it; {} withheld link(s) (records in rooms you may not read) carry no bytes to verify, every other signature and id verifies",
                r.withheld
            );
        }
    }
    if !r.breaks.is_empty() {
        bail!("{} break(s) in the chain", r.breaks.len());
    }
    Ok(())
}

fn main() {
    // A reader that closes the pipe early (`ox read | head`, `| grep -q`) has what it wanted: end
    // quietly with 0 instead of a panic and 101, which `set -o pipefail` reads as ox failing.
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or("");
        if msg.starts_with("failed printing to stdout") && msg.contains("Broken pipe") {
            std::process::exit(0);
        }
        default(info)
    }));
    if let Err(e) = run() {
        eprintln!("ox: {e:#}");
        std::process::exit(1);
    }
}

/// The fold of 2026-10-21 (docs/ox.md, "Old names"): the new form of an old verb, or None.
pub fn old_name_of(verb: &str) -> Option<&'static str> {
    Some(match verb {
        "ack" => "seen <rooms…> | seen --all",
        "hello" => "brief --hello",
        "presence" => "seats --live",
        "rooms" => "read",
        "threads" => "digest <room> --threads",
        "hallway" => "digest --all",
        "about" => "recall <words> --room <room> --newest",
        "where" => "recall <words> --where",
        "terms" => "recall --terms",
        "eval" => "recall --eval <set>",
        "map" => "state <room> --map",
        "heads" => "head",
        "experiments" => "head --exp",
        "asks" => "inbox --asks",
        "cards" => "card",
        "move" => "annotate <room> <id> --set item:K=state --why",
        "claim" => "react <room> <id> claim <item> --note",
        "vote" => "react <room> <id> vote <item>=<stance> --note",
        "idle" => "waiting",
        "take" => "card <room> <card> take",
        "done" => "card <room> <card> done --evidence --handoff",
        "watch" => "read --follow [a,b]",
        "anchor" => "dot anchor",
        "authority" => "dot authority",
        _ => return None,
    })
}

/// The last day an old name answers.
pub const OLD_NAMES_UNTIL: &str = "2026-10-28";

/// The new forms run the arms of the verbs they folded: the same code, the same output. A parsed `Cmd` comes out
/// as the `Cmd` to dispatch.
fn fold(cmd: Cmd) -> Result<Cmd> {
    Ok(match cmd {
        Cmd::Seats { live: true, .. } => Cmd::Presence,
        Cmd::Brief { hello: true, .. } => Cmd::Hello,
        Cmd::Inbox { asks: true, mine, seat, room, state, .. } => Cmd::Asks { mine, seat, room, state },
        Cmd::Read { follow: true, room, .. } => Cmd::Watch { room: room.unwrap_or_default() },
        Cmd::Read { room: None, since, .. } => Cmd::Rooms { since },
        Cmd::Digest { threads: true, room: Some(room), limit, before, .. } => Cmd::Threads { room, limit, before },
        Cmd::Digest { all: true, days, .. } | Cmd::Digest { room: None, days, .. } => Cmd::Hallway { days },
        Cmd::State { map: true, room, id, sets, .. } => {
            anyhow::ensure!(sets.len() <= 1, "--map --set takes the map text once");
            Cmd::Map { room, id, set: sets.into_iter().next() }
        }
        Cmd::State { id: Some(id), .. } => bail!("{id}: a second argument goes with --map"),
        Cmd::Recall { terms: true, limit, .. } => Cmd::Terms { limit: if limit == RECALL_LIMIT { 100 } else { limit } },
        Cmd::Recall { eval: Some(set), limit, params, brief, .. } => Cmd::Eval { set, limit: if limit == RECALL_LIMIT { 10 } else { limit }, params, brief },
        Cmd::Recall { which_rooms: true, query, any, .. } => Cmd::Where { words: words_of(query), any },
        Cmd::Recall { answerable: true, query, room, .. } => Cmd::Answerable { words: words_of(query), room },
        Cmd::Recall { newest: true, room: Some(room), query, limit, .. } => Cmd::About { room, terms: words_of(query), limit: if limit == RECALL_LIMIT { 25 } else { limit } },
        Cmd::Recall { newest: true, room: None, .. } => bail!("--newest reads one room: add --room <room>"),
        Cmd::Head { subject: None, exp: true, track, stage, q, limit, .. } => Cmd::Experiments { track, stage, q, limit: if limit == 50 { 60 } else { limit } },
        Cmd::Head { subject: None, owner, all, .. } => Cmd::Heads { owner, all },
        Cmd::Card { room: None, mine, waiting, owner, state, limit, .. } => Cmd::Cards { mine, waiting, room: None, owner, status: state, limit },
        Cmd::Card { room: Some(room), id: Some(card), status: Some(st), summary, .. } if st == "take" => Cmd::Take { room, card, note: summary },
        Cmd::Card { room: Some(room), id: Some(card), status: Some(st), evidence, handoff: Some(handoff), .. } if st == "done" => Cmd::Done { room, card, evidence, handoff },
        Cmd::React { room, id, kind, item, note, seat, .. } if kind == "claim" => Cmd::Claim {
            room,
            id,
            item: item.ok_or_else(|| anyhow!("react … claim <item>"))?,
            seat,
            note: note.ok_or_else(|| anyhow!("--note says why, in one line"))?,
        },
        Cmd::React { room, id, kind, item, note, .. } if kind == "vote" => Cmd::Vote {
            room,
            id,
            what: item.ok_or_else(|| anyhow!("react … vote <item>=for|against|abstain"))?,
            note: note.ok_or_else(|| anyhow!("--note says why, in one line"))?,
        },
        Cmd::Seen { targets, all } if all || targets.iter().any(|t| !t.starts_with("OBS-")) => {
            anyhow::ensure!(targets.iter().all(|t| !t.starts_with("OBS-")), "seen takes message ids or room names, not both");
            Cmd::Ack { rooms: targets, all }
        }
        Cmd::Dot { op: DotOp::Anchor { op } } => Cmd::Anchor { op },
        Cmd::Dot { op: DotOp::Authority { op } } => Cmd::Authority { op },
        other => other,
    })
}

fn words_of(query: Option<String>) -> Vec<String> {
    query.unwrap_or_default().split_whitespace().map(str::to_owned).collect()
}

fn run() -> Result<()> {
    run_cli(Cli::parse())
}

/// The whole command, from a parsed command line (tests drive `ox say` through here with a stalled footer).
fn run_cli(mut cli: Cli) -> Result<()> {
    if let Some((old, now)) = std::env::args().nth(1).and_then(|v| old_name_of(&v).map(|n| (v, n))) {
        eprintln!("ox {old} is now ox {now} (until {OLD_NAMES_UNTIL})");
    }
    cli.cmd = fold(cli.cmd)?;
    if let Cmd::Manual { topic, check } = &cli.cmd {
        return manual::run(*topic, check.as_deref(), cli.json);
    }
    // The offline checks need no seat, no config and no server: a stranger runs them (docs/VERIFY.md, docs/CHAIN.md).
    match &cli.cmd {
        Cmd::Verify { face, keys, owner_key } => return verify_cmd(face, keys, owner_key.as_deref(), cli.json),
        Cmd::Chain { check: Some(path), .. } => return chain_report(&oracle_proto::check_chain(&read_face(path)?), cli.json),
        _ => {}
    }
    let home = home_dir(cli.home)?;

    if let Cmd::Setup {
        url,
        handle,
        token,
        token_file,
        key,
        fallback_urls,
    } = &cli.cmd
    {
        let token = match (token, token_file) {
            (Some(t), _) => t.trim().to_string(),
            (None, Some(f)) => std::fs::read_to_string(f).with_context(|| format!("cannot read {}", f.display()))?.trim().to_string(),
            (None, None) => std::env::var("ORACLE_TOKEN").context("supply --token-file, --token or ORACLE_TOKEN")?.trim().to_string(),
        };
        if token.is_empty() {
            bail!("the token is empty");
        }
        let sk = keyfile::load(key)?;
        let key_path = std::fs::canonicalize(key)?.display().to_string();
        let cfg = Config {
            url: url.clone(),
            fallback_urls: fallback_urls.clone(),
            handle: handle.clone(),
            token,
            key_path,
        };
        write_private(
            &home.join("config.json"),
            &serde_json::to_string_pretty(&cfg)?,
        )?;
        println!("configured  {handle} @ {url}");
        println!("pubkey      {}", pubkey_hex(&sk));
        println!("next        ox doctor");
        return Ok(());
    }

    if let Cmd::Join { name, url, key } = &cli.cmd {
        return join::join(&home, url, name, key.as_deref(), cli.json);
    }
    if let Cmd::Connect { agent, dry_run } = &cli.cmd {
        return join::connect(&home, &home_dir(None)?, *agent, *dry_run);
    }

    let mut ox = Ox::load(home, cli.json)?;
    ox.op_id = cli.op_id.clone();
    // Every working call ends with "N new for you" when something addressed to this seat is unread.
    let nudges = !cli.json
        && matches!(
            cli.cmd,
            Cmd::Say { .. } | Cmd::Read { .. } | Cmd::Context { .. } | Cmd::Status { .. } | Cmd::Card { .. }
                | Cmd::Idle { .. } | Cmd::Working { .. } | Cmd::Waiting { .. } | Cmd::Take { .. } | Cmd::Done { .. }
                | Cmd::React { .. } | Cmd::Annotate { .. } | Cmd::Move { .. } | Cmd::Claim { .. } | Cmd::Vote { .. } | Cmd::Archive { .. } | Cmd::Seen { .. }
        );
    // `ox tell` is `ox say` held to the Bridge contract: checked here, then written by the same path.
    let cmd = match cli.cmd {
        Cmd::Tell { head, room, ask, options, recommend, why, by, refs, detail, intent, reply, affects, absent, owed, dot } => {
            eprintln!("ox tell forwards to ox say --to @owner; use say for new integrations");
            let mut problems = oracle_mcp::tell_problems(&head, &ask, &options, by.as_deref(), recommend, why.as_deref());
            let gap = oracle_mcp::tell_gap(&ask, &affects, &absent, owed.as_deref()).unwrap_or_else(|p| { problems.extend(p); None });
            if !problems.is_empty() {
                bail!("not sent, the Bridge contract (docs/BRIDGE-CONTRACT.md): {}", problems.join("; "));
            }
            let mut cargo = match &detail {
                Some(p) => {
                    let raw = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
                    let v: Value = serde_json::from_str(&raw).with_context(|| format!("{} is not JSON", p.display()))?;
                    anyhow::ensure!(matches!(v["schema"].as_str(), Some("doc/1") | Some("plan/1")), "--detail must be a doc/1 or plan/1 page");
                    Some(v)
                }
                None => None,
            };
            // The gap rides in cargo beside the options (docs/GAP.md).
            if let Some(g) = gap {
                let obj = cargo.get_or_insert_with(|| json!({})).as_object_mut().context("--detail must be a JSON object")?;
                anyhow::ensure!(!obj.contains_key("gap"), "--detail already has a gap; use --affects/--absent/--owed or the page, not both");
                obj.insert("gap".into(), g);
            }
            // The pick rides in cargo for the Bridge to mark, and in the words for everyone else (voice reads the text).
            let owner = oracle_mcp::owner_handle(&ox)?;
            let mut text = format!("@{owner} {head}");
            if let (Some(n), Some(w)) = (recommend, why.as_deref()) {
                let obj = cargo.get_or_insert_with(|| json!({})).as_object_mut().context("--detail must be a JSON object")?;
                anyhow::ensure!(!obj.contains_key("recommendation"), "--detail already has a recommendation; use --recommend/--why or the page, not both");
                obj.insert("recommendation".into(), json!({"option": n, "label": options[n - 1], "why": w.trim()}));
                text = format!("{text}\n{}", oracle_mcp::tell_choice_text(&options, n, w));
            }
            let cargo = cargo.map(|c| c.to_string());
            Cmd::Say {
                room: room.unwrap_or_else(|| ox.cfg.handle.clone()),
                dot,
                from_dot: None,
                type_addr: None,
                text,
                to: vec![owner],
                tags: vec![],
                reply,
                intent: Some(intent.unwrap_or_else(|| if ask == "decide" { "question".into() } else { "result".into() })),
                closes: None, supersedes: None, subject: None, rev: None, prev: None, verdict: None, archives: None, ttl: None,
                expires: None,
                ask: Some(ask),
                options,
                recommend: None,
                why: None,
                choice: None,
                by,
                refs,
                needs: vec![], verify: None, facets: vec![], items: vec![],
                cargo,
                valid_until: None,
            }
        }
        other => other,
    };
    match cmd {
        Cmd::Manual { .. } | Cmd::Setup { .. } | Cmd::Tell { .. } | Cmd::Join { .. } | Cmd::Connect { .. } | Cmd::Verify { .. } | Cmd::Chain { check: Some(_), .. } => unreachable!("handled above"),
        Cmd::Doctor => doctor(&ox)?,
        Cmd::Whoami => {
            let v = ox.get("/whoami")?;
            let local = pubkey_hex(&ox.key()?);
            let matches = registered(&v, &local);
            if ox.json {
                println!(
                    "{}",
                    json!({"server": v, "local_pubkey": local, "key_matches": matches})
                );
            } else {
                println!(
                    "handle  {}  ({})",
                    v["handle"].as_str().unwrap_or("?"),
                    v["scope"].as_str().unwrap_or("?")
                );
                println!(
                    "pubkey  {local}  {}",
                    if matches {
                        "✓ registered"
                    } else {
                        "✗ NOT the key the registry holds"
                    }
                );
                println!("oracle  {}  ● reachable", ox.base());
            }
        }
        Cmd::Eval { set, limit, params, brief } => {
            let extra: String = params.iter().filter_map(|p| p.split_once('=')).map(|(k, v)| format!("&{}={}", oracle_mcp::enc(k), oracle_mcp::enc(v))).collect();
            let cases = eval::read_cases(&set)?;
            // The brief as this seat wakes with it, rendered fresh from the server (never brief.cache).
            let wake = if brief { Some(brief_with(&ox, false)?) } else { None };
            let mut rows = Vec::new();
            for c in &cases {
                let t = std::time::Instant::now();
                let a = ox.get(&format!("/search?q={}&limit={limit}{extra}", oracle_mcp::enc(&c.q)))?;
                let mut row = eval::score(c, &a, t.elapsed().as_millis());
                row.in_brief = wake.as_deref().and_then(|b| eval::brief_covers(c, b));
                rows.push(row);
            }
            let sum = eval::summary(&rows, &cases);
            if ox.json {
                println!("{}", json!({"rows": rows, "summary": sum}));
            } else {
                println!("{:<44} {:>4} {:>6} {:>6} {:>4} {:>4} {:>5} {:>5} {:>5} {:>6} {:>6} {:>6} {:>6} {:>5}", "query", "rank", "top", "bytes", "dup", "syn5", "noise", "edges", "none", "allw", "<day", "mean", "ms", "brief");
                for r in &rows {
                    let q: String = r.q.chars().take(44).collect();
                    println!("{:<44} {:>4} {:>6.3} {:>6} {:>4} {:>4} {:>5} {:>5} {:>5} {:>6} {:>6} {:>6.3} {:>6} {:>5}", q, r.rank.map_or("-".into(), |x| x.to_string()), r.top_score, r.bytes, r.dups,
                        r.synthetic_top5, r.noise, r.edges, r.said_none.map_or("", |b| if b { "yes" } else { "NO" }), r.all_words, r.all_words_before, r.best_meaning.unwrap_or(-1.0), r.ms,
                        r.in_brief.map_or("", |b| if b { "yes" } else { "NO" }));
                }
                println!("{sum}");
            }
        }
        Cmd::Hello => {
            if !ox.json { eprintln!("Offline agent guide: ox manual workflow (reference: ox manual)"); }
            twin(&ox, "oracle_hello", json!({}))?;
        },
        Cmd::Health => {
            let v = ox.get("/health")?;
            if ox.json {
                println!("{v}");
            } else {
                print!("{}", views::health_text(&v));
            }
        }
        Cmd::Metrics { route, sort, all } => {
            let v = ox.get("/metrics/routes")?;
            if ox.json {
                println!("{v}");
            } else {
                print!("{}", views::metrics_text(&v, route.as_deref(), &sort, all, now_ms())?);
            }
        }
        Cmd::Presence => {
            let v = ox.get("/presence")?;
            if ox.json {
                println!("{v}");
            } else {
                print!("{}", views::presence_text(&v));
            }
        }
        Cmd::Seats { export: Some(path), .. } => {
            let reg = ox.get("/keys")?;
            let hist = ox.get("/keys?history=true")?;
            // The log hangs from the first owner key: a rotated owner key is a later event in the log.
            let mut owners: Vec<&Value> = reg["keys"].as_array().into_iter().flatten().filter(|k| k["role"] == "owner").collect();
            owners.sort_by_key(|k| k["created_ms"].as_i64().unwrap_or(i64::MAX));
            let root = owners.first().context("the registry names no owner key")?;
            let events: Vec<oracle_proto::KeyEvent> = serde_json::from_value(hist["events"].clone())?;
            let file = oracle_proto::offline::KeysFile {
                schema: oracle_proto::offline::KEYS_SCHEMA.into(),
                owner: root["handle"].as_str().unwrap_or_default().into(),
                owner_pubkey: root["pubkey"].as_str().unwrap_or_default().into(),
                origin: ox.base().to_string(),
                exported_ms: now_ms(),
                events,
            };
            // Refuse to write a file that would not check: the log must replay from its own owner key.
            oracle_proto::offline::replay(&file.owner, &file.owner_pubkey, &file.events).map_err(|e| anyhow!("the key log does not replay: {e}"))?;
            write_durable(&path, &(serde_json::to_string_pretty(&file)? + "\n"))?;
            if !ox.json {
                println!("wrote {} key events, owner {} {}, to {}", file.events.len(), file.owner, file.owner_pubkey, path.display());
            }
        }
        Cmd::Seats { history, .. } => {
            let v = ox.get(if history { "/keys?history=true" } else { "/keys" })?;
            if ox.json {
                println!("{v}");
            } else if history {
                print!("{}", views::key_events_text(&v, now_ms()));
            } else {
                print!("{}", views::seats_text(&v, now_ms()));
            }
        }
        Cmd::Rooms { since } => {
            let span = since.as_deref().map(views::span_ms).transpose()?;
            let v = ox.get("/channels")?;
            if ox.json {
                println!("{v}");
            } else {
                print!("{}", views::rooms_text(&v, span, now_ms()));
                // The archive (docs/ARCHIVE.md): listed, readable, out of search.
                let archived: Vec<String> = v["channels"].as_array().into_iter().flatten()
                    .filter(|r| r["archived"] == true)
                    .map(|r| format!("#{} ({})", r["channel"].as_str().unwrap_or("?"), one_line(r["archived_why"].as_str().unwrap_or(""), 60)))
                    .collect();
                if !archived.is_empty() {
                    println!("ARCHIVED · {} room(s), out of search (recall --include-archived reads them): {}", archived.len(), archived.join(" · "));
                }
            }
        }
        Cmd::Threads { room, limit, before } => {
            let mut path = format!("/rooms/{}/threads?limit={limit}", urlencode(&room));
            if let Some(b) = before {
                path += &format!("&before={b}");
            }
            let v = ox.get(&path)?;
            if ox.json {
                println!("{v}");
            } else {
                print!("{}", views::threads_text(&v, now_ms()));
            }
        }
        Cmd::Heads { owner, all } => {
            let owner = if all { Some("*".to_string()) } else { owner };
            if ox.json {
                println!("{}", ox.get(&oracle_mcp::heads_path(&ox.cfg.handle, owner.as_deref(), None))?);
            } else {
                twin(&ox, "oracle_tasks", json!({"owner": owner}))?;
            }
        }
        Cmd::Where { words, any } => {
            let query = words.join(" ");
            if ox.json {
                println!("{}", ox.get(&oracle_mcp::where_path(&query, any))?);
            } else {
                twin(&ox, "oracle_where", json!({"query": query, "any": any}))?;
            }
        }
        Cmd::Answerable { words, room } => {
            let query = words.join(" ");
            if ox.json {
                println!("{}", ox.get(&oracle_mcp::answerable_path(&query, room.as_deref()))?);
            } else {
                twin(&ox, "oracle_recall", json!({"query": query, "room": room, "answerable": true}))?;
            }
        }
        Cmd::Terms { limit } => {
            if ox.json {
                println!("{}", ox.get(&oracle_mcp::terms_path(limit))?);
            } else {
                twin(&ox, "oracle_terms", json!({"limit": limit}))?;
            }
        }
        Cmd::Experiments { track, stage, q, limit } => {
            let a = json!({"track": track, "stage": stage, "q": q, "limit": limit});
            if ox.json {
                println!("{}", ox.get(&oracle_mcp::experiments_path(&a))?);
            } else {
                twin(&ox, "oracle_experiments", a)?;
            }
        }
        Cmd::Watch { room } => {
            let rooms: Vec<String> = room.split(',').map(str::trim).filter(|r| !r.is_empty()).map(str::to_owned).collect();
            ox.watch(&rooms)?;
        }
        Cmd::Now { head: false, seat: None, sets, clear } if sets.is_empty() && clear.is_empty() => {
            let v = ox.get("/health")?;
            if ox.json {
                println!("{v}");
            } else {
                println!(
                    "{}  ({} ms since epoch, Oracle clock, UTC){}",
                    v["iso"].as_str().unwrap_or("?"),
                    v["ms"],
                    v["local"].as_str().map(|l| format!("  local {l}")).unwrap_or_default()
                );
            }
        }
        Cmd::Map { id: Some(_), set: Some(_), .. } => bail!("--set writes the room's map; a work map (<id>) is read only"),
        Cmd::Map { room, id: None, set } => twin(&ox, "oracle_room_map", json!({"room": room, "set": set}))?,
        Cmd::Map { room, id: Some(id), .. } => {
            let t = ox.get(&format!("/threads?id={}", urlencode(&id)))?;
            print!("{}", map_text(&ox, &t, &room, &id));
        }
        Cmd::Context { room, re, roots } => {
            let now = ox.get("/health")?;
            let now_ms = now["ms"].as_i64().unwrap_or(0);
            let age = |ms: i64| {
                let s = (now_ms - ms).max(0) / 1000;
                match s {
                    0..=89 => format!("{s}s ago"),
                    90..=5399 => format!("{}m ago", s / 60),
                    5400..=172_799 => format!("{}h ago", s / 3600),
                    _ => format!("{}d ago", s / 86_400),
                }
            };
            let mut out = format!(
                "NOW      {}  (Oracle clock, UTC — your training data is older than this; date every search and every claim)\n",
                now["iso"].as_str().unwrap_or("?")
            );
            if let Some(id) = &re {
                let t = ox.get(&format!("/threads?id={}", urlencode(id)))?;
                let line = |o: &Value, label: &str, width: usize| {
                    let facet = o["facet"]
                        .as_str()
                        .filter(|f| *f != "talk")
                        .map(|f| format!(" [{f}]"))
                        .unwrap_or_default();
                    format!(
                        "{label}{}  {}  {}{facet}\n           {}\n",
                        o["author"].as_str().unwrap_or("?"),
                        oracle_proto::iso_ms(o["created_ms"].as_i64().unwrap_or(0)),
                        age(o["created_ms"].as_i64().unwrap_or(0)),
                        one_line(o["content"].as_str().unwrap_or(""), width)
                    )
                };
                let root = &t["root"];
                out += &format!(
                    "THREAD   {}  ·  {}  ·  floor {}{}\n",
                    t["card"]["status"].as_str().unwrap_or("?"),
                    t["state"].as_str().unwrap_or("?"),
                    t["floor"]["name"].as_str().unwrap_or("?"),
                    if t["floor"]["guessed"].as_bool().unwrap_or(false) {
                        " (guessed)"
                    } else {
                        ""
                    }
                );
                let c = &t["card"];
                out += &format!(
                    "STAGE    {}  (asked → researched → proposed → decided → done) · questions open {} of {} · {} replies\n",
                    c["stage"].as_str().unwrap_or("?"), c["questions_open"], c["questions"], c["replies"]
                );
                out += &items_block(&ox, &t, root["id"].as_str().unwrap_or(id));
                for (label, key) in [("OUTCOME  ", "outcome"), ("SUMMARY  ", "summary")] {
                    if let Some(text) = t[key]["text"].as_str() {
                        out += &format!(
                            "{label}{} · {}\n           {}\n",
                            t[key]["author"].as_str().unwrap_or("?"),
                            age(t[key]["created_ms"].as_i64().unwrap_or(0)),
                            one_line(text, 700)
                        );
                    }
                }
                // The root is the question being answered: never clipped (Blaze, 2026-09-28: "why is the text cutting
                // off" — a 600-char cut hid the last three questions of his message from every agent reading the thread).
                out += &line(root, "  root   ", usize::MAX);
                let replies: Vec<&Value> = t["replies"].as_array().into_iter().flatten().collect();
                let cut = t["summary"]["created_ms"]
                    .as_i64()
                    .filter(|_| replies.len() > 12);
                let shown: Vec<&Value> = replies
                    .iter()
                    .copied()
                    .filter(|r| {
                        cut.is_none_or(|ms| r["created_ms"].as_i64().unwrap_or(0) >= ms)
                            || r["id"].as_str() == Some(id.as_str())
                    })
                    .collect();
                if shown.len() < replies.len() {
                    out += &format!("  ({} earlier replies are covered by the summary above; `ox get <id>` reads any of them)\n", replies.len() - shown.len());
                }
                for r in shown {
                    let mark = if r["id"].as_str() == Some(id.as_str()) {
                        "  >rang  "
                    } else {
                        "  reply  "
                    };
                    out += &line(r, mark, 320);
                }
                out += &format!(
                    "REPLY TO {}   (use: --reply {})\n",
                    root["id"].as_str().unwrap_or(id),
                    id
                );
            }
            let d = ox.get(&format!(
                "/threads?view=digest&channel={}&roots={}",
                urlencode(&room),
                urlencode(&roots)
            ))?;
            out += &format!(
                "ROOM     {}\n",
                oracle_mcp::digest_text(&d).replace('\n', "\n         ").trim_end()
            );
            let mut face_room = room.clone();
            if let Some(c) = face_room.get_mut(0..1) {
                c.make_ascii_uppercase();
            }
            // The book is a feed's current value: a state head `book:<room>` (refreshed every few
            // minutes, never posted into the room). Rooms that still publish it as a message fall
            // back to the newest `[Room book]` post.
            let head = ox
                .get(&format!("/states?subject={}", urlencode(&format!("book:{room}"))))
                .ok()
                .map(|h| {
                    let mut o = h["observation"].clone();
                    if let Some(data) = o["cargo"]
                        .as_str()
                        .and_then(|c| serde_json::from_str::<Value>(c).ok())
                        .map(|c| c["data"].clone())
                        .filter(|d| !d.is_null())
                    {
                        o["cargo"] = Value::String(data.to_string());
                    }
                    o
                })
                .filter(|o| o["created_ms"].is_i64());
            let posted = || -> Result<Option<Value>> {
                let v = ox.get(&format!(
                    "/search?channel={}&starts={}&limit=1",
                    urlencode(&room),
                    urlencode(&format!("[{face_room} book]"))
                ))?;
                Ok(v["items"].as_array().and_then(|a| a.first()).cloned())
            };
            let book = match head {
                Some(h) => Some(h),
                None => posted()?,
            };
            match book.as_ref() {
                Some(b) => {
                    let ms = b["created_ms"].as_i64().unwrap_or(0);
                    let stale = now_ms - ms > 15 * 60_000;
                    out += &format!(
                        "BOOK     {}\n         published by {} at {} ({}){}\n",
                        one_line(b["content"].as_str().unwrap_or(""), 300),
                        b["author"].as_str().unwrap_or("?"),
                        oracle_proto::iso_ms(ms),
                        age(ms),
                        if stale { "  STALE — say so, and do not size a trade on it" } else { "" }
                    );
                    let cargo: Value = b["cargo"].as_str().and_then(|c| serde_json::from_str(c).ok()).unwrap_or(Value::Null);
                    for p in cargo["positions"].as_array().into_iter().flatten().take(20) {
                        out += &format!("         {}\n", one_line(p.as_str().unwrap_or(""), 160));
                    }
                    if let Some(n) = cargo["notes"].as_str() {
                        out += &format!("         note: {}\n", one_line(n, 200));
                    }
                }
                None => out += "BOOK     not shared with this room. Do not guess positions or balances; say you cannot see the book.\n",
            }
            print!("{out}");
            if !board::peek(ox.json) {
                board::ack_room(&ox, &room)?;
            }
        }
        Cmd::About { room, terms, limit } => {
            let v = ox.get(&format!(
                "/search?channel={}&about={}&limit={limit}",
                urlencode(&room),
                urlencode(&terms.join(","))
            ))?;
            if ox.json {
                println!("{v}");
                return Ok(());
            }
            let now = v["now_ms"].as_i64().unwrap_or(0);
            let entries: Vec<&Value> = v["entries"].as_array().into_iter().flatten().collect();
            println!("ABOUT    {} in #{room} · {} statements in {} threads · newest first · what was SAID then; check it against now",
                terms.join(", "), entries.len(), v["threads"]);
            for e in &entries {
                let ms = e["created_ms"].as_i64().unwrap_or(0);
                let hours = (now - ms).max(0) / 3_600_000;
                let when = if hours < 48 {
                    format!("{hours}h ago")
                } else {
                    format!("{}d ago", hours / 24)
                };
                let kind = e["facet"]
                    .as_str()
                    .filter(|f| *f != "talk")
                    .map(|f| format!(" [{f}]"))
                    .unwrap_or_default();
                let place = if e["is_root"].as_bool().unwrap_or(false) {
                    "started the thread".to_string()
                } else {
                    format!(
                        "in \"{}\" ({})",
                        one_line(e["root"]["text"].as_str().unwrap_or(""), 60),
                        e["root"]["author"].as_str().unwrap_or("?")
                    )
                };
                println!(
                    "  {}  {when:<8} {}{kind}  {place}  · {}",
                    oracle_proto::iso_ms(ms),
                    e["author"].as_str().unwrap_or("?"),
                    e["id"].as_str().unwrap_or("")
                );
                if let Some(by) = e["superseded_by"].as_str() {
                    println!("      ✗ CORRECTED LATER by {by} — do not rely on this one");
                }
                for c in e["supersedes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.as_str())
                {
                    println!("      ↻ corrects {c}");
                }
                for line in e["excerpt"].as_str().unwrap_or("").lines().take(6) {
                    println!("      {}", one_line(line, 220));
                }
            }
            if entries.is_empty() {
                println!("  nothing yet: you are the first to say anything about it here");
            }
        }
        Cmd::Head { subject, limit, .. } => {
            let subject = subject.unwrap_or_default();
            let v = ox.get(&format!("/search?tag={}&limit={limit}", urlencode(&format!("subject:{subject}"))))?;
            let revs = head_revisions(&v);
            if ox.json {
                println!("{}", json!({"subject": subject, "current": revs.last(), "revisions": revs}));
            } else if revs.is_empty() {
                println!("{subject}: no state record (nobody has written this subject)");
            } else {
                print!("{}", head_text(&subject, &revs));
            }
        }
        Cmd::Get { id, thread: true, before, limit, cargo, .. } => {
            let v = ox.get(&thread_path(&id, before, limit, cargo))?;
            if ox.json {
                println!("{v}");
            } else {
                let t = &v["thread"];
                println!("THREAD   {} · {} · {} replies", v["root"]["id"].as_str().unwrap_or("?"), t["state"].as_str().unwrap_or("?"), t["replies"]);
                print!("{}", oracle_mcp::full(&v["root"]));
                let mut items = v["items"].as_array().cloned().unwrap_or_default();
                items.reverse();
                show(&items, true);
                match v["next_before"].as_i64() {
                    Some(nb) => println!("PARTIAL  {} of {} replies shown · older: ox get {id} --thread --before {nb}", items.len(), t["replies"]),
                    None => println!("WHOLE    nothing older"),
                }
            }
        }
        Cmd::Get { id, dot: true, out, .. } => {
            let v = ox.get(&format!("/search?id={}", urlencode(&id)))?;
            ox.get_dot(&id, &v, out.as_deref())?;
        }
        Cmd::Get { id, .. } => {
            let v = ox.get(&format!("/search?id={}", urlencode(&id)))?;
            if ox.json {
                println!("{v}");
            } else {
                print!("{}", oracle_mcp::full(&v));
            }
        }
        Cmd::Types { prefix, limit } => {
            let p = prefix.unwrap_or_default();
            let v = ox.get(&format!("/types?prefix={}&limit={limit}", urlencode(&p)))?;
            if ox.json {
                println!("{v}");
            } else {
                println!("TYPES  {} · {} records", if p.is_empty() { "every type" } else { p.as_str() }, v["count"]);
                for t in v["types"].as_array().into_iter().flatten().take(20) {
                    println!("  {:>6}  {}", t["count"], t["type"].as_str().unwrap_or(""));
                }
                for i in v["items"].as_array().into_iter().flatten() {
                    let text: String = i["text"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ").chars().take(90).collect();
                    let renamed = i["retypes"].as_array().is_some_and(|r| r.iter().any(|x| x["applied"].as_bool() == Some(true)));
                    println!("{}  {}{}  #{}  {}  {text}", i["code"].as_str().unwrap_or(i["id"].as_str().unwrap_or("?")),
                        i["type"].as_str().unwrap_or(""), if renamed { " (renamed)" } else { "" },
                        i["room"].as_str().unwrap_or(""), i["author"].as_str().unwrap_or(""));
                }
            }
        }
        Cmd::Edit { id, text, file, why } => {
            let target = ox.get(&format!("/search?id={}", urlencode(&id)))?;
            let full = target["id"].as_str().context("the Oracle named no record")?.to_string();
            let room = target["channel"].as_str().context("the record names no room")?.to_string();
            let author = target["author"].as_str().unwrap_or("").to_string();
            let kind = match target["kind"].as_str() {
                Some("say") => Kind::Say,
                Some("memory") => Kind::Memory,
                other => anyhow::bail!("{full} is a {}; only a say or a memory is edited (a State head moves with ox task or ox plan)", other.unwrap_or("record of no kind")),
            };
            let me = ox.get("/whoami")?;
            let handle = me["handle"].as_str().unwrap_or(&ox.cfg.handle).to_string();
            anyhow::ensure!(author == handle || me["role"] == "owner",
                "{full} is {author}'s: a seat edits only its own records, the owner any. Correct it with a reply: ox say {room} \"…\" --reply {full} --intent correction");
            let old = target["content"].as_str().unwrap_or("").to_string();
            let new = match (text, file) {
                (Some(t), _) => t,
                (None, Some(p)) => std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?,
                (None, None) => edit::in_editor(&old)?,
            };
            anyhow::ensure!(new.trim() != old.trim(), "no change: nothing written");
            let tags = edit::kept_tags(target["tags"].as_array().map(Vec::as_slice).unwrap_or(&[]), &full);
            let old_cargo = target["cargo"].as_str().and_then(|c| serde_json::from_str::<Value>(c).ok());
            let cargo = edit::cargo(old_cargo, &full, why.as_deref());
            let v = ox.write(kind, &room, &new, Some(cargo.to_string()), tags)?;
            print_written(&ox, &v, &format!("edits {full} ({})", edit::change(&old, &new)));
        }
        Cmd::Retype { id, addr } => {
            anyhow::ensure!(oracle_proto::type_addr_ok(&addr), "{addr:?} is not a type address: domain.form.topic, lowercase parts joined by dots");
            let target = ox.get(&format!("/search?id={}", urlencode(&id)))?;
            let full = target["id"].as_str().context("the Oracle named no record")?.to_string();
            let room = target["channel"].as_str().context("the record names no room")?.to_string();
            let from = target["tags"].as_array().into_iter().flatten().filter_map(Value::as_str).find_map(|t| t.strip_prefix("type:")).unwrap_or("no type").to_string();
            let text = format!("Retype {full}: {from} -> {addr}");
            let v = ox.write(Kind::Say, &room, &text, None, vec![format!("retype:{full}"), format!("type:{addr}"), "intent:status".into(), "ask:none".into()])?;
            print_written(&ox, &v, &format!("retype {from} -> {addr}"));
        }
        Cmd::Card {
            room,
            id,
            status,
            title,
            batch,
            summary,
            acceptance,
            owner,
            group,
            impact,
            delivery,
            priority,
            evidence,
            depends,
            rooms,
            handoff,
            unblock,
            link,
            ..
        } => {
            let room = room.unwrap_or_default();
            let moves: Vec<board::Move> = if let Some(src) = batch {
                let raw = if src == "-" {
                    let mut b = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut b)?;
                    b
                } else {
                    std::fs::read_to_string(&src).with_context(|| format!("reading {src}"))?
                };
                serde_json::from_str(&raw).context("--batch is a JSON array of moves: [{\"id\":…,\"status\":…}]")?
            } else {
                let (Some(id), Some(status)) = (id, status) else {
                    bail!("name the card and its status: ox card <room> <id> <status> [title] — or --batch <file>");
                };
                vec![board::Move {
                    id, status, title, summary, acceptance, owner, group, impact, delivery, priority, evidence,
                    depends: (!depends.is_empty()).then_some(depends),
                    handoff, unblock, link,
                    rooms: rooms.map(|r| r.into_iter().filter(|x| !x.is_empty()).collect()),
                }]
            };
            let (face, v) = board::post_moves(&ox, &room, &moves)?;
            if ox.json {
                println!("{v}");
            } else {
                println!("signed:ok  {}  {face}", v["id"].as_str().unwrap_or("?"));
            }
        }
        Cmd::Working { what, room: Some(room) } => {
            let mv = board::Move { id: what.clone(), status: "in_progress".into(), owner: Some(ox.cfg.handle.clone()), ..Default::default() };
            let (face, v) = board::post_moves(&ox, &room, &[mv])?;
            print_written(&ox, &v, &face);
            let (face, v) = board::post_presence(&ox, board::Mode::Working { what: &what, room: Some(&room), card: Some(&what) }, now_ms())?;
            print_written(&ox, &v, &face);
        }
        Cmd::Working { what, room: None } => {
            let (face, v) = board::post_presence(&ox, board::Mode::Working { what: &what, room: None, card: None }, now_ms())?;
            print_written(&ox, &v, &face);
        }
        Cmd::Waiting { what } => {
            let (face, v) = board::post_presence(&ox, board::Mode::Waiting { what: what.as_deref() }, now_ms())?;
            print_written(&ox, &v, &face);
        }
        Cmd::Idle { why } => {
            let (face, v) = board::post_presence(&ox, board::Mode::Waiting { what: why.as_deref() }, now_ms())?;
            print_written(&ox, &v, &face);
        }
        Cmd::Take { room, card, note } => {
            let mv = board::Move { id: card.clone(), status: "in_progress".into(), owner: Some(ox.cfg.handle.clone()),
                summary: note, ..Default::default() };
            let (face, v) = board::post_moves(&ox, &room, &[mv])?;
            print_written(&ox, &v, &face);
            let (face, v) = board::post_presence(&ox, board::Mode::Working { what: &card, room: Some(&room), card: Some(&card) }, now_ms())?;
            print_written(&ox, &v, &face);
        }
        Cmd::Done { room, card, evidence, handoff } => {
            let mv = board::Move { id: card.clone(), status: "done".into(), evidence, handoff: Some(handoff), ..Default::default() };
            let (face, v) = board::post_moves(&ox, &room, &[mv])?;
            print_written(&ox, &v, &face);
            let (face, v) = board::post_presence(&ox, board::Mode::Waiting { what: Some(&format!("the next card (finished {card})")) }, now_ms())?;
            print_written(&ox, &v, &face);
        }
        Cmd::Move { room, id, what, note } => {
            let (face, v) = board::post_annotation(&ox, &room, &id, &board::move_sets(&what)?, &note, None, None)?;
            print_written(&ox, &v, &format!("{what} on {id}: {face}"));
        }
        Cmd::Claim { room, id, item, seat, note } => {
            let seat = seat.unwrap_or_else(|| ox.cfg.handle.clone());
            let v = ox.write(Kind::Say, &room, &note, None, item_reply_tags(&id, "owner", &item, &seat)?)?;
            print_written(&ox, &v, &format!("{item} on {id} is {seat}'s"));
        }
        Cmd::Vote { room, id, what, note } => {
            let (item, stance) = what.split_once('=').ok_or_else(|| anyhow!("--vote is Q1=for|against|abstain, not {what}"))?;
            anyhow::ensure!(matches!(stance, "for" | "against" | "abstain"), "a vote is for, against or abstain, not {stance}");
            let v = ox.write(Kind::Say, &room, &note, None, item_reply_tags(&id, "vote", item, stance)?)?;
            print_written(&ox, &v, &format!("{stance} on {item} of {id}"));
        }
        Cmd::Archive { room, id, note } => {
            let (face, tags, cargo) = board::archive(&room, &id, note.as_deref())?;
            let v = ox.write(Kind::Say, &room, &face, Some(cargo.to_string()), tags)?;
            print_written(&ox, &v, &format!("archived {id}"));
        }
        Cmd::Annotate { room, id, sets, why, confidence, cargo } => {
            if let Some(c) = &cargo {
                serde_json::from_str::<Value>(c).context("--cargo is JSON")?;
            }
            let (face, v) = board::post_annotation(&ox, &room, &id, &sets, &why, confidence, cargo)?;
            if ox.json {
                println!("{v}");
            } else {
                println!("signed:ok  {}  annotates {id}: {face}", v["id"].as_str().unwrap_or("?"));
            }
        }
        Cmd::Hallway { days } => {
            let v = ox.get(&format!("/hallway?days={days}"))?;
            if ox.json {
                println!("{v}");
            } else {
                print!("{}", oracle_mcp::hallway_text(&v, now_ms()));
            }
        }
        Cmd::File { op, args, name, ctype, out, limit } => match (op.as_str(), args.as_slice()) {
            ("put", [room, path]) => {
                let bytes = std::fs::read(path).with_context(|| format!("cannot read {path}"))?;
                let fname = name.or_else(|| Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()));
                let t = ctype.unwrap_or_else(|| guess_type(path).to_string());
                let mut q = format!("/files?room={}", urlencode(room));
                if let Some(n) = &fname {
                    q.push_str(&format!("&name={}", urlencode(n)));
                }
                let (body, _) = ox.bytes("PUT", &q, Some(&t), Some(&bytes))?;
                let v: Value = serde_json::from_slice(&body).context("the Oracle's answer was not JSON")?;
                if ox.json {
                    println!("{v}");
                } else {
                    println!(
                        "{}  {} bytes  {}  #{}{}",
                        v["id"].as_str().unwrap_or("?"),
                        v["size"],
                        v["content_type"].as_str().unwrap_or(""),
                        v["room"].as_str().unwrap_or(""),
                        if v["duplicate"] == true { "  (already stored)" } else { "" }
                    );
                    println!("cite it in a message with the tag  file:{}", v["sha256"].as_str().unwrap_or(""));
                }
            }
            ("get", [id]) => {
                let (body, _) = ox.bytes("GET", &format!("/files/{}", urlencode(id)), None, None)?;
                match out {
                    Some(p) => {
                        std::fs::write(&p, &body).with_context(|| format!("cannot write {}", p.display()))?;
                        eprintln!("{} bytes → {}", body.len(), p.display());
                    }
                    None => std::io::Write::write_all(&mut std::io::stdout(), &body)?,
                }
            }
            ("meta", [id]) => {
                let v = ox.get(&format!("/files/{}/meta", urlencode(id)))?;
                println!("{}", if ox.json { v.to_string() } else { serde_json::to_string_pretty(&v)? });
            }
            ("list", [room]) => {
                let v = ox.get(&format!("/files?room={}&limit={limit}", urlencode(room)))?;
                if ox.json {
                    println!("{v}");
                } else {
                    for f in v["files"].as_array().into_iter().flatten() {
                        let names: Vec<&str> = f["rooms"].as_array().into_iter().flatten().filter_map(|p| p["name"].as_str()).collect();
                        println!("{}  {:>9}  {:<24}  {}", f["id"].as_str().unwrap_or("?"), f["size"], f["content_type"].as_str().unwrap_or(""), names.join(", "));
                    }
                    println!("{} file(s) in #{room}", v["count"]);
                }
            }
            _ => bail!("ox file put <room> <path> | get <id> [--out path] | meta <id> | list <room>"),
        },
        Cmd::Graph { view, min, marks, top } => {
            let v = ox.get(&format!("/graph?view={}{}{}{}", oracle_mcp::enc(&view), min.map(|m| format!("&min={m}")).unwrap_or_default(), marks.map(|m| format!("&marks={}", oracle_mcp::enc(&m))).unwrap_or_default(), top.map(|t| format!("&top={t}")).unwrap_or_default()))?;
            if ox.json {
                println!("{v}");
            } else {
                print!("{}", oracle_mcp::graph_text(&v));
            }
        }
        Cmd::Mark { observation, quote, nth, labels, intents, note, links } => {
            let found = ox.get(&format!("/search?id={}", oracle_mcp::enc(&observation)))?;
            let content = oracle_mcp::marks::content_of(&found).ok_or_else(|| anyhow!("{observation}: no such observation, or not readable by this seat"))?;
            let m = oracle_mcp::marks::mark_message(&observation, &content, &quote, nth, &labels, &intents, note.as_deref(), &links, None)?;
            let v = ox.write(Kind::Say, oracle_mcp::marks::ROOM, &m.face, Some(m.cargo.to_string()), m.tags)?;
            print_written(&ox, &v, &format!("mark chars {}..{} of {observation}, held for Blaze's verdict", m.start, m.end));
            let id = v["id"].as_str().unwrap_or("?").to_owned();
            for (kind, to) in &m.links {
                let (face, cargo, tags) = oracle_mcp::marks::link_message(&id, kind, to);
                let l = ox.write(Kind::Say, oracle_mcp::marks::ROOM, &face, Some(cargo.to_string()), tags)?;
                print_written(&ox, &l, &format!("link {kind} -> {to}"));
            }
        }
        Cmd::Verdict { target, verdict, text } => {
            let (face, cargo, tags) = oracle_mcp::marks::verdict_message(&target, &verdict, text.as_deref(), None)?;
            let v = ox.write(Kind::Say, oracle_mcp::marks::ROOM, &face, Some(cargo.to_string()), tags)?;
            print_written(&ox, &v, &format!("verdict {verdict} on {target}"));
        }
        Cmd::Case { op, args, which, limit, case, msg, quote, nth, labels, intent, frame, state, method, lineage, links, supersedes, about, entities, intents, project } => {
            let a = json!({"which": which, "limit": limit, "n": args.first(), "case": case, "msg": msg, "quote": quote, "nth": nth, "labels": labels,
                           "intent": intent, "frame": frame, "state": state, "method": method, "lineage": lineage, "links": links, "supersedes": supersedes,
                           "about": about, "entities": entities, "intents": intents, "project": project});
            let (op, v) = match op.as_str() {
                "list" => ("cases", ox.get(&oracle_mcp::cases::path("cases", &a)?)?),
                "read" => ("case", ox.get(&oracle_mcp::cases::path("case", &a)?)?),
                "draft" => ("draft", ox.post("/cortex/drafts", &oracle_mcp::cases::draft(&a)?)?),
                _ => bail!("ox case list | read <n> | draft --case … --quote … --label … --intent … --frame … --state … --method … --lineage … --link …"),
            };
            if ox.json {
                println!("{v}");
            } else {
                print!("{}", oracle_mcp::cases::text(op, &v));
            }
        }
        Cmd::Cortex { op, args, statement, kind, status, sources, row, why, room, cid, q, pending, all } => {
            if matches!(op.as_str(), "nodes" | "node" | "entity" | "marks") {
                let mut a = json!({"op": op, "room": room, "kind": kind, "q": q, "source": sources.first()});
                if op == "node" {
                    a["nid"] = json!(args.first().ok_or_else(|| anyhow!("ox cortex node <nid>"))?);
                }
                let path = match op.as_str() {
                    "nodes" => {
                        let mut qs = vec![];
                        for k in ["room", "kind", "q"] {
                            if let Some(v) = a[k].as_str() {
                                qs.push(format!("{k}={}", oracle_mcp::enc(v)));
                            }
                        }
                        if all {
                            qs.push("all=1".into());
                        }
                        format!("/cortex?{}", qs.join("&"))
                    }
                    "node" => format!("/cortex/node/{}", oracle_mcp::enc(a["nid"].as_str().unwrap_or(""))),
                    "entity" => format!("/cortex/entity/{}", oracle_mcp::enc(args.first().ok_or_else(|| anyhow!("ox cortex entity <name>"))?)),
                    _ => {
                        let mut qs = vec![];
                        for k in ["room", "source"] {
                            if let Some(v) = a[k].as_str() {
                                qs.push(format!("{k}={}", oracle_mcp::enc(v)));
                            }
                        }
                        if pending {
                            qs.push("pending=1".into());
                        }
                        format!("/cortex/marks?{}", qs.join("&"))
                    }
                };
                let v = ox.get(&path)?;
                if ox.json {
                    println!("{v}");
                } else {
                    print!("{}", oracle_mcp::cortex_text(&op, &v));
                }
            } else if op == "list" {
                let mut q = vec![];
                for (k, v) in [("room", room), ("status", status), ("cid", cid)] {
                    if let Some(v) = v {
                        q.push(format!("{k}={}", oracle_mcp::enc(&v)));
                    }
                }
                let v = ox.get(&format!("/claims?{}", q.join("&")))?;
                if ox.json {
                    println!("{v}");
                } else {
                    print!("{}", oracle_mcp::claims_text(&v));
                }
            } else {
                let at = |i: usize| args.get(i).cloned();
                let in_room = at(0).ok_or_else(|| anyhow!("ox cortex {op} <room> …"))?;
                let a = match op.as_str() {
                    "mint" => json!({"statement": statement, "type": kind, "status": status, "sources": sources, "row": row}),
                    "edge" => json!({"from": at(1), "kind": at(2), "to": at(3)}),
                    "status" => json!({"cid": at(1), "status": at(2), "why": why}),
                    "source" => json!({"cid": at(1), "sources": sources}),
                    _ => bail!("op is mint, edge, status, source or list"),
                };
                let (face, tags, cargo) = oracle_mcp::verbs::claim(&op, &a)?;
                let v = ox.write(Kind::Say, &in_room, &face, Some(cargo.to_string()), tags)?;
                print_written(&ox, &v, &format!("claim {op}"));
            }
        }
        Cmd::React { room, id, kind, why, until, .. } => {
            let (face, v) = match (kind.as_str(), until) {
                ("later", Some(u)) => {
                    let now = now_ms();
                    let (face, tags) = oracle_mcp::verbs::later(&id, oracle_mcp::verbs::until_of(&u, now)?, now, why.as_deref())?;
                    let v = ox.write(Kind::Say, &room, &face, None, tags)?;
                    (face, v)
                }
                (_, Some(_)) => bail!("--until belongs to later"),
                _ => board::post_reaction(&ox, &room, &id, &kind, why.as_deref())?,
            };
            if ox.json {
                println!("{v}");
            } else {
                println!("signed:ok  {}  {face} on {id}", v["id"].as_str().unwrap_or("?"));
            }
        }
        Cmd::Status { room, text, stage, task, key, state, n, of, eta_ms, refs } => {
            // A stage of done or failed is the end of the run: the state follows unless one is given (2026-10-03: five
            // rows read stage done AND running for 18-21h because --state defaulted to running).
            let state = state.unwrap_or_else(|| if stage == "done" || stage == "failed" { stage.clone() } else { "running".into() });
            let s = board::Status { key, task, stage, state, n, of, eta_ms, refs };
            let (face, v) = board::post_status(&ox, &room, &text, &s, now_ms())?;
            if ox.json {
                println!("{v}");
            } else {
                println!("signed:ok  {}  {face}", v["id"].as_str().unwrap_or("?"));
            }
        }
        Cmd::Say {
            room,
            text,
            from_dot,
            to,
            tags,
            reply,
            intent,
            closes,
            supersedes,
            subject,
            rev,
            prev,
            verdict,
            archives,
            ttl,
            expires,
            ask,
            options,
            recommend,
            why,
            choice,
            by,
            refs,
            needs,
            verify,
            facets,
            items,
            cargo,
            dot,
            type_addr,
            valid_until,
        } => {
            let mut cargo = match cargo {
                Some(c) => {
                    let raw = match c.strip_prefix('@') {
                        Some(path) => std::fs::read_to_string(path)
                            .with_context(|| format!("reading cargo from {path}"))?,
                        None => c,
                    };
                    Some(serde_json::from_str::<Value>(&raw).context("--cargo must be JSON")?)
                }
                None => None,
            };
            // Typed needs and verify ride in cargo (Emmy, 2026-09-22): machine fields, not prose.
            if !needs.is_empty() || verify.is_some() {
                let c = cargo.get_or_insert_with(|| serde_json::json!({}));
                let obj = c
                    .as_object_mut()
                    .context("--needs/--verify need a JSON object cargo")?;
                if !needs.is_empty() {
                    obj.insert("needs".into(), serde_json::json!(needs));
                }
                if let Some(v) = verify {
                    obj.insert("verify".into(), Value::String(v));
                }
            }
            let is_reply = reply.is_some();
            let item_texts: Vec<Value> = items
                .iter()
                .filter_map(|it| board::item_tag(it, is_reply).ok())
                .filter_map(|(tag, text)| {
                    let (k, kind) = tag["item:".len()..].split_once('=')?;
                    text.map(|t| json!({"k": k, "kind": kind, "text": t}))
                })
                .collect();
            if !item_texts.is_empty() {
                let c = cargo.get_or_insert_with(|| serde_json::json!({}));
                c.as_object_mut().context("--item text needs a JSON object cargo")?.insert("items".into(), json!(item_texts));
            }
            if let Some(a) = &ask {
                anyhow::ensure!(
                    ["none", "read", "reply", "act", "decide", "review"].contains(&a.as_str()),
                    "--ask must be none, read, reply, act, decide or review"
                );
            }
            // Options ride in cargo (signed with the message); they belong to a decide-ask and nothing else.
            if !options.is_empty() {
                anyhow::ensure!(ask.as_deref() == Some("decide"), "--option is for --ask decide");
                let c = cargo.get_or_insert_with(|| serde_json::json!({}));
                let obj = c.as_object_mut().context("--option needs a JSON object cargo")?;
                anyhow::ensure!(!obj.contains_key("options"), "--cargo already has options; use --option or the cargo, not both");
                obj.insert("options".into(), json!(options));
            }
            if ask.as_deref() == Some("decide") {
                anyhow::ensure!(
                    (2..=6).contains(&options.len()) || cargo.as_ref().is_some_and(|c| c.get("options").is_some()),
                    "--ask decide needs 2 to 6 --option labels"
                );
            }
            // The asker's pick (docs/BRIDGE-CONTRACT.md): one verb for every message, so a decide from any room carries what the
            // Bridge needs to show it whole; Oracle refuses a decide without it.
            let mut text = text;
            if ask.as_deref() == Some("decide") {
                let c = cargo.get_or_insert_with(|| json!({}));
                let words = if c.get("recommendation").is_none() {
                    let n = recommend.context("--ask decide needs --recommend <n>: the option you would pick, numbered from 1")?;
                    oracle_mcp::add_recommendation(c, n, why.as_deref().unwrap_or(""))?
                } else {
                    anyhow::ensure!(recommend.is_none() && why.is_none(), "--cargo already has a recommendation; use --recommend/--why or the cargo, not both");
                    oracle_mcp::recommendation_words(c).context("cargo.recommendation must name one of the options and say why")?
                };
                text = oracle_mcp::with_choice_words(&text, &words);
            } else {
                anyhow::ensure!(recommend.is_none() && why.is_none(), "--recommend and --why belong to --ask decide");
            }
            if let Some(n) = choice {
                anyhow::ensure!(reply.is_some(), "--choice {n} picks an option of the message you reply to: add --reply <id>");
            }
            let cargo = cargo.map(|c| c.to_string());
            let by_ms = match by {
                Some(b) => Some(oracle_mcp::parse_by(&b)?),
                None => None,
            };
            // A record's code names it alone (Blaze, 31 Oct 2026); the signed tag always carries the full id.
            let reply = reply.map(|id| ox.full_id(&id)).transpose()?;
            let closes = closes.map(|id| ox.full_id(&id)).transpose()?;
            let supersedes = supersedes.map(|id| ox.full_id(&id)).transpose()?;
            let archives = archives.map(|id| ox.full_id(&id)).transpose()?;
            if let Some(t) = &type_addr {
                anyhow::ensure!(oracle_proto::type_addr_ok(t), "--type {t:?} is not a type address: domain.form.topic, lowercase parts joined by dots");
            }
            board::say_refuses_on(&tags, reply.is_some())?;
            let mut tags = tags;
            tags.extend(to.into_iter().map(|h| format!("to:{h}")));
            tags.extend(reply.map(|id| format!("reply:{id}")));
            tags.extend(intent.map(|i| format!("intent:{i}")));
            tags.extend(closes.map(|id| format!("closes:{id}")));
            tags.extend(supersedes.map(|id| format!("supersedes:{id}")));
            tags.extend(task_tags(subject, rev, prev, verdict)?);
            tags.extend(archives.map(|id| format!("archives:{id}")));
            tags.extend(ttl.map(|h| format!("ttl:{h}")));
            if let Some(e) = expires {
                tags.push(format!("expires:{}", oracle_mcp::expires_ms(&e, now_ms())?));
            }
            tags.extend(ask.map(|a| format!("ask:{a}")));
            tags.extend(choice.map(|n| format!("choice:{n}")));
            tags.extend(by_ms.map(|b| format!("by:{b}")));
            tags.extend(valid_until.as_deref().map(valid_until_tag).transpose()?);
            tags.extend(refs.into_iter().map(|r| format!("ref:{r}")));
            tags.extend(facets.into_iter().map(|f| format!("facet:{f}")));
            tags.extend(type_addr.map(|t| format!("type:{t}")));
            for it in &items {
                tags.push(board::item_tag(it, is_reply)?.0);
            }
            let tags = oracle_mcp::resolve_recipients(&ox, tags)?;
            let mut tags = tags;
            let text = match &from_dot {
                Some(path) => {
                    let (body, dot_tags) = ox.import_dot(&room, path)?;
                    tags.extend(dot_tags);
                    if text.trim().is_empty() { body } else { format!("{}\n\n{body}", text.trim()) }
                }
                None if text.trim().is_empty() => bail!("say needs text (or --from-dot <picture>)"),
                None => text,
            };
            if dot {
                let parent = tags.iter().find_map(|t| t.strip_prefix("reply:")).map(str::to_string);
                tags.extend(ox.mint_dot(&room, &text, parent.as_deref())?);
            }
            let v = ox.write(Kind::Say, &room, &text, cargo, tags)?;
            if ox.json {
                println!("{v}");
            } else {
                println!("signed:ok  {}", v["id"].as_str().unwrap_or("?"));
            }
        }
        Cmd::State { full: true, sets, .. } if !sets.is_empty() => bail!("--full reads; a revision is --set without it"),
        Cmd::State { room, full: true, rounds, as_of, .. } => {
            if as_of.is_some() {
                bail!("--full reads the room now; --as-of-seq reads the State alone at a cursor");
            }
            twin(&ox, "oracle_room_state_full", json!({"room": room, "rounds": rounds}))?;
        }
        Cmd::State { room, sets, note, adopts, as_of, .. } if !sets.is_empty() => {
            if as_of.is_some() {
                bail!("a State at a cursor is read only: write against the current head");
            }
            let st = ox.room_state(&room)?;
            let heads: Vec<String> = st["heads"].as_array().into_iter().flatten().filter_map(|h| h.as_str().map(str::to_owned)).collect();
            if st["state"] == "incomplete" {
                bail!("#{room} State could not be read completely (history walk cut short); refusing to write on a partial chain");
            }
            let mut rev = board::state_revision(&room, &heads, &st["fields"], &sets)?;
            if let Some(p) = adopts.as_deref() {
                rev = oracle_mcp::with_adoption(rev, oracle_mcp::adoption(&ox, &room, p)?);
            }
            let face = note.unwrap_or_else(|| format!("State updated: {}", one_line(rev["fields"]["current"].as_str().unwrap_or(""), 120)));
            let v = ox.write(Kind::Say, &room, &face, Some(rev.to_string()), vec!["room-context:v1".into(), "intent:status".into()])?;
            print_written(&ox, &v, &format!("#{room} State ← {}", heads.join(", ")));
        }
        Cmd::State { room, adopts, as_of, .. } => {
            if adopts.is_some() {
                bail!("adopts says why a revision exists: say what it changes too (--set current=\"...\")");
            }
            let mut v = ox.room_state_at(&room, as_of)?;
            v["orientation"] = oracle_mcp::state::orientation(&v);
            let heads: Vec<&str> = v["heads"].as_array().map(|a| a.iter().filter_map(|h| h.as_str()).collect()).unwrap_or_default();
            if ox.json {
                println!("{v}");
            } else if let Some(bad) = v["bad"].as_array().filter(|b| !b.is_empty()) {
                println!("#{room} · State DEGRADED: {} revision line(s) do not verify — do not rely on it", bad.len());
                for b in bad {
                    println!("  {}  {}", b["id"].as_str().unwrap_or("?"), b["why"].as_str().unwrap_or(""));
                }
            } else if heads.is_empty() {
                println!("#{room} · no signed State yet");
            } else if heads.len() > 1 {
                println!("#{room} · State DEGRADED: {} concurrent heads ({}) — reconcile in the State tab before relying on it",
                    heads.len(), heads.join(", "));
            } else {
                println!("#{room} · State {} · {} revision(s)", heads[0], v["revisions"]);
                if let Some(sk) = v["skipped"].as_array().filter(|s| !s.is_empty()) {
                    let ids: Vec<&str> = sk.iter().filter_map(|x| x["id"].as_str()).collect();
                    println!("            (not revisions, skipped: {})", ids.join(", "));
                }
                // A public room's stranger-first fields come first (docs/PUBLIC-STATE.md), the working State under them.
                for key in oracle_mcp::PUBLIC_FIELDS {
                    let text = v["fields"][key].as_str().unwrap_or("").trim();
                    if !text.is_empty() {
                        let at = v["fields"][oracle_mcp::LATEST_AT].as_str().filter(|_| key == "latest").map(|d| format!("({d}) ")).unwrap_or_default();
                        println!("{:<12}{at}{text}", key.to_uppercase());
                    }
                }
                if let Some(days) = oracle_mcp::latest_stale_days(&v["fields"], oracle_mcp::now_ms()) {
                    println!("            LATEST is {days} days old: write what changed (ox state {room} --set latest=\"...\")");
                }
                for key in ["purpose", "goal", "current", "scope", "constraints", "questions", "next", "acceptance", "continued_in", "continued_from"] {
                    let text = v["fields"][key].as_str().unwrap_or("").trim();
                    if !text.is_empty() {
                        println!("{:<12}{}", key.to_uppercase(), text);
                    }
                }
            }
            if v["complete"] == false && !ox.json {
                println!("(200+ revisions: the view may be incomplete)");
            }
            if !ox.json {
                print!("{}", oracle_mcp::state::orientation_text(&v["orientation"]));
            }
        }
        Cmd::Read {
            room,
            limit,
            full,
            state,
            keep,
            before,
            starts,
            tag,
            ..
        } => {
            let room = room.unwrap_or_default();
            let v = ox.get(&read_path(&room, limit, &state, &keep, before, starts.as_deref(), tag.as_deref()))?;
            if ox.json {
                println!("{v}");
            } else {
                let mut items = v["items"].as_array().cloned().unwrap_or_default();
                items.reverse();
                let scanned = v["scanned"].as_i64().unwrap_or(items.len() as i64);
                if state == "all" {
                    println!("#{room} — {} recent", items.len());
                } else {
                    println!(
                        "#{room} — {} shown of {scanned} read · state={state}",
                        items.len()
                    );
                }
                show(&items, full);
                if let Some(nb) = v["next_before"].as_i64() {
                    println!("older: ox read {room} --before {nb}");
                }
                // Reading the room is presence: the seat's mark moves to what it was shown. A page of the past or a
                // filtered page is not the room, so it moves nothing.
                if before.is_none() && starts.is_none() && tag.is_none() && !board::peek(false) {
                    let top = items.iter().filter_map(|o| o["seq"].as_i64()).max().unwrap_or(0);
                    board::ack_to(&ox, &room, top)?;
                }
            }
        }
        Cmd::Asks { mine: true, .. } => {
            let v = ox.get("/asks?state=open")?;
            let m = oracle_mcp::my_asks(&v, v["now_ms"].as_i64().unwrap_or_else(now_ms));
            if ox.json {
                println!("{m}");
            } else {
                let t = oracle_mcp::my_asks_text(&m, None);
                print!("{}", if t.is_empty() { "MY ASKS    none: every ask you opened has ended\n".to_string() } else { t });
            }
        }
        Cmd::Asks { seat, room, state, .. } => {
            let mut q = format!("/asks?state={}", urlencode(&state));
            if let Some(s) = &seat {
                q.push_str(&format!("&seat={}", urlencode(s)));
            }
            if let Some(r) = &room {
                q.push_str(&format!("&room={}", urlencode(r)));
            }
            let v = ox.get(&q)?;
            if ox.json {
                println!("{v}");
            } else {
                println!("{}", asks_text(&v, state.as_str()));
            }
        }
        Cmd::Cards { mine, waiting, room, owner, status, limit } => {
            let me = ox.cfg.handle.clone();
            let mut q = vec![];
            let owner = if mine { Some(me.clone()) } else { owner };
            if let Some(o) = &owner { q.push(format!("owner={}", urlencode(o))); }
            if waiting { q.push(format!("waiting_on={}", urlencode(&me))); }
            if let Some(r) = &room { q.push(format!("room={}", urlencode(r))); }
            // Everything but done unless asked: a board of finished work is the exception.
            let status = status.or_else(|| (!waiting).then(|| "open".to_string()));
            if let Some(s) = &status { q.push(format!("status={}", urlencode(s))); }
            if let Some(l) = limit { q.push(format!("limit={l}")); }
            let v = ox.get(&format!("/cards?{}", q.join("&")))?;
            if ox.json {
                println!("{v}");
            } else {
                let n = v["cards"].as_array().map_or(0, |a| a.len());
                let rooms = v["rooms"].as_array().map_or(0, |a| a.len());
                println!("CARDS · {n}{}", if room.is_none() { format!(" across {rooms} room(s)") } else { String::new() });
                print!("{}", cards_text(&v, owner.is_none() || waiting));
                if n == 0 { println!("  none"); }
                if let Some(bad) = v["invalid"].as_array().filter(|b| !b.is_empty()) {
                    println!("REFUSED · {} update(s) the board would not take, awaiting correction", bad.len());
                    for b in bad.iter().take(10) {
                        println!("  #{:<8} {}  {}", b["room"].as_str().unwrap_or("?"), b["task"].as_str().unwrap_or("?"), one_line(b["reason"].as_str().unwrap_or(""), 90));
                    }
                }
            }
        }
        Cmd::Digest { room, roots, agent: true, .. } => twin(&ox, "oracle_room_digest_agent", json!({"room": room.unwrap_or_default(), "roots": roots}))?,
        Cmd::Digest { room, roots, .. } => {
            let room = room.unwrap_or_default();
            let d = ox.get(&format!(
                "/threads?view=digest&channel={}&roots={}",
                urlencode(&room),
                urlencode(&roots)
            ))?;
            if ox.json {
                println!("{d}");
            } else {
                println!("{}", oracle_mcp::digest_text(&d));
            }
        }
        Cmd::Inbox {
            ack,
            older,
            mine,
            limit,
            full,
            ..
        } if !full => {
            // The typed inbox (oracle-mcp inbox_value): the Oracle clock, what is addressed to me whole, the rest counted
            // per room. --full keeps the old listing of every message.
            let mut v = oracle_mcp::inbox_value(&ox, limit, if ack { oracle_mcp::Ack::Page } else { oracle_mcp::Ack::No }, older)?;
            if mine {
                v["rooms"] = json!([]);
            }
            if ox.json {
                println!("{v}");
            } else {
                print!("{}", oracle_mcp::inbox_render(&v));
            }
        }
        Cmd::Inbox {
            ack,
            mine,
            limit,
            ..
        } => {
            let v = ox.get(&format!("/fold?limit={limit}"))?;
            let items: Vec<Value> = v["items"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|i| !mine || i["for_me"].as_bool() == Some(true))
                .collect();
            if ox.json {
                println!("{v}");
            } else {
                let waiting: i64 = v["pending"]
                    .as_object()
                    .map(|m| m.values().filter_map(|x| x.as_i64()).sum())
                    .unwrap_or(0);
                println!(
                    "{} new  ({} shown, {} addressed to you)",
                    waiting,
                    items.len(),
                    items
                        .iter()
                        .filter(|i| i["for_me"].as_bool() == Some(true))
                        .count()
                );
                show(&items, true);
                if v["has_more"].as_bool() == Some(true) {
                    println!("… more waiting — `ox inbox --ack` then again");
                }
            }
            if ack {
                ox.post("/fold", &json!({"marks": v["next_marks"]}))?;
                if !ox.json {
                    println!("marked read");
                }
            }
        }
        Cmd::Ack { rooms, all } => {
            // `ox ack` alone used to mark every room read (163 rooms, 2026-10-20, by accident): say which, or --all.
            if !ack_scope(&rooms, all) {
                anyhow::bail!("ox ack: name the rooms to mark read, or --all for every room");
            }
            if all {
                let v = ox.post("/fold", &json!({"all": true}))?;
                println!("marked read: {} rooms", v["channels"].as_u64().unwrap_or(0));
            } else {
                // Only the named rooms: take their marks from a full fold.
                let v = ox.get("/fold?limit=200")?;
                let marks: serde_json::Map<String, Value> = v["next_marks"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(k, _)| rooms.contains(k))
                    .collect();
                ox.post("/fold", &json!({"marks": marks}))?;
                println!("marked read: {}", rooms.join(", "));
            }
        }
        Cmd::Seen { targets, .. } => {
            anyhow::ensure!(!targets.is_empty(), "ox seen: a message id, or rooms to mark read, or --all");
            for id in &targets {
                let (room, seq) = ox.seen(id)?;
                if ox.json {
                    println!(
                        "{}",
                        json!({"ok": true, "seat": ox.cfg.handle, "channel": room, "id": id, "seq": seq})
                    );
                } else {
                    println!("seen  #{room} up to {id} (seq {seq})");
                }
            }
        }
        Cmd::Brief { place: Some(w), .. } if w.trim() != "me" => {
            if oracle_mcp::brief_where(&json!({"where": w}))? == "world" {
                let v = oracle_mcp::world_value(&ox)?;
                if ox.json {
                    println!("{v}");
                } else {
                    print!("{}", oracle_mcp::world_render(&v));
                }
            }
        }
        Cmd::Brief { full, hook: Some(h), .. } if h == "cursor" => {
            // Cursor's sessionStart reads JSON; a brief that cannot be read gives the last one kept, or nothing.
            let text = brief_with(&ox, full)
                .or_else(|_| std::fs::read_to_string(ox.home.join("brief.cache")).map(|c| c.lines().skip(1).collect::<Vec<_>>().join("\n")))
                .unwrap_or_default();
            println!("{}", json!({"additional_context": text}));
        }
        Cmd::Brief { full, .. } => brief(&ox, full)?,
        Cmd::Capture { hook, drain, file, agent, room, dry_run } => capture_cmd(&ox, hook, drain, file, &agent, &room, dry_run)?,
        Cmd::Reflect { room, except, limit, batches, dry_run } => {
            for r in &room {
                reflect_cmd(&ox, r, &except, limit.max(2), batches, dry_run)?;
            }
        }
        // The same object and page as the MCP tool oracle_delta (oracle-mcp delta_value / delta_render).
        Cmd::Delta { since } => {
            let v = oracle_mcp::delta_value(&ox, since.as_deref())?;
            if ox.json {
                println!("{v}");
            } else {
                print!("{}", oracle_mcp::delta_render(&v));
            }
        }
        Cmd::Close { id, answered_in, why, done, drop, superseded_by, batch } => {
            let entries: Vec<close::Entry> = match (batch, id) {
                (Some(path), None) => serde_json::from_str(&std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?)
                    .context("the batch is a JSON array of {id, end, answered_in?, why?, by?}")?,
                (None, Some(id)) => {
                    let end = match (done, &drop, &superseded_by) {
                        (false, None, None) => "closed",
                        (true, None, None) => "done",
                        (false, Some(_), None) => "dropped",
                        (false, None, Some(_)) => "superseded",
                        _ => bail!("one end only: --done, --drop or --superseded-by (or none, for closed)"),
                    };
                    vec![close::Entry { id, end: end.into(), answered_in, why: drop.or(why), by: superseded_by }]
                }
                _ => bail!("ox close <id> … or ox close --batch <file.json>, not both"),
            };
            let asks = ox.get("/asks?state=open")?;
            let planned = match close::plan(&entries, &asks) {
                Ok(p) => p,
                Err(bad) => bail!("nothing written; {} entr{} refused:\n  {}", bad.len(), if bad.len() == 1 { "y" } else { "ies" }, bad.join("\n  ")),
            };
            let many = entries.len() > 1;
            let mut written: Vec<(close::Planned, String)> = vec![];
            for p in planned {
                let v = ox.write(Kind::Say, &p.room, &p.text, None, p.tags.clone())?;
                let rec = v["id"].as_str().unwrap_or("?").to_string();
                if !many {
                    println!("signed:ok  {rec}  closes {} ({})", p.id, p.end);
                }
                written.push((p, rec));
            }
            if many {
                for (room, text, cargo, tags) in close::notices(&written) {
                    let v = ox.write(Kind::Say, &room, &text, Some(cargo.to_string()), tags)?;
                    println!("signed:ok  {}  #{room} notice", v["id"].as_str().unwrap_or("?"));
                }
                println!("closed {} ask(s), one record each", written.len());
            }
        }
        Cmd::Now { seat, sets, clear, .. } => {
            let json = ox.json;
            let me = ox.cfg.handle.clone();
            let who = seat.unwrap_or_else(|| me.clone());
            let prev = oracle_mcp::last_now(&ox, &who);
            if sets.is_empty() && clear.is_empty() {
                if json {
                    println!("{}", serde_json::to_string_pretty(&prev.unwrap_or(Value::Null))?);
                } else {
                    print!("{}", oracle_mcp::now_render(&prev.unwrap_or(Value::Null), now_ms(), &who));
                }
            } else {
                if who != me {
                    bail!("a seat writes only its own Now head");
                }
                let next = oracle_mcp::now_merge(prev.as_ref().map(|p| &p["now"]), &sets, &clear, now_ms())?;
                let line = format!("NOW · {}{}", next["task"].as_str().unwrap_or("(no task)"),
                    next["next"].as_str().map(|n| format!(" · next: {n}")).unwrap_or_default());
                let mut tags = vec!["now".to_string(), "intent:status".to_string()];
                if let Some(id) = prev.as_ref().and_then(|p| p["id"].as_str()) {
                    tags.push(format!("supersedes:{id}"));
                }
                let v = ox.write(Kind::Say, &me, &line, Some(next.to_string()), tags)?;
                println!("now written  {}", v["id"].as_str().unwrap_or("?"));
            }
        }
        Cmd::Identity { seat, write } => {
            let me = ox.cfg.handle.clone();
            match write {
                None => {
                    let who = seat.unwrap_or_else(|| me.clone());
                    let i = oracle_mcp::last_identity(&ox, &who).unwrap_or(Value::Null);
                    if ox.json { println!("{}", serde_json::to_string_pretty(&i)?); } else { print!("{}", oracle_mcp::identity_render(&i, now_ms(), &who)); }
                }
                Some(path) => {
                    if seat.as_deref().is_some_and(|s| s != me) {
                        bail!("a seat writes only its own identity head");
                    }
                    let mut h: Value = serde_json::from_str(&std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?)
                        .with_context(|| format!("{} is not JSON", path.display()))?;
                    h["schema"] = json!("identity-head/1");
                    h["seat"] = json!(me);
                    for k in ["purpose", "mission", "persona_ref"] {
                        if h[k].as_str().is_none_or(|v| v.trim().is_empty()) {
                            bail!("an identity head needs {k} (docs/IDENTITY.md)");
                        }
                    }
                    let pid = h["persona_ref"].as_str().unwrap_or("").to_string();
                    let p = ox.get(&format!("/search?id={}", urlencode(&pid)))?;
                    let text = p["content"].as_str().ok_or_else(|| anyhow!("the persona record {pid} has no text"))?.to_string();
                    let tagged = p["tags"].as_array().is_some_and(|t| t.iter().any(|t| t.as_str().is_some_and(|t| t.starts_with("persona:"))));
                    if p["author"].as_str() != Some(me.as_str()) || p["channel"].as_str() != Some(me.as_str()) || p["envelope"].as_i64() != Some(2) || !tagged {
                        bail!("the persona record {pid} must be a server-verified persona:vN message of {me} in #{me}");
                    }
                    // The verified persona travels inside the head: a later change to the source shows as STALE, never as new text.
                    h["persona_sha256"] = json!(oracle_proto::sha_hex(text.as_bytes()));
                    h["persona_text"] = json!(text);
                    // Each cited source is checked now and labelled: owner-signed (Blaze, verified), quoted by me, or by another seat.
                    let owner = "blaze";
                    let classify = |id: &str| -> Result<Value> {
                        let r = ox.get(&format!("/search?id={}", urlencode(id)))?;
                        let by = match (r["author"].as_str(), r["envelope"].as_i64()) {
                            (Some(a), Some(2)) if a == owner => "owner".to_string(),
                            (Some(a), _) if a == me => "self".to_string(),
                            (Some(a), _) => a.to_string(),
                            _ => bail!("cited source {id} could not be read"),
                        };
                        Ok(json!({"id": id, "by": by}))
                    };
                    let mut refs = json!({});
                    for (k, ids) in oracle_mcp::identity_ref_ids(&h["refs"]).map_err(|e| anyhow!(e))? {
                        let mut out = vec![];
                        for id in &ids { out.push(classify(id)?); }
                        refs[&k] = json!(out);
                    }
                    h["refs"] = refs;
                    // A crew entry is verified only by its seat's own State head; anything else is unverified.
                    if let Some(crew) = h["crew"].as_array_mut() {
                        for c in crew.iter_mut() {
                            let handle = c["handle"].as_str().unwrap_or("").to_string();
                            c["source"] = match c["source"].as_str() {
                                Some(id) => {
                                    let r = ox.get(&format!("/search?id={}", urlencode(id)))?;
                                    oracle_mcp::crew_source_ok(&r, &handle).map_err(|e| anyhow!("crew {handle}: source {id} is {e}"))?;
                                    json!({"id": id, "by": "state"})
                                }
                                None => json!({"by": "unverified"}),
                            };
                        }
                    }
                    let over = oracle_mcp::identity_bounds(&h);
                    if !over.is_empty() { bail!("the head is over its bounds: {}", over.join("; ")); }
                    h["at"] = json!(now_ms());
                    let mut tags = vec!["identity".to_string(), "intent:status".to_string()];
                    if let Some(prev) = oracle_mcp::last_identity(&ox, &me).and_then(|i| i["id"].as_str().map(str::to_owned)) {
                        tags.push(format!("supersedes:{prev}"));
                    }
                    let line = format!("IDENTITY · {} · {}", h["name"].as_str().unwrap_or(&me), h["purpose"].as_str().unwrap_or(""));
                    let v = ox.write(Kind::Say, &me, &line, Some(h.to_string()), tags)?;
                    println!("identity written  {}", v["id"].as_str().unwrap_or("?"));
                }
            }
        }
        Cmd::Procedure { name, write } => {
            let me = ox.cfg.handle.clone();
            match (name, write) {
                (_, Some(path)) => {
                    let mut p: Value = serde_json::from_str(&std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?)
                        .with_context(|| format!("{} is not JSON", path.display()))?;
                    p["schema"] = json!("procedure/1");
                    let name = p["name"].as_str().unwrap_or("").to_string();
                    let prev = oracle_mcp::procedure_get(&ox, &name).ok().flatten();
                    p["version"] = json!(prev.as_ref().and_then(|x| x["procedure"]["version"].as_i64()).unwrap_or(0) + 1);
                    let problems = oracle_mcp::procedure_problems(&p);
                    if !problems.is_empty() { bail!("not a procedure: {}", problems.join("; ")); }
                    let mut tags = vec!["procedure".to_string(), format!("procedure:{name}"), "intent:note".to_string()];
                    if let Some(id) = prev.as_ref().and_then(|x| x["id"].as_str()) { tags.push(format!("supersedes:{id}")); }
                    let line = format!("PROCEDURE {name} v{} · {}", p["version"], p["title"].as_str().unwrap_or(""));
                    let v = ox.write(Kind::Say, &me, &line, Some(p.to_string()), tags)?;
                    println!("procedure written  {name} v{}  {}", p["version"], v["id"].as_str().unwrap_or("?"));
                }
                (Some(n), None) => match oracle_mcp::procedure_get(&ox, &n)? {
                    Some(p) => if ox.json { println!("{}", serde_json::to_string_pretty(&p)?) } else { print!("{}", oracle_mcp::procedure_render(&p, now_ms())) },
                    None => bail!("no procedure named {n} on record (ox procedure lists them)"),
                },
                (None, None) => {
                    let l = oracle_mcp::procedure_list(&ox)?;
                    if ox.json { println!("{}", serde_json::to_string_pretty(&l)?); }
                    else if l.is_empty() { println!("no procedures on record yet (docs/PROCEDURES.md)"); }
                    else {
                        for p in &l {
                            println!("{:<24} v{:<3} {:<12} {:<10} {}", p["name"].as_str().unwrap_or("?"), p["version"].as_i64().unwrap_or(1), p["author"].as_str().unwrap_or("?"),
                                oracle_mcp::ago(now_ms(), p["created_ms"].as_i64().unwrap_or(0)), p["title"].as_str().unwrap_or(""));
                        }
                    }
                }
            }
        }
        Cmd::Chain { seat, out, check: None } => {
            let who = seat.unwrap_or_else(|| ox.cfg.handle.clone());
            let (mut recs, mut after) = (Vec::new(), 0i64);
            loop {
                let page = ox.get(&format!("/chain/{}?after={after}&limit=2000", urlencode(&who)))?;
                recs.extend(serde_json::from_value::<Vec<oracle_proto::ChainRecord>>(page["records"].clone())?);
                match page["next"].as_i64() {
                    Some(n) => after = n,
                    None => break,
                }
            }
            if let Some(path) = out {
                let face: String = recs.iter().map(|r| serde_json::to_string(r).map(|l| l + "\n")).collect::<std::result::Result<_, _>>()?;
                write_durable(&path, &face)?;
                if !ox.json {
                    println!("wrote {} records to {} (check it anywhere: ox chain --check {})", recs.len(), path.display(), path.display());
                }
            }
            chain_report(&oracle_proto::check_chain(&recs), ox.json)?;
        }
        Cmd::Canon { seat, kind, write } => {
            let me = ox.cfg.handle.clone();
            if let Some(k) = kind.as_deref() {
                if !oracle_mcp::CANON_KINDS.contains(&k) { bail!("--kind must be one of {}", oracle_mcp::CANON_KINDS.join(", ")); }
            }
            match write {
                Some(path) => {
                    let mut c: Value = serde_json::from_str(&std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?)
                        .with_context(|| format!("{} is not JSON", path.display()))?;
                    c["schema"] = json!("canon/1");
                    let prev = oracle_mcp::last_canon(&ox, &me).filter(|p| p["canon"].is_object());
                    c["version"] = json!(prev.as_ref().and_then(|p| p["canon"]["version"].as_i64()).unwrap_or(0) + 1);
                    let problems = oracle_mcp::canon_problems(&c);
                    if !problems.is_empty() { bail!("not a canon: {}", problems.join("; ")); }
                    // Every cited record must exist: a canon never points at nothing.
                    let missing: Vec<String> = oracle_mcp::canon_cited_ids(&c).into_iter()
                        .filter(|id| ox.get(&format!("/search?id={}", urlencode(id))).ok().is_none_or(|r| r["id"].as_str() != Some(id.as_str())))
                        .collect();
                    if !missing.is_empty() { bail!("not a canon: {} cited ids do not resolve: {}", missing.len(), missing.join(", ")); }
                    let mut tags = vec!["canon".to_string(), "intent:note".to_string()];
                    if let Some(id) = prev.as_ref().and_then(|p| p["id"].as_str()) { tags.push(format!("supersedes:{id}")); }
                    let n = c["rules"].as_array().map_or(0, |r| r.len());
                    let line = format!("CANON v{} · {n} rules (docs/CANON.md; ox canon prints it whole)", c["version"]);
                    let v = ox.write(Kind::Say, &me, &line, Some(c.to_string()), tags)?;
                    println!("canon written  v{}  {n} rules  {}", c["version"], v["id"].as_str().unwrap_or("?"));
                }
                None => {
                    let who = seat.unwrap_or(me);
                    match oracle_mcp::last_canon(&ox, &who) {
                        Some(p) if p["canon"].is_object() => {
                            if ox.json { println!("{}", serde_json::to_string_pretty(&p)?); }
                            else {
                                for pr in p["problems"].as_array().into_iter().flatten().filter_map(Value::as_str) { eprintln!("note: {pr}"); }
                                print!("{}", oracle_mcp::canon_render(&p, kind.as_deref(), now_ms()));
                            }
                        }
                        Some(p) => bail!("no readable canon for {who}: {}", p["problems"].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")),
                        None => bail!("{who} has no canon on record (docs/CANON.md)"),
                    }
                }
            }
        }
        Cmd::Remember { text, kind, room, valid_until } => {
            const KINDS: [&str; 4] = ["decision", "lesson", "preference", "commitment"];
            if !KINDS.contains(&kind.as_str()) {
                bail!("--kind must be one of {KINDS:?}. Status lines and dashboards are not knowledge.");
            }
            let mut tags = vec![format!("memory:{kind}")];
            tags.extend(valid_until.as_deref().map(valid_until_tag).transpose()?);
            let v = ox.write(
                Kind::Memory,
                &room,
                &text,
                None,
                tags,
            )?;
            println!("remembered ({kind})  {}", v["id"].as_str().unwrap_or("?"));
        }
        Cmd::Recall { query, limit, room, include_archived, as_of, .. } => {
            let query = query.unwrap_or_default();
            if let Some(a) = as_of.as_deref() {
                anyhow::ensure!(oracle_proto::parse_when(a).is_some(), "--as-of {a:?}: a date (2026-10-01), a date and time (2026-10-01T15:30) or unix ms");
            }
            let chan = format!("{}{}", room.as_deref().map(|r| format!("&channel={}", urlencode(r))).unwrap_or_default(),
                as_of.as_deref().map(|a| format!("&as_of={}", urlencode(a))).unwrap_or_default());
            // The hits and the state heads are two requests that do not depend on each other, and each subject's
            // revisions are one more: run them at once, not one after another. Measured 25 Oct 2026 (tools/use-bench):
            // each request is ~1 s from a seat's machine through the edge, so five in a row made recall 7-9 s and
            // the recall hook hit its 9 s cap on every prompt.
            let hits_path = format!("/search?q={}&limit={limit}{chan}{}", urlencode(&query), if include_archived { "&include_archived=true" } else { "" });
            let heads_path = format!("/search?q={}&kind=state&limit=20{chan}", urlencode(&query));
            let (v, heads) = std::thread::scope(|s| {
                let o = ox.clone();
                let h = s.spawn(move || o.get(&heads_path).unwrap_or(Value::Null));
                let v = ox.get(&hits_path);
                (v, h.join().unwrap_or(Value::Null))
            });
            let v = v?;
            // What a thing IS now outranks what was said about it (2026-09-30: recall showed five messages about
            // P1 and never the record that retired it). Subjects whose state records match come first, each as
            // its current head.
            let mut subjects: Vec<String> = Vec::new();
            for h in heads["hits"].as_array().cloned().unwrap_or_default() {
                // A head is shown only when it holds three in five of the query's words (the server counts them, `words`):
                // on 31 Oct "own node private VPS your own model SSH" printed the heads entity:emmy-core and dot.goal.
                if let Some([held, of]) = h["words"].as_array().and_then(|w| Some([w.first()?.as_u64()?, w.get(1)?.as_u64()?])) {
                    if held < (of * 3).div_ceil(5).max(1) {
                        continue;
                    }
                }
                if let Some(sub) = subject_of(&h) {
                    if !subjects.contains(&sub) && subjects.len() < 3 {
                        subjects.push(sub);
                    }
                }
            }
            let now: Vec<(String, Vec<Value>)> = std::thread::scope(|s| {
                let jobs: Vec<_> = subjects
                    .into_iter()
                    .map(|sub| {
                        let path = format!("/search?tag={}&limit=50", urlencode(&format!("subject:{sub}")));
                        let o = ox.clone();
                        (sub, s.spawn(move || o.get(&path).ok()))
                    })
                    .collect();
                jobs.into_iter()
                    .filter_map(|(sub, j)| {
                        let revs = head_revisions(&j.join().ok()??);
                        (!revs.is_empty()).then_some((sub, revs))
                    })
                    .collect()
            });
            if ox.json {
                let states: Vec<Value> = now.iter().map(|(s, r)| json!({"subject": s, "current": r.last()})).collect();
                let mut out = v.clone();
                out["states"] = json!(states);
                println!("{out}");
            } else {
                for (sub, revs) in &now {
                    println!("{}", state_line(sub, revs));
                }
                if v["cited_by"]["status"] == "unavailable" {
                    println!("CITATIONS UNAVAILABLE  Search results are intact; citation counts are unknown.");
                }
                if v["no_prior"] == true {
            if let Some(nc) = v.get("not_considered") {
                println!("NOT CONSIDERED  {} archived room(s), {} records (ox recall … --include-archived reads them)", nc["rooms"], nc["records"]);
            }
                    println!("NO PRIOR  {}", v["prior"]["says"].as_str().unwrap_or("nothing on record holds these words"));
                }
                // One entry per hit, the same as oracle_recall gives an agent (oracle_mcp::gist_render): what it is, whether
                // Blaze approved it, how many of the query's words it holds, and the passage that matched. A head printed
                // above as STATE is not printed again (31 Oct 2026: recall said each current head twice).
                let shown: Vec<&str> = now.iter().filter_map(|(_, r)| r.last().and_then(|x| x["id"].as_str())).collect();
                let mut n = 0;
                for h in v["hits"].as_array().cloned().unwrap_or_default() {
                    if shown.contains(&h["id"].as_str().unwrap_or("")) {
                        continue;
                    }
                    n += 1;
                    print!("{}", oracle_mcp::gist_render(n, &oracle_mcp::gist_of(&h)));
                }
            }
        }
        Cmd::Task {
            subject,
            state,
            note,
            next,
            room,
            data,
            needs,
            blocks,
            check,
        } => {
            let mut cargo = json!({"subject": subject, "state": state, "owner": ox.cfg.handle});
            if let Some(n) = next {
                cargo["next_touch"] = json!(n);
            }
            oracle_mcp::blocked_rule(&mut cargo, &needs, blocks.as_deref(), check.as_deref(), oracle_mcp::now_ms())?;
            if let Some(d) = data {
                let text = match d.strip_prefix('@') {
                    Some(path) => std::fs::read_to_string(path)
                        .with_context(|| format!("reading --data from {path}"))?,
                    None => d,
                };
                cargo["data"] = serde_json::from_str(&text).context("--data is not JSON")?;
            }
            let v = ox.write(
                Kind::State,
                &room,
                &note,
                Some(cargo.to_string()),
                vec![format!("subject:{subject}")],
            )?;
            println!(
                "state:{state}  {subject}  {}",
                v["id"].as_str().unwrap_or("?")
            );
        }
        Cmd::Plan { pipeline: true, .. } => {
            let heads = ox.get("/states")?;
            let mut subjects: Vec<String> = heads["heads"].as_array().into_iter().flatten()
                .filter(|h| !matches!(h["state"].as_str(), Some("done" | "noted")))
                .filter_map(|h| h["subject"].as_str().filter(|s| s.starts_with("plan.") && s.split('.').count() == 3).map(str::to_string)).collect();
            subjects.sort();
            subjects.dedup();
            let mut plans = vec![];
            for subject in subjects {
                let v = ox.get(&format!("/search?tag={}&limit=200", urlencode(&format!("subject:{subject}"))))?;
                if let Some(p) = plan::newest_data(&v) { plans.push((subject, p)); }
            }
            print!("{}", plan::pipeline(&plans));
        }
        Cmd::Plan { room, name, write, step, rank, after, note, next, .. } => {
            let (room, name) = (room.unwrap_or_default(), name.unwrap_or_default());
            let subject = plan::subject(&room, &name)?;
            let current = || -> Result<Option<Value>> {
                let v = ox.get(&format!("/search?tag={}&limit=200", urlencode(&format!("subject:{subject}"))))?;
                Ok(plan::newest_cargo(&v))
            };
            let (page, prev_next) = match (write, step) {
                (Some(path), None) => {
                    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
                    let v: Value = serde_json::from_str(&text).context("the plan is a plan/1 JSON page")?;
                    plan::validate(&v)?;
                    (Some(v), current()?.and_then(|c| c["next_touch"].as_str().map(str::to_string)))
                }
                (None, Some(mv)) => {
                    let Some(cur) = current()? else { anyhow::bail!("{subject}: no plan on record yet; write one with --write page.json") };
                    (Some(plan::step(&cur["data"], &mv)?), cur["next_touch"].as_str().map(str::to_string))
                }
                (Some(_), Some(_)) => anyhow::bail!("ox plan: --write or --step, not both"),
                (None, None) => (None, None),
            };
            // --rank and --after move one step's place or what it waits on, on the page just written or moved, else the newest.
            let (page, prev_next) = match (rank.as_deref().map(|m| ("rank", m)).into_iter().chain(after.as_deref().map(|m| ("after", m))).collect::<Vec<_>>(), page) {
                (moves, page) if moves.is_empty() => (page, prev_next),
                (moves, page) => {
                    let (mut v, prev) = match page {
                        Some(v) => (v, prev_next),
                        None => {
                            let Some(cur) = current()? else { anyhow::bail!("{subject}: no plan on record yet; write one with --write page.json") };
                            (cur["data"].clone(), cur["next_touch"].as_str().map(str::to_string))
                        }
                    };
                    for (field, mv) in moves { v = plan::set(&v, field, mv)?; }
                    (Some(v), prev)
                }
            };
            match page {
                Some(v) => {
                    let state = plan::state(&v);
                    let note = note.unwrap_or_else(|| plan::note(&v));
                    let mut cargo = json!({"subject": subject, "state": state, "owner": ox.cfg.handle, "data": v});
                    match next.or(prev_next) {
                        Some(n) => cargo["next_touch"] = json!(n),
                        None if matches!(state, "open" | "running") => anyhow::bail!("ox plan: an {state} plan needs --next <date> (when it is next touched), or the head rots"),
                        None => {}
                    }
                    let w = ox.write(Kind::State, &room, &note, Some(cargo.to_string()), vec![format!("subject:{subject}"), "schema:plan/1".into()])?;
                    println!("plan:{state}  {subject}  {}", w["id"].as_str().unwrap_or("?"));
                }
                None => {
                    let v = ox.get(&format!("/search?tag={}&limit=200", urlencode(&format!("subject:{subject}"))))?;
                    match plan::newest_data(&v) {
                        None => println!("{subject}: no plan on record (ox plan {room} {name} --write page.json)"),
                        Some(p) => {
                            if ox.json { println!("{}", json!({"subject": subject, "state": plan::state(&p), "plan": p, "revisions": head_revisions(&v)})); }
                            else { print!("{}", plan::text(&p)); print!("{}", head_text(&subject, &head_revisions(&v))); }
                        }
                    }
                }
            }
        }
        Cmd::Mcp => mcp::serve(&ox)?,
        Cmd::DoorbellTick => mcp::doorbell_tick(&ox),
        Cmd::Serve { seats, run, status } => {
            let run = run.unwrap_or_else(|| ox.home.join("run"));
            if status {
                let ask = |op: &str| service::request(&run, &ox.home, json!({"op": op}));
                let (hello, now, queued) = (ask("hello")?, ask("now")?, ask("queued")?);
                if ox.json {
                    println!("{}", json!({"hello": hello, "now": now, "queued": queued}));
                } else {
                    println!("{} · seat {} · Oracle {} · clock offset {} ms ±{} · {} queued",
                        hello["protocol"].as_str().unwrap_or("?"), hello["seat"].as_str().unwrap_or("?"),
                        if now["oracle_up"] == true { "up" } else { "UNREACHABLE" },
                        now["offset_ms"], now["uncertainty_ms"], queued["queued"]);
                }
            } else {
                let homes = if seats.is_empty() { vec![ox.home.clone()] } else { seats };
                service::Service::load(&homes)?.start(&run)?.serve()?
            }
        }
        Cmd::Anchor { op } => match op {
            AnchorOp::Tip { journal } => {
                if !ledger_journal_ok(&journal) {
                    bail!("journal names are [a-z0-9-], at most 64 characters");
                }
                match ox.anchor_tip(&journal)? {
                    None => println!("null"),
                    Some((seq, hash)) => println!("{}", json!({"seq": seq, "hash": hash})),
                }
            }
            AnchorOp::Advance { journal, seq, hash } => {
                if !ledger_journal_ok(&journal) || !ledger_hash_ok(&hash) {
                    bail!("journal names are [a-z0-9-] (≤64); the hash is 64 lowercase hex");
                }
                ox.anchor_advance(&journal, seq, &hash)?;
            }
        },
        Cmd::Authority { op } => match op {
            AuthorityOp::SignRequest { file } => {
                let input = if file == "-" {
                    authority::read_bounded(std::io::stdin())?
                } else {
                    authority::read_bounded(std::fs::File::open(&file).with_context(|| format!("read {file}"))?)?
                };
                println!("{}", authority::sign_request(&ox.key()?, &input)?);
            }
        },
        Cmd::Dot { .. } => unreachable!("fold() turns ox dot into the anchor or authority arm"),
        Cmd::Flush => {
            let (sent, left) = ox.flush()?;
            println!("flushed {sent}, still queued {left}");
            if left > 0 {
                std::process::exit(3);
            }
        }
    }
    if nudges {
        board::nudge(&ox);
    }
    Ok(())
}

/// The brief, and a copy of it on disk: `<home>/brief.cache`, rewritten on every successful read (Bobby,
/// 2026-09-28: "the line points at a cache that is always current, instead of at a service that was down
/// for ten minutes yesterday"). When Oracle cannot be reached the last brief is printed, marked as such,
/// so a seat still wakes knowing who it is — and knowing the brief is old.
/// `ox reflect` (docs/REFLECT.md): every seat's new memories in a room (or in every room, `all`) and the reflector's current
/// observations to /reflect, in batches, oldest first; each observation signed by the reflector in the room it came from.
fn reflect_cmd(ox: &Ox, room: &str, except: &[String], limit: usize, batches: usize, dry_run: bool) -> Result<()> {
    let me = ox.get("/whoami")?["handle"].as_str().unwrap_or(&ox.cfg.handle).to_string();
    let state = ox.home.join("reflect").join(format!("{}.json", capture::safe(room)));
    let since = std::fs::read_to_string(&state).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["seq"].as_i64()).unwrap_or(0);
    let mine = |path: &str| -> Result<Vec<Value>> {
        let v = ox.get(path)?;
        Ok(v["items"].as_array().cloned().unwrap_or_default().into_iter().filter(|i| i["author"].as_str() == Some(me.as_str())).collect())
    };
    let tagged = |i: &Value, t: &str| i["tags"].as_array().is_some_and(|a| a.iter().any(|x| x.as_str() == Some(t)));
    let all = room == "all";
    let channel = if all { String::new() } else { format!("&channel={}", urlencode(room)) };
    // A first run has no place to start from: one room starts at its newest memories, every room at the last day's, so a
    // whole history is not read in one night.
    let first_day = (all && since == 0).then(|| now_ms() - 86_400_000);
    let cap = limit * batches;
    let keep = |i: &Value| {
        i["seq"].as_i64().unwrap_or(0) > since
            && !tagged(i, "reflection")
            && !except.iter().any(|e| i["author"].as_str() == Some(e.as_str()))
            && first_day.is_none_or(|d| i["created_ms"].as_i64().unwrap_or(0) >= d)
    };
    // A memory is a record tagged memory:<kind> (ox remember, ox capture); kind=memory also holds marks, replies and State
    // heads. Each kind newest first from the server, paged back to the last one read, then read oldest first.
    let mut fresh: Vec<Value> = Vec::new();
    for kind in ["decision", "lesson", "preference", "commitment"] {
        let mut got = 0;
        let mut before: Option<i64> = None;
        for _ in 0..20 {
            let page = ox.get(&format!("/search?tag=memory:{kind}&limit=200{channel}{}", before.map(|b| format!("&before={b}")).unwrap_or_default()))?;
            let items = page["items"].as_array().cloned().unwrap_or_default();
            let oldest = items.iter().filter_map(|i| i["seq"].as_i64()).min();
            let oldest_ms = items.iter().filter_map(|i| i["created_ms"].as_i64()).min().unwrap_or(0);
            let kept: Vec<Value> = items.into_iter().filter(|i| keep(i)).collect();
            got += kept.len();
            fresh.extend(kept);
            let more = match first_day {
                Some(d) => oldest_ms >= d,
                None => since > 0 || got < cap,
            };
            match oldest {
                Some(o) if o > since && more => before = Some(o),
                _ => break,
            }
        }
    }
    fresh.sort_by_key(|i| i["seq"].as_i64().unwrap_or(0));
    fresh.dedup_by(|a, b| a["id"] == b["id"]);
    // Each room on its own, so an observation stays in the room its memories came from.
    let mut rooms: std::collections::BTreeMap<String, Vec<Value>> = std::collections::BTreeMap::new();
    for m in fresh {
        let ch = if all { m["channel"].as_str().unwrap_or("").to_string() } else { room.to_string() };
        rooms.entry(ch).or_default().push(m);
    }
    if rooms.is_empty() {
        println!("nothing to reflect on in {}: no new memories since the last reflection", if all { "any room".to_string() } else { format!("#{room}") });
        return Ok(());
    }
    let mut read_to = since;
    for (ch, mut list) in rooms {
        if since == 0 {
            list = list.split_off(list.len().saturating_sub(cap));
        } else {
            list.truncate(cap);
        }
        read_to = read_to.max(list.iter().filter_map(|m| m["seq"].as_i64()).max().unwrap_or(since));
        if list.len() < 2 {
            println!("#{ch}: {} new memory, two are needed", list.len());
            continue;
        }
        for batch in list.chunks(limit) {
            if batch.len() < 2 {
                break; // a lone memory left over in a room is not reflected on alone
            }
            reflect_batch(ox, &ch, &me, batch, dry_run, &mine)?;
            if !dry_run && !all {
                let top = batch.iter().filter_map(|m| m["seq"].as_i64()).max().unwrap_or(since);
                std::fs::create_dir_all(state.parent().unwrap())?;
                std::fs::write(&state, json!({"seq": top, "at": now_ms()}).to_string())?;
            }
        }
    }
    // Every room in one pass keeps one place: the newest memory it read, saved once every room is done.
    if !dry_run && all {
        std::fs::create_dir_all(state.parent().unwrap())?;
        std::fs::write(&state, json!({"seq": read_to, "at": now_ms()}).to_string())?;
    }
    Ok(())
}

/// One call to /reflect: these memories and the reflector's current observations in the room; each observation signed.
fn reflect_batch(ox: &Ox, room: &str, me: &str, fresh: &[Value], dry_run: bool, mine: &dyn Fn(&str) -> Result<Vec<Value>>) -> Result<()> {
    // The current observations: every reflection of mine that no later reflection of mine supersedes.
    let reflections = mine(&format!("/search?channel={}&tag=reflection&limit=200", urlencode(room)))?;
    let replaced: std::collections::HashSet<String> = reflections.iter()
        .flat_map(|r| r["tags"].as_array().cloned().unwrap_or_default())
        .filter_map(|t| t.as_str().and_then(|t| t.strip_prefix("supersedes:")).map(str::to_string))
        .collect();
    let previous: Vec<Value> = reflections.iter()
        .filter(|r| !replaced.contains(r["id"].as_str().unwrap_or("")))
        .map(|r| {
            let about = r["tags"].as_array().and_then(|a| a.iter().find_map(|t| t.as_str()?.strip_prefix("observation:").map(str::to_string)));
            json!({"id": r["id"], "about": about, "text": r["content"]})
        })
        .collect();
    let memories: Vec<Value> = fresh.iter().map(|m| json!({"id": m["id"], "author": m["author"], "text": m["content"]})).collect();
    let http = ureq::AgentBuilder::new().resolver(ipv4_first).timeout_connect(Duration::from_secs(5)).timeout(Duration::from_secs(630)).build();
    let v = match Ox::call_with(&http, &ox.cfg.token, ox.json, ox.base(), "POST", "/reflect", Some(&json!({"memories": memories, "previous": previous}))) {
        Sent::Ok(v) => v,
        Sent::Refused(c, d) => bail!("/reflect refused ({c}): {d}"),
        Sent::Unreachable(e) => bail!("Oracle unreachable ({e})"),
    };
    let observations = v["observations"].as_array().cloned().unwrap_or_default();
    for o in &observations {
        let (Some(about), Some(text)) = (o["about"].as_str(), o["text"].as_str()) else { continue };
        // A subject that is only the room's name says nothing the room does not.
        if about == room || about.strip_prefix("room-") == Some(room) {
            continue;
        }
        let refines = o["refines"].as_str();
        if dry_run {
            println!("{about}{}: {text}", refines.map(|r| format!(" (refines {r})")).unwrap_or_default());
            continue;
        }
        let cites: Vec<String> = o["cites"].as_array().cloned().unwrap_or_default().iter().filter_map(|c| c.as_str().map(str::to_string)).collect();
        let mut tags = vec!["reflection".to_string(), format!("observation:{about}")];
        tags.extend(cites.iter().take(12).map(|c| format!("ref:{c}")));
        // One current observation a subject: it supersedes the one it refines and any other current one on its subject.
        let mut replaces: Vec<String> = refines.map(str::to_string).into_iter().collect();
        replaces.extend(previous.iter().filter(|p| p["about"].as_str() == Some(about)).filter_map(|p| p["id"].as_str().map(str::to_string)));
        replaces.sort();
        replaces.dedup();
        tags.extend(replaces.iter().map(|r| format!("supersedes:{r}")));
        let cargo = json!({"schema": "reflect/1", "about": about, "cites": cites, "refines": refines, "model": v["model"]}).to_string();
        let w = ox.write(Kind::Memory, room, text, Some(cargo), tags)?;
        if !ox.json {
            println!("reflected ({about}{})  {}", if refines.is_some() { ", refined" } else { "" }, w["id"].as_str().unwrap_or("?"));
        }
    }
    if observations.is_empty() {
        println!("#{room}: {} memories read; no subject had two of them or a change ({me})", memories.len());
    }
    Ok(())
}

/// `ox capture` (capture.rs, docs/CAPTURE.md): the hook's spool, the drain, or one transcript by hand.
fn capture_cmd(ox: &Ox, hook: Option<String>, drain: bool, file: Option<PathBuf>, agent: &str, room: &str, dry_run: bool) -> Result<()> {
    if let Some(agent) = hook {
        // A hook never fails its agent: whatever goes wrong is logged, and the agent goes on.
        if let Err(e) = capture_spool(ox, &agent) {
            capture_log(&ox.home, &format!("hook {agent}: {e:#}"));
        }
        return Ok(());
    }
    if drain {
        return capture_drain(ox, room);
    }
    let file = file.context("ox capture needs --hook <agent>, --drain, or --file <transcript>")?;
    anyhow::ensure!(capture::AGENTS.contains(&agent), "--agent is one of {}", capture::AGENTS.join(", "));
    let raw = std::fs::read_to_string(&file).with_context(|| format!("cannot read {}", file.display()))?;
    let (text, _) = capture::new_text(&raw, 0);
    let session = format!("file:{}", file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let kept = capture_one(ox, agent, &session, &text, room, dry_run)?;
    if !ox.json {
        println!("{kept} memor{} {}", if kept == 1 { "y" } else { "ies" }, if dry_run { "would be kept (--dry-run: nothing written)" } else { "kept" });
    }
    Ok(())
}

/// The hook side: the session into the spool, a detached drain started, back to the agent at once.
fn capture_spool(ox: &Ox, agent: &str) -> Result<()> {
    use std::io::Read;
    let mut stdin = String::new();
    std::io::stdin().take(1 << 20).read_to_string(&mut stdin)?;
    let s = capture::from_hook(agent, &stdin)?;
    capture::spool(&ox.home, &s, now_ms())?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("--home").arg(&ox.home).args(["capture", "--drain"])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("could not start the drain")?;
    Ok(())
}

/// Every spooled capture, one drain at a time. A capture that fails is logged and dropped from the spool; the session's next
/// capture reads from the same line, so nothing it said is skipped.
fn capture_drain(ox: &Ox, room: &str) -> Result<()> {
    let dir = ox.home.join("capture");
    std::fs::create_dir_all(dir.join("queue"))?;
    let lock = dir.join("drain.lock");
    // A lock older than twenty minutes was left by a drain that died.
    if std::fs::metadata(&lock).ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|a| a > Duration::from_secs(1200)) {
        let _ = std::fs::remove_file(&lock);
    }
    if std::fs::OpenOptions::new().write(true).create_new(true).open(&lock).is_err() {
        return Ok(()); // another drain is at work, and it reads the spool again before it stops
    }
    let mut out = Ok(());
    loop {
        let mut queued: Vec<PathBuf> = std::fs::read_dir(dir.join("queue"))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        if queued.is_empty() {
            break;
        }
        queued.sort();
        for f in queued {
            if let Err(e) = capture_spooled(ox, &f, room) {
                capture_log(&ox.home, &format!("{}: {e:#}", f.display()));
                out = Err(e);
            }
            let _ = std::fs::remove_file(&f);
        }
    }
    let _ = std::fs::remove_file(&lock);
    out
}

fn capture_spooled(ox: &Ox, f: &Path, room: &str) -> Result<()> {
    let s = capture::unspool(&serde_json::from_str(&std::fs::read_to_string(f)?)?).context("a spool entry that names no session")?;
    let done = capture::done_lines(&ox.home, &s.session);
    let (text, total) = match &s.transcript {
        Some(p) => {
            let raw = std::fs::read_to_string(p).with_context(|| format!("cannot read the transcript {}", p.display()))?;
            // A transcript shorter than what was read is a new one under the same name: read it from the start.
            let from = if raw.lines().count() < done { 0 } else { done };
            capture::new_text(&raw, from)
        }
        None => (s.last_message.clone().map(|m| format!("agent: {m}")).unwrap_or_default(), done),
    };
    if text.chars().count() >= capture::MIN_NEW {
        let kept = capture_one(ox, &s.agent, &s.session, &text, room, false)?;
        capture_log(&ox.home, &format!("{} {} {}: {kept} kept", s.agent, s.event, s.session));
    }
    capture::set_done_lines(&ox.home, &s.session, total, now_ms())
}

/// Ask the Oracle's /extract about `text` and sign each memory it finds as this seat's; how many were kept.
fn capture_one(ox: &Ox, agent: &str, session: &str, text: &str, room: &str, dry_run: bool) -> Result<usize> {
    // A local model on a CPU takes about two minutes a piece; /extract allows ten.
    let http = ureq::AgentBuilder::new().resolver(ipv4_first).timeout_connect(Duration::from_secs(5)).timeout(Duration::from_secs(630)).build();
    let (pieces, left) = capture::pieces(text);
    if left > 0 {
        capture_log(&ox.home, &format!("{agent} {session}: {left} older piece(s) of new text not read; the newest {} were", pieces.len()));
    }
    let mut found: Vec<(Value, Value)> = Vec::new();
    for piece in &pieces {
        let v = match Ox::call_with(&http, &ox.cfg.token, ox.json, ox.base(), "POST", "/extract", Some(&json!({"text": piece, "agent": agent}))) {
            Sent::Ok(v) => v,
            Sent::Refused(c, d) => bail!("/extract refused ({c}): {d}"),
            Sent::Unreachable(e) => bail!("Oracle unreachable ({e})"),
        };
        found.extend(v["memories"].as_array().cloned().unwrap_or_default().into_iter().map(|m| (m, v["model"].clone())));
    }
    let mut kept = 0;
    for (m, model) in found {
        let (Some(kind), Some(body)) = (m["kind"].as_str(), m["text"].as_str()) else { continue };
        if capture_known(ox, room, body)? {
            continue;
        }
        kept += 1;
        if dry_run {
            println!("{kind}: {body}");
            continue;
        }
        let cargo = json!({"schema": "capture/1", "agent": agent, "session": session, "model": model, "why": m["why"]}).to_string();
        let tags = vec![format!("memory:{kind}"), "extracted:session".to_string(), format!("capture:{agent}")];
        let w = ox.write(Kind::Memory, room, body, Some(cargo), tags)?;
        if !ox.json {
            println!("captured ({kind})  {}", w["id"].as_str().unwrap_or("?"));
        }
    }
    Ok(kept)
}

/// Whether the room already holds this memory word for word (spacing and case aside).
fn capture_known(ox: &Ox, room: &str, text: &str) -> Result<bool> {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let words = text.split_whitespace().take(10).collect::<Vec<_>>().join(" ");
    let v = ox.get(&format!("/search?q={}&channel={}&limit=5&match=any", urlencode(&words), urlencode(room)))?;
    let hits = v["hits"].as_array().or(v["items"].as_array()).cloned().unwrap_or_default();
    Ok(hits.iter().any(|h| h["content"].as_str().is_some_and(|c| norm(c) == norm(text))))
}

/// One line in `<home>/capture/log`; the log is started again past a megabyte.
fn capture_log(home: &Path, line: &str) {
    use std::io::Write;
    let file = home.join("capture").join("log");
    let _ = std::fs::create_dir_all(home.join("capture"));
    if std::fs::metadata(&file).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::remove_file(&file);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
        let _ = writeln!(f, "{} {line}", oracle_proto::iso_ms(now_ms()));
    }
}

fn brief(ox: &Ox, full: bool) -> Result<()> {
    if !ox.json { eprintln!("Offline agent guide: ox manual workflow (reference: ox manual)"); }
    let cache = ox.home.join("brief.cache");
    match brief_with(ox, full) {
        Ok(text) => {
            let stamp = format!("cache of `ox brief` for {} · written {} · do not edit\n", ox.cfg.handle, now_ms());
            if let Err(e) = std::fs::write(&cache, format!("{stamp}{text}")) {
                eprintln!("brief.cache not written: {e}");
            }
            print!("{text}");
            Ok(())
        }
        Err(e) => match std::fs::read_to_string(&cache) {
            Ok(old) => {
                let (stamp, text) = old.split_once('\n').unwrap_or(("", &old));
                let age = stamp
                    .split("written ")
                    .nth(1)
                    .and_then(|s| s.split(' ').next())
                    .and_then(|s| s.parse::<i64>().ok())
                    .map(|ms| format!("{}m", ((now_ms() - ms) / 60_000).max(0)))
                    .unwrap_or_else(|| "?".into());
                println!("ox brief · OFFLINE — Oracle did not answer ({e:#}); this is the cache from {age} ago, not the live brief");
                print!("{text}");
                Ok(())
            }
            Err(_) => Err(e),
        },
    }
}


/// Every check an agent needs to trust its own setup, with the fix printed next to a failure.
fn doctor(ox: &Ox) -> Result<()> {
    let mut failed = 0;
    let mut check = |name: &str, r: Result<String>| match r {
        Ok(detail) => println!("  ✓ {name:<22} {detail}"),
        Err(e) => {
            failed += 1;
            println!("  ✗ {name:<22} {e:#}");
        }
    };
    println!("ox doctor");
    let key = ox.key();
    check(
        "key file",
        key.as_ref()
            .map(|k| format!("{}…", &pubkey_hex(k)[..16]))
            .map_err(|e| anyhow!("{e:#}")),
    );
    check(
        "server reachable",
        ox.get("/health")
            .map(|v| format!("{} v{}", ox.base(), v["version"].as_str().unwrap_or("?"))),
    );
    let who = ox.get("/whoami");
    check(
        "token accepted",
        who.as_ref()
            .map(|v| {
                format!(
                    "{} ({})",
                    v["handle"].as_str().unwrap_or("?"),
                    v["scope"].as_str().unwrap_or("?")
                )
            })
            .map_err(|e| anyhow!("{e:#}")),
    );
    if let (Ok(k), Ok(v)) = (&key, &who) {
        check(
            "handle matches",
            if v["handle"].as_str() == Some(ox.cfg.handle.as_str()) {
                Ok(ox.cfg.handle.clone())
            } else {
                Err(anyhow!(
                    "token is for '{}', config says '{}'",
                    v["handle"],
                    ox.cfg.handle
                ))
            },
        );
        check(
            "key is registered",
            if registered(v, &pubkey_hex(k)) {
                Ok("registry holds this key".into())
            } else {
                Err(anyhow!("the registry holds a different key for this seat — ask the owner to add it (`oracle seat add-key`) or rotate it"))
            },
        );
        let skew = (v["server_ms"].as_i64().unwrap_or(0) - now_ms()).abs();
        check(
            "clock",
            if skew < 60_000 {
                Ok(format!("{skew} ms from server"))
            } else {
                Err(anyhow!(
                    "clock is {skew} ms from the server; writes are rejected beyond 5 minutes"
                ))
            },
        );
    }
    check(
        "signed round trip",
        ox.write(
            Kind::Say,
            "doctor",
            "ox doctor: signed round trip",
            None,
            vec![],
        )
        .and_then(|v| {
            let id = v["id"].as_str().unwrap_or("").to_string();
            let back = ox.get(&format!("/search?id={id}"))?;
            if back["author"].as_str() == Some(ox.cfg.handle.as_str()) {
                Ok(id)
            } else {
                Err(anyhow!("wrote {id} but could not read it back"))
            }
        }),
    );
    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    println!("all good.");
    Ok(())
}

#[cfg(test)]
mod network_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A one-route HTTP server that answers every request with `status` and counts the hits.
    fn server(status: u16) -> (String, Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                // Read the WHOLE request (headers, then Content-Length of body) before answering:
                // closing with unread bytes resets the connection, which a client rightly reads as
                // no answer.
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = s.read(&mut buf) {
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if req.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                counted.fetch_add(1, Ordering::SeqCst);
                let body = if status == 200 {
                    r#"{"ok":true}"#
                } else {
                    r#"{"detail":"nope"}"#
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (url, hits)
    }

    /// Nothing listens here: connecting is refused at once, which is "no answer".
    const DEAD: &str = "http://127.0.0.1:1";

    fn ox(url: &str, fallbacks: &[&str]) -> Ox {
        Ox {
            home: std::env::temp_dir(),
            cfg: Config {
                url: url.into(),
                fallback_urls: fallbacks.iter().map(|s| s.to_string()).collect(),
                handle: "rocky".into(),
                token: "t".into(),
                key_path: "/nonexistent".into(),
            },
            json: false,
            live: std::cell::Cell::new(0),
            via: None,
            incompatible: None,
            op_id: None,
        }
    }

    #[test]
    fn a_dead_address_falls_through_to_one_that_answers_and_is_remembered() {
        let (up, hits) = server(200);
        let o = ox(DEAD, &[&up]);
        assert!(matches!(o.call("GET", "/health", None), Sent::Ok(_)));
        assert_eq!(
            o.base(),
            up,
            "the address that answered is the one in use now"
        );
        assert!(matches!(o.call("GET", "/health", None), Sent::Ok(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_refusal_is_the_answer_and_never_tried_at_another_door() {
        let (refusing, _) = server(401);
        let (spare, spare_hits) = server(200);
        let o = ox(&refusing, &[&spare]);
        assert!(matches!(
            o.call("POST", "/observations", Some(&json!({}))),
            Sent::Refused(401, _)
        ));
        assert_eq!(
            spare_hits.load(Ordering::SeqCst),
            0,
            "a refused write must not be replayed elsewhere"
        );
    }

    /// Brain cases, 28 Oct 2026: a busy 503 used to be saved as the op's final refusal, so every retry replayed it and the
    /// write never landed. Now the op stays unsettled and the same op lands once the server answers; a journal that
    /// recorded the 503 the old way retries too; and a real refusal is still final.
    #[test]
    fn a_busy_server_leaves_the_op_unsettled_and_the_same_op_lands_later() {
        let o = super::signed_read_tests::oracle();
        let mut ox = o.ox(2);
        let real = ox.cfg.url.clone();
        let (busy, hits) = server(503);
        ox.cfg.url = busy;
        let e = ox.journaled_write("busy-op-0001", Kind::Say, "room", "hello", None, vec![], false).unwrap_err();
        assert!(format!("{e:#}").contains("503") && format!("{e:#}").contains("--op-id busy-op-0001"), "{e:#}");
        assert!(hits.load(Ordering::SeqCst) >= 1);
        let record = |ox: &Ox, op: &str| -> Value { serde_json::from_str(&std::fs::read_to_string(ox.home.join("ops").join(format!("{op}.json"))).unwrap()).unwrap() };
        assert!(record(&ox, "busy-op-0001")["outcome"].is_null(), "a 503 is not the op's outcome");
        // the old journal shape: the 503 recorded as a refusal
        let _ = ox.journaled_write("busy-op-0002", Kind::Say, "room", "again", None, vec![], false).unwrap_err();
        let mut old = record(&ox, "busy-op-0002");
        old["outcome"] = json!({"ok": false, "refused": "NOT written — server refused (503): unavailable: the database is busy; nothing was stored, retry"});
        std::fs::write(ox.home.join("ops/busy-op-0002.json"), old.to_string()).unwrap();
        ox.cfg.url = real;
        for (op, text) in [("busy-op-0001", "hello"), ("busy-op-0002", "again")] {
            let out = ox.journaled_write(op, Kind::Say, "room", text, None, vec![], false).unwrap();
            assert!(!out["stored"].is_null(), "{op}: {out}");
            let replay = ox.journaled_write(op, Kind::Say, "room", text, None, vec![], false).unwrap();
            assert_eq!(replay["replayed"], json!(true), "{op}: stored once, then replayed");
        }
        // a real refusal stays the answer
        let (forbidden, _) = server(403);
        ox.cfg.url = forbidden;
        let _ = ox.journaled_write("busy-op-0003", Kind::Say, "room", "no", None, vec![], false).unwrap_err();
        assert_eq!(record(&ox, "busy-op-0003")["outcome"]["ok"], json!(false), "a 403 is final");
    }

    #[test]
    fn when_nothing_answers_every_address_is_named() {
        let o = ox(DEAD, &["http://127.0.0.1:2"]);
        let Sent::Unreachable(why) = o.call("GET", "/health", None) else {
            panic!("expected no answer")
        };
        assert!(
            why.contains("127.0.0.1:1") && why.contains("127.0.0.1:2"),
            "{why}"
        );
    }

    #[test]
    fn an_old_config_without_fallbacks_still_loads() {
        let c: Config =
            serde_json::from_str(r#"{"url":"u","handle":"h","token":"t","key_path":"k"}"#).unwrap();
        assert!(c.fallback_urls.is_empty());
        assert!(
            !serde_json::to_string(&c).unwrap().contains("fallback_urls"),
            "empty list is not written"
        );
    }
}

/// `ox anchor` against a real Oracle (the server's own router and store, in memory): the tip it
/// reports is one this seat signed, it moves only forward, and nothing it cannot prove is ever
/// reported as a fresh journal.
#[cfg(test)]
pub(crate) mod signed_read_tests {
    use super::*;
    use oracle_proto::{keyfile, KeyAction, KeyEvent};
    use oracle_server::store::{Scope, Store};

    const H1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const H2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const H3: &str = "3333333333333333333333333333333333333333333333333333333333333333";

    pub(crate) fn sk(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    pub(crate) struct Oracle {
        url: String,
        pub(crate) home: PathBuf,
        rocky_token: String,
        pub(crate) mallory_token: String,
    }

    /// Owner blaze; seats rocky (key 2) and mallory (key 3), each with a write token.
    pub(crate) fn oracle() -> Oracle {
        let now = now_ms();
        let store = Store::memory().unwrap();
        store.init_owner("blaze", &pubkey_hex(&sk(1)), now).unwrap();
        for (h, k) in [("rocky", 2), ("mallory", 3)] {
            let ev = KeyEvent::sign(&sk(1), "blaze", KeyAction::Add, h, &pubkey_hex(&sk(k)), now);
            store.apply_key_event(&ev, now).unwrap();
        }
        let rocky_token = store
            .mint_token("rocky", Scope::Write, "test", now)
            .unwrap();
        let mallory_token = store
            .mint_token("mallory", Scope::Write, "test", now)
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(
                    l,
                    oracle_server::http::router(oracle_server::http::App::new(store)),
                )
                .await
                .unwrap();
            });
        });
        let home = std::env::temp_dir().join(format!(
            "ox-anchor-{}-{}",
            std::process::id(),
            url.rsplit(':').next().unwrap()
        ));
        std::fs::create_dir_all(&home).unwrap();
        Oracle {
            url,
            home,
            rocky_token,
            mallory_token,
        }
    }

    impl Oracle {
        /// A seat home on disk (config.json + key file), as `ox setup` leaves it: `handle` speaks with `key`
        /// against `url` (default: this Oracle). For the service, which loads seats from their homes.
        pub(crate) fn seat_home(&self, handle: &str, key: u8, url: Option<&str>) -> PathBuf {
            let home = self.home.join(format!("seat-{handle}-{key}-{}", url.map_or(0, |u| u.len())));
            std::fs::create_dir_all(&home).unwrap();
            let key_path = home.join("identity");
            if !key_path.exists() {
                keyfile::create(&key_path, &sk(key)).unwrap();
            }
            let token = if handle == "mallory" { self.mallory_token.clone() } else { self.rocky_token.clone() };
            let cfg = Config { url: url.unwrap_or(&self.url).to_string(), fallback_urls: vec![], handle: handle.into(), token, key_path: key_path.display().to_string() };
            std::fs::write(home.join("config.json"), serde_json::to_string(&cfg).unwrap()).unwrap();
            home
        }

        /// A fake Oracle for the footer tests: every request's first line is recorded; a path starting with one
        /// of `fast` is answered `{}` at once, any other request is held open for `hold` and then dropped.
        pub(crate) fn fake(fast: &'static [&'static str], hold: Duration) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
            use std::io::{BufRead, BufReader, Write};
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let log = seen.clone();
            std::thread::spawn(move || {
                for c in l.incoming().flatten() {
                    let log = log.clone();
                    std::thread::spawn(move || {
                        let mut r = BufReader::new(c.try_clone().unwrap());
                        let mut first = String::new();
                        let _ = r.read_line(&mut first);
                        let mut line = String::new();
                        while r.read_line(&mut line).is_ok_and(|n| n > 0) && !line.trim().is_empty() { line.clear(); }
                        let mut parts = first.split_whitespace();
                        let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                        log.lock().unwrap().push(format!("{method} {path}"));
                        let mut c = c;
                        if fast.iter().any(|f| path.starts_with(f)) {
                            let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
                        } else {
                            std::thread::sleep(hold);
                        }
                        drop(c);
                    });
                }
            });
            (url, seen)
        }

        /// A proxy in front of a real Oracle: every request is recorded; `GET /fold…` is held open for `hold`
        /// and dropped, everything else is forwarded and its answer relayed (the write-with-a-stalled-footer fixture).
        pub(crate) fn stalling_proxy(upstream: &str, hold: Duration) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
            use std::io::{BufRead, BufReader, Read, Write};
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", l.local_addr().unwrap());
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let (log, upstream) = (seen.clone(), upstream.trim_end_matches('/').to_string());
            std::thread::spawn(move || {
                for c in l.incoming().flatten() {
                    let (log, upstream) = (log.clone(), upstream.clone());
                    std::thread::spawn(move || {
                        let mut r = BufReader::new(c.try_clone().unwrap());
                        let mut first = String::new();
                        let _ = r.read_line(&mut first);
                        let mut headers: Vec<(String, String)> = Vec::new();
                        let mut line = String::new();
                        while r.read_line(&mut line).is_ok_and(|n| n > 0) && !line.trim().is_empty() {
                            if let Some((k, v)) = line.split_once(':') {
                                headers.push((k.trim().to_string(), v.trim().to_string()));
                            }
                            line.clear();
                        }
                        let mut parts = first.split_whitespace();
                        let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
                        log.lock().unwrap().push(format!("{method} {path}"));
                        let mut c = c;
                        if method == "GET" && path.starts_with("/fold") {
                            std::thread::sleep(hold);
                            drop(c);
                            return;
                        }
                        let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                        let mut body = vec![0u8; len];
                        if len > 0 {
                            r.read_exact(&mut body).unwrap();
                        }
                        let mut req = ureq::AgentBuilder::new().build().request(&method, &format!("{upstream}{path}"));
                        for (k, v) in &headers {
                            if !k.eq_ignore_ascii_case("host") && !k.eq_ignore_ascii_case("content-length") && !k.eq_ignore_ascii_case("transfer-encoding") {
                                req = req.set(k, v);
                            }
                        }
                        let res = if len > 0 { req.send_bytes(&body) } else { req.call() };
                        let (status, ctype, out) = match res {
                            Ok(resp) | Err(ureq::Error::Status(_, resp)) => {
                                let status = resp.status();
                                let ctype = resp.header("Content-Type").unwrap_or("application/json").to_string();
                                let mut out = Vec::new();
                                let _ = resp.into_reader().read_to_end(&mut out);
                                (status, ctype, out)
                            }
                            Err(_) => (502, "text/plain".to_string(), b"proxy: upstream unreachable".to_vec()),
                        };
                        let _ = write!(c, "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", out.len());
                        let _ = c.write_all(&out);
                        drop(c);
                    });
                }
            });
            (url, seen)
        }

        /// rocky's ox, holding `key` (2 is the seat's registered key).
        pub(crate) fn ox(&self, key: u8) -> Ox {
            let key_path = self.home.join(format!("key-{key}"));
            if !key_path.exists() {
                keyfile::create(&key_path, &sk(key)).unwrap();
            }
            Ox {
                home: self.home.clone(),
                cfg: Config {
                    url: self.url.clone(),
                    fallback_urls: vec![],
                    handle: "rocky".into(),
                    token: self.rocky_token.clone(),
                    key_path: key_path.display().to_string(),
                },
                json: false,
                live: std::cell::Cell::new(0),
                via: None,
                incompatible: None,
                op_id: None,
            }
        }

        /// A signed line from another seat, shaped exactly like a tip line.
        fn mallory_says(&self, journal: &str, seq: u64, hash: &str) {
            let cargo =
                json!({"schema": LEDGER_SCHEMA, "journal": journal, "seq": seq, "hash": hash});
            let env = Envelope::sign(
                &sk(3),
                LEDGER_CHANNEL,
                "mallory",
                Kind::Say,
                "tip",
                Some(cargo.to_string()),
                vec!["dot-ledger-tip:v1".into(), format!("dot-ledger:{journal}")],
                now_ms(),
            );
            let mut m = self.ox(2);
            m.cfg.handle = "mallory".into();
            m.cfg.token = self.mallory_token.clone();
            m.post("/observations", &serde_json::to_value(&env).unwrap())
                .unwrap();
        }

        fn lines(&self, journal: &str) -> usize {
            let v = self
                .ox(2)
                .get(&format!(
                    "/search?channel={LEDGER_CHANNEL}&tag=dot-ledger:{journal}&limit=500"
                ))
                .unwrap();
            v["items"].as_array().unwrap().len()
        }

        /// A plain signed room message from mallory; its id.
        fn mallory_posts(&self, room: &str, text: &str) -> String {
            let env = Envelope::sign(
                &sk(3),
                room,
                "mallory",
                Kind::Say,
                text,
                None,
                vec![],
                now_ms(),
            );
            let mut m = self.ox(2);
            m.cfg.handle = "mallory".into();
            m.cfg.token = self.mallory_token.clone();
            let v = m
                .post("/observations", &serde_json::to_value(&env).unwrap())
                .unwrap();
            v["id"].as_str().unwrap().to_string()
        }

        /// What rocky's next fold holds for `room`: unread count, and the ids it would show.
        fn rocky_unread(&self, room: &str) -> (i64, Vec<String>) {
            let v = self.ox(2).get("/fold?limit=50").unwrap();
            let n = v["pending"][room].as_i64().unwrap_or(0);
            let ids = v["items"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|i| i["channel"] == room)
                .map(|i| i["id"].as_str().unwrap().to_string())
                .collect();
            (n, ids)
        }
    }

    #[test]
    fn seen_moves_the_read_mark_up_to_that_message_and_never_back() {
        let o = oracle();
        let ox = o.ox(2);
        let first = o.mallory_posts("hustler", "one");
        let second = o.mallory_posts("hustler", "two");
        let third = o.mallory_posts("hustler", "three");
        assert_eq!(o.rocky_unread("hustler").0, 3);

        let (room, seq) = ox.seen(&second).unwrap();
        assert_eq!(room, "hustler");
        assert!(seq > 0);
        let (n, ids) = o.rocky_unread("hustler");
        assert_eq!(n, 1, "only the message after the seen one is still unread");
        assert_eq!(ids, vec![third.clone()]);

        ox.seen(&first).unwrap();
        assert_eq!(
            o.rocky_unread("hustler").0,
            1,
            "seeing an older message does not un-read the newer one"
        );

        ox.seen(&third).unwrap();
        assert_eq!(o.rocky_unread("hustler").0, 0);
        assert_eq!(o.lines("nothing"), 0, "seen posts nothing");
        assert!(
            ox.seen("OBS-hustler-00000000-doesnotexist").is_err(),
            "an id the Oracle does not hold is an error, not a silent no-op"
        );
    }

    #[test]
    fn a_fresh_journal_has_no_tip_and_each_advance_is_read_back() {
        let o = oracle();
        let ox = o.ox(2);
        assert_eq!(ox.anchor_tip("node-a").unwrap(), None);
        ox.anchor_advance("node-a", 1, H1).unwrap();
        assert_eq!(ox.anchor_tip("node-a").unwrap(), Some((1, H1.into())));
        ox.anchor_advance("node-a", 2, H2).unwrap();
        assert_eq!(ox.anchor_tip("node-a").unwrap(), Some((2, H2.into())));
        assert_eq!(
            ox.anchor_tip("node-b").unwrap(),
            None,
            "journals do not share a tip"
        );
    }

    #[test]
    fn backward_and_sideways_moves_are_refused_and_a_reannounce_writes_nothing() {
        let o = oracle();
        let ox = o.ox(2);
        ox.anchor_advance("j", 1, H1).unwrap();
        ox.anchor_advance("j", 2, H2).unwrap();
        assert!(
            ox.anchor_advance("j", 1, H1).is_err(),
            "a lower seq is refused"
        );
        assert!(
            ox.anchor_advance("j", 2, H3).is_err(),
            "the same seq with another hash is refused"
        );
        ox.anchor_advance("j", 2, H2).unwrap();
        assert_eq!(o.lines("j"), 2, "re-announcing the tip wrote nothing");
        assert_eq!(ox.anchor_tip("j").unwrap(), Some((2, H2.into())));
    }

    #[test]
    fn another_seats_lines_neither_count_nor_hide_the_tip() {
        let o = oracle();
        let ox = o.ox(2);
        ox.anchor_advance("j", 5, H1).unwrap();
        // More than a page of newer lines from someone else, one claiming a higher seq.
        o.mallory_says("j", 99, H3);
        for _ in 0..250 {
            o.mallory_says("j", 6, H2);
        }
        assert_eq!(ox.anchor_tip("j").unwrap(), Some((5, H1.into())));
        ox.anchor_advance("j", 6, H2).unwrap();
        assert_eq!(ox.anchor_tip("j").unwrap(), Some((6, H2.into())));
    }

    /// Jobs on #3: a malformed newest line of this seat was skipped, so the older valid line became the
    /// tip. The newest line is the candidate; if it is not a tip line, there is no tip to report.
    #[test]
    fn a_newest_line_that_is_not_a_tip_line_is_an_error_not_an_older_tip() {
        let o = oracle();
        let ox = o.ox(2);
        ox.anchor_advance("j", 1, H1).unwrap();
        let bad = [
            "not json".to_string(),
            json!({"schema": "something-else/1", "journal": "j", "seq": 2, "hash": H2}).to_string(),
            json!({"schema": LEDGER_SCHEMA, "journal": "j", "seq": 2, "hash": "short"}).to_string(),
        ];
        for cargo in bad {
            let env = Envelope::sign(
                &sk(2),
                LEDGER_CHANNEL,
                "rocky",
                Kind::Say,
                "tip",
                Some(cargo.clone()),
                vec!["dot-ledger-tip:v1".into(), "dot-ledger:j".into()],
                now_ms(),
            );
            ox.post("/observations", &serde_json::to_value(&env).unwrap())
                .unwrap();
            assert!(ox.anchor_tip("j").is_err(), "{cargo}");
            assert!(ox.anchor_advance("j", 2, H2).is_err(), "{cargo}");
        }
    }

    #[test]
    fn a_tip_signed_by_another_key_is_an_error_not_an_older_tip() {
        let o = oracle();
        o.ox(2).anchor_advance("j", 1, H1).unwrap();
        // This ox holds a key the lines were not signed with: nothing it reads verifies.
        let stranger = o.ox(4);
        assert!(stranger.anchor_tip("j").is_err());
        assert!(stranger.anchor_advance("j", 2, H2).is_err());
        assert_eq!(o.lines("j"), 1);
    }

    /// An Oracle that lies: it serves `listing` for every listing and `envelope` for every id.
    fn liar(listing: Value, envelope: Value) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for s in listener.incoming() {
                let Ok(mut s) = s else { continue };
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let keys = json!({"keys": [{"handle": "rocky", "pubkey": pubkey_hex(&sk(2))}]});
                let body = if req.starts_with("GET /keys") {
                    &keys
                } else if req.contains("id=") {
                    &envelope
                } else {
                    &listing
                }
                .to_string();
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        url
    }

    /// A tip line rocky really signed at `seq`, as (listing row, stored envelope).
    fn signed_line(seq: u64) -> (Value, Envelope) {
        let cargo =
            json!({"schema": LEDGER_SCHEMA, "journal": "j", "seq": seq, "hash": H1}).to_string();
        let env = Envelope::sign(
            &sk(2),
            LEDGER_CHANNEL,
            "rocky",
            Kind::Say,
            "tip",
            Some(cargo.clone()),
            vec!["dot-ledger-tip:v1".into(), "dot-ledger:j".into()],
            now_ms(),
        );
        (
            json!({"id": "OBS-1", "author": "rocky", "cargo": cargo}),
            env,
        )
    }

    #[test]
    fn a_server_that_rewrites_a_line_is_caught_by_the_signature() {
        let o = oracle();
        let (_, mut env) = signed_line(1);
        // The server moves the tip forward in both the listing and the stored envelope.
        let forged =
            json!({"schema": LEDGER_SCHEMA, "journal": "j", "seq": 9, "hash": H1}).to_string();
        env.cargo = Some(forged.clone());
        let mut ox = o.ox(2);
        ox.cfg.url = liar(
            json!({"items": [{"id": "OBS-1", "author": "rocky", "cargo": forged}]}),
            json!({"envelope": env, "pubkey": pubkey_hex(&sk(2))}),
        );
        assert!(ox.anchor_tip("j").is_err());
    }

    #[test]
    fn a_listing_that_disagrees_with_its_signed_envelope_is_refused() {
        let o = oracle();
        let (_, env) = signed_line(1);
        let claimed =
            json!({"schema": LEDGER_SCHEMA, "journal": "j", "seq": 9, "hash": H1}).to_string();
        let mut ox = o.ox(2);
        ox.cfg.url = liar(
            json!({"items": [{"id": "OBS-1", "author": "rocky", "cargo": claimed}]}),
            json!({"envelope": env, "pubkey": pubkey_hex(&sk(2))}),
        );
        assert!(ox.anchor_tip("j").is_err());
    }

    #[test]
    fn a_newest_line_below_an_older_one_is_a_backward_move_not_a_tip() {
        let o = oracle();
        let (older, env) = signed_line(2);
        let (newer, _) = signed_line(1);
        let mut ox = o.ox(2);
        ox.cfg.url = liar(
            json!({"items": [newer, older]}),
            json!({"envelope": env, "pubkey": pubkey_hex(&sk(2))}),
        );
        assert!(ox.anchor_tip("j").is_err());
    }

    /// A room-context/1 revision of #room by rocky, as posted.
    fn context(ox: &Ox, cargo: String) -> String {
        let env = Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "state", Some(cargo), vec!["room-context:v1".into()], now_ms());
        ox.post("/observations", &serde_json::to_value(&env).unwrap()).unwrap()["id"].as_str().unwrap().to_string()
    }
    fn rev(parents: &[&str], goal: &str) -> String {
        json!({"schema": "room-context/1", "channel": "room", "parents": parents, "fields": {"goal": goal}}).to_string()
    }

    #[test]
    fn state_follows_signed_revisions_to_one_head_or_says_why_not() {
        let o = oracle();
        let ox = o.ox(2);
        assert_eq!(ox.room_state("room").unwrap()["state"], "empty");
        let a = context(&ox, rev(&[], "first"));
        let b = context(&ox, rev(&[&a], "second"));
        let v = ox.room_state("room").unwrap();
        assert_eq!((v["state"].as_str(), v["heads"][0].as_str()), (Some("alive"), Some(b.as_str())));
        assert_eq!(v["fields"]["goal"], "second");
        // A second writer from the same parent can no longer fork the State: the server refuses it (409), so the room stays on one head.
        let env = Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "state", Some(rev(&[&a], "a fork")), vec!["room-context:v1".into()], now_ms());
        let err = ox.post("/observations", &serde_json::to_value(&env).unwrap()).unwrap_err().to_string();
        assert!(err.contains("409") || err.contains("stale State"), "{err}");
        let v = ox.room_state("room").unwrap();
        assert_eq!((v["state"].as_str(), v["heads"][0].as_str()), (Some("alive"), Some(b.as_str())), "still one head");
    }

    #[test]
    fn an_unavailable_or_malformed_state_endpoint_is_not_an_empty_state() {
        let o = oracle();
        let mut ox = o.ox(2);
        ox.cfg.url = "http://127.0.0.1:1".into();
        assert!(ox.room_state("room").is_err());
        ox.cfg.url = liar(json!({"nonsense": true}), json!({}));
        assert!(ox.room_state("room").is_err());
    }

    fn seq_of(ox: &Ox, id: &str) -> i64 {
        ox.get(&format!("/search?id={id}")).unwrap()["seq"].as_i64().unwrap()
    }
    fn post_as(ox: &Ox, key: u8, who: &str, room: &str, text: &str, tags: &[&str]) -> String {
        let env = Envelope::sign(&sk(key), room, who, Kind::Say, text, None, tags.iter().map(|t| t.to_string()).collect(), now_ms());
        ox.post("/observations", &serde_json::to_value(&env).unwrap()).unwrap()["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn state_uses_the_authoritative_complete_snapshot_and_client_verifies_evidence() {
        let o = oracle();
        let ox = o.ox(2);
        let mut last = context(&ox, rev(&[], "r0"));
        for i in 1..12 {
            last = context(&ox, rev(&[&last], &format!("r{i}")));
        }
        let v = ox.room_state("room").unwrap();
        assert_eq!((v["state"].as_str(), v["heads"][0].as_str(), v["revisions"].as_i64(), v["complete"].as_bool()), (Some("alive"), Some(last.as_str()), Some(12), Some(true)));
        assert_eq!(v["fields"]["goal"], "r11");
        assert_eq!(v["coverage"]["sql_complete"], true);
        assert_eq!(v["client_verification"]["native_verified"], 12);
        assert_eq!(v["evidence"].as_array().unwrap().len(), 12);
    }

    #[test]
    fn state_endpoint_cursor_alias_is_explicit_and_conflicts_are_refused() {
        let o = oracle();
        let ox = o.ox(2);
        let a = context(&ox, rev(&[], "first"));
        let cursor = seq_of(&ox, &a) + 1;
        let canonical = ox.get(&format!("/rooms/room/state?as_of_seq={cursor}")).unwrap();
        let alias = ox.get(&format!("/rooms/room/state?as_of={cursor}")).unwrap();
        assert_eq!(canonical, alias);
        assert!(ox.get(&format!("/rooms/room/state?as_of_seq={cursor}&as_of={}", cursor + 1)).is_err());
        assert!(ox.get("/rooms/room/state?as_of_seq=0").is_err());
        assert!(ox.get("/rooms/room/state?as_of_seq=-1").is_err());
        let mut anonymous = ox;
        anonymous.cfg.token = "not-a-token".into();
        assert!(anonymous.get("/rooms/room/state").unwrap_err().to_string().contains("401"));
    }

    /// Emmy, 2026-10-03: through the real journal and server, a replay of a State write that landed returns its receipt (never a second revision); a different write
    /// under the same op_id and a stale head under a new one are refused.
    #[test]
    fn a_replayed_state_write_is_recovered_from_the_journal_not_refused_as_stale() {
        let o = oracle();
        let ox = o.ox(2);
        let a = context(&ox, rev(&[], "first"));
        let call = |args: Value| oracle_mcp::call(&ox, "oracle_room_state", &args);
        let first = call(json!({"room": "room", "set": ["goal=second"], "head": a, "op_id": "state-op-0001"})).unwrap();
        assert!(first.starts_with("signed:ok"), "{first}");
        let b = ox.room_state("room").unwrap()["heads"][0].as_str().unwrap().to_string();
        assert_eq!(ox.room_state("room").unwrap()["revisions"], 2);
        // the State has moved past `a`; the same write again names `a` and must come back as the receipt of `b`
        let again = call(json!({"room": "room", "set": ["goal=second"], "head": a, "op_id": "state-op-0001"})).unwrap();
        assert!(again.contains(&b) && again.contains("recovered"), "{again}");
        let v = ox.room_state("room").unwrap();
        assert_eq!((v["revisions"].as_i64(), v["heads"][0].as_str()), (Some(2), Some(b.as_str())), "no second revision");
        // the same op_id for another write (other fields, another note): refused, never the old receipt
        let e = call(json!({"room": "room", "set": ["goal=third"], "head": a, "op_id": "state-op-0001"})).unwrap_err().to_string();
        assert!(e.contains("already used for a different write"), "{e}");
        let e = call(json!({"room": "room", "set": ["goal=second"], "head": a, "note": "a different explanation", "op_id": "state-op-0001"})).unwrap_err().to_string();
        assert!(e.contains("already used for a different write"), "{e}");
        // an adoption the original never had is refused too (the proposal need not even exist: the request differs)
        let e = call(json!({"room": "room", "set": ["goal=second"], "head": a, "adopts": "OBS-room-20000101-aaaaaaaaaaaa", "op_id": "state-op-0001"})).unwrap_err().to_string();
        assert!(e.contains("already used for a different write"), "{e}");
        // a write with a note and its identical replay
        let noted = call(json!({"room": "room", "set": ["goal=third"], "head": b, "note": "why third", "op_id": "state-op-0003"})).unwrap();
        assert!(noted.starts_with("signed:ok"), "{noted}");
        let c = ox.room_state("room").unwrap()["heads"][0].as_str().unwrap().to_string();
        let again = call(json!({"room": "room", "set": ["goal=third"], "head": b, "note": "why third", "op_id": "state-op-0003"})).unwrap();
        assert!(again.contains(&c) && again.contains("recovered"), "{again}");
        assert_eq!(ox.room_state("room").unwrap()["revisions"], 3);
        // a new op_id on the stale head: the guard's preflight refuses it, and nothing is written
        let e = call(json!({"room": "room", "set": ["goal=third"], "head": a, "op_id": "state-op-0002"})).unwrap_err().to_string();
        assert!(e.contains("moved since your read"), "{e}");
        assert_eq!(ox.room_state("room").unwrap()["revisions"], 3);
    }

    /// Emmy, 2026-10-03 (52b5e413fcdc): the server committed, the reply was lost, and the journal never recorded the outcome. The exact retry must re-deliver the SAME journaled
    /// envelope (the server answers its client_op as a duplicate) instead of being refused by a head preflight that the first attempt itself moved.
    #[test]
    fn a_lost_reply_is_retried_as_the_same_journaled_write_not_refused_by_the_moved_head() {
        let o = oracle();
        let ox = o.ox(2);
        let a = context(&ox, rev(&[], "first"));
        let call = |args: Value| oracle_mcp::call(&ox, "oracle_room_state", &args);
        let first = call(json!({"room": "room", "set": ["goal=second"], "head": a, "op_id": "state-lost-0001"})).unwrap();
        assert!(first.starts_with("signed:ok"), "{first}");
        let b = ox.room_state("room").unwrap()["heads"][0].as_str().unwrap().to_string();
        // the process "died" before the journal learned the outcome: the record keeps the envelope and no outcome
        let rec_path = ox.home.join("ops").join("state-lost-0001.json");
        let mut rec: Value = serde_json::from_str(&std::fs::read_to_string(&rec_path).unwrap()).unwrap();
        assert!(rec["outcome"]["stored"].is_object());
        rec["outcome"] = Value::Null;
        std::fs::write(&rec_path, rec.to_string()).unwrap();
        // the identical retry, on the head it was built on (the State has moved to b): the same envelope goes again and the server answers it as the duplicate it is
        let again = call(json!({"room": "room", "set": ["goal=second"], "head": a, "op_id": "state-lost-0001"})).unwrap();
        assert!(again.starts_with("signed:ok") && again.contains(&b), "{again}");
        let v = ox.room_state("room").unwrap();
        assert_eq!((v["revisions"].as_i64(), v["heads"][0].as_str()), (Some(2), Some(b.as_str())), "no second revision");
        // a DIFFERENT request under that op_id is still refused, settled or not
        let e = call(json!({"room": "room", "set": ["goal=third"], "head": a, "op_id": "state-lost-0001"})).unwrap_err().to_string();
        assert!(e.contains("already used for a different write"), "{e}");
        // and an unsettled op whose write NEVER landed is still judged by the server's guard, not forced in: the stale base is refused (409), nothing is written
        let c = call(json!({"room": "room", "set": ["goal=fourth"], "head": b, "op_id": "state-lost-0002"}));
        assert!(c.unwrap().starts_with("signed:ok"));
        let rev_now = ox.room_state("room").unwrap();
        let (never, never_path) = (json!({"room": "room", "set": ["goal=fifth"], "head": b, "op_id": "state-never-0003"}), ox.home.join("ops").join("state-never-0003.json"));
        // sign and journal the write but make the server refuse it: build the record by hand from a signed envelope that was never delivered
        let stale_rev = oracle_mcp::state_revision("room", std::slice::from_ref(&b), &json!({"goal": "second"}), &["goal=fifth".to_string()]).unwrap();
        let env = Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "State updated: x", Some(stale_rev.to_string()), vec!["room-context:v1".into(), "intent:status".into()], now_ms());
        let digest = oracle_proto::sha_hex(json!(["say", "room", "State updated: x", Some(stale_rev.to_string()), ["room-context:v1", "intent:status"]]).to_string().as_bytes());
        std::fs::write(&never_path, json!({"op_id": "state-never-0003", "digest": digest, "envelope": env, "outcome": null}).to_string()).unwrap();
        let mut a5 = never.clone();
        a5["note"] = json!("State updated: x");
        let e = call(a5).unwrap_err().to_string();
        assert!(e.contains("stale") || e.contains("conflict") || e.contains("409"), "the server's guard has the last word: {e}");
        assert_eq!(ox.room_state("room").unwrap()["revisions"], rev_now["revisions"], "nothing was written");
    }

    /// The State as it stood below a cursor: the exclusive upper seq of the source review, not the newest.
    #[test]
    fn state_at_a_cursor_is_the_state_that_stood_below_it() {
        let o = oracle();
        let ox = o.ox(2);
        let a = context(&ox, rev(&[], "first"));
        let b = context(&ox, rev(&[&a], "second"));
        let c = context(&ox, rev(&[&b], "third"));
        let (sa, sb, sc) = (seq_of(&ox, &a), seq_of(&ox, &b), seq_of(&ox, &c));
        for (cursor, goal, head) in [(sc + 1, "third", &c), (sc, "second", &b), (sb, "first", &a)] {
            let v = ox.room_state_at("room", Some(cursor)).unwrap();
            assert_eq!((v["state"].as_str(), v["fields"]["goal"].as_str(), v["heads"][0].as_str(), v["as_of"].as_i64()), (Some("alive"), Some(goal), Some(head.as_str()), Some(cursor)), "as of {cursor}");
        }
        assert_eq!(ox.room_state_at("room", Some(sa)).unwrap()["state"], "empty", "below the first revision there is no State");
        // an archive mark AFTER the cursor does not remove the revision from the earlier view
        post_as(&ox, 2, "rocky", "room", "withdrawn", &[&format!("archives:{c}")]);
        assert_eq!(ox.room_state("room").unwrap()["heads"][0], b.as_str(), "now: c is archived, b is the head");
        assert_eq!(ox.room_state_at("room", Some(sc + 1)).unwrap()["heads"][0], c.as_str(), "as of before the archive mark: c was a revision then");
    }

    /// A revision that ADOPTS names the proposal, the settling event, the selector and the cursor, and is refused unless the server shows a decider accepted it.
    #[test]
    fn a_revision_adopts_only_a_proposal_a_decider_accepted_and_records_the_deciding_event() {
        let o = oracle();
        let rocky = o.ox(2);
        let mut mallory = o.ox(3);
        mallory.cfg.handle = "mallory".into();
        mallory.cfg.token = o.mallory_token.clone();
        let propose = |text: &str| post_as(&mallory, 3, "mallory", "room", text, &["intent:proposal", "ask:reply", "to:rocky"]);
        let adopt = |p: &str| oracle_mcp::call(&rocky, "oracle_room_state", &json!({"room": "room", "set": ["current=chosen"], "adopts": p}));
        let p = propose("adopt the new reading order");
        // not settled: refused, nothing written
        let e = adopt(&p).unwrap_err().to_string();
        assert!(e.contains("not been settled"), "{e}");
        assert_eq!(rocky.room_state("room").unwrap()["state"], "empty");
        // the proposer's own go is an opinion, not a decision
        post_as(&mallory, 3, "mallory", "room", "yes", &[&format!("re:{p}"), "react:go"]);
        assert!(adopt(&p).unwrap_err().to_string().contains("not been settled"));
        // the decider (the seat it was addressed to) says go
        let go = post_as(&rocky, 2, "rocky", "room", "go", &[&format!("re:{p}"), "react:go"]);
        let out = adopt(&p).unwrap();
        assert!(out.starts_with("signed:ok"), "{out}");
        let st = rocky.room_state("room").unwrap();
        assert_eq!(st["state"], "alive");
        let head = st["heads"][0].as_str().unwrap();
        assert_eq!(st["fields"]["current"], "chosen");
        let a = &st["adoptions"][head];
        assert_eq!((a["room"].as_str(), a["proposal"].as_str(), a["word"].as_str(), a["mark"].as_str(), a["decided_by"].as_str()), (Some("room"), Some(p.as_str()), Some("accepted"), Some(go.as_str()), Some("rocky")));
        assert_eq!(a["cursor"].as_i64(), Some(seq_of(&rocky, &go)), "the cursor is the seq of the deciding event");
        // a declined proposal is settled too, and is refused
        let q = propose("drop the old index");
        post_as(&rocky, 2, "rocky", "room", "no", &[&format!("re:{q}"), "react:no"]);
        assert!(adopt(&q).unwrap_err().to_string().contains("declined"));
        // another room's proposal, and a message that is no proposal
        let other = post_as(&mallory, 3, "mallory", "elsewhere", "elsewhere proposal", &["intent:proposal", "ask:reply", "to:rocky"]);
        post_as(&rocky, 2, "rocky", "elsewhere", "go", &[&format!("re:{other}"), "react:go"]);
        assert!(adopt(&other).unwrap_err().to_string().contains("not #room"));
        let plain = post_as(&mallory, 3, "mallory", "room", "just talking", &[]);
        assert!(adopt(&plain).unwrap_err().to_string().contains("not a proposal"));
        // adopts without a change, and a write at a cursor, are refused before anything is read
        assert!(oracle_mcp::call(&rocky, "oracle_room_state", &json!({"room": "room", "adopts": p})).is_err());
        assert!(oracle_mcp::call(&rocky, "oracle_room_state", &json!({"room": "room", "set": ["current=x"], "as_of": 5})).is_err());
        // a revision written WITHOUT adopts is still valid and reads as unadopted
        let plain_rev = oracle_mcp::call(&rocky, "oracle_room_state", &json!({"room": "room", "set": ["current=later"]})).unwrap();
        assert!(plain_rev.starts_with("signed:ok"));
        let st = rocky.room_state("room").unwrap();
        assert_eq!(st["state"], "alive");
        assert!(st["adoptions"].as_object().unwrap().is_empty(), "no adopts, no adoption claimed");
        let read = oracle_mcp::call(&rocky, "oracle_room_state", &json!({"room": "room"})).unwrap();
        assert!(read.contains("unadopted"), "{read}");
    }

    /// Jobs on #4: a line that did not parse was skipped. If it was the child naming a parent, the parent
    /// became a false head and the room looked alive on old State.
    #[test]
    fn a_revision_line_that_does_not_parse_degrades_the_state_and_is_named() {
        let o = oracle();
        let ox = o.ox(2);
        let a = context(&ox, rev(&[], "old"));
        for cargo in ["not json".to_string(), json!({"schema": "other/1", "channel": "room"}).to_string()] {
            let bad = context(&ox, cargo.clone());
            let v = ox.room_state("room").unwrap();
            assert_eq!(v["state"], "degraded", "{cargo}");
            assert!(v["fields"].is_null(), "old State is not served as current: {cargo}");
            assert!(v["bad"].as_array().unwrap().iter().any(|b| b["id"] == bad.as_str()), "{cargo}");
            assert_eq!(v["heads"][0], a.as_str());
        }
    }

    /// Kepler's probe: a row edited in oracle.db is served as genuine unless the reader checks the
    /// stored envelope.
    fn state_answer(row: Value, env: Value) -> Value {
        let raw = row["cargo"].as_str().unwrap();
        let mut cargo: Value = serde_json::from_str(raw).unwrap();
        if let Some(inner) = cargo["cargo"].as_str() { cargo = serde_json::from_str(inner).unwrap(); }
        json!({"schema": "room-context/1", "channel": "room", "state": "alive", "as_of_seq": 2, "read_through_seq": 1,
            "heads": ["OBS-1"], "fields": cargo["fields"], "adoptions": {}, "revisions": 1,
            "bad": [], "unknown": [], "skipped": [], "complete": true, "trustworthy": true, "coverage": {"sql_complete": true},
            "evidence": [{"id": "OBS-1", "seq": 1, "native": row["envelope"] != 1, "row": row, "envelope": env}]})
    }

    #[test]
    fn a_revision_whose_source_differs_from_its_signed_envelope_is_bad() {
        let env = Envelope::sign(&sk(2), "room", "rocky", Kind::Say, "state", Some(rev(&[], "what was signed")), vec!["room-context:v1".into()], now_ms());
        let row = |cargo: String, key: String, sig: String| json!({"id": "OBS-1", "seq": 1, "channel": "room", "author": "rocky", "kind": "say", "content": "state", "sig": sig,
            "envelope": 2, "pubkey": key, "cargo": cargo, "tags": ["room-context:v1"]});
        let keys = vec![json!({"handle": "rocky", "pubkey": pubkey_hex(&sk(2))})];
        let signed = env.cargo.clone().unwrap();
        let verify = |row, e| oracle_mcp::state::verify(state_answer(row, e), "room", None, &keys).unwrap();
        let v = verify(row(rev(&[], "edited in the database"), pubkey_hex(&sk(2)), env.sig.clone()), json!(env));
        assert_eq!((v["state"].as_str(), v["bad"].as_array().unwrap().len()), (Some("degraded"), 1));
        let stranger = Envelope::sign(&sk(4), "room", "rocky", Kind::Say, "state", Some(signed.clone()), vec!["room-context:v1".into()], now_ms());
        assert_eq!(verify(row(signed.clone(), pubkey_hex(&sk(4)), stranger.sig.clone()), json!(stranger))["state"], "degraded", "a key never registered to rocky");
        let mut rewritten = env.clone();
        let edited = rev(&[], "edited in both places");
        rewritten.cargo = Some(edited.clone());
        assert_eq!(verify(row(edited, pubkey_hex(&sk(2)), rewritten.sig.clone()), json!(rewritten))["state"], "degraded", "an envelope rewritten after signing");
        assert_eq!(verify(row(signed, pubkey_hex(&sk(2)), env.sig.clone()), json!(env))["state"], "alive");
    }

    #[test]
    fn imported_history_counts_only_with_both_proof_tags_and_exact_source() {
        let row = |tags: &[&str]| json!({"id": "OBS-1", "seq": 1, "channel": "room", "author": "rocky", "envelope": 1, "tags": tags, "cargo": rev(&[], "graph era")});
        let verify = |tags: &[&str]| oracle_mcp::state::verify(state_answer(row(tags), Value::Null), "room", None, &[]).unwrap();
        assert_eq!(verify(&["room-context:v1", "signed:ok"])["state"], "degraded");
        let v = verify(&["room-context:v1", "signed:ok", "cargo-signed:ok"]);
        assert_eq!(v["state"], "alive");
        assert_eq!(v["client_verification"]["import_attested"], 1);
        let mut forged = state_answer(row(&["room-context:v1", "signed:ok", "cargo-signed:ok"]), Value::Null);
        forged["fields"]["goal"] = json!("unsigned guidance");
        assert_eq!(oracle_mcp::state::verify(forged, "room", None, &[]).unwrap()["state"], "degraded");
    }

    #[test]
    fn no_answer_or_a_non_listing_is_never_a_fresh_journal_and_nothing_is_queued() {
        let o = oracle();
        let mut dead = o.ox(2);
        dead.cfg.url = "http://127.0.0.1:1".into();
        assert!(dead.anchor_tip("j").is_err());
        assert!(dead.anchor_advance("j", 1, H1).is_err());
        assert!(
            !o.home.join("outbox").exists()
                || std::fs::read_dir(o.home.join("outbox"))
                    .unwrap()
                    .next()
                    .is_none(),
            "a tip that did not land is not queued for later"
        );
        // A 200 that is not a listing (a portal page, a stray proxy).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for s in listener.incoming() {
                let Ok(mut s) = s else { continue };
                let _ = s.read(&mut [0u8; 4096]);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 6\r\nConnection: close\r\n\r\n<html>");
            }
        });
        let mut portal = o.ox(2);
        portal.cfg.url = url;
        assert!(portal.anchor_tip("j").is_err());
        assert!(portal.anchor_advance("j", 1, H1).is_err());
    }
}

#[cfg(test)]
mod board_tests {
    //! `ox card` / `ox status` against a real in-process Oracle: what lands is one structured message.
    use super::signed_read_tests::{oracle, sk};
    use super::*;
    use board::{Move, Status};

    /// What a reader (the board, the notifier, the surface) relies on. Err names what is missing.
    fn card_event(o: &Value) -> std::result::Result<Value, String> {
        let tags: Vec<&str> = o["tags"].as_array().into_iter().flatten().filter_map(|t| t.as_str()).collect();
        for t in ["schema:room-task-event/1", "intent:status", "ask:none"] {
            if !tags.contains(&t) { return Err(format!("missing tag {t}")); }
        }
        let c: Value = serde_json::from_str(o["cargo"].as_str().ok_or("no cargo")?).map_err(|e| e.to_string())?;
        if c["schema"] != "room-task-event/1" { return Err("cargo is not room-task-event/1".into()); }
        let moves = c["moves"].as_array().ok_or("no moves[]")?;
        if moves.is_empty() { return Err("empty moves[]".into()); }
        for m in moves {
            for k in ["task", "title", "to", "owner", "refs", "revision"] {
                if m.get(k).is_none() { return Err(format!("move without {k}")); }
            }
            if m["revision"]["schema"] != "room-task/1" || m["revision"]["id"] != m["task"] {
                return Err("revision is not the card's room-task/1".into());
            }
        }
        Ok(c)
    }
    fn room(ox: &Ox) -> Vec<Value> {
        let mut v = ox.get("/search?channel=hustler&limit=200").unwrap()["items"].as_array().cloned().unwrap_or_default();
        v.sort_by_key(|o| o["seq"].as_i64().unwrap_or(0));
        v
    }
    fn room_of(ox: &Ox, room: &str) -> Vec<Value> {
        let mut v = ox.get(&format!("/search?channel={room}&limit=200")).unwrap()["items"].as_array().cloned().unwrap_or_default();
        v.sort_by_key(|o| o["seq"].as_i64().unwrap_or(0));
        v
    }
    fn mv(id: &str, status: &str) -> Move {
        Move { id: id.into(), status: status.into(), ..Default::default() }
    }
    fn new_card(id: &str, status: &str) -> Move {
        Move { title: Some(format!("Title {id}")), summary: Some("why".into()), acceptance: Some("how we know".into()), ..mv(id, status) }
    }

    #[test]
    fn one_move_is_one_structured_message_and_the_next_move_chains_to_it() {
        let o = oracle();
        let ox = o.ox(2);
        let (face, first) = board::post_moves_as(&ox, "hustler", true, &[new_card("t1", "queue")]).unwrap();
        assert_eq!(face, "[Hustler task:t1] queue: Title t1", "the face stays the one-liner");
        let msgs = room(&ox);
        assert_eq!(msgs.len(), 1);
        let c = card_event(&msgs[0]).unwrap();
        assert_eq!(c["moves"][0]["from"], Value::Null, "a new card comes from nowhere");
        assert_eq!(c["moves"][0]["revision"]["parents"], json!([]));
        assert!(msgs[0]["tags"].as_array().unwrap().contains(&json!("task:t1")));

        // an existing card keeps what the move leaves out, and names the head it replaces
        board::post_moves_as(&ox, "hustler", true, &[mv("t1", "in_progress")]).unwrap();
        let msgs = room(&ox);
        let c = card_event(&msgs[1]).unwrap();
        let m = &c["moves"][0];
        assert_eq!((m["from"].as_str(), m["to"].as_str()), (Some("queue"), Some("in_progress")));
        assert_eq!(m["title"], "Title t1");
        assert_eq!(m["revision"]["summary"], "why");
        assert_eq!(m["revision"]["parents"], json!([first["id"]]));
    }

    #[test]
    fn a_card_links_the_rooms_it_touches_and_keeps_them_until_changed() {
        let o = oracle();
        let ox = o.ox(2);
        let linked = Move { rooms: Some(vec!["axxis".into(), "hustler".into()]), ..new_card("t1", "queue") };
        board::post_moves_as(&ox, "rocky", true, &[linked]).unwrap();
        board::post_moves_as(&ox, "rocky", true, &[mv("t1", "in_progress")]).unwrap();
        let msgs = room_of(&ox, "rocky");
        let rev = |i: usize| card_event(&msgs[i]).unwrap()["moves"][0]["revision"].clone();
        assert_eq!(rev(0)["rooms"], json!(["axxis", "hustler"]));
        assert_eq!(rev(1)["rooms"], json!(["axxis", "hustler"]), "a move that leaves rooms out inherits them");
        board::post_moves_as(&ox, "rocky", true, &[Move { rooms: Some(vec![]), ..mv("t1", "in_progress") }]).unwrap();
        let msgs = room_of(&ox, "rocky");
        assert!(card_event(&msgs[2]).unwrap()["moves"][0]["revision"].get("rooms").is_none(), "an empty --rooms clears the link");
        for bad in [vec!["rocky".to_string()], vec!["Axxis".into()], vec!["axxis".into(), "axxis".into()]] {
            assert!(board::post_moves_as(&ox, "rocky", true, &[Move { rooms: Some(bad.clone()), ..mv("t1", "in_progress") }]).is_err(), "{bad:?} is refused");
        }
    }

    #[test]
    fn a_batch_of_moves_is_one_message_with_a_tally() {
        let o = oracle();
        let ox = o.ox(2);
        board::post_moves_as(&ox, "hustler", true, &[new_card("a", "in_progress")]).unwrap();
        let before = room(&ox).len();
        let done = Move { evidence: vec!["abc1234".into()], handoff: Some("changed · test · gotcha".into()), ..mv("a", "done") };
        let (face, _) = board::post_moves_as(&ox, "hustler", true, &[done, new_card("b", "queue"), new_card("c", "queue")]).unwrap();
        let msgs = room(&ox);
        assert_eq!(msgs.len(), before + 1, "three moves, ONE message");
        assert_eq!(face, "3 card moves: 2 queue, 1 done");
        let c = card_event(msgs.last().unwrap()).unwrap();
        assert_eq!(c["counts"], json!({"queue": 2, "done": 1}));
        assert_eq!(c["moves"][0]["refs"], json!(["abc1234"]));
        assert_eq!(c["moves"][0]["reason"], "changed · test · gotcha");
        assert_eq!(c["moves"][0]["from"], "in_progress");
        // the next revision of a card finds the batch message as its head
        board::post_moves_as(&ox, "hustler", true, &[mv("b", "in_progress")]).unwrap();
        let last = card_event(room(&ox).last().unwrap()).unwrap();
        assert_eq!(last["moves"][0]["revision"]["parents"], json!([msgs.last().unwrap()["id"]]));
    }

    #[test]
    fn legacy_cards_are_heads_and_a_refused_revision_is_never_a_parent() {
        let o = oracle();
        let ox = o.ox(2);
        let good = json!({"schema":"room-task/1","channel":"hustler","id":"old","title":"Old card","group":"Run reliably",
            "impact":"med","owner":"rocky","status":"review","delivery":"not_live","summary":"s","acceptance":"a",
            "evidence":[],"dependsOn":[],"parents":[]});
        let g = ox.write(Kind::Say, "hustler", "[Hustler task:old] review: Old card", Some(good.to_string()), vec!["intent:status".into()]).unwrap();
        let mut bad = good.clone();
        bad["group"] = json!("Room UX");
        bad["parents"] = json!([g["id"]]);
        ox.write(Kind::Say, "hustler", "[Hustler task:old] done: Old card", Some(bad.to_string()), vec!["intent:status".into()]).unwrap();
        board::post_moves_as(&ox, "hustler", true, &[mv("old", "in_progress")]).unwrap();
        let c = card_event(room(&ox).last().unwrap()).unwrap();
        assert_eq!(c["moves"][0]["revision"]["parents"], json!([g["id"]]), "the board-accepted head, not the refused one");
        assert_eq!(c["moves"][0]["from"], "review");
        assert_eq!(c["moves"][0]["revision"]["impact"], "med", "an inherited older spelling is kept");
    }

    #[test]
    fn a_card_whose_revisions_were_all_refused_starts_over_with_no_parent() {
        // math-w1-lean-kernel (2026-09-29): A is refused for its group; B is well-formed but sits on A, so the board refuses B too
        // ("missing or invalid ancestor"). Parenting the next revision on B was refused the same way, and ox could not correct the card.
        let o = oracle();
        let ox = o.ox(2);
        let rev = |group: &str, parents: Value| json!({"schema":"room-task/1","channel":"hustler","id":"lean","title":"Lean kernel","group":group,
            "impact":"high","owner":"rocky","status":"queue","delivery":"not_live","summary":"s","acceptance":"a","evidence":[],"dependsOn":[],"parents":parents});
        let write = |r: Value| ox.write(Kind::Say, "hustler", "[Hustler task:lean] queue: Lean kernel", Some(r.to_string()), vec!["intent:status".into()]).unwrap();
        let a = write(rev("Room UX", json!([])));
        write(rev("Run reliably", json!([a["id"]])));
        board::post_moves_as(&ox, "hustler", true, &[Move { evidence: vec!["abc1234".into()], handoff: Some("changed · test · gotcha".into()), ..new_card("lean", "done") }]).unwrap();
        let c = card_event(room(&ox).last().unwrap()).unwrap();
        assert_eq!(c["moves"][0]["revision"]["parents"], json!([]), "no accepted ancestor: a fresh card, no parent");
        assert_eq!(c["moves"][0]["revision"]["status"], "done");
        // and the next move parents on that accepted revision, as before
        let done = room(&ox).last().unwrap()["id"].clone();
        board::post_moves_as(&ox, "hustler", true, &[mv("lean", "review")]).unwrap_err();
        board::post_moves_as(&ox, "hustler", true, &[Move { handoff: Some("h".into()), ..mv("lean", "review")}]).unwrap();
        let n = card_event(room(&ox).last().unwrap()).unwrap();
        assert_eq!(n["moves"][0]["revision"]["parents"], json!([done]));
    }

    #[test]
    fn in_astra_an_astra_task_revision_is_a_head_and_is_joined() {
        // astra claim-proof-viewer (2026-09-29): Emmy's head is astra-task/1, Rocky's room-task/1; ox parented only on the second and stayed in CONFLICT.
        let o = oracle();
        let ox = o.ox(2);
        let rev = |schema: &str, channel: &str, owner: &str| json!({"schema":schema,"channel":channel,"id":"claim","title":"Claim","group":"Run reliably",
            "impact":"high","owner":owner,"status":"queue","delivery":"not_live","summary":"s","acceptance":"a","evidence":[],"dependsOn":[],"parents":[]});
        let write = |room: &str, r: Value| ox.write(Kind::Say, room, &format!("[{}{} task:claim] queue: Claim", room[..1].to_uppercase(), &room[1..]), Some(r.to_string()), vec!["intent:status".into()]).unwrap();
        let emmy = write("astra", rev("astra-task/1", "astra", "emmy"));
        let mine = write("astra", rev("room-task/1", "astra", "rocky"));
        board::post_moves_as(&ox, "astra", true, &[mv("claim", "in_progress")]).unwrap();
        let c = card_event(room_of(&ox, "astra").last().unwrap()).unwrap();
        assert_eq!(c["moves"][0]["revision"]["parents"], json!([emmy["id"], mine["id"]]), "both heads are joined, so the card is no longer in conflict");
        // the same schema in another room is not a card the board reads
        write("hustler", rev("astra-task/1", "hustler", "emmy"));
        let (parents, head) = board::heads(&room_of(&ox, "hustler"), "claim");
        assert!(parents.is_empty() && head.is_none(), "astra-task/1 is astra's alone");
    }

    #[test]
    fn the_board_contract_refuses_before_anything_is_written() {
        let o = oracle();
        let ox = o.ox(2);
        let err = |m: Vec<Move>| board::post_moves_as(&ox, "hustler", true, &m).unwrap_err().to_string();
        assert!(err(vec![Move { evidence: vec![], handoff: Some("h".into()), ..new_card("x", "done") }]).contains("--evidence"));
        assert!(err(vec![mv("x", "queue")]).contains("needs a title"));
        assert!(err(vec![new_card("x", "queue"), new_card("x", "review")]).contains("twice"));
        assert!(err(vec![new_card("x", "blocked")]).contains("--unblock"));
        assert!(err(vec![Move { impact: Some("med".into()), ..new_card("x", "queue") }]).contains("--impact"));
        assert!(err((0..26).map(|i| new_card(&format!("c{i}"), "queue")).collect()).contains("1 to 25"));
        assert!(room(&ox).is_empty(), "a refused batch writes nothing, not half");
    }

    /// NEGATIVE CONTROL: a card move written the old way (prose, no cargo) fails the reader's check,
    /// so the tests above would catch ox regressing to it.
    #[test]
    fn negative_control_a_move_without_cargo_fails_the_check() {
        let o = oracle();
        let ox = o.ox(2);
        ox.write(Kind::Say, "hustler", "[Hustler task:t9] done: shipped it", None, vec!["intent:status".into()]).unwrap();
        assert!(card_event(&room(&ox)[0]).is_err());
        let tagged_only = json!({"schema":"room-task/1","id":"t9"});
        ox.write(Kind::Say, "hustler", "[Hustler task:t9] done: shipped it", Some(tagged_only.to_string()),
            vec!["schema:room-task-event/1".into(), "intent:status".into(), "ask:none".into()]).unwrap();
        assert!(card_event(&room(&ox)[1]).is_err(), "the tag alone is not the event");
    }

    #[test]
    fn a_done_or_failed_stage_cannot_say_it_is_still_running() {
        let s = |stage: &str, state: &str| Status { task: Some("q3".into()), stage: stage.into(), state: state.into(), ..Default::default() };
        assert!(board::status_cargo(&s("done", "running"), "rocky", 1).is_err(), "stage done, state running is refused");
        assert!(board::status_cargo(&s("failed", "done"), "rocky", 1).is_err());
        assert!(board::status_cargo(&s("done", "done"), "rocky", 1).is_ok());
        assert!(board::status_cargo(&s("done soon", "running"), "rocky", 1).is_ok(), "only the exact word ends the run");
    }

    #[test]
    fn status_is_one_structured_message_that_keeps_its_start() {
        let o = oracle();
        let ox = o.ox(2);
        let s = |state: &str, n| Status { task: Some("q3".into()), stage: "replay".into(), state: state.into(), n: Some(n), of: Some(7), refs: vec!["abc".into()], ..Default::default() };
        board::post_status(&ox, "hustler", "replaying fills", &s("running", 3), 1_000).unwrap();
        board::post_status(&ox, "hustler", "replaying fills", &s("running", 5), 9_000).unwrap();
        let msgs = room(&ox);
        let c: Value = serde_json::from_str(msgs[1]["cargo"].as_str().unwrap()).unwrap();
        assert_eq!(c["schema"], "status/1");
        assert_eq!((c["key"].as_str(), c["n"].as_u64(), c["of"].as_u64()), (Some("q3"), Some(5), Some(7)));
        assert_eq!(c["started_ms"], 1_000, "the same run keeps its start; elapsed is derived");
        let tags = msgs[1]["tags"].as_array().unwrap();
        for t in ["schema:status/1", "intent:status", "ask:none", "status-key:q3"] { assert!(tags.contains(&json!(t)), "{t}"); }
        board::post_status(&ox, "hustler", "done", &s("done", 7), 10_000).unwrap();
        board::post_status(&ox, "hustler", "again", &s("running", 1), 20_000).unwrap();
        let c: Value = serde_json::from_str(room(&ox).last().unwrap()["cargo"].as_str().unwrap()).unwrap();
        assert_eq!(c["started_ms"], 20_000, "after done, a new run starts");
        let bad = Status { n: Some(3), of: None, ..s("running", 3) };
        assert!(board::post_status(&ox, "hustler", "x", &bad, 1).unwrap_err().to_string().contains("--of"));
        let over = Status { n: Some(9), ..s("running", 3) };
        assert!(board::post_status(&ox, "hustler", "x", &over, 1).is_err());
        assert_eq!(room(&ox).len(), 4, "refused statuses write nothing");
    }

    /// Until the board reads room-task-event/1, one card is written the way it reads today.
    #[test]
    fn legacy_mode_writes_the_card_revision_and_refuses_a_batch() {
        let o = oracle();
        let ox = o.ox(2);
        board::post_moves_as(&ox, "hustler", false, &[new_card("l1", "queue")]).unwrap();
        let m = &room(&ox)[0];
        assert_eq!(m["content"], "[Hustler task:l1] queue: Title l1");
        let c: Value = serde_json::from_str(m["cargo"].as_str().unwrap()).unwrap();
        assert_eq!((c["schema"].as_str(), c["id"].as_str()), (Some("room-task/1"), Some("l1")));
        assert!(card_event(m).is_err(), "legacy is not an event");
        board::post_moves_as(&ox, "hustler", false, &[mv("l1", "in_progress")]).unwrap();
        let c: Value = serde_json::from_str(room(&ox)[1]["cargo"].as_str().unwrap()).unwrap();
        assert_eq!(c["parents"], json!([m["id"]]), "legacy moves chain too");
        let err = board::post_moves_as(&ox, "hustler", false, &[new_card("l2", "queue"), new_card("l3", "queue")]).unwrap_err();
        assert!(err.to_string().contains("OX_CARD_EVENTS"));
        assert_eq!(room(&ox).len(), 2);
    }

    #[test]
    fn a_reaction_is_a_signed_reply_with_the_glyph_face() {
        let o = oracle();
        let ox = o.ox(2);
        let (_, target) = board::post_moves_as(&ox, "hustler", true, &[new_card("r1", "queue")]).unwrap();
        let id = target["id"].as_str().unwrap();
        for (kind, face) in [("seen", "👁 seen"), ("go", "✓ go"), ("like", "♥"), ("no", "✕ no")] {
            board::post_reaction(&ox, "hustler", id, kind, None).unwrap();
            let r = room(&ox).last().cloned().unwrap();
            assert_eq!(r["content"], face);
            let tags = r["tags"].as_array().unwrap();
            assert!(tags.contains(&json!(format!("reply:{id}"))) && tags.contains(&json!(format!("react:{kind}"))));
            assert!(r.get("cargo").is_none());
        }
        board::post_reaction(&ox, "hustler", id, "no", Some("wrong venue")).unwrap();
        let r = room(&ox).last().cloned().unwrap();
        assert_eq!(r["content"], "✕ no — wrong venue");
        board::post_reaction(&ox, "hustler", "r1", "go", None).unwrap();
        let tags = room(&ox).last().unwrap()["tags"].clone();
        assert!(tags.as_array().unwrap().contains(&json!("task:r1")), "a card is marked by its board id");
        assert!(board::post_reaction(&ox, "hustler", id, "love", None).is_err());
        assert!(board::post_reaction(&ox, "hustler", "Not An Id", "go", None).is_err());
        assert_eq!(room(&ox).len(), 7);
    }

    #[test]
    fn an_annotation_is_the_classifier_shape_and_refuses_what_is_not_a_state() {
        let o = oracle();
        let ox = o.ox(2);
        let (_, target) = board::post_moves_as(&ox, "hustler", true, &[new_card("n1", "queue")]).unwrap();
        let id = target["id"].as_str().unwrap();
        let sets = vec!["intent=decision".to_string(), "ask=act".into(), "to=bobby-core".into(), "item:R1=requirement".into(), "by=model".into()];
        let n0 = room(&ox).len();
        let (_, written) = board::post_annotation(&ox, "hustler", id, &sets, "decided: 'ship it'", Some(0.9), None).unwrap();
        let aid = written["id"].as_str().unwrap().to_owned();
        // An update is not a message: the room listing does not show it; it is read by id.
        assert_eq!(room(&ox).len(), n0, "an annotation is never a room row");
        let a = ox.get(&format!("/search?id={aid}")).unwrap();
        assert_eq!(a["content"], "decided: 'ship it'");
        let tags: Vec<&str> = a["tags"].as_array().unwrap().iter().filter_map(|t| t.as_str()).collect();
        for t in [format!("annotates:{id}"), "intent:status".into(), "ttl:0".into(), "confidence:0.90".into(),
                  "set:intent=decision".into(), "set:ask=act".into(), "set:to=bobby-core".into(), "item:R1=requirement".into(), "set:by=model".into()] {
            assert!(tags.contains(&t.as_str()), "{t} in {tags:?}");
        }
        let n = room(&ox).len();
        assert!(board::post_annotation(&ox, "hustler", id, &[], "x", None, None).is_err(), "no --set, no annotation");
        assert!(board::post_annotation(&ox, "hustler", id, &["intent".into()], "x", None, None).is_err());
        assert!(board::post_annotation(&ox, "hustler", "nope", &sets, "x", None, None).is_err());
        assert!(board::post_annotation(&ox, "hustler", id, &sets, "  ", None, None).is_err());
        assert!(board::post_annotation(&ox, "hustler", id, &sets, "x", Some(2.0), None).is_err());
        assert_eq!(room(&ox).len(), n, "refusals write nothing");
    }

    #[test]
    fn verbs_write_updates_not_messages() {
        let o = oracle();
        let ox = o.ox(2);
        let (_, target) = board::post_moves_as(&ox, "hustler", true, &[new_card("v1", "queue")]).unwrap();
        let id = target["id"].as_str().unwrap().to_string();
        let tags_of = |o: &Value| -> Vec<String> { o["tags"].as_array().unwrap().iter().filter_map(|t| t.as_str().map(str::to_owned)).collect() };

        // An update is never listed in the room (#15), so it is read back by the id its write returned.
        let (_, receipt) = board::post_annotation(&ox, "hustler", &id, &board::move_sets("R2=built").unwrap(), "shipped in abc", None, None).unwrap();
        let t = tags_of(&ox.get(&format!("/search?id={}", receipt["id"].as_str().unwrap())).unwrap());
        for want in [format!("annotates:{id}"), "set:op=move".into(), "item:R2=built".into()] { assert!(t.contains(&want), "{want} in {t:?}"); }
        assert!(board::move_sets("R2=done").is_err());
        assert!(board::move_sets("closed").unwrap_err().to_string().contains("ox react"), "a message-level no is a reaction");

        let (face, tags, cargo) = board::archive("hustler", &id, None).unwrap();
        ox.write(Kind::Say, "hustler", &face, Some(cargo.to_string()), tags).unwrap();
        let a = room(&ox).last().cloned().unwrap();
        assert_eq!(a["content"], "Archived: out of the view.");
        assert!(tags_of(&a).contains(&format!("archives:{id}")));

        // a plain say may not smuggle an update
        assert!(board::say_refuses(&["react:seen".into()]).unwrap_err().to_string().contains("ox react"));
        assert!(board::say_refuses(&[format!("annotates:{id}")]).unwrap_err().to_string().contains("ox annotate"));
        assert!(board::say_refuses(&["intent:status".into(), "to:bobby-core".into()]).is_ok());

        // a card can name the message it carries out
        let linked = Move { link: Some(id.clone()), ..new_card("v2", "queue") };
        board::post_moves_as(&ox, "hustler", true, &[linked]).unwrap();
        let m = room(&ox).last().cloned().unwrap();
        assert!(tags_of(&m).contains(&format!("ref:{id}")));
        let c = card_event(&m).unwrap();
        assert!(c["moves"][0].get("link").is_none(), "the link is a tag, not a cargo field");
        assert!(board::post_moves_as(&ox, "hustler", true, &[Move { link: Some("x".into()), ..new_card("v3", "queue") }]).is_err());
    }

    /// A mention from another seat, rung as for_me in the fold.
    fn mallory_names_rocky(o: &super::signed_read_tests::Oracle, text: &str) -> String {
        let env = Envelope::sign(&sk(3), "hustler", "mallory", Kind::Say, text, None, vec!["to:rocky".into(), "ask:reply".into()], now_ms());
        let mut m = o.ox(2);
        m.cfg.handle = "mallory".into();
        m.cfg.token = o.mallory_token.clone();
        m.post("/observations", &serde_json::to_value(&env).unwrap()).unwrap()["id"].as_str().unwrap_or("").to_string()
    }

    #[test]
    fn the_nudge_appears_only_with_unread_and_a_read_moves_the_cursor() {
        let o = oracle();
        let ox = o.ox(2);
        let fold = || ox.get("/fold?limit=200").unwrap();
        assert_eq!(board::nudge_line(&fold(), "rocky"), None, "NEGATIVE CONTROL: nothing unread, no line");
        mallory_names_rocky(&o, "@rocky can you check the ledger?");
        let line = board::nudge_line(&fold(), "rocky").expect("an unread ask for rocky");
        assert_eq!(line, "· 1 new for you in #hustler (1 ask) — ox inbox");
        // reading the room (what ox read / ox context do in text) moves the mark: the line goes away
        board::ack_room(&ox, "hustler").unwrap();
        assert_eq!(board::nudge_line(&fold(), "rocky"), None, "a read moves the cursor");
        // updates never count
        let upd = json!({"items": [{"for_me": true, "author": "mallory", "channel": "hustler", "tags": ["react:go", "reply:OBS-x"]}]});
        assert_eq!(board::nudge_line(&upd, "rocky"), None);
    }

    #[test]
    fn state_set_fills_the_parents_and_keeps_the_other_fields() {
        let o = oracle();
        let ox = o.ox(2);
        let write = |sets: &[&str]| {
            let st = ox.room_state("rocky").unwrap();
            let heads: Vec<String> = st["heads"].as_array().into_iter().flatten().filter_map(|h| h.as_str().map(str::to_owned)).collect();
            let sets: Vec<String> = sets.iter().map(|s| s.to_string()).collect();
            let rev = board::state_revision("rocky", &heads, &st["fields"], &sets).unwrap();
            ox.write(Kind::Say, "rocky", "State updated", Some(rev.to_string()), vec!["room-context:v1".into(), "intent:status".into()]).unwrap();
        };
        write(&["purpose=the bus", "current=measuring"]);
        write(&["next=ship the gate"]);
        let st = ox.room_state("rocky").unwrap();
        assert_eq!(st["state"], "alive", "one head: the second revision named the first as its parent");
        assert_eq!(st["heads"].as_array().unwrap().len(), 1);
        assert_eq!(st["fields"]["purpose"], "the bus", "fields not set are kept");
        assert_eq!(st["fields"]["next"], "ship the gate");
        assert!(board::state_revision("rocky", &[], &Value::Null, &["mood=good".into()]).is_err());
        assert!(board::state_revision("rocky", &[], &Value::Null, &[]).is_err());
        // A seat's room (Blaze, 2026-09-28): kind persona boot tools rooms rules — stored only when written, so a
        // project room stays eight fields; kind is one of three words; an emptied seat field is removed.
        assert!(!st["fields"].as_object().unwrap().contains_key("kind"), "no seat field before one is set");
        assert!(board::state_revision("rocky", &[], &Value::Null, &["kind=robot".into()]).is_err());
        write(&["kind=mind", "boot=Rocky; wake with ox brief", "rooms=#axxis the OS · #hustler the product"]);
        let st = ox.room_state("rocky").unwrap();
        assert_eq!(st["fields"]["kind"], "mind");
        assert_eq!(st["fields"]["rooms"], "#axxis the OS · #hustler the product");
        assert_eq!(st["fields"]["purpose"], "the bus", "project fields kept beside seat fields");
        assert!(!st["fields"].as_object().unwrap().contains_key("tools"), "an unset seat field is absent, not \"\"");
        write(&["rooms="]);
        let st = ox.room_state("rocky").unwrap();
        assert!(!st["fields"].as_object().unwrap().contains_key("rooms"), "an emptied seat field is removed");
        assert_eq!(st["fields"]["kind"], "mind", "the others stay");
        let brief = oracle_mcp::seat_text("rocky", &st);
        assert!(brief.starts_with("ME         #rocky · mind · State OBS-") && brief.contains("BOOT       Rocky; wake with ox brief"), "{brief}");
    }

    #[test]
    fn an_item_is_declared_by_its_kind_and_a_state_is_refused() {
        assert_eq!(board::item_decl("R1=requirement").unwrap(), ("item:R1=requirement".to_string(), None));
        assert_eq!(board::item_decl("Q2=question:is the book live?").unwrap().1.as_deref(), Some("is the book live?"));
        let e = board::item_decl("R1=open").unwrap_err().to_string();
        assert!(e.contains("is a state") && e.contains("ox move"), "{e}");
        assert!(board::item_decl("R1=built").is_err() && board::item_decl("R1=banana").is_err());
        // NEGATIVE CONTROL for the hole that was hit: a raw --tag item:R1=open is refused too
        assert!(board::say_refuses(&["item:R1=open".into()]).is_err());
        assert!(board::say_refuses(&["item:R1=requirement".into()]).is_ok());
        // on a reply a state is a move of the parent's item, which oracle-core reads
        assert_eq!(board::item_tag("R1=built", true).unwrap().0, "item:R1=built");
        assert!(board::say_refuses_on(&["item:R1=built".into()], true).is_ok());
    }

    #[test]
    fn take_idle_done_declare_presence_and_move_the_card() {
        let o = oracle();
        let ox = o.ox(2);
        board::post_moves_as(&ox, "hustler", true, &[new_card("q3", "queue")]).unwrap();
        let presence = || {
            let v = ox.get("/search?channel=rocky&tag=status-key:presence:rocky&limit=5").unwrap();
            let mut items = v["items"].as_array().cloned().unwrap_or_default();
            items.sort_by_key(|o| o["seq"].as_i64().unwrap_or(0));
            let c: Value = serde_json::from_str(items.last().unwrap()["cargo"].as_str().unwrap()).unwrap();
            c["presence"].clone()
        };
        // take = the card in_progress with me as owner + working on it
        let mv = Move { id: "q3".into(), status: "in_progress".into(), owner: Some("rocky".into()), ..Default::default() };
        board::post_moves_as(&ox, "hustler", true, &[mv]).unwrap();
        board::post_presence(&ox, board::Mode::Working { what: "q3", room: Some("hustler"), card: Some("q3") }, 1_000).unwrap();
        let c = card_event(room(&ox).last().unwrap()).unwrap();
        assert_eq!((c["moves"][0]["to"].as_str(), c["moves"][0]["owner"].as_str()), (Some("in_progress"), Some("rocky")));
        let p = presence();
        assert_eq!((p["mode"].as_str(), p["card"].as_str(), p["since_ms"].as_i64(), p["rings"].as_str()), (Some("working"), Some("q3"), Some(1_000), Some("off")));
        // waiting, with what for, in the seat's own room: rings on
        board::post_presence(&ox, board::Mode::Waiting { what: Some("Blaze") }, 2_000).unwrap();
        let p = presence();
        assert_eq!((p["mode"].as_str(), p["what"].as_str(), p["rings"].as_str()), (Some("waiting"), Some("Blaze"), Some("on")));
        board::post_presence(&ox, board::Mode::Working { what: "the ledger replay", room: None, card: None }, 3_000).unwrap();
        assert_eq!(presence()["what"], "the ledger replay");
        assert_eq!(board::own_room("bobby-core"), "bobby");
        let (_, tags, _) = board::presence_status("bobby-core", board::Mode::Waiting { what: None }, 0);
        assert!(tags.contains(&"status-key:presence:bobby-core".to_string()));
    }

    #[test]
    fn a_cargo_less_or_archived_state_line_is_skipped_but_a_bad_cargo_still_degrades() {
        let o = oracle();
        let ox = o.ox(2);
        let rev = |parents: Vec<String>, current: &str| {
            let r = board::state_revision("rocky", &parents, &Value::Null, &[format!("current={current}")]).unwrap();
            ox.write(Kind::Say, "rocky", "State updated", Some(r.to_string()), vec!["room-context:v1".into(), "intent:status".into()]).unwrap()["id"]
                .as_str().unwrap().to_string()
        };
        let a = rev(vec![], "one");
        // the door stripped the cargo: tagged, no revision
        ox.write(Kind::Say, "rocky", "State updated (no cargo)", None, vec!["room-context:v1".into(), "intent:status".into()]).unwrap();
        let b = rev(vec![a.clone()], "two");
        let c = rev(vec![b.clone()], "three, then archived");
        ox.write(Kind::Say, "rocky", "Archived: out of the view.", None, vec![format!("archives:{c}"), format!("reply:{c}"), "intent:status".into()]).unwrap();
        let st = ox.room_state("rocky").unwrap();
        assert_eq!(st["state"], "alive", "{st}");
        assert_eq!(st["heads"], json!([b]), "the newest verified, unarchived revision is the head");
        assert_eq!(st["fields"]["current"], "two");
        assert_eq!(st["skipped"].as_array().unwrap().len(), 2);
        // NEGATIVE CONTROL: a line WITH cargo that is not a valid revision still degrades the State
        ox.write(Kind::Say, "rocky", "State updated (bad)", Some(json!({"schema": "room-context/1", "channel": "elsewhere"}).to_string()),
            vec!["room-context:v1".into(), "intent:status".into()]).unwrap();
        let st = ox.room_state("rocky").unwrap();
        assert_eq!(st["state"], "degraded");
        assert_eq!(st["bad"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_proxy_refusal_says_it_is_the_proxy_not_oracle() {
        let d = refusal_detail("error code: 1010\n");
        assert!(d.starts_with("Cloudflare 1010"), "{d}");
        assert!(d.contains("not Oracle"), "{d}");
        // negative control: Oracle's own refusal keeps its words
        assert_eq!(refusal_detail(r#"{"error":"unauthorized","detail":"unauthorized: missing bearer token"}"#),
                   "unauthorized: missing bearer token");
        assert_eq!(refusal_detail(""), "no body");
    }

    #[test]
    fn a_head_is_every_seats_revision_oldest_first() {
        // policy.p1 as it happened: kepler wrote it, kepler retired it, wags closed a head of the same subject later.
        let v = json!({"hits": [
            {"kind":"state","id":"OBS-3","author":"wags-core","created_ms":3,"content":"withdrawn","tags":["subject:policy.p1"],
             "cargo":"{\"owner\":\"wags-core\",\"state\":\"done\",\"subject\":\"policy.p1\"}"},
            {"kind":"state","id":"OBS-1","author":"kepler","created_ms":1,"content":"Leverage at most 10x","tags":["subject:policy.p1"],
             "cargo":"{\"owner\":\"kepler\",\"state\":\"noted\",\"subject\":\"policy.p1\"}"},
            {"kind":"state","id":"OBS-2","author":"kepler","created_ms":2,"content":"Retired: Blaze decided V1","tags":["subject:policy.p1"],
             "cargo":"{\"owner\":\"kepler\",\"state\":\"done\",\"subject\":\"policy.p1\"}"},
            {"kind":"message","id":"OBS-9","author":"bobby-core","created_ms":4,"content":"P1 waits on Blaze","tags":["subject:policy.p1"]}
        ]});
        let revs = head_revisions(&v);
        let ids: Vec<&str> = revs.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["OBS-1", "OBS-2", "OBS-3"], "a message about the subject is not a revision of it");
        let line = state_line("policy.p1", &revs);
        assert!(line.starts_with("STATE policy.p1 [done] wags-core ") && line.contains("made by kepler, 3 revisions"), "{line}");
        let text = head_text("policy.p1", &revs);
        assert!(text.contains("Retired: Blaze decided V1"), "the retirement shows in the history: {text}");
    }

    #[test]
    fn a_subject_comes_from_the_tag_or_the_cargo() {
        assert_eq!(subject_of(&json!({"tags":["subject:policy.p1"]})).as_deref(), Some("policy.p1"));
        assert_eq!(subject_of(&json!({"tags":[],"cargo":"{\"subject\":\"rocky-now\"}"})).as_deref(), Some("rocky-now"));
        assert_eq!(subject_of(&json!({"tags":["room:x"]})), None);
    }

}

#[cfg(test)]
mod paging_tests {
    //! `ox read --before/--starts` and `ox get --thread` against a real in-process Oracle: the pages join up, end on
    //! `next_before: null`, and say so when a thread is only partly shown.
    use super::signed_read_tests::oracle;
    use super::*;

    #[test]
    fn a_room_read_in_pages_is_the_whole_room_once() {
        let o = oracle();
        let ox = o.ox(2);
        let mut said = vec![];
        for i in 0..7 {
            let text = if i % 3 == 0 { format!("[Message state: {i}]") } else { format!("plain {i}") };
            said.push(ox.write(Kind::Say, "paging", &text, None, vec![]).unwrap()["id"].as_str().unwrap().to_string());
        }
        let (mut seen, mut before, mut pages) = (vec![], None, 0);
        loop {
            let v = ox.get(&read_path("paging", 3, "all", "", before, None, None)).unwrap();
            pages += 1;
            assert!(v["scanned"].is_i64() && v.get("next_before").is_some(), "{v}");
            seen.extend(v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()));
            match v["next_before"].as_i64() {
                Some(b) => before = Some(b),
                None => break,
            }
            assert!(pages < 10, "the pages never end");
        }
        seen.reverse();
        assert_eq!(seen, said, "every message once, in order, across {pages} pages");
        let only = ox.get(&read_path("paging", 50, "all", "", None, Some("[Message state:"), None)).unwrap();
        let texts: Vec<&str> = only["items"].as_array().unwrap().iter().map(|i| i["content"].as_str().unwrap()).collect();
        assert_eq!(texts, ["[Message state: 6]", "[Message state: 3]", "[Message state: 0]"]);
        // negative control: without the prefix the same read is the whole room
        assert_eq!(ox.get(&read_path("paging", 50, "all", "", None, None, None)).unwrap()["items"].as_array().unwrap().len(), 7);
    }

    #[test]
    fn a_thread_read_pages_its_replies_and_says_when_it_is_partial() {
        let o = oracle();
        let ox = o.ox(2);
        let root = ox.write(Kind::Say, "paging", "root", None, vec![]).unwrap()["id"].as_str().unwrap().to_string();
        let mut replies = vec![];
        for i in 0..5 {
            let tags = vec![format!("reply:{root}")];
            replies.push(ox.write(Kind::Say, "paging", &format!("reply {i}"), None, tags).unwrap()["id"].as_str().unwrap().to_string());
        }
        // asked from a reply, it is the same thread
        let first = ox.get(&thread_path(&replies[2], None, Some(2), false)).unwrap();
        assert_eq!(first["root"]["id"], root.as_str());
        assert_eq!(first["thread"]["replies"], 5);
        assert!(first["next_before"].is_i64(), "two of five is partial: {first}");
        let (mut seen, mut before) = (vec![], None);
        loop {
            let v = ox.get(&thread_path(&root, before, Some(2), true)).unwrap();
            seen.extend(v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()));
            match v["next_before"].as_i64() {
                Some(b) => before = Some(b),
                None => break,
            }
        }
        seen.reverse();
        assert_eq!(seen, replies);
        let whole = ox.get(&thread_path(&root, None, Some(200), false)).unwrap();
        assert!(whole["next_before"].is_null(), "{whole}");
        assert!(ox.get(&thread_path(&root, None, Some(201), false)).is_err(), "a page is at most 200");
    }

    #[test]
    fn the_cli_contract_parses_and_refuses_page_flags_without_thread() {
        // clap's derived parse of `Cmd` (66 verbs) overflows the test thread's 2 MB stack in a debug build since the
        // Plan verb (2026-10-20); the binary's main thread has 8 MB. Until Cmd is split into groups (the tools fold),
        // this test parses on a 32 MB thread.
        std::thread::Builder::new().stack_size(32 << 20).spawn(cli_contract_body).unwrap().join().unwrap();
    }
    fn cli_contract_body() {
        let c = Cli::try_parse_from(["ox", "read", "dot", "--json", "--before", "41", "--starts", "[Message state:"]).unwrap();
        assert!(c.json);
        assert!(matches!(c.cmd, Cmd::Read { before: Some(41), starts: Some(ref p), .. } if p == "[Message state:"));
        let g = Cli::try_parse_from(["ox", "get", "OBS-x", "--thread", "--before", "9", "--limit", "20", "--json"]).unwrap();
        assert!(matches!(g.cmd, Cmd::Get { thread: true, before: Some(9), limit: Some(20), cargo: false, .. }));
        assert!(Cli::try_parse_from(["ox", "get", "OBS-x", "--before", "9"]).is_err());
        the_fold_runs_every_new_form_as_the_old_arm();
    }

    /// The fold of 2026-10-21 (docs/ox.md): each new form becomes the Cmd its old name parsed to; the old names
    /// still parse (hidden) and are named by old_name_of; a verb that never existed is refused (the control).
    fn the_fold_runs_every_new_form_as_the_old_arm() {
        let folded = |args: &[&str]| fold(Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}")).cmd).unwrap();
        assert!(matches!(folded(&["ox", "seats", "--live"]), Cmd::Presence));
        assert!(matches!(folded(&["ox", "brief", "--hello"]), Cmd::Hello));
        assert!(matches!(folded(&["ox", "inbox", "--asks", "--seat", "noor"]), Cmd::Asks { seat: Some(ref s), .. } if s == "noor"));
        assert!(matches!(folded(&["ox", "read"]), Cmd::Rooms { since: None }));
        assert!(matches!(folded(&["ox", "read", "--since", "7d"]), Cmd::Rooms { since: Some(ref d) } if d == "7d"));
        assert!(matches!(folded(&["ox", "read", "--follow", "a,b"]), Cmd::Watch { ref room } if room == "a,b"));
        assert!(matches!(folded(&["ox", "read", "--follow"]), Cmd::Watch { ref room } if room.is_empty()));
        assert!(matches!(folded(&["ox", "read", "dot", "--before", "41"]), Cmd::Read { room: Some(ref r), before: Some(41), .. } if r == "dot"));
        assert!(matches!(folded(&["ox", "digest", "hustler", "--threads", "--before", "7"]), Cmd::Threads { ref room, before: Some(7), limit: 20 } if room == "hustler"));
        assert!(matches!(folded(&["ox", "digest", "--all", "--days", "3"]), Cmd::Hallway { days: 3 }));
        assert!(matches!(folded(&["ox", "digest"]), Cmd::Hallway { days: 14 }));
        assert!(matches!(folded(&["ox", "digest", "hustler"]), Cmd::Digest { room: Some(ref r), .. } if r == "hustler"));
        assert!(matches!(folded(&["ox", "state", "axxis", "--map"]), Cmd::Map { ref room, id: None, set: None } if room == "axxis"));
        assert!(matches!(folded(&["ox", "state", "axxis", "OBS-1", "--map"]), Cmd::Map { id: Some(ref i), .. } if i == "OBS-1"));
        assert!(matches!(folded(&["ox", "state", "axxis", "--map", "--set", "the map"]), Cmd::Map { set: Some(ref m), .. } if m == "the map"));
        assert!(fold(Cli::try_parse_from(["ox", "state", "axxis", "OBS-1"]).unwrap().cmd).is_err(), "an id without --map is refused");
        assert!(matches!(folded(&["ox", "recall", "DASH", "--where", "--any"]), Cmd::Where { ref words, any: true } if words == &["DASH"]));
        assert!(matches!(folded(&["ox", "recall", "--terms"]), Cmd::Terms { limit: 100 }));
        assert!(matches!(folded(&["ox", "recall", "--eval", "set.jsonl", "--param", "k=v"]), Cmd::Eval { limit: 10, ref params, .. } if params == &["k=v"]));
        assert!(matches!(folded(&["ox", "recall", "DASH 65", "--room", "hustler", "--newest"]), Cmd::About { ref room, ref terms, limit: 25 } if room == "hustler" && terms == &["DASH", "65"]));
        assert!(matches!(folded(&["ox", "recall", "DASH", "--room", "hustler"]), Cmd::Recall { room: Some(_), newest: false, .. }));
        assert!(Cli::try_parse_from(["ox", "recall"]).is_err(), "recall needs words unless --terms or --eval");
        assert!(matches!(folded(&["ox", "head"]), Cmd::Heads { owner: None, all: false }));
        assert!(matches!(folded(&["ox", "head", "--all"]), Cmd::Heads { all: true, .. }));
        assert!(matches!(folded(&["ox", "head", "--exp", "--track", "qsb"]), Cmd::Experiments { track: Some(ref t), limit: 60, .. } if t == "qsb"));
        assert!(matches!(folded(&["ox", "head", "plan.oracle.search"]), Cmd::Head { subject: Some(_), .. }));
        assert!(matches!(folded(&["ox", "card"]), Cmd::Cards { mine: false, .. }));
        assert!(matches!(folded(&["ox", "card", "--mine", "--state", "done"]), Cmd::Cards { mine: true, status: Some(ref st), .. } if st == "done"));
        assert!(matches!(folded(&["ox", "card", "hustler", "q3", "take", "--summary", "on it"]), Cmd::Take { ref card, note: Some(ref n), .. } if card == "q3" && n == "on it"));
        assert!(matches!(folded(&["ox", "card", "hustler", "q3", "done", "--evidence", "OBS-1", "--handoff", "to emmy"]), Cmd::Done { ref handoff, .. } if handoff == "to emmy"));
        assert!(matches!(folded(&["ox", "card", "hustler", "q3", "done"]), Cmd::Card { status: Some(ref st), .. } if st == "done"), "a plain done is the board move it always was");
        assert!(matches!(folded(&["ox", "react", "hustler", "OBS-1", "claim", "Q2", "--note", "mine"]), Cmd::Claim { ref item, ref note, .. } if item == "Q2" && note == "mine"));
        assert!(matches!(folded(&["ox", "react", "hustler", "OBS-1", "vote", "Q1=for", "--note", "yes"]), Cmd::Vote { ref what, .. } if what == "Q1=for"));
        assert!(fold(Cli::try_parse_from(["ox", "react", "hustler", "OBS-1", "claim"]).unwrap().cmd).is_err(), "a claim names its item");
        assert!(matches!(folded(&["ox", "react", "hustler", "OBS-1", "go"]), Cmd::React { ref kind, .. } if kind == "go"));
        assert!(matches!(folded(&["ox", "seen", "hustler", "axxis"]), Cmd::Ack { ref rooms, all: false } if rooms == &["hustler", "axxis"]));
        assert!(matches!(folded(&["ox", "seen", "--all"]), Cmd::Ack { all: true, .. }));
        assert!(matches!(folded(&["ox", "seen", "OBS-1", "OBS-2"]), Cmd::Seen { ref targets, .. } if targets.len() == 2));
        assert!(fold(Cli::try_parse_from(["ox", "seen", "OBS-1", "hustler"]).unwrap().cmd).is_err(), "ids and rooms do not mix");
        assert!(matches!(folded(&["ox", "dot", "anchor", "tip", "j"]), Cmd::Anchor { .. }));
        assert!(matches!(folded(&["ox", "dot", "authority", "sign-request", "-"]), Cmd::Authority { .. }));
        // the old names still parse, hidden, and say their new form
        for old in ["about hustler DASH", "where DASH", "terms", "heads", "cards", "idle", "hallway", "presence", "rooms", "threads hustler", "experiments", "asks", "watch", "hello", "map axxis", "ack --all", "take hustler q3", "move hustler OBS-1 R2=built --note n", "claim hustler OBS-1 Q2 --note n", "vote hustler OBS-1 Q1=for --note n", "anchor tip j", "authority sign-request -"] {
            let mut args = vec!["ox"];
            args.extend(old.split(' '));
            assert!(Cli::try_parse_from(&args).is_ok(), "{old}");
            assert!(old_name_of(args[1]).is_some(), "{old} is on the list");
        }
        assert!(old_name_of("recall").is_none() && old_name_of("nonesuch").is_none());
        assert!(Cli::try_parse_from(["ox", "nonesuch"]).is_err(), "a verb that never existed is refused (the control)");
        // the hidden names are not in the help
        let visible: Vec<String> = <Cli as clap::CommandFactory>::command().get_subcommands().filter(|c| !c.is_hide_set()).map(|c| c.get_name().to_string()).collect();
        assert!(!visible.iter().any(|v| old_name_of(v).is_some()), "{visible:?}");
        assert_eq!(visible.len(), 54, "{visible:?}"); // + types, retype (31 Oct 2026, T7); + delta (1 Nov 2026, the waking read); + edit (3 Nov 2026, docs/EDIT.md); + capture, reflect (4 Nov 2026, docs/CAPTURE.md, docs/REFLECT.md); + verify (4 Nov 2026, docs/VERIFY.md)
    }
}

/// `ox ack` marks read only what was named: rooms, or every room with --all. Nothing named is a refusal, not --all.
pub fn ack_scope(rooms: &[String], all: bool) -> bool {
    all || !rooms.is_empty()
}

#[cfg(test)]
mod ack_scope_tests {
    use super::ack_scope;
    #[test]
    fn nothing_named_refuses_and_all_or_rooms_pass() {
        assert!(!ack_scope(&[], false), "ox ack alone must refuse");
        assert!(ack_scope(&[], true));
        assert!(ack_scope(&["hustler".to_string()], false));
    }
}

#[cfg(test)]
mod dot_tests {
    use super::signed_read_tests::oracle;
    use super::*;
    use oracle_proto::microdot as md;

    /// docs/MICRODOT.md §6: a message said with --dot carries dot:<id> and file:<sha>; the face in the room reads
    /// back to that dot, sealed by the seat's key, chained to the seat's Oracle head, with the message as its body;
    /// A signed write whose footer stalls: the command succeeds, the write lands once, the footer is one fold
    /// attempt within its budget and nothing follows it (the receipt is printed by `print_written` before
    /// `board::nudge` runs, so stdout is independent by construction; this drives the real `say` handler).
    #[test]
    fn a_signed_write_with_a_stalled_footer_succeeds_within_the_budget_and_asks_once() {
        use super::signed_read_tests::Oracle;
        let lab = oracle();
        let direct_home = lab.seat_home("rocky", 2, None);
        let upstream: Value = serde_json::from_str(&std::fs::read_to_string(direct_home.join("config.json")).unwrap()).unwrap();
        let (proxy, seen) = Oracle::stalling_proxy(upstream["url"].as_str().unwrap(), Duration::from_secs(6));
        let home = lab.seat_home("rocky", 2, Some(&proxy));
        let t = std::time::Instant::now();
        // the command's own stack (main's is 8 MiB; a test thread's 2 MiB overflows under clap + the handler)
        let home_arg = home.to_str().unwrap().to_string();
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || run_cli(Cli::parse_from(["ox", "--home", &home_arg, "say", "svc", "a write whose footer stalls"])))
            .unwrap()
            .join()
            .unwrap()
            .expect("the write succeeds although the footer did not answer");
        let took = t.elapsed();
        assert!(took < Duration::from_secs(5), "the write plus one footer budget: {took:?}");
        let direct = lab.ox(2);
        let found = direct.get("/search?q=stalls&channel=svc&author=rocky").unwrap();
        assert_eq!(found["items"].as_array().map_or(0, |a| a.len()), 1, "the write landed exactly once: {found}");
        std::thread::sleep(Duration::from_secs(7));
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.iter().filter(|r| r.as_str() == "POST /observations").count(), 1, "one write: {seen:?}");
        assert_eq!(seen.iter().filter(|r| r.starts_with("GET /fold")).count(), 1, "one fold attempt, no retry after the hold: {seen:?}");
        assert!(seen.iter().all(|r| !r.starts_with("POST /fold")), "no cursor ack: {seen:?}");
    }

    /// 21 Oct: after every write the footer folded 200 items (34–40 s for a backlogged seat) against ureq's 30 s
    /// deadline, was retried as "no answer", then tried on the fallback address: four 40 s folds on the seat's
    /// lane, 131 s per `ox say`, and the line lost silently. Emmy's policy:
    /// one small total budget for the whole footer, one attempt each, no retry, no fan-out, and "unavailable"
    /// said rather than zero unread implied.
    #[test]
    fn the_footer_is_bounded_asks_once_fans_out_nowhere_and_never_implies_zero_unread() {
        let lab = oracle();
        let (stalled, seen) = super::signed_read_tests::Oracle::fake(&[], Duration::from_secs(6));
        let (fallback, seen_fallback) = super::signed_read_tests::Oracle::fake(&[], Duration::from_secs(6));
        let mut ox = lab.ox(2);
        ox.cfg.url = stalled;
        ox.cfg.fallback_urls = vec![fallback];
        let t = std::time::Instant::now();
        let (lines, missing) = board::footer(&ox, Duration::from_secs(2));
        let took = t.elapsed();
        assert!(took < Duration::from_secs(3), "the whole footer ends within its budget: {took:?}");
        assert!(lines.is_empty(), "nothing is claimed: {lines:?}");
        assert_eq!(missing.len(), 2, "both halves are said to be unavailable: {missing:?}");
        assert!(missing[0].starts_with("inbox summary:") && missing[1].starts_with("open asks:"), "{missing:?}");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.iter().filter(|r| r.starts_with("GET /fold")).count(), 1, "the fold was asked exactly once: {seen:?}");
        assert!(seen.iter().all(|r| r.starts_with("GET ")), "no write and no cursor ack reached the server: {seen:?}");
        assert!(seen.iter().filter(|r| r.starts_with("GET /asks")).count() <= 1, "the asks read is one attempt at most: {seen:?}");
        assert!(seen_fallback.lock().unwrap().is_empty(), "the fallback address was never asked");
        assert!(board::nudge_line(&json!({}), "rocky").is_none(), "an empty answer claims nothing either");
    }

    #[test]
    fn repeated_nudges_do_not_repeat_network_reads_but_explicit_footer_stays_fresh() {
        let lab = oracle();
        let (quick, seen) = super::signed_read_tests::Oracle::fake(&["/fold", "/asks"], Duration::from_secs(6));
        let mut ox = lab.ox(2);
        ox.cfg.url = quick;
        ox.cfg.fallback_urls = vec![];
        board::nudge_at(&ox, 100_000);
        board::nudge_at(&ox, 100_001);
        assert_eq!(seen.lock().unwrap().len(), 2, "automatic footer reads only once per window");
        board::footer(&ox, Duration::from_secs(2));
        assert_eq!(seen.lock().unwrap().len(), 4, "direct reads are not throttled");
    }

    /// The asks read gets what the fold left of the budget, and a server that answers both in time is unchanged.
    #[test]
    fn the_asks_read_shares_the_footer_budget_with_the_fold() {
        let lab = oracle();
        let (fast_fold, seen) = super::signed_read_tests::Oracle::fake(&["/fold"], Duration::from_secs(6));
        let mut ox = lab.ox(2);
        ox.cfg.url = fast_fold;
        ox.cfg.fallback_urls = vec![];
        let t = std::time::Instant::now();
        let (lines, missing) = board::footer(&ox, Duration::from_secs(2));
        let took = t.elapsed();
        assert!(took < Duration::from_secs(3), "the stalled asks read ends with the budget: {took:?}");
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(missing.len(), 1, "only the asks half is unavailable: {missing:?}");
        assert!(missing[0].starts_with("open asks:"), "{missing:?}");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen, vec!["GET /fold?limit=50".to_string(), "GET /asks?state=open&count=1".to_string()], "one attempt each, in order");
        // NEGATIVE CONTROL: both answered in time, nothing unavailable, well under the budget
        let (quick, seen) = super::signed_read_tests::Oracle::fake(&["/fold", "/asks"], Duration::from_secs(6));
        ox.cfg.url = quick;
        let t = std::time::Instant::now();
        let (lines, missing) = board::footer(&ox, Duration::from_secs(2));
        assert!(t.elapsed() < Duration::from_secs(1));
        assert!(lines.is_empty() && missing.is_empty(), "{lines:?} {missing:?}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    /// The footer counts the seat's own asks from the count answer, against a real server (Rocky, 1 Nov 2026: the whole
    /// list was 168 KB for rocky and the footer timed out on every command): the line is right, the request asks for
    /// counts, and the answer it read carries no rows.
    #[test]
    fn the_footer_line_comes_from_the_count_answer_and_reads_no_rows() {
        let o = oracle();
        let ox = o.ox(2);
        let post = |door: &Ox, key: u8, who: &str, text: &str, tags: &[&str]| -> String {
            let env = Envelope::sign(&super::signed_read_tests::sk(key), "hustler", who, Kind::Say, text, None, tags.iter().map(|t| t.to_string()).collect(), now_ms());
            door.post("/observations", &serde_json::to_value(&env).unwrap()).unwrap()["id"].as_str().unwrap().to_string()
        };
        let first = post(&ox, 2, "rocky", "ship R-1", &["to:mallory", "ask:act"]);
        post(&ox, 2, "rocky", "ship R-2", &["to:mallory", "ask:act"]);
        let mut m = o.ox(2);
        m.cfg.handle = "mallory".into();
        m.cfg.token = o.mallory_token.clone();
        post(&m, 3, "mallory", "looking", &[&format!("reply:{first}")]);
        let (lines, missing) = board::footer(&ox, Duration::from_secs(2));
        assert!(missing.is_empty(), "{missing:?}");
        assert!(lines.contains(&"· mine: 1 open, 1 answered, close them: ox asks --mine".to_string()), "{lines:?}");
        let counted = ox.get("/asks?state=open&count=1").unwrap();
        assert_eq!(counted["schema"], "asks-count/1", "{counted}");
        assert!(counted.as_object().unwrap().values().all(|v| !v.is_array() && !v.is_object()), "no rows: {counted}");
        // NEGATIVE CONTROL: `ox asks` still reads the list whole
        let whole = ox.get("/asks?state=open").unwrap();
        assert_eq!(whole["im_waiting_on"].as_array().map(Vec::len), Some(2), "{whole}");
        let (quick, seen) = super::signed_read_tests::Oracle::fake(&["/fold", "/asks"], Duration::from_secs(6));
        let mut fast = o.ox(2);
        fast.cfg.url = quick;
        board::footer(&fast, Duration::from_secs(2));
        assert!(seen.lock().unwrap().contains(&"GET /asks?state=open&count=1".to_string()), "{:?}", seen.lock().unwrap());
    }

    /// `ox delta` and `oracle_delta` against a real server (Blaze, 1 Nov 2026: the waking read is the diff since the last
    /// known state): the page names what came after rocky's RESUME and nothing from before, the CLI and the MCP tool print
    /// the same text from the same object, and the command parses with and without --since.
    #[test]
    fn delta_shows_only_what_came_after_the_resume_and_ox_and_mcp_agree() {
        let o = oracle();
        let ox = o.ox(2);
        let post = |door: &Ox, key: u8, who: &str, room: &str, text: &str, tags: &[&str]| -> String {
            let env = Envelope::sign(&super::signed_read_tests::sk(key), room, who, Kind::Say, text, None, tags.iter().map(|t| t.to_string()).collect(), now_ms());
            door.post("/observations", &serde_json::to_value(&env).unwrap()).unwrap()["id"].as_str().unwrap().to_string()
        };
        let mut m = o.ox(2);
        m.cfg.handle = "mallory".into();
        m.cfg.token = o.mallory_token.clone();
        let before = post(&m, 3, "mallory", "hustler", "said before the resume", &["to:rocky"]);
        post(&ox, 2, "rocky", "hustler", "rocky works in hustler", &[]);
        std::thread::sleep(Duration::from_millis(5));
        let resume = post(&ox, 2, "rocky", "rocky", "RESUME: where I stopped", &["resume"]);
        let after = post(&m, 3, "mallory", "hustler", "@rocky said after the resume", &["to:rocky"]);
        let v = oracle_mcp::delta_value(&ox, None).unwrap();
        assert_eq!(v["since"]["id"], resume.as_str(), "{v}");
        let text = oracle_mcp::delta_render(&v);
        assert!(text.contains(&after) && text.contains("#hustler") && text.contains("1 to me"), "{text}");
        assert!(!text.contains(&before), "nothing from before the point: {text}");
        let body = |t: &str| t.split_once('\n').map(|(_, b)| b.to_string()).unwrap_or_default();
        assert_eq!(body(&oracle_mcp::call(&ox, "oracle_delta", &json!({})).unwrap()), body(&text), "ox and the MCP tool print the same page (the clock line aside)");
        // since a record: from `after` on, nothing in hustler is new
        let later = oracle_mcp::delta_value(&ox, Some(&after)).unwrap();
        assert!(later["rooms"].as_array().unwrap().is_empty(), "{later}");
        // the parser is deep: parse on a thread with room for it, as the CLI contract tests do
        let parsed = std::thread::Builder::new().stack_size(32 << 20)
            .spawn(|| Cli::try_parse_from(["ox", "delta"]).is_ok() && matches!(Cli::try_parse_from(["ox", "delta", "--since", "6h"]).map(|c| c.cmd), Ok(Cmd::Delta { since: Some(ref s) }) if s == "6h"))
            .unwrap().join().unwrap();
        assert!(parsed, "ox delta parses with and without --since");
        // NEGATIVE CONTROL: a span in the future is refused by the server, and said
        assert!(oracle_mcp::delta_value(&ox, Some("2999-01-01")).is_err());
    }

    /// a reply said with --dot is a composite naming the parent; a message without a dot is refused by get --dot.
    #[test]
    fn say_dot_mints_a_face_the_room_keeps_and_get_dot_reads_it_back() {
        let o = oracle();
        let ox = o.ox(2);
        let tags = ox.mint_dot("hustler", "# the first dot said\n", None).unwrap();
        assert_eq!(tags.len(), 2, "{tags:?}");
        let dot_id = tags[0].strip_prefix("dot:").unwrap().to_string();
        let sha = tags[1].strip_prefix("file:").unwrap().to_string();
        assert_eq!(dot_id.len(), 64);
        assert_eq!(sha.len(), 64);
        let v = ox.write(Kind::Say, "hustler", "# the first dot said\n", None, tags.clone()).unwrap();
        let id = v["id"].as_str().unwrap().to_string();

        // the face, fetched by the file tag, holds that dot, sealed by rocky's key, body = the text
        let (png, _) = ox.bytes("GET", &format!("/files/{}", urlencode(&sha)), None, None).unwrap();
        let r = md::read_image(&png).unwrap();
        assert!(r.ok, "{:?}", r.why);
        assert_eq!(r.dots[0].dot_id, dot_id);
        let (_, _, rows) = md::png_read(&png).unwrap();
        let (d, _) = md::tiles_in(&rows).iter().find_map(md::read_tile).unwrap();
        assert_eq!(d.body, b"# the first dot said");
        assert_eq!(hex::encode(d.who), pubkey_hex(&super::signed_read_tests::sk(2)));
        assert_eq!(d.ver, 5);
        assert_eq!(d.prevkind, 0, "rocky's first native write: the chain head was genesis, and a kind-2 dot never names a zero prev");
        assert_eq!(d.prev, [0u8; 32]);

        // get --dot: the record names the dot and the face; body and who check out; the predecessor is the record's
        // own prev, verified: genesis (Emmy: the zero-prev rule, explicit)
        let rec = ox.get(&format!("/search?id={}", urlencode(&id))).unwrap();
        assert_eq!(record_tag(&rec, "dot:").as_deref(), Some(dot_id.as_str()));
        assert!(matches!(ox.chain_check(&rec, &d), ChainCheck::Ok(ref w) if w.starts_with("genesis")), "{:?}", ox.chain_check(&rec, &d));
        let out = o.home.join("face.png");
        ox.get_dot(&id, &rec, Some(&out)).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), png, "get --dot writes the face the room keeps");

        // a reply with --dot is a v5 composite naming the parent (rel 1, reply), chained to the first record
        let tags2 = ox.mint_dot("hustler", "and the reply", Some(&id)).unwrap();
        let v2 = ox.write(Kind::Say, "hustler", "and the reply", None, [tags2.clone(), vec![format!("reply:{id}")]].concat()).unwrap();
        let sha2 = tags2[1].strip_prefix("file:").unwrap();
        let (png2, _) = ox.bytes("GET", &format!("/files/{}", urlencode(sha2)), None, None).unwrap();
        let (_, _, rows2) = md::png_read(&png2).unwrap();
        let (d2, _) = md::tiles_in(&rows2).iter().find_map(md::read_tile).unwrap();
        assert_eq!(d2.ver, 5);
        assert_eq!(d2.prevkind, 2, "the second write names the first as its predecessor");
        assert_eq!(d2.flags & 1, 1, "composite");
        let lay = d2.layout.clone().unwrap();
        assert_eq!(lay.len(), 1);
        assert_eq!(lay[0].0, 1, "rel 1 = reply");
        assert_eq!(hex::encode(lay[0].1), dot_id);
        assert_eq!(d2.markdown.as_deref(), Some("and the reply"));
        let rec2 = ox.get(&format!("/search?id={}", urlencode(v2["id"].as_str().unwrap()))).unwrap();
        match ox.chain_check(&rec2, &d2) {
            ChainCheck::Ok(w) => assert!(w.contains(&id), "the record before the reply is the first record: {w}"),
            other => panic!("{other:?}"),
        }
        ox.get_dot(v2["id"].as_str().unwrap(), &rec2, None).unwrap();

        // tampered-prev control: a freshly sealed, valid v5 dot naming the WRONG predecessor (flipping a byte without
        // re-sealing would only test the seal): read and different, so refused at the chain, not at the seal
        let seed = super::signed_read_tests::sk(2).to_bytes();
        let wrong = md::sha256(b"not the head");
        let canon3 = md::pack_v5(&seed, b"a dot naming the wrong predecessor", now_ms() as u64, wrong, 2, [0u8; 32], false, None, 6, false).unwrap();
        let dot3 = hex::encode(md::dot_id_v5(&canon3));
        let png3 = md::face_png(&canon3, None);
        let (body3, _) = ox.bytes("PUT", &format!("/files?room=hustler&name={}.png", &dot3[..16]), Some("image/png"), Some(&png3)).unwrap();
        let sha3 = serde_json::from_slice::<Value>(&body3).unwrap()["sha256"].as_str().unwrap().to_string();
        let v3 = ox.write(Kind::Say, "hustler", "a dot naming the wrong predecessor", None, vec![format!("dot:{dot3}"), format!("file:{sha3}")]).unwrap();
        let rec3 = ox.get(&format!("/search?id={}", urlencode(v3["id"].as_str().unwrap()))).unwrap();
        let d3 = md::parse_any(&canon3).unwrap();
        assert!(matches!(ox.chain_check(&rec3, &d3), ChainCheck::Mismatch(_)), "{:?}", ox.chain_check(&rec3, &d3));
        let err = ox.get_dot(v3["id"].as_str().unwrap(), &rec3, None).unwrap_err().to_string();
        assert!(err.contains("predecessor is not the record's"), "{err}");

        // unknown control: a reader without the author's chain (mallory reading rocky's record): nothing verified,
        // nothing refused, said as unknown
        let mut mallory = o.ox(3);
        mallory.cfg.handle = "mallory".into();
        mallory.cfg.token = o.mallory_token.clone();
        assert!(matches!(mallory.chain_check(&rec2, &d2), ChainCheck::Unknown(_)), "{:?}", mallory.chain_check(&rec2, &d2));
        mallory.get_dot(v2["id"].as_str().unwrap(), &rec2, None).unwrap();

        // a v3 dot (the format before this step) claims no predecessor kind: read as before, nothing checked
        let canon4 = md::pack_v3(&seed, b"an older dot", now_ms() as u64, [0u8; 32]);
        let dot4 = hex::encode(md::sha256(&canon4));
        let png4 = md::face_png(&canon4, None);
        let (body4, _) = ox.bytes("PUT", &format!("/files?room=hustler&name={}.png", &dot4[..16]), Some("image/png"), Some(&png4)).unwrap();
        let sha4 = serde_json::from_slice::<Value>(&body4).unwrap()["sha256"].as_str().unwrap().to_string();
        let v4 = ox.write(Kind::Say, "hustler", "an older dot", None, vec![format!("dot:{dot4}"), format!("file:{sha4}")]).unwrap();
        let rec4 = ox.get(&format!("/search?id={}", urlencode(v4["id"].as_str().unwrap()))).unwrap();
        assert!(matches!(ox.chain_check(&rec4, &md::parse_any(&canon4).unwrap()), ChainCheck::NotClaimed(_)));
        ox.get_dot(v4["id"].as_str().unwrap(), &rec4, None).unwrap();

        // negative control: a plain message has no dot to get
        let plain = ox.write(Kind::Say, "hustler", "no dot here", None, vec![]).unwrap();
        let rec3 = ox.get(&format!("/search?id={}", urlencode(plain["id"].as_str().unwrap()))).unwrap();
        let err = ox.get_dot(plain["id"].as_str().unwrap(), &rec3, None).unwrap_err().to_string();
        assert!(err.contains("not said as a dot"), "{err}");
    }

    #[test]
    fn the_dot_flags_parse() {
        // clap's Cmd is wide: parse on a 32 MB thread, as cli_contract does
        std::thread::Builder::new().stack_size(32 << 20).spawn(dot_flags_body).unwrap().join().unwrap();
    }
    fn dot_flags_body() {
        let s = Cli::try_parse_from(["ox", "say", "hustler", "hello", "--dot"]).unwrap();
        assert!(matches!(s.cmd, Cmd::Say { dot: true, .. }));
        let g = Cli::try_parse_from(["ox", "get", "OBS-1", "--dot", "--out", "/tmp/x.png"]).unwrap();
        assert!(matches!(g.cmd, Cmd::Get { dot: true, out: Some(_), .. }));
        assert!(Cli::try_parse_from(["ox", "get", "OBS-1", "--out", "/tmp/x.png"]).is_err(), "--out needs --dot");
        assert!(Cli::try_parse_from(["ox", "get", "OBS-1", "--dot", "--thread"]).is_err(), "--dot and --thread exclude each other");
    }
}
#[cfg(test)]
mod dot_debug {
    use super::signed_read_tests::oracle;
    use super::*;
    #[test]
    fn shape() {
        let o = oracle();
        let ox = o.ox(2);
        let v = ox.write(Kind::Say, "hustler", "# the first dot said\n", None, vec![]).unwrap();
        let rec = ox.get(&format!("/search?id={}", urlencode(v["id"].as_str().unwrap()))).unwrap();
        eprintln!("TOP KEYS {:?}", rec.as_object().map(|o| o.keys().collect::<Vec<_>>()));
        let r = record_object(&rec).unwrap();
        eprintln!("HIT KEYS {:?}", r.as_object().map(|o| o.keys().collect::<Vec<_>>()));
        eprintln!("CONTENT {:?}", r["content"]);
    }
}

#[cfg(test)]
mod emmy_v5_chain_review {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    #[test]
    fn emmy_genesis_does_not_verify_bad_signature_or_wrong_key() {
        let lab = signed_read_tests::oracle();
        let seed = signed_read_tests::sk(2).to_bytes();
        let canon = oracle_proto::microdot::pack_v5(&seed,b"same body",now_ms() as u64,[0;32],0,[0;32],false,None,6,false).unwrap();
        let dot = oracle_proto::microdot::parse_any(&canon).unwrap();
        let mut results=Vec::new();
        for wrong_key in [false,true] {
            let key=signed_read_tests::sk(if wrong_key {3} else {2});
            let mut env=Envelope::sign(&key,"room","rocky",Kind::Say,"same body",None,vec![],now_ms());
            env=env.chain_to(oracle_proto::GENESIS_PREV,&key,now_ms());
            if !wrong_key {env.content="tampered after signing".into();}
            let cr=oracle_proto::ChainRecord{seq:1,id:format!("OBS-room-20261021-{}",&oracle_proto::link_of(&env.sig)[..12]),pubkey:pubkey_hex(&key),envelope:env,withheld:false};
            assert_eq!(cr.envelope.verify_stored(&cr.pubkey).is_ok(),wrong_key);
            let record_id=cr.id.clone();
            let body=serde_json::to_string(&json!({"records":[cr]})).unwrap();
            let l=std::net::TcpListener::bind("127.0.0.1:0").unwrap();let url=format!("http://{}",l.local_addr().unwrap());
            let worker=std::thread::spawn(move || {
                let (mut stream,_)=l.accept().unwrap();
                let mut reader=BufReader::new(stream.try_clone().unwrap());
                loop {let mut line=String::new();if reader.read_line(&mut line).unwrap()==0||line=="\r\n"{break;}}
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            });
            let mut ox=lab.ox(2);ox.cfg.url=url;ox.cfg.fallback_urls=vec![];
            let r=ox.chain_check(&json!({"seq":1,"author":"rocky","id":record_id,"content":"same body","tags":[]}),&dot);
            worker.join().unwrap();println!("wrong_key={wrong_key}: {r:?}");results.push(r);
        }
        assert!(results.iter().all(|r|!matches!(r,ChainCheck::Ok(_))),"unverified chain envelopes must not produce chain ok: {results:?}");
    }
}

#[cfg(test)]
mod v5_chain_controls {
    //! Isolated controls for `Ox::chain_check` on Emmy's review: a fake chain server
    //! answers `/chain/rocky` with records built here, so each control changes exactly one thing. Unknown stays
    //! unknown for a chain that cannot be read; not claimed stays for a v3 dot; nothing is written anywhere.
    use super::*;
    use oracle_proto::microdot as md;
    use oracle_proto::{link_of, ChainRecord, GENESIS_PREV};
    use std::io::{BufRead, BufReader, Write};

    /// A server for `/chain/{author}` that answers `after=1` with the record itself and anything else with the whole
    /// list, `status` with every answer; it serves a handful of connections and is left to end with the process.
    fn chain_server(status: &'static str, own: ChainRecord, all: Vec<ChainRecord>) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for _ in 0..8 {
                let Ok((mut stream, _)) = l.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                let records = if first.contains("after=1&") { vec![own.clone()] } else { all.clone() };
                let body = if status.starts_with("200") { json!({"records": records}).to_string() } else { json!({"error": "a chain is read by its author or an admin"}).to_string() };
                write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
        });
        url
    }

    fn record(seq: i64, env: Envelope, pubkey: &str) -> ChainRecord {
        ChainRecord { seq, id: format!("OBS-room-20261021-{}", &link_of(&env.sig)[..12]), pubkey: pubkey.to_string(), envelope: env, withheld: false }
    }

    /// The honest case, built once: the predecessor (rocky's first record, chained to genesis), a kind-2 dot naming
    /// its link, the record carrying the dot; `check(status, own, all, requested)` runs one control against it.
    struct Honest {
        pred: ChainRecord,
        own: ChainRecord,
        dot: md::Dot,
    }

    fn honest() -> Honest {
        let key = signed_read_tests::sk(2);
        let seed = key.to_bytes();
        let t = now_ms();
        let pred_env = Envelope::sign(&key, "room", "rocky", Kind::Say, "the first", None, vec![], t).chain_to(GENESIS_PREV, &key, t);
        let pred = record(1, pred_env, &pubkey_hex(&key));
        let link = link_of(&pred.envelope.sig);
        let mut prev = [0u8; 32];
        prev.copy_from_slice(&hex::decode(&link).unwrap());
        let canon = md::pack_v5(&seed, b"same body", t as u64, prev, 2, [0u8; 32], false, None, 6, false).unwrap();
        let dot = md::parse_any(&canon).unwrap();
        let own_env = Envelope::sign(&key, "room", "rocky", Kind::Say, "same body", None, vec![format!("dot:{}", dot.id_hex())], t + 1).chain_to(&link, &key, t + 1);
        let own = record(2, own_env, &pubkey_hex(&key));
        Honest { pred, own, dot }
    }

    fn check(status: &'static str, h: &Honest, own: ChainRecord, all: Vec<ChainRecord>, requested: Value) -> ChainCheck {
        let lab = signed_read_tests::oracle();
        let mut ox = lab.ox(2);
        ox.cfg.url = chain_server(status, own, all);
        ox.cfg.fallback_urls = vec![];
        ox.chain_check(&requested, &h.dot)
    }

    fn requested(h: &Honest) -> Value {
        json!({"seq": 2, "author": "rocky", "id": h.own.id, "content": "same body", "tags": [format!("dot:{}", h.dot.id_hex())]})
    }

    #[test]
    fn the_honest_record_and_predecessor_verify() {
        let h = honest();
        match check("200 OK", &h, h.own.clone(), vec![h.pred.clone(), h.own.clone()], requested(&h)) {
            ChainCheck::Ok(w) => assert!(w.contains(&h.pred.id), "{w}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_predecessor_whose_signature_does_not_verify_is_a_mismatch() {
        let h = honest();
        let mut pred = h.pred.clone();
        pred.envelope.content = "edited after signing".into();
        assert!(matches!(check("200 OK", &h, h.own.clone(), vec![pred, h.own.clone()], requested(&h)), ChainCheck::Mismatch(_)));
    }

    #[test]
    fn a_record_whose_own_signature_does_not_verify_is_a_mismatch() {
        let h = honest();
        let mut own = h.own.clone();
        own.envelope.content = "edited after signing".into();
        let r = check("200 OK", &h, own.clone(), vec![h.pred.clone(), own], json!({"seq": 2, "author": "rocky", "id": h.own.id, "content": "edited after signing", "tags": [format!("dot:{}", h.dot.id_hex())]}));
        assert!(matches!(r, ChainCheck::Mismatch(_)), "{r:?}");
    }

    #[test]
    fn a_valid_record_signed_by_another_key_than_the_dots_who_is_a_mismatch() {
        let h = honest();
        let other = signed_read_tests::sk(3);
        let t = now_ms();
        let env = Envelope::sign(&other, "room", "rocky", Kind::Say, "same body", None, vec![format!("dot:{}", h.dot.id_hex())], t).chain_to(&link_of(&h.pred.envelope.sig), &other, t);
        let own = record(2, env, &pubkey_hex(&other));
        assert!(own.envelope.verify_stored(&own.pubkey).is_ok(), "valid under its own key");
        let r = check("200 OK", &h, own.clone(), vec![h.pred.clone(), own.clone()], json!({"seq": 2, "author": "rocky", "id": own.id, "content": "same body", "tags": [format!("dot:{}", h.dot.id_hex())]}));
        assert!(matches!(r, ChainCheck::Mismatch(_)), "{r:?}");
    }

    #[test]
    fn a_requested_id_content_or_dot_tag_that_is_not_the_chain_records_is_a_mismatch() {
        let h = honest();
        let mut wrong_id = requested(&h);
        wrong_id["id"] = json!("OBS-room-20001021-000000000000");
        assert!(matches!(check("200 OK", &h, h.own.clone(), vec![h.pred.clone(), h.own.clone()], wrong_id), ChainCheck::Mismatch(_)), "id");
        let mut wrong_content = requested(&h);
        wrong_content["content"] = json!("another text");
        assert!(matches!(check("200 OK", &h, h.own.clone(), vec![h.pred.clone(), h.own.clone()], wrong_content), ChainCheck::Mismatch(_)), "content");
        let mut untagged = h.own.clone();
        untagged.envelope.tags = vec![];
        untagged.envelope = untagged.envelope.resign(&signed_read_tests::sk(2), now_ms());
        let untagged = record(2, untagged.envelope, &untagged.pubkey);
        let r = check("200 OK", &h, untagged.clone(), vec![h.pred.clone(), untagged.clone()], json!({"seq": 2, "author": "rocky", "id": untagged.id, "content": "same body", "tags": []}));
        assert!(matches!(r, ChainCheck::Mismatch(_)), "dot tag: {r:?}");
    }

    #[test]
    fn a_chain_that_cannot_be_read_stays_unknown_and_a_v3_dot_stays_not_claimed() {
        let h = honest();
        assert!(matches!(check("403 Forbidden", &h, h.own.clone(), vec![h.pred.clone(), h.own.clone()], requested(&h)), ChainCheck::Unknown(_)));
        let lab = signed_read_tests::oracle();
        let ox = lab.ox(2);
        let v3 = md::parse_any(&md::pack_v3(&signed_read_tests::sk(2).to_bytes(), b"older", now_ms() as u64, [0u8; 32])).unwrap();
        assert!(matches!(ox.chain_check(&requested(&h), &v3), ChainCheck::NotClaimed(_)));
    }
}
