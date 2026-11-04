# Searching Oracle

## Contents
- The four reads, and which to use
- Writing a query
- Reading a hit
- When to stop

## The four reads

| You want | MCP | ox |
|---|---|---|
| Anything said about a topic, ranked | `oracle_recall` `query` | `ox recall "<words>"` |
| One record whole, with its links | `oracle_get` `id` | `ox get <id>` |
| A message and every reply | `oracle_thread` `id` | `ox get <id> --thread` |
| A room's latest, or where it stands | `oracle_read` / `oracle_digest` | `ox read <room>` / `ox state <room>` |

Pick the narrowest read that answers the turn. If you already have an id, `get` it; do not search for it.

## Writing a query

- Use the words the record itself would hold: names, ids, nouns of the thing. "release Windows smoke failed" beats
  "what went wrong with the release".
- Three to six content words. Filler words are dropped anyway.
- A person's own phrasing finds their records best. If they said it in a different language or shorthand, try that.
- `room` narrows to one room; `--where` (`where: true`) first tells you which rooms talk about it.
- `--newest` with a room reads what that room said about it, newest first, with corrections.

## Reading a hit

Each hit carries: id, room, author, date, the snippet, `words: [held, of]`, and when they apply `approved: true`,
`superseded_by`, `same_as`, `synthetic: true`.
- **`prior: none`** means nothing passed the floor. Say "nothing in Oracle says this", not that it is false.
- **`superseded_by`**: read the successor; the hit is history.
- **`synthetic: true`**: a summary or graph record, not first-hand. Find the source before quoting it.
- **The date matters.** A record is what was true when it was written. The newest signed record on a subject wins.

## When to stop

One search for a new topic, three at most in a turn, each with new words. After three misses, act on what you have
and say what you could not find. Report the miss (see evaluate.md) if you later find the record another way.
