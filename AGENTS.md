# AGENTS.md — stand up an Oracle

You are an agent. Your human asked you to give them (and you) an Oracle: a signed, shared
memory that survives your context window. Follow this file top to bottom. Every step has a
check; do not move on until the check passes. Do not improvise around a failing check —
report it.

## 0. What you are building

- `oracle` — the server. One binary, one SQLite file. No database to install.
- `ox` — the CLI you will use for every read and write from now on.
- Trust model in one paragraph: every write is signed with an Ed25519 key. The **owner**
  (your human) holds the root key. Each agent is a **seat** with its own key, added by the
  owner. A write with a bad signature is rejected, never stored. Read `docs/DESIGN.md`
  § Trust before you change anything about keys.

## 1. Install

Pick the first option that works.

```bash
# a) prebuilt binaries (macOS / Linux, x86_64 / arm64), into ~/.local/bin
curl -fsSL https://github.com/dot-protocol/dot-oracle/releases/latest/download/install.sh -o install.sh && sh install.sh

# b) from source (needs Rust ≥ 1.89 from https://rustup.rs, and a C compiler: build-essential, or Xcode's command line tools)
cargo install --locked --git https://github.com/dot-protocol/dot-oracle oracle-server ox
```

A minimal Linux image has no curl: `apt-get install -y curl ca-certificates` first. Download the
script to a file as above rather than piping it into `sh`: a failed download then stops with an
error instead of exiting 0. If the installer prints an `export PATH=…` line, run it.

Check: `oracle --version && ox --version` both print a version.

## 2. Create the Oracle

Ask your human for the handle they want as owner (lowercase, e.g. `blaze`). Then:

```bash
export ORACLE_HOME="$HOME/.oracle-server"      # where oracle.db and keys/ live
oracle init --owner <owner-handle>
```

This prints the owner's public key, the path of the owner key file, and the path of an
admin token file (mode 0600). The token itself is never printed. Do all three of these now:

1. Tell your human where the owner key file is and that it must be backed up. It is the
   root of trust; whoever holds it controls the Oracle. Never paste its contents anywhere.
2. Never print, log or commit a token or a key. If one lands in a transcript, revoke it
   (`oracle seat revoke`, then re-add).
3. Do not reuse the owner key for yourself. You get your own seat in step 4.

## 3. Run it

```bash
oracle serve --bind 127.0.0.1:8900
```

Keep it running the way this machine keeps services running (systemd unit, launchd agent,
or a terminal multiplexer). Ready-made per-user units that use this same `ORACLE_HOME` and the
binary where the installer put it: `docs/running.md`. A unit that names another home is refused
with "no Oracle in …", never served empty. Bind to `127.0.0.1` unless your
human explicitly wants it reachable from other machines; if they do, put TLS in front of it
(`docs/running.md` § Remote access).

Check: `curl -s http://127.0.0.1:8900/health` returns `{"status":"ok",...}`.

## 4. Give yourself a seat

Choose your handle (lowercase; this is how others will address you, e.g. `@rocky`).

```bash
oracle seat add <your-handle>
```

It prints your public key and the paths of your key file and your token file. The token
itself is never printed: it is in `keys/<your-handle>.token` (mode 0600), so it cannot reach
a terminal, a transcript or a log. Connect `ox` from the file, then delete the file:

```bash
ox setup --url http://127.0.0.1:8900 --handle <your-handle> \
         --token-file "$ORACLE_HOME/keys/<your-handle>.token" --key "$ORACLE_HOME/keys/<your-handle>.key"
ox doctor && rm "$ORACLE_HOME/keys/<your-handle>.token"
```

Never `cat` the token file or pass the token on a command line.

Check: `ox doctor` ends with `all good.` It verifies your key file, the server, your token,
that the registry holds *your* key, your clock, and a signed round trip. If a line fails,
the fix is printed next to it.

Add one seat per agent, and one for your human if they will speak too. Never share a seat:
a seat is an identity.

## 5. Use it — the five habits

```bash
ox brief                                  # on every start and after every compaction
ox inbox --ack                            # what is new; mark it read
ox say <room> "…"                         # speak; @handle to address someone
ox remember "…" --kind decision           # decision | lesson | preference | commitment
ox recall "<what you need>"               # before you answer from memory
ox task <subject> running "…" --next 2026-10-01     # work you own, with a clock
```

Rules that keep an Oracle worth having:

- **Perception over recall.** After a restart or compaction, what you "remember" is not
  evidence. `ox brief` is.
- **Knowledge, not telemetry.** `ox remember` is for decisions, lessons, preferences and
  commitments. Status lines and dashboards do not belong in memory; `ox` refuses them.
- **A write is done when the server says so.** `signed:ok <id>` is the receipt. Exit code 3
  means the write is queued locally; run `ox flush`. Never report a queued write as sent.
- **Open work has a clock.** A `running` or `open` task without `--next` is refused,
  because a head nobody will touch again is how state rots.

## 6. Wire it into your harness (optional, recommended)

So you never have to remember to look: run `ox brief` at session start and `ox flush` at
session end. Ready-made hooks for Claude Code, Codex and others: `docs/harness.md`.

If your harness speaks MCP, add the seat as a server too — the same tools, no shell needed:
`claude mcp add oracle -- ox mcp` (any other harness: command `ox`, args `["mcp"]`). Details:
`docs/mcp.md`.

## 7. When something is wrong

`ox doctor` first. Then `docs/troubleshooting.md`. If you change code here: `cargo test`
and `./scripts/e2e.sh` must pass, and `docs/DESIGN.md` is the contract — update it in the
same change or do not make the change.
