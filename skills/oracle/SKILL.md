---
name: oracle
description: Use Oracle, the signed memory shared by every agent and person in a room, through the oracle MCP tools or the ox CLI. Covers waking (brief, inbox), when and how often to search, reading a record whole, telling the owner, writing back (say, remember, task, State, marks), linking records, and checking that a write and a recall actually worked. Use at session start, after a compaction, before acting on anything remembered or decided earlier, before building something that may exist, and whenever a result, decision or question is owed to someone.
license: Apache-2.0
compatibility: Needs an Oracle seat (ox join) and either the oracle MCP server (ox connect <agent>) or the ox CLI on PATH.
metadata:
  version: "1.0"
  source: dot-protocol/dot-oracle skills/oracle
---

# Oracle

Oracle is the room's memory. What you remember is not evidence; a signed record is. Each tool below has an MCP
name and an `ox` command; they are the same call, signed by the same seat.

## The turn, in order

1. **Wake** once per session and again after every compaction: `oracle_brief` (`ox brief`), then
   `oracle_inbox` (`ox inbox --mine`). The brief is who you are, what is addressed to you, what you own. Do not
   re-read it mid-session; it does not change because you asked twice.
2. **Search before you act from memory** (rules below). Read the record a hit points to before you rely on it.
3. **Do the work.**
4. **Write back what is owed** (rules below), and only what is owed.
5. **Check the receipt.** A write is stored only when the result says `signed:ok <id>`. `QUEUED` means it is on
   disk, not sent: run `oracle_flush` (`ox flush`). An error means nothing was stored.

## When to search, and how often

Search when the turn depends on something said, decided or built before:
- the person names a past thing ("we decided", "last time", "the X plan", a record id, a seat, a room);
- you are about to build a tool, service or doc: search by its function and two synonyms first;
- you are about to answer in a thread: read the thread whole (`oracle_thread`, `ox get <id> --thread`), not a search;
- you are about to state a rule, a number or a status as current.

Do not search for general knowledge, for facts the repository holds (read the code), or before every tool step.

**Budget per turn: one search for a new topic, at most three in a turn.** A search costs seconds and context
(measured on one Oracle: p50 2.6 s, p95 7.8 s). Spend the second and third on new framings (the exact words a
person would have used, a name, an id), not on the same words reordered. If nothing is found after three, say so
and act on what you have; a missing hit is not proof that nothing exists.

## How to search well

- `oracle_recall` / `ox recall "<words>"`: ranked hits. Use the content words and names that would be in the
  record. `room` narrows to one room; `ox recall --where <word> --any` says which rooms and dates hold a word.
- Read the `prior` line: `found`, `thin` or `none`. `thin` means read the hits before trusting them.
- Approved records rank first and say `approved: true`; a record that was replaced ranks under its successor and
  names it in `superseded_by`. **The newest signed record wins**, never what you remember.
- A hit is a pointer. `oracle_get` (`ox get <id>`) reads it whole, with its links. Quote its id when you use it.
- Details and examples: [references/search.md](references/search.md).

## Telling the owner

The owner reads Oracle (the Bridge), not your terminal. A result, an answer or a question for them is a signed
message addressed to them, never only text in your session.
- `ox tell "<head>"` or `oracle_say` with `to: ["@owner"]`. The head is at most 40 words; the detail goes in the
  room or a page.
- Answering something they said: reply to that message (`--reply <id>`, `reply`), in its room.
- A decision only they can make: `ask: decide`, 2 to 6 options in their words, your recommendation and one reason,
  a deadline. One question per message.
- Contract and examples: [references/messaging.md](references/messaging.md).

## Writing back

| What happened | Write | Not |
|---|---|---|
| A result, an answer, a finding others need | `oracle_say` (`ox say <room>`), as a reply when it answers something | a status line every few minutes |
| A decision, a lesson, a stated preference, a promise | `oracle_remember` kind `decision` / `lesson` / `preference` / `commitment` | status, progress, tool output |
| Work you own and will keep doing | `oracle_task` with a `next` date | a task for work already finished |
| What a room is for and where it stands | `oracle_room_state` set | chat in the State |
| You read, accepted, finished or are blocked on an ask | `oracle_react` seen / ack / done / stuck | a new message saying "ok" |

**A durable save names its consumer**: who or what reads it next (an addressee, an ask, a law, a State or a task it
changes). A record no one will read just lies in a room.

**Most turns write nothing durable.** Before `remember`, ask: would a new agent with no context act differently
because of this? If not, do not write it. Never write secrets, keys or tokens: say that a credential exists and
where it is kept, never its value. Details: [references/writing.md](references/writing.md).

## Connecting records

A record is worth more linked than alone. Reply to the message you answer; cite the ids you relied on; when you
correct or replace something, say which record (`closes`, `supersedes`, or the id in the text). Do not link by
guess: a link claims a relation someone can check. Record shapes and tags: [references/schema.md](references/schema.md).

**Linking is the one place to search more than three times.** Before you write a record that cites others (a decision,
a lesson, a reading of someone's words), recall each idea in it in three framings or more at `--limit 15` or above:
their own words, the concept, the name it later took, the problem it solved. Read the strongest hits whole with
`ox get` before citing them, and put the dates in the text. One phrasing finds a fraction: on 3 Nov 2026 "how Pixar
made animation" found the February research but not the studio built on it; "Pixar room, media house, AI studio" did.
- **A repeat is linked, never skipped.** When an idea comes back, link the new record to every earlier one, so it gets
  stronger, and say how often and since when.
- `part-of` for an elaboration, `answers` for a reply, `refines` only when the newer record replaces the older one
  (it makes the older read superseded), `is-same-as` only for the same words, `generates` only with evidence that the
  later thing grew from it.

## Checking Oracle worked

- Every write: `signed:ok <id>` or it did not happen.
- Every recall you relied on: did the record you used actually say it? Quote it.
- A recall that missed something you later found: report it with the query and the id it should have found, so it
  joins the frozen set (`ox recall --eval <set.jsonl>`). Details: [references/evaluate.md](references/evaluate.md).
