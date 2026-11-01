# ox: the agent workflow

This guide and `ox manual` are embedded in your installed binary. They work offline,
without a seat or keys. `ox manual commands` lists its visible command paths;
`ox <command> --help` describes the exact flags. Live Oracle state can change after
this binary was built; read the live procedures before deployment or other operations.

1. Wake: run `ox brief` after starting or compacting; use `ox brief --hello` when new.
   Read `ox inbox` and fetch the relevant message with `ox get <id> --thread`.
   Read before acknowledging with `ox seen <id>`; acknowledging is not completing work.
2. Recover evidence: `ox recall "<question>"`, then `ox get <id>` for the full source.
   Read `ox state <room>` and `ox procedure <name>` when applicable. A stale brief
   cache is explicitly offline, not current evidence. Missing search results are not
   proof that no prior work exists.
3. Own an outcome: inspect `ox card --mine`, claim the assigned card with
   `ox card <room> <id> take`, and maintain `ox now --set last="…" --set next="…"`.
   Check live ownership before editing shared files; preserve other people's changes.
4. Execute and verify: change the source, run the relevant tests and a negative
   control, and record exact revisions and artifacts. Distinguish built, committed,
   deployed and user-visible acceptance. Follow the human's approval boundaries.
5. Communicate through the Bridge, every seat, every model (Claude, Codex, any other): Blaze does not
   read terminals. He reads the Bridge, in DOT Terminal and as the Bridge/Inbox in the AXXIS web app
   (/os); both show only what `ox tell` addressed to him. So every answer, result or question for him
   is `ox tell "<head>"` (at most 40 words; detail in the room or `--detail <page.json>`), never only
   terminal text (docs/BRIDGE-CONTRACT.md).
   - Answering something he said: `--reply <OBS-id of his message> --room <its room>`; a new topic
     goes top-level in your own room.
   - A decision only he can make: `--ask decide` with 2 to 6 `--option` written in his words,
     `--recommend <n>` and `--why` (your pick and one plain reason), `--by <deadline>`, and the gap:
     `--affects "<who>: <how>"`, `--absent "<who>: <why>"` (or `none`), `--owed "<what you deliver>"`.
     One question per message; while a decide waits, it stays your last message to him.
   - `ox tell --help` prints the installed contract; a malformed tell is refused, not sent.
   Options are structured, not a numbered paragraph posing as interactive controls.
6. Check receipts: only `signed:ok <id>` means stored in Oracle; it does not prove Blaze saw or heard it. When it
   matters, check separately that it shows on the Bridge. Exit 3 / QUEUED means
   durable locally; run `ox flush`. Retry an uncertain write with the same `--op-id`.
   Never expose private keys or bearer tokens in logs, chat or documentation.
7. Finish with evidence: `ox card <room> <id> done --evidence "…" --handoff "…"`.
   Close asks you own with `ox close <id>` only when settled. Use `ox remember`
   for durable decisions and lessons, not routine status. Update NOW before handoff.

If a command fails, retain the error, consult its help and the live procedure;
`ox doctor` checks configuration and connectivity and performs a signed round trip.
Do not initialize a new Oracle or replace a seat to repair an existing installation.
