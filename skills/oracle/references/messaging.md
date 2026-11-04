# Telling the owner

The owner reads Oracle's Bridge (the owner's inbox), not your session. Text you print in a terminal is not a message.

## The shape

- **Head**: at most 40 words, readable cold, in plain words. It is what they hear first.
- **Detail**: in the room, or a page passed with `--detail page.json` (`doc/1` or `plan/1`). Never in the head.
- **Where**: the room the work belongs to. Answering them: `--reply <their message id>` in its room. A new topic:
  top level in your own room.

## The three asks

| ask | Use when | Required |
|---|---|---|
| `none` (default) | a result or a note | nothing more |
| `read` | they should read something, no answer needed | nothing more |
| `decide` | only they can choose | 2 to 6 `option`s in their words, `recommend` (1-based), `why` (one sentence), `by` (deadline), and the gap: `affects "<who>: <how>"`, `absent "<who>: <why>"` (`none` only with where they spoke), `owed` |

**Only a decide becomes a card on the owner's Bridge today** (verified 3 Nov 2026: dotd turns asks waiting on the owner
into cards; `none` and `read` are stored but not shown). Anything the owner must see is a decide. After sending, check
that the card landed: the Bridge store (`~/.dot-terminal/workspace/bridge/events.sqlite`) holds an event whose
`event_id` is `<id>#ask` within a minute. `ox brief` prints this template under NOW, so it is never looked up.

One question per message. A choice written as a numbered list in prose, addressed to the owner, is refused by the
store; use `ask: decide`.

## Examples

```bash
# a note in a thread: stored and read in the room, but not a card on the Bridge
ox tell "Release v0.1.0 is built and passes on all five platforms." --reply OBS-… --room oracle
# anything the owner must see or answer: a decide
ox tell "Make the repo public now?" --ask decide --option "Make it public and tag v0.1.0 today" \
  --option "Wait for the review first" --recommend 2 --why "The review is the last check before an irreversible step." \
  --by 2d --affects "users: can install it the day it is public" --absent "emmy: has not reviewed the release" \
  --owed "the release receipt" --room oracle
```

Refusals name the rule they hit: `--affects` must read `<who>: <how>`; `--absent none` must cite where they spoke.

MCP: `oracle_say` with `to: ["@owner"]`, `ask`, `options`, `recommend`, `why`, `by`, `reply`.

## Seats

`@handle` in the text, or `to`, addresses another seat. Ask it for something with `ask: act`, `reply` or `review`.
Close what you asked once it is settled (`closes`), so it leaves the open list.
