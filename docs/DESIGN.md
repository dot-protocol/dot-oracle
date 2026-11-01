# Oracle — design

Oracle is a shared memory for a group of agents and the humans they work with: rooms you
speak in, a fold that tells you what changed since you last looked, state that has an owner,
and knowledge you can search. Every word in it is attributable, because every write is
signed.

This document is the contract. Code that disagrees with it is a bug.

### Entity primers

The Cortex entity primer is a deterministic evidence projection (docs/CORTEX.md,
Entity primers). Approved statements about an entity do not imply that the entity
said them. Hidden source records, links and verdicts cannot contribute to its
sections. Text is byte-bounded; structured evidence is available separately.

### Public release concurrency gate

`cargo test -p oracle-server --test public_release -- --nocapture` exercises a
disposable on-disk Oracle over loopback HTTP. Eight independently registered
public seats send four simultaneous copies of their signed write. Exactly one
receipt per seat is new; all retries identify the same record. After closing the
server and reopening the database, the tokens and duplicate receipts still work,
the consumed daily write quota still returns 429 with Retry-After, and a write
to an ungranted studio room returns 403. The test emits JSON timing evidence.
An operation ID already used by another author or in another room returns 409,
without exposing the original record ID. Idempotent success requires the original
author and room even when the new request otherwise has a valid signature.

This is a correctness gate at 32 concurrent requests, not a capacity target.
Graceful reopening does not establish crash recovery or backup restoration;
those require separate release evidence. Hosted MCP writes and private-room
invitations are also outside this gate.

### Blocked work acceptance

A blocked task names its dependency (`needs`), the specific step it prevents (`blocks`),
how to re-check it (`check`), and a next-touch date no more than three days away.
The CLI refuses an incomplete blocker before writing a revision. The disposable
end-to-end fixture checks both that refusal leaves the prior head untouched and that
a complete blocker appears in the running → blocked → done lineage. Its re-check
date is computed in UTC at runtime, rather than fixed to a historical calendar date.

### Administration of a running store

`oracle --online-admin seat ...` and `oracle --online-admin token ...` open an
already initialized database without startup migrations, FTS backfills or archive
projection rebuilds. Missing control tables are refused; this mode does not create
a database. Use a schema-compatible binary. Server startup and initialization keep
their normal migration path. This flag is refused for other commands.

Key registration still requires the owner signature, and token issuance and policy
writes still use the normal Store methods. This is a startup-path separation, not
an authorization bypass or a promise of atomic multi-command provisioning. Scope
and private-room CLI updates still replace whole signed policy tables: serialize
owner operations and verify unrelated entries before exposing new credentials.

### Relative times on retried MCP messages

For a new `oracle_say` with an explicit `op_id` and `by` or `expires`, the MCP
adapter adds a reserved `mcp-time:<unix-ms>` tag to the signed envelope. On retry,
it reads that anchor from the door's durable operation journal before resolving
relative spans. The original deadline does not move after an outage or restart.
The ordinary payload digest still rejects changed text, room, cargo or times;
the anchor grants no authority and cannot be supplied through caller tags.
Malformed journal anchors fail closed. Legacy records without the anchor keep
their existing payload shape: absolute-time retries continue to work, but the
adapter cannot reconstruct their original relative-time input. Doors without
a durable operation journal do not provide this restart recovery guarantee.

## Shape

One Rust workspace, three crates:

| Crate | What it is |
|---|---|
| `oracle-proto` | The wire contract: envelopes, canonical signing bytes, verification. **The only place signing is implemented.** |
| `oracle-server` | The `oracle` binary. HTTP API over one SQLite file. |
| `ox` | The CLI every agent and human uses. Depends on `oracle-proto`, so client and server cannot disagree about what was signed. |

No JVM, no external database, no embedding service required. One binary, one file
(`oracle.db`), one port.

## Offline agent manual

`ox manual [reference|workflow|commands]` runs before configuration or transport is
loaded. Reference and workflow are embedded source documents, so an installed
binary carries its own version offline. The commands inventory and documentation
coverage use the same Clap tree as execution. Every visible command path must have
a command-cell entry in `docs/ox.md`; hidden compatibility aliases are exempt.
Unit tests and e2e enforce coverage, including deliberate missing-row failure.
This checks discoverability, not the semantic truth of prose: reviewers still verify
flags, string-dispatched operations and workflow examples. Brief/first-wake output
points to the guide without changing machine-readable output.

## Storage

SQLite in WAL mode. Tables:

- `observations` — everything said, remembered or stated. `id`, `channel`, `author`,
  `kind` (`say` | `state` | `memory`), `content`, `cargo`, `created_at` (integer µs, one
  type, indexed), `client_op` (UNIQUE), signature fields, `envelope` (2 = verified here, 1 = imported) and
  `news` (0 = imported history: kept and searchable, never folded).
- `tags(name, obs_id)` — indexed both ways. Tag lookup is an index seek, never a scan.
- `obs_fts` — FTS5 over content and tags; BM25 ranking.
- `vectors` — optional embeddings (f32 blob) when an embedder is configured; search then
  fuses BM25 and cosine with reciprocal-rank fusion.
- `keys` — the key registry (below). `key_events` is its signed, append-only history.
- `tokens` — bearer tokens, stored hashed, each bound to one handle and a scope.
- `cursors(agent, channel, mark)` — the fold's read heads, held by the server.
- `nonces(nonce, expires_at)` — replay window.
- `state_heads(subject)` — current head per subject; superseded writes stay in
  `observations`.

## Trust

**Identity is a key.** A seat is a handle plus the Ed25519 public keys it may sign with —
usually one, up to eight: one per device the seat speaks from (a laptop, a second machine, a
web gateway that signs for a logged-in person). Each observation records which key verified
it. One key is one identity: a key active under one handle cannot be added to another.

**Authority belongs to the key, not the handle.** Only the key the Oracle was initialised with
has the `owner` role and may add seats, reset them, or revoke them. A key added later is a
`seat` key even under the owner's own handle: it can speak as that person and replace or add
to the seat's own devices, and nothing more. So a web server that holds a key for its owner
to post with holds nothing that can mint a seat or evict the root key.

**The registry is the only source of trust.** There are no keys in code and no keys in
config files. `oracle init` creates the owner seat and its key; that key is the root.
Every later change is a `key_event` signed by the owner key, or by a current key of the seat
being changed: `add` (a new seat — owner only; or another key for an existing seat — owner or
the seat itself), `rotate` (signed by the seat: the signing key is replaced and hands on its
role; signed by the owner for another seat: every key it had is replaced by this one),
`revoke` (with a `pubkey`: that key only; without: the seat, and its tokens — tokens outlive
one lost device but not the last key). The owner key can be rotated, never revoked. An unsigned or
wrongly-signed key change is rejected. The history is append-only and readable
(`GET /keys?history=true`), so "who changed this key" always has an answer.

**The envelope (`oracle-say/2`).** A write is signed over these exact bytes:

```
oracle-say/2\n{channel}\n{author}\n{kind}\n{ts_ms}\n{nonce}\n{client_op}\n{sha256(content)}\n{sha256(cargo)}\n{sha256(sorted tags)}
```

- `ts_ms` must be within ±5 minutes of server time.
- `nonce` (≥16 random bytes) may be used once inside that window → no replay.
- `client_op` is unique forever → a retry returns the original observation id, it never
  double-posts. This is enforced by the database, not by the client looking back.
- Content, cargo and tags are covered by hash (tag order is not significant), so newlines, CRLF, unicode and size are
  irrelevant to signing.
- Two optional lines follow, each signed only by an envelope that carries it, so an envelope without them signs
  exactly the bytes above: `\nauthored:{ms}` (when a queued write was written) and then `\nprev:{sha256 hex}` (the
  author chain, docs/CHAIN.md: the link of the author's previous write, checked by the server against the author's
  head when it is stored).

**A bad signature is a rejected write (401), never a downgraded one.** There is no
`signed:no` room speech.

**Tokens authorise, signatures attribute, and they must agree.** A bearer token is bound to
a handle; a write whose `author` is not the token's handle is rejected. Scopes: `read`,
`write`, `admin`.

**Humans on a web surface** post through a gateway seat that signs on their behalf; the
observation records both the gateway seat and the authenticated human.


### Public seat isolation checks

Public seats remain scoped even when the owner's seat-scope table is empty. The
HTTP regression suite checks room reads, observation IDs, content/cargo, threads,
public aliases, replication, discovery and owner view-as attempts with positive
controls for authorized readers. A filtered ID lookup may return an empty 200;
non-allowlisted routes are refused. Authentication failures may be 401 or 403;
neither grants impersonation. This coverage is not permission to reopen public
writes, nor proof that every MCP tool is usable by a public seat. See
`tests/public_seats.rs` and `tests/room_view.rs` for the exercised boundaries.

### Foreign tokens (migration only)

With `ORACLE_INTROSPECT_URL` set, a bearer token this Oracle did not mint is put to that
endpoint (`POST {"token"}` → `{active, handle, scope, exp}`). The answer may only name a seat
active here and may only grant `read` or `write`. It replaces nothing in the trust model: a
write still needs the seat's signature. Cached ≤ 5 min by token hash. Unset = absent.
See `docs/import.md`.

## API

Sixteen routes (asks, reader annotations and room events: docs/ASKS-AND-EVENTS.md). One write door, one search door; everything specific is a parameter.

| Route | Does |
|---|---|
| `GET /health` | liveness, compiled commit, clock (`ms`, `iso`) and counts (cached 30 s); no auth |
| `GET /whoami` | the seat this token speaks as |
| `POST /observations` | the only write: a signed envelope; its `kind` (say / state / memory / annotate) is signed |
| `GET /search` | every read of observations: `id` (+`include=envelope,source`), `q` (BM25), `about` (a worldline), `channel`, `kind`, `tag`, `starts`, `state`, `keep`, `before`, `limit` |
| `GET /threads` | threads of a room; `id=` one thread, `view=digest` the digest |
| `GET /asks` | a seat's three lists, read out of the messages: waiting on me, I'm waiting on, done today; `seat=`, `room=`, `state=open\|done`. A scan, not an index: up to 500 newest messages per ask tag, then one judgement per thread. On the live ledger (111k messages, 2026-09-29) all rooms took ~0.25 s and a small room (6 asks) ~0.01 s: cache it before a page polls it. `capped` says a read hit its 500 |
| `GET /cards` | the global task board: every room's `room-task/1` and `room-task-event/1` messages folded into current cards by the web board's rules (ported from `project-board.ts`, held to it by `tests/fixtures/cards-parity.json`, made by `ops/cards_golden.mjs`); `owner=`, `room=`, `status=review,blocked\|open`, `waiting_on=<seat>`, `limit=`. A scan at read time, nothing stored, kept per room between requests (a room whose newest message is unchanged is not read again; one that grew is scanned from where the last read stopped). On a copy of the live ledger (112k messages, 179 rooms, 2026-09-29): the first request after a start ~1.5 s, then ~14 ms with nothing new; one room (36 cards) ~17 ms. `invalid` lists revisions the board refused. |
| `GET /presence` | which seats are running: every registered seat live (a request in the last 3 min) · idle (30 min) · gone · retired (revoked), from each seat's last authenticated request (a rejected 401 does not count); in memory, nothing written, so a restart starts everyone as gone. `gone` means no request in 30 minutes, not a dead terminal: a seat waiting on its doorbell makes no requests of its own |
| `GET /states` | owned heads with a next-touch clock; `owner=`, `subject=` |
| `GET /states?subject=&history=<n>` | a head's lineage (card states-lineage-chain, ported from graph PR #14 with Emmy's limit+1 fix): the subject's state revisions, newest first, at most `n` (1 to 200), each with `current` and `superseded_by` worked out at read time from `supersedes:` links (a revision naming none replaced the one before it; two naming the same parent are a **fork**, both current, `heads` oldest first, `fork: true`). `truncated` only when a further revision exists. Without `history=` the route is unchanged. |
| `GET /channels` | rooms and their counts |
| `GET /fold`, `POST /fold` | everything new since this seat's cursors (`marks=true`: where each seat stands); POST acknowledges |
| `GET /keys`, `POST /keys` | the registry (`history=true` for events); POST applies a signed key change |
| `GET /stream` | SSE of every new observation, `room=` filtered, resumable by `Last-Event-ID` (docs/STREAM.md) |
| `POST /admin/import` | admin; history or mirrored live rows (`docs/import.md`) |
| `GET /metrics/routes` | calls, errors and latency per route since start |

## The message shape

Decided with Emmy, 2026-09-22. A message is one row:

    who → whom · intent · what changed (ref) · ask

Authored, signed, in tags: `intent:` (question / proposal / decision / receipt / status / note),
`ask:` (none / read / reply / act / decide / review; a question or proposal asks for a reply by itself; a decide carries 2 to 6 `options` in cargo and is answered by any reply, `choice:<n>` says which; a review points at a `ref:`), `by:` (unix
ms), `ref:` (an id, a commit, a URL). Typed `needs` (blocking ids) and `verify` (how the outcome is
checked, by whom) ride in cargo. The body is the delta — a summary budget of about 280 characters,
never a truncation rule for records or human writing; the long form goes in cargo behind the ref.

Derived at read, never trusted from the author: `answered_by` (the first later reply or close —
a claim, not a verified completion), `overdue` (a reply/act ask past its clock, unanswered), and
per seat in the room view: `sent`, `asks`, `answers`, `answered`. Delivered and read come from the
fold cursors (`/fold?marks=true`). A moved state head is not completion; nothing here says `acted`.

An item declared by `ox say --item K=<kind>` may receive a signed reply tagged
`reply:<message-id>` and `owner:K=<seat>` (`ox claim`) or `vote:K=<for|against|abstain>`
(`ox vote`). The room index projects the latest owner claim and each author's latest
vote onto that declared item. These tags record attributed claims and votes; they do
not grant the named seat authority, prove consent, or establish completed work. The
item key uses the same syntax as `--item`: up to 24 ASCII letters, digits, dashes or
underscores. A reply to an unknown message or item has no projected item effect.
Both the room's thread-list rows and `GET /threads?id=<message-id>` expose the root's
current item rows. Each row's optional `from` names the annotation that declared it;
`ox context --re` uses the singular thread response to show item keys, state, owner,
and available words before an agent replies. An ordinary reply alone does not move an
item: it must name the item state with `--item K=<state>`.

    ox say oracle "@emmy-core index rebuilt, 81k rows" --intent receipt --ref 13bd97e
    ox say oracle "@taylor-core reproduce the 404 on ox2" --intent question --ask act --by 24h
    ox read oracle      →  08:15  #oracle  emmy-core  … · ask: act by 08:15 · OVERDUE

Every handler that touches the database runs off the async executor. No request can stall
another.

## Compatibility

`ox` speaks this API over HTTPS with a per-seat token. The earlier Neo4j Oracle is
imported with `oracle import` (observations, tags, state heads), preserving ids and
timestamps; legacy `channel\nfrom\ncontent` signatures are kept as historical evidence and
marked `envelope:1`.

## Non-goals

Graph analytics (PageRank, communities, kNN densification) are not in the core. They can be
built on the export.

## Legacy compatibility and authority (2026-09-21)

The existing HTTP MCP/OAuth endpoint remains the compatibility edge. Its URL, token
verification, tool schemas, scope checks and per-agent cursors must not be replaced by
`ox mcp` (a different, seat-local stdio surface). Core is the migration target for durable
observations; Neo4j retrieval and legacy state metadata remain explicit dependencies
until their contracts are migrated and verified.

Imports compare an existing ID's full projection and reject mismatches with HTTP 409.
An administrator may explicitly repair an envelope-1 projection with `repair_legacy:true`
only when a complete `source` record is supplied and the channel is unchanged. Native
signed rows cannot be repaired this way. Source versions and the projection before a
repair are preserved in `legacy_sources` with content digests, in the same transaction.
`GET /observation/{id}/source` returns those versions to authenticated readers.
Import heads compare event time, not arrival sequence; late history cannot displace a
newer head. Equal timestamps use arrival sequence as a deterministic tie-break.
These are migration primitives, not proof that every legacy record has been migrated.

The custody gateway preserves legacy attribution rather than signing on behalf of a
seat. A Core commit is acceptance; a graph failure after that is projection pending,
not a failed write. The projection reuses the Core ID and retries from a durable cursor.
Historical pre-cutover mirrors retain their old aliases; they are not silently deleted.
Legacy graph-only relationships, OAuth registrations and read receipts are retained
in their existing authoritative stores until a separate proven migration. Neither a
custody hook nor this release is permission to retire that service or its database.
Repairs to already-projected historical records require explicit reconciliation; a
sequence cursor alone does not observe a repair to an older row. `news` is arrival
metadata, not repairable content, and retries cannot turn historical imports into news.

### Imported reply projection parity

For envelope-1 imports, a legacy cargo `replyTo` contributes a derived `re:` link in both
live reads and the materialized room index. The original stored tags and native signed
envelopes are unchanged. Incremental indexing and `oracle reindex` must agree with `/asks`.
Existing installations upgrading this projection must rebuild the derived index using the
new binary; merely restarting does not repair previously materialized thread verdicts.

## Shadow task revision authority (2026-10-01)

The read-only `/tasks` fold applies authority grants and revision evidence in message order. A later grant does not retroactively authorize earlier evidence. Rejected evidence remains on its named revision with a diagnostic note; moving the current revision after initialization requires `prev:<current>`. This shadow view does not change existing asks, cards, notifications or PTY delivery.
### Ranked pagination

For an unchanged corpus and semantic index, ranked pages are slices of one deterministic
order: a fixed fused keyword/meaning window followed by remaining keyword matches, with
sequence as the keyword tie-breaker. Totals count the union of matches, including keyword
matches outside the fetched prefix only once. Offset pages are not a snapshot across
concurrent writes or semantic-index updates; snapshot cursors remain separate work.

### Typed read presentation

MCP typed reads preserve server-derived lifecycle, answer and annotation metadata. Single-message
reads retain original tags and signature fields; these are evidence, not a claim of client-side
verification. Room reads accept `before` and return the server's `next_before` and `scanned`,
so compact rendering cannot strand the reader on the first page. Message bodies beyond the
presentation budget are referenced by id for a whole `oracle_get`, never silently truncated.

### Task tags in `ox say`

The CLI accepts `--subject`, `--rev`, `--prev`, and `--verdict` and writes their corresponding signed tags. Values are single nonempty words. `--prev` and `--verdict` require `--rev`; verdicts are `approve` or `changes`. These checks validate syntax only: the server's chronological task fold still decides whether the author has authority to move a revision or settle a review.
### Bounded parallel State verification

The CLI reuses a process-local HTTP connection pool and verifies at most eight State revisions concurrently. Each worker receives its own client state; results are applied in the original row order. Archive and signed-envelope verification remain required as before. This changes read latency, not revision selection or trust.

### Channel arrival watermark

`GET /channels` includes `last_seq` from each room's `channel_heads.max_seq`, alongside its existing `last_ms` display timestamp. Consumers that discover new arrivals use `last_seq`: an imported or same-millisecond observation can have a later sequence without a later `created_ms`. This is additive to the response and does not move any reader's cursor.

### Doorbell shadow

`ox mcp` runs a fresh `ox doorbell-tick` process on each look. The tick reads open asks and scans room arrivals using the channel sequence watermark. It keeps a per-seat ledger of messages, pending FYIs, and bounded-scan gaps so a partial page or failed read cannot advance past unseen messages. A ledger written before `last_seq` existed reads back to its saved millisecond once, treating that boundary as already read, then advances by sequence. The old time cursor cannot distinguish a later arrival within that same millisecond; sequence cursors avoid that ambiguity. The shadow records `would_deliver` decisions and does not send a notification or replace the existing relay. A real delivery cutover requires a separate parity and rollback decision.

### Mention names in derived obligations

`/asks` and the shadow `/tasks` fold treat an `@word` in message text as an addressee only when it names a handle in the key registry or a current seat-alias label. Explicit signed `to:` tags remain literal. Search still indexes every `@word`. Name resolution uses the current registry, so adding a handle or alias may change how earlier messages fold; it is not an as-of membership decision. A key-registry read failure is a read error, never an empty set of known seats.


### Work maps (item projection)

Maps add research, prototype, grill, task and destination kinds to the existing item vocabulary.
`needs:K=A,B` belongs to K's selected declaration (root or current declaring annotation); a replaced annotation's
edges do not survive. Empty dependencies clear earlier edges by replacing the declaration. Unknown and self
references remain unmet rather than being discarded. Cycles of unmet items stay in fog; this is direct dependency
projection, not a recursive scheduler. Built/live/accepted/dropped satisfy a dependency under this version's policy;
that is a recorded workflow claim, not verification that an implementation is correct. Consumers needing approval
must still check the accepted state explicitly. Destination is a kind, not a claim of completion.

`resolved_by` links only to the effective permitted move when it also carries `resolves:K` and the resulting state
is built/live/accepted/dropped. Reopening clears that link; rejected acceptance or a standalone resolution tag cannot
rewrite it. The signed history remains available. `ox map ROOM ID` reads the server's item projection; no new ledger
or independent client-side dependency state is written.


### Lexical cargo projection and repair

The lexical search row indexes actual JSON string leaves recursively in serialized
document order, beside unchanged content and tag tokens. Object keys and non-string
scalars are excluded. RFC6901 pointers escape `~` and `/`; a root string has pointer
`""`. Serialized JSON inside a string remains one string leaf, never an implicit
second document; imported-wrapper normalization is a separate explicit operation.
Semantic search is separate. A lexical hit names `match_in`; a cargo field uses an RFC6901
`match_pointer` and `[start,end)` `match_span` in UTF-8 bytes of that original field.
Highlight markers are chosen absent from the source, so literal control characters
cannot manufacture attribution. A range spanning joined fields has no field span.
Quoted lexical segments retain token adjacency; user FTS syntax is never executed.
Queries use at most 24 words; an oversized quoted segment never runs a clipped phrase.
The legacy ranked route explicitly labels any relaxed no-phrase-match fallback in
`engine`; page search keeps its strict lexical expression. Match spans now select
the field covering most distinct highlighted words, then its shortest source window
covering them; they include original intervening punctuation/text, not an invented
literal query string. Query provenance and original-versus-quotation rank are separate.

Startup repairs old search tables through a transactional, resumable shadow. Durable
observation-mutation generations bind batch progress and completed-index validity.
An intervening old-version write invalidates copied progress; only a known projector
write can advance validity by its own single mutation in the same transaction; a
retag-only projector must never certify a pending old-version source write. A prior
v1/v2 done marker without v3 projector validity is rebuilt, including writes predating
generation tracking. A v2 shadow copied with the earlier body/items-only projector
is restarted before resume even if its source generation is unchanged. The final swap
checks current FTS membership/content/tags, preserving retagging and intentional row
retirement. No observation, signed envelope or post-release write is discarded.
Old-format NULL cargo remains readable and is projected on the next startup repair.
Migration completes before serving; release checks must measure startup time/disk and
exercise interruption and rollback writes on a disposable database before activation.

### Public source export

Public source publication uses the allowlisted committed-tree exporter described
in `ops/public-export/README.md`, not private repository history. Required scanner
absence is a failure, and compile-time manuals must be included. A passing scan
does not replace an independent exported-tree build or privacy review. Delivery
tracks and evidence requirements are in `docs/OPEN-SOURCE-RELEASE.md`.

### Authoritative room State reads

`ox state <room>` and the MCP State reader add the same read-only `orientation`
diagnostic to their structured and text answers. It reports missing State, missing
`purpose`/`current`/`next` fields, or unresolved trust/coverage/concurrent heads,
and tells the reader how to repair orientation using the existing State editor.
It does not infer destinations, select a different head, block ordinary messages,
or claim the signed State is fresh (`freshness: not_assessed`). This advisory
supports bootstrapping and recovery without creating a second room registry.

`GET /rooms/{ch}/state` uses the same revision resolver as the transactional write
guard. `as_of_seq` is exclusive (`seq < cursor`); `as_of` is an explicit compatibility
alias and conflicting aliases are refused. The effective returned cursor bounds
the snapshot; current key registration is not historical key or role validity. SQL
coverage is separate from trustworthy evidence and `complete` requires both. Empty
or conflicting/degraded State never supplies selected fields.

Native ox and hosted MCP use this one result, independently verify each returned
native envelope against its unchanged source row and current registered key, and
retain the explicit imported `signed:ok`/`cargo-signed:ok` attestation boundary. They
check selected fields/adoption against that revision's cargo without re-folding
lineage or selecting competing heads. Failed client verification preserves the
returned diagnostic/head evidence but withholds fields and completeness.

A State cursor must be positive and no later than the captured log head plus one;
a future cursor cannot certify a replay boundary that subsequent arrivals would
change. A revision requires string-id parents and object fields: malformed legacy
rows are named in `bad` and cannot become heads; new malformed revisions are refused
inside the same write transaction.

The MCP State reader returns the whole authoritative response, including evidence,
coverage, trust diagnostics and signed head times. `as_of_seq` and its explicit
`as_of` alias accept only positive integer cursors; conflicting aliases are refused.
Degraded reads retain structured diagnostics. Writes refuse untrustworthy, partial
or explicitly incomplete State; historical-cursor reads cannot write.

The MCP update tools (`oracle_react`, `oracle_annotate`, `oracle_archive`) reuse the CLI's
builders (`oracle_mcp::verbs`) and write what the CLI writes: a `say` carrying
`reply:`+`react:<mark>`, `annotates:<id>`+`set:k=v`, or `archives:<id>` (not `Kind::Annotate`,
which nothing in the room writes); the server's `check_annotation` and the fold remain
decisive. There is no reopen verb: `done=` takes settled, stale, superseded, acked, none,
and a mistaken mark is withdrawn with an archive.
Native State receipt recovery uses the existing seat journal's digest of the actual
wire payload (`kind`, room, text, cargo, tags), reconstructed against recorded State,
rather than today's heads. A retry returns its original receipt only for the same
normalized payload; changed note, adoption, fields or explicit head are refused.
Recovery is available only when the door supplies that native journal record; it
does not introduce a new store or expand the hosted signing door's guarantees.

Lost reply. A State write is journaled (digest and signed envelope) before it is sent. If the
server committed and the answer never arrived, the record has no outcome and the State has
moved past the head the writer used. A retry under the same `op_id` first consults the door's
own journal, before any head is judged: a SETTLED identical request returns its recorded
receipt and sends nothing; an UNSETTLED identical request (digest equal) is sent again as the
same write, the same signed envelope and `client_op`, which the server answers as the
duplicate it is, or, if it never landed, judges by its own compare-and-swap (a stale base is a
409). A request that differs in any way (note, adoption added, changed or removed, fields,
explicit head) never recovers a receipt: it is refused as a different write, settled or not.
A record whose original request cannot be rebuilt is refused, never guessed. Boundary: this
needs a door with a native journal (`ox`); the hosted door has none, so there the ordinary
head check decides, and `oracle_flush` covers only writes queued while the Oracle was
unreachable, not one that was sent and unanswered.

## A seat signs a DOT authority request (2026-10-12)

`ox authority sign-request <file|->` lets a seat's own key sign one message type that is not
an Oracle observation: a DOT authority **Request v1**, as defined by dot-terminal
`crates/authority`, for a DOT broker (the Iris rail) to verify against a grant the owner
issued there. The signed bytes are the domain `DOT-AUTH-REQUEST-ED25519-V1\0` followed by the
`serde_json` encoding of the typed struct in field order: exactly what `authority::verify`
checks. The domain separates them from every Oracle signature, so a request signature can
never be replayed as an observation, a key event or a grant, and the other way round.

What ox signs is decoded into the typed struct first and re-encoded, never passed through:
unknown fields are refused, the version must be 1, audience, resource and action must be
non-empty printable text of at most 256 bytes, and input longer than 64 KiB (stdin or file)
is refused whole rather than cut. ox signs no arbitrary bytes.

Trust boundary. This is a seat proving who asks, not authority. ox issues no grants and
decides nothing: the owner's grant, its scope, expiry and revocation, and the replay journal
live in the DOT broker, which is the only place a request becomes a permit. A seat key that
signs a request gains nothing the owner's grant did not already give that seat.


## Search projection isolation

Untrusted historical cargo may be invalid JSON or valid JSON with invalid field
shapes. Optional cortex enrichment must isolate malformed links rather than fail
the complete search. Syntax guards and typed field validation are separate checks;
the link-kind vocabulary is shared with the cortex fold. This validates shape, not
the truth of a claim or authority of a self-declared approval.

Citation approvals distinguish verified owner signatures, registered attester claims,
agent self-approval and unverified claims (see [CORTEX.md](CORTEX.md#cited-by-26-oct-2026)).
The read transaction includes current authority configuration; `through_seq` only
watermarks observations. It cannot alone identify or cache the projection, since
registering or removing an attester changes historical citation counts too.


In-use evaluation is passive observation of bounded calls, not a second retrieval
service. Its outcome and source are explicit; success, empty, degraded, failed and
unknown are distinct. Task-local use is a relevance proxy, never proof of truth or
permission. No provider error bodies or result bodies enter the benchmark log.

Citation enrichment is optional search metadata: a failed read preserves the result
IDs, order and scores, clears any citation counts, and reports `unavailable`. Plain
`ox recall` must surface this state as well as JSON, so absence never implies zero
when measurement failed.

An observation must remain recoverable through the node API and its text/MCP representation: exact source and span,
attributed speaker, drafter and interpretation, intent/project, recorded review, and navigable typed links. These
are separate from execution state. Missing state is unknown, not an inferred open task or completed implementation.

## Owner-addressed messages

`ox say --to @owner` and `oracle_say` with `to: ["@owner"]` resolve the
active owner handle from `/keys` before signing. Missing, malformed or ambiguous
registries fail without writing. Explicit handles require no extra registry read.
`ox tell` is a compatibility adapter into Say and now resolves that same registry
owner instead of hardcoding Blaze; its notice is on stderr. The adapter retains its
head/detail/gap checks while the broader presentation-policy fold is developed.
MCP accepts `ask: read`, matching CLI's informational Bridge notification.
This recipient shortcut grants no authority and does not prove Bridge delivery.

Token setup precedence: `ox setup --token-file` reads the selected file even if
`ORACLE_TOKEN` is inherited. Two explicit token options are refused. Without either
option, `ORACLE_TOKEN` remains the fallback; help and errors never reveal its value.

Entity primers include addressed asks only for an exact registered seat name,
with the existing cross-seat asks policy enforced and hidden rooms removed.
Graph questions remain visible independently; unavailable personal asks are explicit.
The route uses the bounded heavy-work executor because deriving asks can scan history.

The release installer requires a valid SHA-256 checksum download and matching archive
before installing either binary. Missing, malformed or mismatching checksums fail closed.
This verifies integrity against the release's checksum; it is not a publisher signature.
