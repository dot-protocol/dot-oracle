# Wiring Oracle into an agent harness

The goal: the agent never has to *remember* to look at its memory.

## Claude Code

`~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "ox brief" }] }],
    "Stop":         [{ "hooks": [{ "type": "command", "command": "ox flush >/dev/null 2>&1 || true" }] }]
  }
}
```

`SessionStart` also fires after a compaction, which is exactly when a fresh brief matters.
And in `CLAUDE.md`: `Oracle = ox. Unsure what to do? Run ox brief.`

## Codex, Cursor, OpenCode and others

Any harness that reads `AGENTS.md` or a rules file: add the line above, and run `ox brief`
as the first command of a session. Harnesses with start/stop hooks can use the same two
commands.

## Several agents, one Oracle

Give each agent its own seat (`oracle seat add <handle>`) and its own `OX_HOME`:

```bash
OX_HOME=~/.oracle-rocky ox brief
OX_HOME=~/.oracle-kin   ox brief
```

They talk through rooms: `ox say planning "@kin can you review the schema?"`. The message
is in kin's next `ox inbox`, marked as addressed to it.

## Live wake-ups

`GET /stream` is a server-sent-event feed of every new observation (docs/STREAM.md: `room=` filters on the server,
`Last-Event-ID` resumes without a gap). A small watcher can wake an idle agent when something addressed to it arrives:

```bash
curl -sN -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8900/stream \
  | grep --line-buffered '"mention:rocky"\|"to:rocky"' | while read -r _; do wake-rocky; done
```

## A web app that signs for the people logged into it

A person who signs in to your web app with an email link or a password should post as
*themselves*. The shape:

1. Your app authenticates the person however it already does. The account is the identity.
2. The app maps the account to a seat (`alice@example.com` → `alice`) and holds an Ed25519
   seed for that seat — one per person, or one for a shared workspace seat.
3. Register that key as one of the seat's devices: `oracle seat add-key alice --pubkey <hex>`.
   If `alice` is also the owner, this key still cannot touch the registry — authority
   belongs to the key (docs/DESIGN.md § Trust) — so a breach of the web server costs a
   voice, revocable with `oracle seat revoke alice --pubkey <hex>`, not the Oracle.
4. Mint the gateway a token bound to that seat (`oracle token new alice`), and sign each post
   with [`examples/sign.mjs`](../examples/sign.mjs) — forty lines of Node, no dependencies,
   checked against the Rust implementation by `scripts/e2e.sh`.

The same person can keep a key on their laptop for `ox`. Both are `alice`; each observation
records which key signed it.
