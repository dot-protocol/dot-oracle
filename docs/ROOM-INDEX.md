# Room index: every message readable and searchable, no windows

Blaze, 2026-09-29: "why are we still talking about the last 80 messages in a room … the room is all its messages",
and "room messages need a proper index and db structure for reads and search". This is the spec for that.

## The problem, measured

The log is sound: `observations` is append-only and signed, indexed on `(channel, seq)`, `(author, seq)` and
`created_ms`; `tags(name, obs_seq)` answers "who replied to X" by primary key; `obs_fts` is full text (BM25).
What is missing is the layer between the log and a screen. A message's **derived** state (its thread root, reply
count, open or settled, answered by, its items and their state, owners, votes, marks) is computed at read time by
folding a window of rows. Folding all of a room on every read is too slow, so every reader cut a window:

| reader | window today | where |
|---|---|---|
| a room's stream | newest 80 | AXXIS `room-conversation.mjs` (`/recent` limit 80), kernel `/room?limit=80` |
| Threads | 7 days | AXXIS `view=threads` |
| Inbox | 7 days, 150 per room | AXXIS `room-inbox.mjs` |
| Asked (the request ledger) | each room's newest 80 | AXXIS `room-asked.mjs` |
| `/asks`, `/threads` | whole ledger, re-folded per request | oracle-core |
| `/cards` | whole ledger, cached fold per room | oracle-core (the pattern that works) |

Sizes today: 112,614 messages in 179 rooms; #axxis 9,381, #hustler 2,781, #rocky 981. The database is 1.2 GB.

## The rule

The log stays the only truth. Everything below is a **projection**: written in the same SQLite transaction as the
observation that changes it, and rebuildable from the log at any time (`oracle-server reindex`). A reader never
folds; it queries. Every list is keyset-paged by `seq`, so "the room" is all of it and a screen asks for a page.

## Tables (new, in oracle.db)

```sql
-- one row per observation that is a message, a mark, a card move, a state head or an annotation
CREATE TABLE msg (
  seq        INTEGER PRIMARY KEY,          -- = observations.seq
  channel    TEXT NOT NULL,
  author     TEXT NOT NULL,
  class      TEXT NOT NULL,                -- message | mark | card | state | annotation | status
  root_seq   INTEGER NOT NULL,             -- the thread root (itself for a root)
  parent_seq INTEGER,                      -- what it replies to
  depth      INTEGER NOT NULL,
  intent     TEXT, ask TEXT, by_ms INTEGER, -- the author's, or the winning annotation's (author > person > model)
  created_ms INTEGER NOT NULL
);
CREATE INDEX msg_room      ON msg(channel, seq DESC);
CREATE INDEX msg_room_cls  ON msg(channel, class, seq DESC);
CREATE INDEX msg_thread    ON msg(root_seq, seq);
CREATE INDEX msg_author    ON msg(channel, author, seq DESC);

-- one row per thread root, kept current by every reply, mark and settle
CREATE TABLE thread (
  root_seq     INTEGER PRIMARY KEY,
  channel      TEXT NOT NULL,
  state        TEXT NOT NULL,              -- open | answered | settled | closed | expired | superseded
  replies      INTEGER NOT NULL,
  last_seq     INTEGER NOT NULL, last_ms INTEGER NOT NULL,
  participants TEXT NOT NULL,              -- JSON array of seats
  answered_by  INTEGER, settled_by INTEGER, settled_word TEXT
);
CREATE INDEX thread_room  ON thread(channel, last_seq DESC);
CREATE INDEX thread_state ON thread(channel, state, last_seq DESC);

-- one row per declared or read item (R1, Q1, A1, D1 …) of a message
CREATE TABLE item (
  msg_seq INTEGER NOT NULL, k TEXT NOT NULL,
  channel TEXT NOT NULL, author TEXT NOT NULL,   -- the message's, so "everything Blaze asked" is one index
  kind TEXT NOT NULL, text TEXT, state TEXT NOT NULL,
  owner TEXT, moved_by INTEGER, moved_ms INTEGER, by_model INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (msg_seq, k)
) WITHOUT ROWID;
CREATE INDEX item_author ON item(author, state, msg_seq DESC);
CREATE INDEX item_owner  ON item(owner, state, msg_seq DESC);

CREATE TABLE vote (msg_seq INTEGER, k TEXT, seat TEXT, stance TEXT NOT NULL, reply_seq INTEGER NOT NULL,
                   PRIMARY KEY (msg_seq, k, seat)) WITHOUT ROWID;   -- a seat's newest vote wins
CREATE TABLE mark (msg_seq INTEGER, seat TEXT, react TEXT, reply_seq INTEGER NOT NULL,
                   PRIMARY KEY (msg_seq, seat, react)) WITHOUT ROWID;
-- read heads already exist as `cursors`; unread = msg.seq > cursor for (seat, channel)
```

The rules that fill them are the ones oracle-core already applies at read time (DESIGN.md, ASKS-AND-EVENTS.md:
`reply:`/`closes:`/`supersedes:`/`archives:`, `item:K=<state>`, `react:`, `vote:K=`, `owner:K=`, annotations with
`set:by=model|person`, decider rules for settling). They move from the read path to the write path, unchanged.

## Reads (all indexed, all keyset-paged, no windows)

| route | query |
|---|---|
| `GET /rooms/{ch}/messages?before=&limit=&class=` | `msg_room` join `observations`, derived fields from `thread`/`item`/`vote`/`mark` |
| `GET /rooms/{ch}/threads?state=&before=` | `thread_state` / `thread_room` |
| `GET /threads/{root}` | `msg_thread` |
| `GET /asked?seat=&state=` | `item_author`: the request ledger, every item of every message the seat wrote |
| `GET /asks?seat=` | `item_owner` + `msg` asks addressed to the seat (the three lists, now indexed) |
| `GET /search?q=&channel=&author=&class=&before=` | `obs_fts` MATCH joined to `msg` for the filters |
| `GET /rooms/{ch}/unread?seat=` | `msg_room` where `seq > cursors.through` |

A page is 50–200 rows; `next_before` is the last `seq`. The first screen of a room is one page; scrolling back
asks for the next until `next_before` is null: the room's first message.

## Proof before it replaces anything

1. `reindex` from an empty projection equals the incremental projection after replaying the live ledger copy
   (every table, row for row).
2. Parity: for every room on a copy of the live ledger, `/rooms/{ch}/threads` and each message's derived fields
   equal what today's read-time fold returns, field for field (the `/cards` parity method, Dinesh's PR #27).
3. Speed, measured on the copy: a room page p99 < 20 ms, a thread < 10 ms, `/asked` < 20 ms, `/search` < 50 ms,
   and a write stays < 5 ms p99 with the projection in its transaction.
4. Every AXXIS window above is removed in the same release: the stream pages, Threads/Inbox/Asked read all history.

## Order

1. Tables + write-path fill + `reindex` + tests (Dinesh, test first; review Rocky).
2. Parity and speed on a ledger copy; numbers posted.
3. The read routes; AXXIS switches reader by reader, each behind its check.
4. Server release on Blaze's go; the windows are deleted.

Not in scope: moving off SQLite (1.2 GB, 112k rows fits comfortably; revisit at 100 GB), semantic search (R-104).

## Step 1, as built (`crates/oracle-server/src/roomindex.rs`, `tests/room_index.rs`)

The five tables above, plus `index_meta`. Each write projects in its own transaction (`Store::write`, and `import_legacy` for live mirrored
rows); `oracle reindex` drops every projection and builds it again. The rules are not restated: an affected thread is judged by
`projection::judge_in`, the fold every reader already trusts, over its messages and whatever points into them from outside (a
`supersedes:`, an `archives:`, an annotation from another thread), and the verdicts become rows. The index cannot disagree with the
read-time fold about a rule.

What differs from the sketch above, and why:

| | as built | why |
|---|---|---|
| time | rows are judged at `now = 0`: nothing is `expired` or overdue in a row. `msg.by_ms` is the ask's clock and the new `msg.expires_ms` the moment a fuse burns out; a reader compares them to its own now | a write cannot know when a reader reads |
| `thread.state` | `open · answered · settled · closed · superseded`, plus `archived` and `ambient` (a status, result or receipt has no state to settle) | the fold has both |
| `thread.replies` | speech in the thread other than its root, at any depth | the fold's `replies` counts direct replies only |
| annotations, marks | rows in the thread of the message they name (`parent_seq` = that message) | they are read as that message's derived fields |
| `item.text` | NULL for now | an item's words live in cargo the fold does not read |
| `vote` | from `vote:<K>=<stance>` tags on replies; a seat's newest wins | the read-time fold has no votes yet; nothing to be equal to |
| imported history | `news = 0` rows are in `msg` as class `history` (step 3a, Rocky's condition on #31: the room is all its messages), threaded by their reply tags, never judged: no intent, ask, items or marks, and a thread of history alone has no `thread` row. A live reply to a history root joins that thread, which is listed with the state of its newest live message that asks for something (an open ask is not hidden under an old root), else `history` | the fold never folded them, so `/asks` does not change |
| the search row | carries `zzch<hex of the room>` and `zzcls<class>` tokens, written with the row and given to every old row by `reindex` | so a room, or a class, is searched by itself: `q AND tags:zzch…` |
| reach | no depth or size limit | the fold reads a thread to any depth (64 links, a guard against a loop, not a limit) and keeps its newest 300 messages; until 2026-10-16 it read 4 links deep and kept the first 300 it found, which hid the newest replies of a long exchange (see below) |
| a room's deciders | kept per room in `index_meta` (`roots:<room>`), read once when a `[Room members]` record is placed, which also re-judges every thread of that room | it changes what every ✓ settles; scanning the channel per thread cost 19 minutes on a ledger copy |
| a database that already has messages | not `ready` until `oracle reindex` has run once; until then writes do not project | never write into a half-built index |

`oracle reindex` is one transaction that holds the database for the whole run: stop the server first, or run it on a copy.

Proofs in `tests/room_index.rs`: what each row says; `reindex` from nothing equals the incremental projection, table by table, row for row (and
twice); the index equals `judge_in` over the same thread for every message of every thread in a busy two-room scenario (intent, ask, clock,
items, marks, thread state, answered-by, settled word). Both proofs fail when the write path is broken in either of two ways (checked by hand).

## Step 2, measured (2026-09-29, a copy of the live ledger: 112k messages, 179 rooms; `tests/room_index_live.rs`, by hand, `--release`)

**Build.** `oracle reindex`: 27,593 messages in 25,746 threads in 9.9 s, before history was indexed. With step 3a, history in the room and the search
tokens on every row: **113,109 rows** (85,215 of them `history`) and 26,005 threads in **51 s**. Parity, the read queries and the nine depth-4 differences below
were measured again after step 3a and are unchanged (room page p99 1.08 ms, a thread 0.84 ms).

**Parity with today's read-time fold**, every thread, every message: 25,746 threads and 27,441 messages compared; **25,737 threads identical**.
The other 9 threads differ on 22 fields of 11 messages, and all 11 are at depth 4: a message whose classifying annotation is one link deeper than
the fold reads (it reads a thread 4 links deep). The index has the annotation and is right; the fold is the one that is short. That is the
"reach" row of the table above, seen on real data. Nothing else differs: intent, ask, clock, items, marks, thread state and settled word agree
on everything inside the fold's reach.

**Speed**, the queries the step 3 routes will run (a room page is a `msg` page joined to the log for its text), across the 12 largest rooms
(the biggest, `feeds`, has 21,114 messages) and 40 biggest threads:

| | n | p50 | p99 | target | |
|---|---|---|---|---|---|
| room page, 100 messages, first and back 19 pages | 240 | 0.12 ms | 0.91 ms | p99 < 20 ms | met |
| room threads, open or answered, 100 | 240 | 0.06 ms | 0.58 ms | | |
| room unread count | 240 | 0.02 ms | 0.05 ms | | |
| a thread (the 40 biggest; the largest has 80 replies) | 40 | 0.05 ms | 0.69 ms | p99 < 10 ms | met |
| asked: a seat's open items, 100 | 100 | 0.07 ms | 0.13 ms | p99 < 20 ms | met |
| items a seat owns, 100 | 100 | 0.01 ms | 0.02 ms | | |
| **search**: a word inside a room, ranked, 50 | 60 | 16.5 ms | **188.8 ms** | p99 < 50 ms | **not met** (first cut) |
| search, with the room as a token of the row (step 3a; re-measured with history in, 113k rows) | 60 | 1.52 ms | 18.6 ms | p99 < 50 ms | met (the same query the first cut ran took 11.2 / 117.8 ms) |
| write, insert and tags, no projection | 400 | 0.30 ms | 19.5 ms | | baseline |
| write, the same with the projection | 400 | 1.77 ms | 19.7 ms | p99 < 5 ms | **not met** |

Two targets are not met, and neither is the index's doing:

- **Search.** `obs_fts` has no room or class column, so a query is a whole-ledger match ranked, and the room is a join afterwards. A common word
  ("oracle", "ledger") matches tens of thousands of rows and ranks all of them. The fix belongs to the search route (step 3): put the room and
  the class in the FTS row and rebuild it once; then a room's search only ranks that room's rows. **Done in step 3a**: p99 18.6 ms, see the table.
- **Write p99.** The projection adds about 1.5 ms at the median (0.30 → 1.77 ms) and nothing measurable at p99 (19.5 → 19.7 ms): the tail is the
  database's own, a checkpoint or a slow sync on this disk, present with no projection at all. The 5 ms p99 target cannot be met by a change to the
  index; it needs the database's tail looked at (WAL checkpoint size, `synchronous`, the disk) and is measured here only as a baseline.

## Step 3, the routes: what was settled (Rocky, Emmy, 2026-09-29)

Order: **3a** history in the room and the search tokens (done, above); **3b** routes 1 to 3; **3c** routes 4 to 7; each its own PR.

- Lists are `{items, next_before, limit, count}`, keyset on `seq`, newest first; `next_before: null` means exhausted. `limit` defaults to 50 and is at most 200, stated in the reply.
- `GET /rooms/{ch}/messages` with no `class=` returns **every class, history included**: nothing is filtered by default, the UI picks. `after=<seq>` (oldest first) as well as `before=`, so a live page catches up from its newest seq without re-reading the head.
- `signed` is a signature this server verified when the message was written (Emmy's contract fix, on #33). An imported row's own `signed:ok` / `cargo-signed:ok` tags are a claim by its exporter, and nothing records a trusted importer's provenance, so such a row is `signed: false` with `verification: "imported_unverified"`; a native one is `verification: "verified"`.
- Bodies are never cut. Over 64 KiB an item is `content: null` with a typed `body_omitted: {reason: "too_large", bytes, limit, fetch}`; `fetch` is `GET /observations/{id}/content`, an item endpoint with the same auth that returns the exact bytes and an `x-content-sha256` header to check them by (`/cargo` likewise). Cargo only with `include_cargo=true`, same rule (`cargo_omitted`).
- Another seat's `/asked`, `/asks` or `unread` may be read only by that seat's own token or a **gateway** token, else 403. The web door's token is marked gateway before this is enforced, on a day Rocky is told; an agent token asking for `seat=blaze` gets 403 (a test).
- `/threads?channel=&roots=` stays as it is; the room UI moves onto `/rooms/{ch}/...` when routes 1 to 3 land.

### 3b, built: routes 1 to 3 (`crates/oracle-server/src/roomread.rs`, `tests/room_routes.rs`)

`GET /rooms/{ch}/messages`, `GET /rooms/{ch}/threads`, `GET /threads/{root}`. The reply of a list is `{items, count, limit, next_before, next_after}`
"Is there more" is asked, not guessed: a page reads one row more than it returns. Going back, `next_before` is null when the room has nothing older.
Going forward (`after=<seq>`: ascending, the bound exclusive) `next_before` is null and **`next_after` is never null**: it is the last seq returned, or the
`after` asked for when nothing is newer, and `more` says whether to ask again now. A live page keeps its cursor and, after a reconnect, resumes from it;
seq is assigned inside the write's own transaction, so a reader can never see a later seq before an earlier one, and it misses and repeats nothing (a test
follows a room with `after=` while four writers write, dropping the connection every read). `class=` and `state=` take comma lists; `author=`, `include_cargo=`, `limit=` (1 to 200, else 422). `/threads/{root}` takes an observation id or a
seq, and any message of a thread names it; the root comes first as `root`, the rest is paged newest first. A thread of imported history alone has no row in
`/rooms/{ch}/threads`. Until `oracle reindex` has built the index every one of them answers **503**, never a short page (`docs/running.md` has the runbook: it
holds the write lock about 51 s).

An item is `{id, seq, room, author, class, created_ms, signed, content, content_bytes, cargo?, cargo_bytes, thread:{root, root_seq, parent, depth}, intent, ask,
by_ms, expires_ms, superseded_by, supersedes, archived_by, items, marks, votes}` (plus `verification`, and `body_omitted` / `cargo_omitted` when a body is over
64 KiB). Native messages cannot exceed the cap (the envelope refuses more), so only imported rows are ever omitted; a native cargo can be (up to 1 MiB).

Measured on a copy of the live ledger (113,196 rows; the biggest room, `feeds`, has 21,369), whole replies with every item built:

| | n | p50 | p99 | target | |
|---|---|---|---|---|---|
| `/rooms/{ch}/messages`, newest 50 and 200 | 24 | 3.9 ms | 12.1 ms | p99 < 20 ms | met |
| … with `include_cargo`, 50 | 12 | 2.0 ms | 4.8 ms | | |
| … 100 back from the middle of the room | 12 | 3.0 ms | 7.5 ms | | |
| … 100 forward from the middle (`after=`) | 12 | 3.1 ms | 7.8 ms | | |
| `/rooms/{ch}/threads`, 50 | 12 | 0.2 ms | 14.4 ms | | first call in a room is cold |
| `/threads/{root}`, the 40 biggest (largest 80 replies), 200 | 40 | 1.6 ms | 8.3 ms | p99 < 10 ms | met |

A page chain to the room's first message, in pages of 200, returns exactly the rows the index holds in all 12 of the largest rooms. The parity with the
fold and the reindex time (51 s) are unchanged.

### 3c, built: the ledger, unread, and who may read another seat's lists (`roomread.rs`, `store.rs`, `tests/room_asked.rs`)

- `GET /asked?seat=&state=&before=&limit=`: every item (R1, Q1, D1 ...) of every message a seat wrote, newest message first, each with its **words**
  (`text`, read from the cargo that declared it: the message's own `{"items":[{k,kind,text}]}`, else the newest annotation's), state, owner and who moved it last.
  `state=` is a comma list of item states and keeps only those items and the messages that have any. It pages by the message's seq (`before`, `next_before`
  null when nothing is older; `limit` counts messages). This replaces the read-time scan the view used (about 10 s).
- `GET /rooms/{ch}/unread?seat=`: the seat's read cursor, how many other seats' messages, statuses and cards came after it (capped at 1000), where they start and
  the room's newest seq. Page them with `/rooms/{ch}/messages?after=<cursor>`.
- `GET /health` gains `index: {ready, through_seq, log_seq}` and `cross_seat: "audit" | "enforce"`.
- **Another seat's lists** (`/asks?seat=`, `/asked?seat=`, `unread?seat=`; Emmy's Q1). A seat always reads its own. Anyone else needs to be a registered
  **gateway** for that route: a named service identity, written only by the owner-side CLI (`oracle gateway add <seat> --routes asks,asked,unread`); the
  seat must have an active key. Nothing in a request can make a token a gateway: policy reads the registry, keyed by the handle the server took from the token.
  Every cross-seat request is **audited** (`oracle gateway audit`: caller, target, route, whether policy allowed it, whether a refusal was returned).
  The server starts in **audit mode**: every cross-seat read is let through and recorded as what enforcing would have done. `ORACLE_CROSS_SEAT=enforce`
  refuses (403). **Flip after dev parity, with a way back**: run the dev Inbox, Tasks and Asked in audit mode, read the audit for every caller that
  would be refused, register the web door as a gateway, then enforce; the rollback is to unset the variable and restart (the registrations stay).
  The day of the flip is Rocky's to be told.

Measured on a ledger copy (113,508 rows): `/asked` p50 4.4 ms, p99 9.0 ms (50 messages, the busiest seats; unsettled-only p99 10.0 ms); `unread` p50 6.1 ms,
p99 18.1 ms; reindex 55.9 s and parity with the fold unchanged (26,244 of 26,253 threads identical, the same depth-4 nine). `item.text` is now filled where a
cargo carries it (198 items on the copy).

### 3d, built: search as a page (`roomread::search`, `tests/room_search.rs`)

`GET /search?q=` keeps its legacy reply (`{count, hits}`) for callers that use only `q`, `channel`, `kind` and `limit`. Any of **`author`, `class`, `order`, `match`,
`include_cargo`, `before`, or `format=page`** returns the paged item shape instead: `{q, room, items, count, limit, order, truncated, next_before}`, each item the
same item every other route returns plus a `score`. The room (`channel=`), the author and the classes (a comma list, an OR) are asked for by name; the room and class
are tokens of the search row (step 3a), so a room's search only matches and ranks that room's rows, history included.

- `order=newest` (default): matches page by seq, `before=` exclusive, `next_before` null when exhausted, a chain reaches the oldest match with nothing twice.
- `order=rank`: the best `limit` matches (bm25), best first, `truncated: true` when there were more; there is no `before=` (a rank has no stable keyset), page with `newest`.
- `match=all` (default, every word) or `any`. Words are alphanumerics; operators and punctuation are words to look for, never syntax. `limit` is 1 to 200; a bad
  class, order or limit is 422; 503 until `oracle reindex` has built the index (the tokens depend on it).

Measured on a ledger copy (113,648 rows): a word in a room, newest 50: p50 3.2 ms, **p99 7.9 ms**; ranked 50: p50 3.3 ms, **p99 30.6 ms** (max 45 ms); room + author +
classes: p99 6.7 ms; a word across every room, newest 50: p99 6.7 ms. Target p99 < 50 ms: met (the first cut was p99 189 ms). Reindex 53.4 s. Parity with the fold:
26,298 of 26,311 threads identical; the 13 that differ are all messages at depth 4, the fold's reach limit (the same cause as before; the ledger has grown).

## Thread state from `done`

`thread.state` comes from the root's verdict; a `done` verdict (`settled`, `stale`, `superseded`, `acked`) is the state after closed, superseded, archived and expired, before ambient, answered and open. `?state=stale` and `?state=acked` filter on it. `reindex` reproduces it from the annotations alone.

## Room State: complete, at a cursor, and why a revision exists

A room's State is a signed say tagged `room-context:v1` whose cargo is `room-context/1` (`channel`, `parents`, `fields`); the heads are the revisions no other names as a parent. The reader (`ox state`, `oracle_room_state`) walks the listing page by page along `next_before` (an empty page that still carries a cursor is not the end) and answers `state: incomplete`, `complete: false`, with no fields, if the walk hits its page budget: a State read off part of its history may name a head that is not the newest. `as_of=<seq>` resolves the State below an exclusive cursor, so the source cursor U of a review and the projection cursor V after later events are two different numbers. Keys and envelopes are read as they are now; archive marks count only below the cursor. `missing_parents` lists parents no listed revision carries (reported, not fatal: State lines older than the tag name some).

A revision may add `adopts`: `{room, proposal, mark, cursor, decided_by, word}`. At write time the proposal must be a `proposal` of the same room that `GET /threads/{id}` shows `settled_word: accepted`, with `settled_by` the deciding mark (a decider's mark: the seat it was addressed to, or a room root; anyone else's go is an opinion and settles nothing). `mark` is the settling event, `cursor` its seq, `decided_by` its author. This is provenance the writer signs after a check, not enforcement: the server accepts any signed revision, so nothing stops a client that skips the check, and a later grant never rewrites an earlier adoption. A revision without `adopts` is valid and is shown as unadopted. The atomic expected-head and authority guard on the write itself belongs to `Store::write` and is not part of this change.

**The resolver (`Store::room_state_at(room, as_of_seq: Option<i64>) -> Value`).** One fold, shared with the guard (`store::room_revisions`). `as_of_seq` is exclusive (rows with seq < it; `None` is now and the answer names it). The answer: `schema, channel, state` (`empty | alive | degraded`), `as_of_seq` (effective; also `as_of`, and `as_of_requested`), `read_through_seq`, `coverage {sql_complete, revisions, bad, skipped}`, `trustworthy`, `complete` (= trustworthy; SQL coverage is not trust), `heads`, `head_seqs [{id, seq, author, received_ms, signed_ms}]`, `fields` (only when alive, else null), `revisions`, `bad [{id, why}]`, `unknown` (always empty: a failed read is an error, never a partial answer), `skipped`, `adoptions {head: adopts}`, `missing_parents`, `trust` (the boundary, in words) and `evidence`: for every counted revision `{id, seq, author, received_ms, signed_ms, native, envelope (the signed object, native only), pubkey, registered, row (the stored Observation read in the same snapshot, native and imported), parents, fields, adopts}`. A verifier re-checks `envelope` against `pubkey` and `row.cargo` itself; parents, fields and adopts are projections of the cargo and are not evidence. Keys are read as of now.

**Server guard.** `Store::write` enforces both claims in the transaction that stores the revision: `parents` must equal the room's current head set (computed as the reader computes it; a stale, delayed or concurrent writer gets 409 `stale State` and must read the head again), and an `adopts` must name a proposal of the same room, settled `accepted` by a decider, by the mark that is the thread's settlement now, with that mark's seq as `cursor` and its author as `decided_by` (400 otherwise). An idempotent retry of a write that already landed is still a success. A tagged line that is not a revision of its own room is stored as before and reported `bad` by the reader. Revisions older than this guard are not re-judged. Who could decide is judged AT THE MARK from facts that cannot change afterwards, not from today's fold: `adopts.basis` is `addressed` (the proposal's own signed `to:` names the marking seat; an annotation cannot add one) or `membership` (the newest `[<Room> members]` record below the mark, signed by an owner-key seat, lists the marking seat as a root), and the claim must be true; a grant made after the mark does not authorize it, and no past role history is invented. Trust of the signing key is as the server verified it when the mark was written. The guard counts exactly the heads the reader counts (a native row counts only if its key is registered to its author and its stored envelope verifies with this cargo), so a valid client write is never refused for a head only one side sees. A losing writer in a race gets 409 naming the head to rebase on: the write lock is taken first, and a busy database is a 503 `retry`, never a 500.

## The fold's reach, fixed (2026-10-16)

Blaze's room, 16 Oct: Bobby's buzz at 03:35Z named a thread whose new message was its sixth reply. `GET /thread` resolved the root (it walks up
8 links) and then read the members 4 links deep (`Store::thread_members`), so the message the buzz pointed at was not in the answer, and the
seat rightly saw nothing new. The parity runs above had been showing the same limit as the "depth-4 differences" since step 2.

Now: `thread_members` follows replies until the thread ends (64 links at most, against a loop), collects up to 2,000 members, and keeps the
newest `max` (300) when there are more, so a long thread loses its oldest messages, never its newest. `thread_root` walks up as far. `GET /thread?id=`
always includes the message it was asked about, even when the thread is longer than the cap. Membership is unchanged otherwise: the fold still
takes resolvers (a `closes:` or `annotates:` from another thread) that the index files under their own thread, which is why the routes do not
simply read `msg` by `root_seq`. Proof: `tests/room_view.rs` (a seven-deep exchange, the cap, the deep root) and the parity test of the index
against the fold, which now has no depth-4 exceptions to explain.
