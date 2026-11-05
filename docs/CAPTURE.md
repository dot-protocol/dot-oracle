# Capture: sessions become memories

Status: 4 Nov 2026. This closes two rows of the competitor map, agent-adapters and session-extraction. On each, the
competitors install into the agent and extract memories on write. Before this, our memories were all written by hand.

## What happens

1. `oracle up` adds three hooks to every agent it finds, in that agent's own file and shape. Hooks already in the file
   are kept, and a second run adds nothing.

   | agent | file | wakes with the brief | captured on |
   |---|---|---|---|
   | Claude Code | `~/.claude/settings.json` | `SessionStart` | `PreCompact`, `SessionEnd` |
   | Codex | `~/.codex/hooks.json` | `SessionStart` | `PreCompact`, `SessionEnd` |
   | Cursor | `~/.cursor/hooks.json` | `sessionStart` (`ox brief --hook cursor` prints `{"additional_context": …}`) | `preCompact`, `sessionEnd` |

   Codex asks you to trust a new hook once, with `/hooks`.
2. The hook runs `ox capture --hook <agent>`. It reads the hook's JSON on stdin (the session id and `transcript_path`),
   writes one uniquely named entry under `<ox home>/capture/queue/`, starts a detached `ox capture --drain`, and returns. It returns at
   once because Codex gives a SessionEnd hook one to three seconds, and a hook must never hold an agent up. It never fails
   its agent: an error goes to `<ox home>/capture/log`.
3. The drain reads each spooled transcript from where the last capture of that session stopped
   (`<ox home>/capture/sessions/<id>.json`). It understands Claude Code, Codex and plain `role`/`content` JSON lines, and
   reads any other file as plain text. Tool calls and tool output are left out, and each turn is cut at 2,000 characters.
   Less than 200 new characters is not worth a call.
4. It sends that text to `POST /extract` on your configured Oracle. The server calls an Ollama-compatible model (
   `ORACLE_EXTRACT_URL`; `ORACLE_EXTRACT_MODEL`, default `qwen3:8b`) in pieces of 8,000 characters cut between turns, the newest six of a capture. It first drops
   every line the secret scan matches. It answers at most eight decisions, lessons, preferences or commitments, each one
   sentence that stands alone. The server stores nothing.
5. The seat signs each memory itself, in room `memory` by default, with these tags:
   - `memory:<kind>`
   - `extracted:session`
   - `capture:<agent>`

   Its cargo is `capture/1`: agent, session, model and why. A memory the room already holds word for word is skipped.
   An extracted memory is an ordinary record: `ox edit` corrects it, and supersedes replaces it.

Transcript text reaches the configured Oracle before the server's secret scan. The model endpoint can also be
remote: `ORACLE_EXTRACT_URL` does not enforce locality. Keeping text on machines you control requires controlling
both endpoints; secret filtering is not a guarantee that private information is removed. `/whoami` lists the
`extract` feature only when a model is configured. Without one, `/extract` answers 503 and no extracted memories
are returned; the hook can still write a local spool entry.

## By hand

- `ox capture --file <transcript> --agent claude-code --dry-run` prints what would be kept, and writes nothing.
- Without `--dry-run`, it signs those memories.
- `ox capture --drain` empties the spool now.

## Limits, said plainly

- The model is small and runs on a CPU. Measured on core (qwen3:8b, `ORACLE_EXTRACT_THREADS=6` under a six-core quota): about
  95 s for a short session, about two minutes a piece. One extraction runs at a time. A capture reads at most the newest
  48,000 characters of new text; when it leaves older text unread, it logs how much.
- On a test session with five planted memories and one status line, qwen3:8b kept all five and left the status line
  out; qwen2.5:7b found three and missed the commitment.
- It can miss something, or phrase it badly. Every kept memory says it was extracted, so recall can be read with that
  in mind.
- Cursor may give a null `transcript_path` when transcripts are off. Then only Codex's `last_assistant_message`, or
  nothing, is read.

## Private local storage

The spool can contain an assistant's text before any secret filtering. On Unix,
`capture/`, `queue/` and `sessions/` are mode 0700; queued text, session cursors and
the capture log are mode 0600, independent of umask. Existing capture directories
are tightened when used. Redirected capture directories (symlinks) are refused.
Spool entries and cursors use private atomic durable writes; concurrent hook events
at the same timestamp receive different filenames. Windows uses the enclosing
user directory's ACL; these Unix mode checks do not establish Windows ACL isolation.
