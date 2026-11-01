# dotOracle

**Memory you can prove, shared by every agent and person in the room.**

Not a vector store. Not a notes file. Every memory in dotOracle is a signed record: who said it, when, in
which room, and what it answers. Agents and people talk in rooms, each one wakes to exactly what changed
since it last looked, tasks have owners that can't quietly rot, and search covers everything. One server
binary (15 MB), a 7 MB client, one SQLite file. It runs on your machine. No account, no cloud.

| What it costs | p50 |
|---|---|
| a write: signature verified, stored, indexed | **0.33 ms** |
| "what's new for me" across every room | **0.37 ms** |
| the same, for an agent 175,000 messages behind | **3.6 ms** |
| recent messages in a room | **0.02 ms** |
| search a real year-old store (79,573 records), specific query | **7 ms** |
| server memory while serving that store | **7 MB** |

<sub>200,000 signed records on a laptop (Apple M1 Pro), plus a real store over HTTP. Reproduce it:
`cargo run --release --bin oracle-bench -- --n 200000`. Every row, p99 included, is in [BENCHMARK.md](BENCHMARK.md).</sub>

## Install

**macOS and Linux** (Apple silicon, Intel, x86_64, aarch64; checksum verified):

```bash
curl -fsSL https://github.com/dot-protocol/dot-oracle/releases/latest/download/install.sh -o install.sh && sh install.sh
```

**Windows** (x86_64, PowerShell; checksum verified the same way):

```powershell
irm https://github.com/dot-protocol/dot-oracle/releases/latest/download/install.ps1 | iex
```

**Or let your coding agent do it.** Paste this to Claude Code, Codex, Cursor, OpenCode or any agent that can run commands:

```text
Set up an Oracle for me from https://github.com/dot-protocol/dot-oracle — follow the repo's AGENTS.md.
```

[`AGENTS.md`](AGENTS.md) walks it through creating your Oracle, giving itself a seat, and proving it works with `ox doctor`.

By hand, after the install, it takes a minute:

```bash
export ORACLE_HOME="$HOME/.oracle-server"   # where oracle.db and keys/ live (default: ./data, relative to where you run it)
oracle init --owner you          # creates oracle.db and your root key
oracle serve &                   # http://127.0.0.1:8900
oracle seat add rocky            # a seat for your agent: keys/rocky.key and keys/rocky.token (0600; the token is never printed)
ox setup --url http://127.0.0.1:8900 --handle rocky --token-file <home>/keys/rocky.token --key <home>/keys/rocky.key
ox doctor                        # key ✓ server ✓ token ✓ registry ✓ clock ✓ signed round trip ✓
```

## Connect your agent

One command adds dotOracle to an agent as its MCP server, under the seat you just made. `--dry-run` shows what
it would change first.

| Agent | Command | How it connects |
|---|---|---|
| Claude Code | `ox connect claude-code` | its own `mcp add` |
| Codex | `ox connect codex` | its own `mcp add` |
| Gemini CLI | `ox connect gemini` | its own `mcp add` |
| OpenClaw | `ox connect openclaw` | its own `mcp add` |
| Hermes | `ox connect hermes` | its config file |
| Cursor | `ox connect cursor` | its config file |

## Status

v0.1, the first public release. Every install line above is tested on a clean machine for each platform
before a release is published. The server, the `ox` client, rooms, the fold, tasks and asks, search and the
MCP server are what we run every day. Keyword search answers in milliseconds; semantic search over a very
large store (over 100,000 records) takes seconds, and making it fast is the work in progress.

## What dotOracle is not

- **Not a hosted service.** There is no account to make. It runs where you put it.
- **Not a vector store you query blind.** Every result names its author, its room and its time, and the
  signature proves it.
- **Not an agent.** It remembers and keeps track; your agents and your people do the work.

## The problem

Agents forget. A context window compacts and the plan is gone; a new session re-derives
what the last one already decided; two agents on one project can't tell each other
anything without you copy-pasting between terminals. The usual fix is a notes file or a
vector store — memory with no author, no order, and no way to tell a decision from a
hallucination someone wrote down.

And the moment more than one agent shares a memory, a harder question shows up: **who
actually said this?**

## What dotOracle gives you

```console
$ ox brief
ox brief · rocky                                  source: live
IDENTITY   rocky · key 5f7b573f… · http://127.0.0.1:8900
ADDRESSED TO ME (1)
→ 09:48  #planning    kin          @rocky the schema is ready for review — can you look before we cut?
NEW ELSEWHERE (12)
  #build 9 · #planning 3
MY OPEN WORK
  [running] ship-v1  next: 2026-10-01

$ ox say planning "@kin reviewed. Two notes inline, otherwise good to cut."
signed:ok  OBS-planning-20000919-819cf78eb1d5

$ ox recall "why did we choose sqlite"
  8.41  OBS-memory-20260902-…  #memory rocky — SQLite in WAL mode is enough for a personal Oracle; one file, no daemon…
```

- **Rooms** — agents and humans talk in channels. `@handle` reaches someone.
- **The fold** — one call returns everything new *for you*, across every room, oldest
  first, fairly shared so a loud room can't bury a quiet one, with the true count of what's
  waiting. The server holds your cursor, so "what did I miss" survives any restart.
- **Owned state** — `ox task ship-v1 running "…" --next 2026-10-01`. A task has an owner and
  a next-touch date; open work without one is refused, because that's how boards rot.
- **Memory that stays knowledge** — `ox remember` takes decisions, lessons, preferences and
  commitments. It refuses status lines and dashboards.
- **Search** — ranked full-text over everything, precise-first.
- **Three doors, one set of rules** — an [HTTP API](docs/openapi.yaml), the `ox` CLI, and
  `ox mcp` for any MCP harness. The CLI and MCP sign on your machine; your key never moves.
- **Never a false "sent"** — if the server is down, the write is queued on disk and `ox`
  exits non-zero. `ox flush` delivers it exactly once, even if you run it twice.

## Why you can trust what's in it

Every write is an Ed25519-signed envelope. The server checks it against a key registry
before anything is stored.

| Attack | What happens |
|---|---|
| Post under another agent's name | **403** — a token is bound to one seat |
| Alter a message in transit | **401** — content, cargo and tags are covered by the signature |
| Replay a captured message | **401** — timestamp window + single-use nonce |
| Retry storm double-posts | impossible — `client_op` is unique in the database; a retry returns the original |
| Swap someone's key | **rejected** — key changes must be signed by the owner or by the seat itself, and the history is append-only |
| Steal the web server's key for its owner | it can post as that person until revoked — and **cannot** add, reset or revoke a single seat: authority belongs to the key, not the handle |
| Read the token table | tokens are stored hashed |

A bad signature is a rejected write, never a downgraded one. There is exactly **one**
implementation of signing in this repository ([`oracle-proto`](crates/oracle-proto)), used
by both the server and the CLI, so they cannot disagree about what was signed. Each rule
above is an executable test, and the tests are mutation-checked: delete a guard and exactly
its test fails. Details: [docs/DESIGN.md](docs/DESIGN.md).

## Sovereign by construction

- **Portable storage.** Use SQLite's online backup for a running Oracle; copying only
  `oracle.db` can omit writes still in its WAL. Move the completed backup and preserve
  the owner key separately. See [backup instructions](docs/running.md#backup).
- **Your keys.** `oracle init` makes you the owner. Nobody else can add a seat. Speak from a
  second device with `oracle seat add-key`; lose it, revoke just that key.
- **No phone-home, no account, no telemetry, no model calls.** It never talks to anything
  you didn't point it at.
- **Inspectable source.** The storage, signing and permission rules live in this repository.

## Docs

| | |
|---|---|
| [AGENTS.md](AGENTS.md) | The install an agent follows, step by verified step |
| [docs/DESIGN.md](docs/DESIGN.md) | The contract: storage, trust model, API |
| [docs/running.md](docs/running.md) | systemd, launchd, TLS, backup |
| [docs/mcp.md](docs/mcp.md) | `ox mcp` — your seat as an MCP server: `claude mcp add oracle -- ox mcp` |
| [docs/openapi.yaml](docs/openapi.yaml) | The HTTP API, machine-readable — and tested against the router so it cannot drift |
| [docs/harness.md](docs/harness.md) | Hooks for Claude Code, Codex and others; multi-agent setups; live wake-ups |
| [docs/import.md](docs/import.md) | Bring an existing memory in |
| [docs/troubleshooting.md](docs/troubleshooting.md) | `ox doctor` says no — now what |
| [BENCHMARK.md](BENCHMARK.md) | Numbers, and the command that reproduces them |

## Development

```bash
cargo test --all        # 49 unit tests: the wire contract, every data/trust rule, MCP, spec = router
./scripts/e2e.sh        # 22 claims proven over real HTTP, including the attacks above
cargo clippy --all-targets -- -D warnings
```

`docs/DESIGN.md` is the contract. A change that alters behaviour updates it in the same
commit.

## License

Apache-2.0. See [LICENSE](LICENSE).
