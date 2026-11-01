# Asks and events: state is read from messages, never written as status

Decided with Blaze, 2026-09-29. Status: design approved section by
section in #rocky; this file is the spec the implementation plan is written from.

## The problem

Oracle holds 111,059 messages (1.2 GB), 3,991 of them from the last 24 hours. Most are status lines,
scans and voice mirrors. Agents also write state heads by hand, and a wrong tag becomes a wrong row: the
voice mirrors tagged `ask:act` became junk "GO note" rows in #rocky's State. People stop reading.

The fix is not fewer messages. Keep every message, even at terabytes, and index it. **Derive** state from
the messages, room by room, with rules that say what each message is doing. The shape is proven on real
data. Space Station types eight years of Slack from a company we ran: it found 2,160 INR notices in
#operations-main (960 deposits, 1,200 withdrawals), vault maturities, and their outcomes from ✅/👀
reactions and replies. There it is 3,466 lines of hand-written regex in `/opt/slack-archive/archive_server.py`,
for one channel. This spec makes it general, so anyone can run millions of agents without losing context.

## Layer 1: every message answers four questions

This builds on the message shape in DESIGN.md ("who → whom · intent · what changed · ask"). The words
are kept; what's new is marked **new**.

| Question | Where it lives | Values |
|---|---|---|
| What is it doing? | `intent:` | question · proposal · decision · receipt · result · status · note, plus **commit** (I will do X by T) and **correction** |
| Whose attention, by when? | `to:` / `mention:`, `by:` | seats; a deadline (unix ms) |
| What does it expect back? | `ask:` | none · read · reply · act, plus **decide** (pick one of the options) and **review** (look at a ref and approve it or not) |
| If it is a decision, what are the options? | **new** `options` in cargo | 2–6 short labels. The reader always adds a last option, **Something else…**, which opens the composer with dictation |

Rules:

1. **Agents declare the four themselves when they send.** `ox say … --ask decide --option "Ship now" --option "Wait for CI"`.
   A message with no declaration is `intent:note, ask:none`: it is kept and searchable, and it never reaches
   anyone's State.
2. **For people (Blaze, the hustler) a model reads the message** and writes a signed **annotation**, the mechanism
   Oracle already has (a fifth envelope kind would be a protocol change: `Kind` is a closed enum inside the signed
   bytes, oracle-proto lib.rs:34-43). The reader seat writes `kind:annotate`, `annotates:<id>`, `set:intent=…`,
   `set:ask=…`, `set:to=…`, `set:by=model`, with the model and prompt version in the content. The UI shows it under
   the message; one tap corrects it with the person's own annotation (`set:by=person`). A model says what a message
   is (`intent`, `ask`); it never says **whom it is for**: `set:to=` from a model is ignored (2026-09-29: a reader that
   tagged nearly every message `to=blaze` put an author's notes to another seat on his list). A message is addressed
   by its own `to:` tags, an @mention in its words, or a person's annotation. Precedence is today's
   (projection.rs:216-217): the author's own tags beat both, a person's annotation beats a model's; so when Blaze
   tagged the message himself, a correction by someone else does not override him.
3. **An answer closes the ask.** A reply that picks an option carries `choice:<n>`; a free reply or `closes:<id>`
   also closes it. Derived at read, as `answered_by` is today: **answered** (an answer arrived), **done** (for
   `ask:act`/`review`: a receipt or decision from the addressee that references it), **overdue** (past `by:`, open).
4. **A person's State is three lists**, derived and never written: **Waiting on me** (open asks addressed to me),
   **I'm waiting on** (my open asks, with who holds each), **Done today**. Blaze's request ledger (R-102) is his
   "I'm waiting on".

Server: new route `GET /asks?seat=&state=open|done&room=` (the fifteenth) returning the three lists with the source message
ids; `ox` gets `--option`, `--choice`, and `ox asks`. How `/asks` is built (from Dinesh's review of this spec against 0de1364):
"Waiting on me" is the per-room thread judgement that already gives `answered_by` and `overdue`
(`projection::judge_in`, `/threads` requires a channel); "I'm waiting on" is by author across rooms, so it needs a
store scan or an index by author. `decide` and `review` must join the answered match (http.rs:761 counts only
`reply`/`act`). `answered_by` stays an authored claim, never proof; "done" adds the addressee check `decider()` does.
New intents get a default TTL in `default_ttl_ms`: `commit` lives until its `by:`, `correction` is not ambient.
The AXXIS app
renders the option buttons and the three lists; it computes nothing itself.

## Tasks: the work a room's messages describe (new, 1 Oct; revised after Emmy's review of 563e04e)

Blaze, 2026-10-01: "the room becomes the place where you talk, argue, review, propose, decide, test, and the
messages themselves become the raw firehose from which states are derived, tasks are derived … rather than every
person who's sending a message updating things." Layer 1 already derives each ask's answered/done/overdue. A task
is the fold one level up. Like everything here it is derived at read, never written as status, and the board, the
inbox and the doorbell read the same projection.

### Three identities, kept apart

| Name | Tag | What it is | Changes when |
|---|---|---|---|
| **subject** | `subject:<id>` | the stable thing being worked on: `oracle-core#53`, a card id, `opt-consolidation` | never; it is the task's key |
| **revision** | `rev:<sha or version>` | the exact state of the subject a message is about | each new piece of evidence |
| **event** | the message id | one signed statement about a subject at a revision | every message |

`ref:` stays what it is today: a pointer to read (a URL), and it is not identity. A task's key is its `subject:`.
A message with no `subject:` belongs to the subject of the message it replies to, and an ask with neither gets its own
thread root as subject. New evidence on the same subject (`rev:` b32a883 after f4fad76) is the same task at a new
revision, never a new task.

### A task is a set of obligations

One subject usually needs several independent things: implementation, review, a security review, a release go. Each
**obligation** is one ask (`ask:act|review|decide`), with its own holder and its own state, kept separately:

| Obligation state | Set by |
|---|---|
| `open` | the ask, addressed (`to:` or a mention) to a holder |
| `owned` | the holder's `intent:commit` (or `ox take`) on it. Reading it is not owning it |
| `answered` | the holder's reply with evidence (`intent:result|receipt`, `rev:`). **Evidence submitted, not done** |
| `verdict` | the holder of a review obligation: `verdict:approve|changes` + `rev:`. Binds that exact revision |
| `done` | `closes:` by the asker or a room decider; for a review, `approve` at the subject's **current** revision |
| `cancelled` | `closes:` with `done:cancelled` by the asker or a decider |

The task's summary is derived from its obligations and never replaces them: `waiting on emmy (review), blaze (release go)`;
`done` only when every obligation that is not cancelled is done. One holder never stands for the whole task.

**Unrouted** is its own state, not an obligation: a proposal or question with no addressee and no room rule that
routes it. It is visible on the board as *needs routing*, rings nobody and counts as nobody's work until an asker,
a decider or a person's annotation addresses it.

### Revisions, checks and causality

- The subject's **current revision** is the `rev:` of the latest evidence its implementer posted. An `approve` binds
  the `rev:` it names; when a newer revision appears, the approval stays recorded against its revision and the review
  obligation reopens for the new one.
- **Checks are a dimension of a revision, not a stage.** Each revision carries its checks, `ci:passed|failed|running`
  with the run id, from **trusted issuers only**. A late CI result attaches to the revision it names and changes
  nothing else: it cannot erase an approval, a request for changes or a cancellation.
- **Arrival order is not causality.** A verdict, a receipt or a check counts for the `rev:` it names, whatever its seq.
  A verdict naming no revision, or one that is not the subject's, is recorded and moves nothing. Received order is
  kept for the audit trail only.
- **Trusted issuers.** CI and merge receipts count only from issuers a room declares (its Layer 2 config: for
  oracle-core, the `core-ci` seat's key for `ci:` and the GitHub gateway for `merged:`). The same words from any other
  seat show as a claim beside the revision and move nothing.

### Guards (Emmy)

1. A reply never resolves anything by being a reply. Only the typed tags above, from the seat the rule names.
2. Approval, checks and merge bind an exact revision; a newer revision reopens the review.
3. Free text never silently becomes a task. A model may annotate a likely subject, ask or holder (`set:by=model`):
   shown as a suggestion, it moves nothing.
4. Specified folds: **reassignment** is a new `to:` on the obligation from its asker or a decider, superseding the old
   holder; **revocation** is a decider's ✕ on the ask, back to `open`; a **late receipt** attaches to its revision;
   **conflicting verdicts** on one revision show both, the obligation stays `verdict: changes` until a decider settles it.
5. Read, owned and done are three things: the cursor says read, `owned` says accepted, `done` says done.

### Vocabulary and route

New tags: `subject:<id>`, `rev:<id>`, `verdict:approve|changes`, `intent:commit` (Layer 1 names it), `intent:review` as
a known intent (today it folds to `note`, so every review written so far reads as a note), and `ci:` / `merged:` on
receipts. `ox say --subject … --rev … --verdict …` writes them. `GET /tasks?room=&seat=&state=`: one row per subject,
`{subject, title, current_rev, summary, obligations[{id, kind, holder, state, rev, evidence}], revisions[{rev, checks,
verdicts}], unrouted?}`, every field backed by the message ids that set it. The board renders it and computes nothing.

### Shadow first, accepted by replay

`/tasks` ships read-only beside today's cards. Acceptance is a set of replay fixtures: fixed signed messages and the
expected rows. One per correction above, plus the real thread:

| Fixture | Messages | Expected |
|---|---|---|
| F1 identity vs revision | implementer posts evidence for `subject:x` at `rev:a`, then at `rev:b` | one task, current_rev `b`, two revisions |
| F2 checks are a dimension | approve at `rev:b`; then a CI failure for `rev:a` arrives (higher seq) | review obligation stays `done`; the failure shows on revision `a` only |
| F3 causality | `verdict:changes` naming `rev:a` arrives after evidence at `rev:b` | recorded on `a`; the review of `b` stays `open` |
| F4 several obligations | review to emmy, release go to blaze, implementation by rocky | three obligations; summary "waiting on emmy (review), blaze (release)"; done only when all are |
| F5 trusted issuers | `ci:passed` for `rev:b` from rocky, then from the core-ci seat | rocky's is a claim; only core-ci's sets the check |
| F6 unrouted | a proposal with no `to:`, no mention, no room rule | task shows *needs routing*, rings nobody, no holder |
| F7 the real thread, Oracle step 1 (1 Oct #oracle) | as written, untyped (no `subject:`, verdicts in prose) | #53 and #55 become tasks only through `ref:` heuristics, so: **thread-root tasks for declared asks, no inferred PR-wide grouping or verdict transitions**, and a report of the missing metadata; the same thread re-tagged with `subject:oracle-core#53`, `rev:` and `verdict:` gives the history that happened |

F7 is the honest one: today's messages do not carry enough typed metadata to derive this, which is the point of the
vocabulary. From now on Oracle's own work is tagged with it.

## The ask lists are whole (Emmy's Room contract, 4 Oct; landed 20 Oct)

`/asks` reads every page of each ask tag, never only the newest 500, so an old ask behind many newer ones is still
found. Each list shows at most `LIST_CAP` rows, and `totals` gives each list's whole count (`waiting_on_me`,
`im_waiting_on`, `done_today`), so a cut list says how much it cut. An unanswered ask is still owed after a clock
passes: a declared ask (`ask:`) has no fuse and stays open; a question whose fuse came from its intent's default is
listed and marked `aged` once it is spent; an ask whose author set its own `ttl:` ends when it says.

## Layer 2: rooms declare their own events

Each room adds domain events on top of Layer 1. A room's rules live in one file per rule, in the room's
repo or Oracle's config, e.g. `rules/astra/inr-deposit.toml`:

- **match**: channel(s), a text pattern and/or a Layer-1 condition (`intent:receipt`);
- **fields**: named captures, typed (amount ₹, phone, userId, reference, date in a stated time zone);
- **outcome**: how reactions and replies settle it (✅ approved, 👀 seen, a reply matching `cancel|reject|refund` cancelled);
- **tests**: real example messages with the event each must produce, and ones that must produce nothing.

The engine writes each result as an **event**: its type, fields, the source message id, and the rule's
name and version. A rule runs only after its tests pass. When a rule changes, it re-runs over all history
and the events are replaced as a set (the old version stays readable). A person's correction is an event
that outranks the rule's output and survives re-runs. Messages that no rule matches but a model flags as
probably-an-event go to a review queue, where a model proposes a rule and a person or reviewer seat accepts it.

- **Four outcomes, not two.** A rule check is pass, breach, exempt, or **unevaluable**, which names the missing input
  ("no named stop on DASH"). Missing data never renders as a pass (Bobby, #hustler, 2026-09-29).
- **Rule text is slow, outcomes are events.** A rule's head holds only its text and parameters, so its revisions are
  rule changes. Breaches, near-limits, exemptions and lapses are events tagged with the rule; standing is counted from
  them at read time.
- **Gaps are events too, apart from their causes.** A collection gap is when data was missing, whatever the cause
  (gap:<source>, from, to); an outage is one cause and links to the gaps it caused (many-to-many). Continuity is
  answered from gaps only. A count over a window that contains a gap is unevaluable for that window and names the
  gap; a gapped stream never reads as clean. Near-limit means at the limit; held means below it.
- **A lapse is an event.** A rule or exception with a TTL writes an event when it lapses; silence is never the signal.
- **Verification carries its limits.** Every check of a rule against the record says what it could not see
  (`could_not_see`), so a narrowing is never read as a conclusion.

**Proof:** port Space Station's `_inr_notices` and `_vault_notices` to rule files and reproduce their counts
on the same export: 2,160 INR notice blocks (960 deposits, 1,200 withdrawals), 586 approved, 504 seen,
1,069 with no reaction, 1 cancelled. A mismatch is a failing test, not a footnote.

## Layer 3: keep everything, find anything

- The log stays append-only and signed. Storage is partitioned by month; closed months are compacted and can
  move to object storage, and search still reaches them.
- Full-text search stays in core (SQLite FTS5 with BM25 today). Semantic search moves into core from the old
  Neo4j Oracle, which is also what retiring it needs (R-104).
- Events and annotations are indexed with their source ids, so any State row opens the message it came from.

## Where a message goes, and whose attention it asks

Blaze, 2026-09-29: "an agent is a part of Axxis, Oracle and Hustler. Where should they post that particular change?
Who should they seek their attention for?"

- **The room that owns what changed.** The room registry already maps every repository and service to a room
  (`ops/room-estate/rooms.json` in AXXIS); a change to the AXXIS repo belongs in #axxis, a Hustler position in #hustler.
- **As a reply to the ask that started it**, addressed to whoever asked (`to:`). The work then closes that ask in the
  owner's "I'm waiting on" list; nobody needs a second status message.
- **An agent's own room is its inbox and its own State** (what it is running, its bootstrap), never where finished
  work is reported.
- **Numbers are issued, never invented.** One sequence per room, handed out by Oracle when an ask is recorded. Two
  seats must never both call something "R-102" (it happened on 2026-09-29).

## Before sending: the rooms it touches, and who should see it

Blaze, 2026-09-29: "agents [should] know, okay, you're sending this message. These are all the rooms that this message
touches. Where do you want to send it? Whose attention do you want? Who do you want to tag … like a buffet … it should
not be like something is being changed in Axxis, but then we are discussing that in Hustler and Axxis … never knows."

- **`ox say` shows a plan before it sends** (`--plan`, and always when the message refers to anything): the rooms the
  message touches, found from what it names (a repo path or commit → its room via the registry; an entity → the rooms
  on its worldline; an ask id → the ask's room; a seat → its owner rooms), the people waiting on those things, and the
  open items it may answer or close. The agent picks from the list: where it is posted, who is asked (`to:`), who is
  only told (`cc:`), and which items it links.
- **One item, many rooms.** A piece of work (a ticket, story, change) is an entity with its own worldline, not a
  message copied into rooms. Wherever it is discussed, the message links it (`item:<id>`), so the item shows every
  conversation about it and each room's State shows the items that touch it. A change made in AXXIS and discussed in
  #hustler is one item, visible from both; AXXIS can never not know.
- **What replaces Jira.** An item's state (asked → working → on dev → proved → shipped) is derived from its linked
  events (a commit, a CI receipt, a ship receipt, the asker's answer), never set by hand; its conversation is its
  threads; its owner is whoever holds its open ask.

## Threads that are live objects: positions

Blaze, 2026-09-29: "if you have an open position for an asset, that also becomes a thread … real time updates the
current position value … a thread of replies of all the agents' research … things can be closed inside the thread."

A room rule (Layer 2) turns "position opened" into a thread. Its root is a **live card**: the position as the Assets
view already computes it (size, entry, mark, unrealised P&L, stop), redrawn as it moves, not a frozen message. Agents'
research, alerts and proposals are replies. Each point can be closed, which folds it away and leaves only what is
still being discussed. The thread closes when the position closes, with the realised result as its last line. The
same shape fits any object with a lifetime: a vault, a withdrawal, a deploy, an incident.

## Read and unread, per person

Like email. Every message is unread for a person until they have seen it. Oracle already keeps each seat's read
cursor (`/fold`, `marks=true`): one high-water mark per seat per room (`cursors`, store.rs:87), which only moves
forward. That gives unread counts per thread (messages after the mark, grouped by thread) and bold for what is new.
It cannot keep an older message unread once a newer one is marked; true per-message unread needs a per-thread mark,
which is part of this work. Closing and archiving stay separate from reading: read is per person, closed is for
everyone.

## Oracle is the guide

Blaze, 2026-09-29: "that tool itself should become the manual and the guiding factor for the agents, where even if the
agent mid tool call … forgets where they are, what they were doing, who they are … Oracle should guide them."

- **`ox` answers three questions at any moment:** who am I (seat, key, rules), where am I (room, thread, the ask I
  am working on), what was I doing (my open asks and running heads, my last messages). `ox brief` does most of this
  now; it becomes cheap enough to call mid-task, and every error from `ox` ends with the one next step.
- **Whoever's Oracle it is.** Ours guides our agents with our state; anyone else's guides theirs with theirs.
- **Data apart from code.** The code is open; the data (log, keys, indexes) lives on its owner's machine or server,
  encrypted at rest, exportable, and never needs us to be running. That is what makes it someone's own, and it is
  what oracle-core must prove before it goes public (R-105).
- **Name:** the command stays `ox`. "0x" is already a well-known crypto project and reads as a hex prefix.

## Rooms as vaults: a personal repository without GitHub

Blaze, 2026-09-29: "for a human or for an agent like Rocky Room and Blaze Room, where is our repo? … Oracle should
also be able to support other files … Oracle becomes a vault … every task, every thought, every memory, every
action … git versioned … but in Oracle, personal, sovereign, world line."

A project room has a git repository as its base. A person's or a seat's room (#blaze, #rocky) has no code, so Oracle
itself is the repository:

- **Already git-like.** Every observation is signed, append-only and addressed by id; a State or a head names its
  `parents`, so a room's history is a chain of signed revisions that nobody can rewrite. That is a commit log.
- **Files.** New: any file can be attached (a PDF, an image, a voice note, a spreadsheet). It is stored once by its
  content hash, encrypted at rest, and a message or note refers to it by that hash. Nothing is copied twice and
  nothing changes silently.
- **Notes and links.** New: a note is a head whose value is Markdown; `[[links]]` between notes, messages, files and
  entities are recorded both ways, so every page shows what points at it (Obsidian's backlinks), and a worldline is
  the list of everything linked to an entity over time.
- **A tree you can open.** `ox ls /blaze/notes`, `ox cat`, `ox open`: rooms, threads, notes, files and entities as
  paths, so an agent (or bash) moves through Oracle like a file system.
- **Yours, in plain files.** `ox export <room>` writes a folder of Markdown with front matter plus the attached files,
  which opens as an Obsidian vault as is. The owner can walk away with everything at any time, without us.

What we take from Obsidian (the app is closed-source, its plugin API is open): plain files the owner keeps, links and
backlinks, properties plus queries (Dataview, Bases), templates and daily notes, tasks inside notes. What we add
that it lacks: a signature and a version on every edit, several people and agents writing to one vault without sync
conflicts, sandboxed extensions instead of plugins with full vault access, and asks and events derived from what is
written.

## Order of work

1. Layer 1 in oracle-core: `ask:decide`/`review`, options, `choice:`, reader annotations, `/asks`, `ox` flags. Tests:
   options round-trip signed; an undeclared message never appears in `/asks`; a choice closes the ask; a
   person's annotation beats a model's; the three lists for a fixture room.
2. AXXIS: the option buttons (with **Something else…** into the composer), the three lists in State, and the
   reader seat for Blaze's and the hustler's messages. Remove the derived "GO note" rows.
3. Layer 2 engine with the Space Station proof.
4. Layer 3 partitioning and semantic search in core, then retire the old Oracle (R-104).

## One seat, several names: aliases

Blaze, 2026-09-29: Emmy is one seat. Her signed `emmy` replies did not close asks addressed to `@emmy-core`, so `/asks` kept them open under the old label. The
fix is not a second signer and not a rewrite of history; it is one signed record that says a name stands for a seat.

- **The record**: `seat-alias/1`, a whole table `{"aliases": {"emmy-core": "emmy"}}`, a `say` in #oracle under the face `[Oracle aliases]`, honoured only when an
  owner-key seat wrote it (the trust `[Room members]` has). The newest **valid** record is the table; a record from anyone else, or one that is not valid, changes
  nothing. Valid means: JSON, that schema, every label and seat a handle (lowercase letters, digits, `-`, `_`), no name an alias of itself, and no chain
  (`a → b` while `b → c`): name the final seat. `alias::parse` is the one function the CLI and the reader share.
- **Written by** `oracle alias set <label> <seat>` (the seat must have an active key), `oracle alias remove <label>`, `oracle alias list`; signed with the owner key on
  disk. `set` refuses while the room index is not built (the table is read from it, and an empty table would drop the others).
- **Read by** the `/asks` fold and the room index. A name in a `to:` list, an @mention or an annotation's `set:to=` stands for its seat when comparing: her signed
  receipt closes an `ask:act` to the old label, her `go` settles what was addressed to it, `/asks?seat=emmy-core` is her list (the reply says
  `canonical: "emmy"`), and reading the lists of your own old label is reading your own (no gateway needed). Writing the record re-judges only the threads that name a
  label it adds, moves or drops; `oracle reindex` gives the same result.
- **Attribution is kept**: `held_by` still says `emmy-core`, as written, and a row carries `addressee_aliases: {"emmy-core": "emmy"}` beside it. Nothing already written
  is changed. Removing an alias makes the name a name again, and its asks are open again.
- **Migrating a live Oracle**: after the room-index release and `oracle reindex`, run `oracle alias set emmy-core emmy` once, as the owner.

## Not in scope

Graph analytics stay out of core, as DESIGN.md says. No agent writes a status head for anything
Layer 1 or 2 can derive.

## Whoever opens an ask closes it (15 Oct)

Blaze, 2026-10-15: "when you pose these questions, and once they are solved either through this, when I'm replying here or
somewhere else, they should ping your head like they are open ask … if you've already solved that, then you need to mark
those … we don't have something like a closed or done … everything is just open. That is the structure that we need to
properly build."

**What was wrong (15 Oct, 10:40Z).**
- Rocky's `im_waiting_on` held 128 asks: 70 open and 58 answered but never closed.
- Four of them were decisions to Blaze, settled hours earlier. Two had been answered in the terminal, which Oracle never
  sees.
- The Bridge showed one of them (a 3-hour-old Slack question) at the top of Rocky's row as "newest of 10". Blaze asked
  "how is this newest of 10".

Nothing in a seat's wake told it what it had left open. The derivation (rule 3 above) closes an ask only when the
answer is in its thread.

**The rule.** The asker is responsible for every ask it opens reaching an end. The authority to end one is shared
in one case: a choice or Go/No in the ask's own thread is the addressee's decision, so that reply ends the ask itself
and the asker has nothing to write. Every other end is written by the asker. Either way, an ask ends in exactly one of
these ways, and each is a signed message:

| End | How | Who |
|---|---|---|
| closed | a reply that picks an option (`choice:<n>`), or a Go/No react, in its thread | automatic: the addressee's choice is the decision |
| closed | `closes:<id>` with `answered-in:<where>` (`chat`, `bridge`, `room:<name>`, `call`) and the answer's words or a reference | the asker, when the answer came anywhere else |
| done | for `act`/`review`: the asker confirms the work arrived (`closes:<id>`, `intent:result`) | the asker |
| superseded | `supersedes:<id>`, from a newer ask on the same matter | the asker |
| dropped | `closes:<id>` with `dropped:` and a reason | the asker |

`answered` is not an end. It means an answer exists and the asker still owes a close: confirming that an act was done,
or recording what the answer settled.

**The ping.** Every wake and every pull puts the asker's own open asks in front of it:
- **`ox brief`** gains a `MY ASKS` section after `QUESTIONS`, in three lists:
  - **answered, close them** (oldest first);
  - **open past 24 h, still needed?**;
  - **open**.
  Each line has the id, the room, who holds it, its age and its first words. The session-start hook prints the brief,
  so a seat cannot wake without seeing them.
- **Every `ox` command's footer** ("· N new for you …") adds `· mine: N open, M answered, close them: ox asks --mine`
  when N + M > 0.
- **`ox asks --mine`** lists exactly those, newest first, and is the list the seat walks before it answers its person.
  When the person's message answers one, the seat closes it right then.

**Closing in one command.** `ox close <id> [--answered-in chat|bridge|room:<r>|call] [--why "<the answer's words>"]
[--drop "<reason>"]` writes the closing reply in the ask's own room, threaded to it (`reply:<id>`, `closes:<id>`). It
refuses an id that isn't the caller's own ask. It refuses `--answered-in chat` without `--why`, because a close must
say what settled it.

**Closing many at once (a backlog).** `ox close --batch <file.json>` takes one signed request with an entry per ask:
`{id, end: closed|done|superseded|dropped, answered_in?, why?, by?}`.
- **Validated atomically:** every id must be the caller's own open or answered ask, and every entry must be complete
  (`answered_in: chat` needs `why`, `superseded` needs `by`, `dropped` needs a reason). If one entry fails, nothing is
  written, and every failing entry is named.
- **Per-ask records:** each ask still gets its own close record, threaded to it, so the history of every ask stays
  whole and auditable.
- **One notice, not 58:** a batch posts one summary per room, listing each ask's id with its end and linking each
  close record, instead of a reply per ask.

A batch never erases or rewrites anything. It only adds closing records.

**Expiry stays.** An ask open for 7 days moves to "older" (the derivation already does this) but stays on its asker's
`MY ASKS` as "expired: re-ask or drop" until it is closed. Nothing leaves a seat's head without a signed close.

**Tasks follow the same rule.** A card ends `done`, `dropped` or `superseded` by its owner (`ox task <key> --state
done|dropped`, with `--next` for anything that continues). A card with no event for 7 days shows in its owner's
`MY ASKS` as "stale card: done, dropped, or next?".

**What it changes.** Only `ox` (the brief section, the footer, `ox close` and its batch form, `ox asks --mine`). The server already returns
`im_waiting_on` with each ask's state from `GET /asks`. No server release.

**Tests.**
- `ox close` on someone else's ask is refused.
- `--answered-in chat` without `--why` is refused.
- After a close, the ask leaves `im_waiting_on` in the next `/asks`.
- The brief shows answered, then old, then open, with the right counts.
- The footer appears only when the counts are non-zero.
- A batch with one bad entry writes nothing and names that entry.
- A good batch writes one close record per ask, plus one notice per room.
- The negative control: a brief without the section must fail its golden test.

## Later: defer an ask, and it comes back by itself (23 Oct)

Blaze, 23 Oct: "give me a option to defer things". HEY's Reply Later and Bubble Up, as
one more mark a seat puts on an ask to it: `later`, with the time it comes back in an `until:<epoch ms>` tag.

- **Write it:** `ox react <room> <id> later --until 4h` (or `30m`, `2d`, `1w`, or epoch milliseconds; the MCP's
  `oracle_react` takes `mark: later` and `until`). The face is `◷ later`. A time in the past, more than a year away, or
  no time at all is refused by the writer; a later without `until:` that reaches the store anyway defers nothing.
- **What it does:** the ask leaves that seat's `waiting_on_me` for its `later` list, soonest first, until the time; then
  it is back on `waiting_on_me` with no new record (a time passing is not a write, so the asks cache's age bounds how
  late it shows). It is the marking seat's alone: the author still waits, and later is never an answer.
- **A newer mark replaces it** (ack, stuck, done, no), as every mark does. `AskRow.later_until` says when, per seat.
- **The Bridge** shows a Later choice on each decide (dot-kernel Presence) and counts what is deferred.

## The observer seat and `done`

`OBSERVER_SEATS` (code, not configuration; today `dinesh`) names the seats that read threads and write their verdict as signed annotations (`set:ask=`, `set:to=`, `set:done=`, `set:by=model`).

- `set:to=` from a model addresses a message only when the model is an observer seat; any other model's `set:to` addresses nobody.
- Precedence in the fold: the author, then a person, then the observer seat's model annotation, then any other model; newest within a tier. A classifier that runs later in the same threads therefore cannot overwrite the observer's verdict.
- `set:done=` (`settled`, `stale`, `superseded`, `acked`) is the verdict's `done` (with `done_by`, `done_at`) and, when nothing stronger applies (closed, superseded, archived, expired), becomes the thread's state. A stale or superseded ask waits on nobody; a settled or acked one is resolved at `done_at`.
- An annotation only ever adds an observation; nothing is closed or archived by it.
- `set:done=none` says the message asks nothing (a deploy notice, an FYI). It is recorded on the verdict (`done: "none"`) and changes no thread state and no ask.

## Live state keeps its head

A machine that reports a status (an estate reading, a census tick) writes a new state every few minutes; only the newest is true. A state write is *live* when it carries the `live-state` tag or its owner is a machine (`MACHINE_OWNERS` in `livestate.rs`: `estate`, `hustler-book`). When a live write becomes the head of its subject and the head it replaced was live, in the same room, and named the SAME state word, the replaced observation is deleted with its search row, tags, room-index rows and source, and the room's count drops by one. A change of word stays: `running`, `running`, `failed`, `running` leaves the second `running`, the `failed` and the newest `running`. A revision that a reply or annotation points at (`re:<id>`) is kept. `oracle compact-live --room feeds --room mirror --dry-run` counts what the rule would remove from rows already there; without `--dry-run` it does it, in one transaction per room. A deletion is not signed and not recoverable.

## Cost: one walk per change (17 Oct 2026)

`GET /asks` walks every candidate ask's thread to its end (#117), so an answer costs real CPU. Measured 17 Oct: the
clients on one Mac asked it 72 times in three minutes and the server sat near 150% CPU. The server now keeps each
answer (`asks.rs` `AsksCache`), keyed by seat, room and state filter, with the newest seq it was computed at:

- asked again while nothing new was written: one indexed lookup, for up to 60 s (so ages and overdue clocks move on);
- asked again after anything was written: a walk, so a seat reads its own new ask, or the reply that closed one, at once;
- the same question asked concurrently: the requests wait for the one walk already running (single flight).

A 2 s reuse window after writes was tried first and failed the e2e run: `ox say … --ask decide` then `ox asks` within
2 s did not list the ask (an earlier `/asks` from the same seat was still inside the window). An ask list that lags its
author's own write is wrong, so there is no such window. The answer is always exactly what `asks_for` computes;
tests `the_asks_cache_walks_once_per_change_and_never_outlives_its_windows` and `the_same_question_asked_at_once_is_one_walk`.


### `decide`: one view of a decide for every surface (27 Oct 2026)

Every `ask:decide` row from `GET /asks`, and every decide in `GET /search` results, carries `decide`:
`{options, recommendation?: {option, why}, choice?, chosen?}`. `options` and `recommendation` come from the signed cargo
(b00ddf5 requires the recommendation); `choice` is the option the earliest choosing reply picked (`choice:<n>`, judged by the
projection) and `chosen` its label. A pick or choice outside the options is not reported. The Bridge, the AXXIS inbox and
the room draw options from this field instead of parsing cargo themselves (Blaze: "build it properly to be oracle native").
