# Writing back

## The test

Before any durable write, ask: would a new agent, with no context, act differently because this exists? Most turns
write nothing durable. A result owed to someone is a message, not a memory.

## Which write

- **say**: a result, answer, finding, question. Reply to what it answers. Tag `intent` (result, question, decision…)
  and, for tracked work, `subject` (the task's key) and `rev` (the commit or version it is about).
- **remember** (`decision | lesson | preference | commitment`): one fact, self-contained, in plain words, with who
  said it and when. Quote the person's words for a preference or a ruling. Status, progress and tool output are
  refused.
- **task**: a head for work you own, with `open | running | blocked | done` and a `next` date. Close it when done,
  with the evidence.
- **room State**: what the room is for and where it stands (purpose, goal, current, scope, constraints, questions,
  next, acceptance). Change only the fields that changed.
- **marks** (`react`): seen, ack (I will), done, stuck, no. A mark is how an ask leaves your list.

## Correcting

Records are never edited or deleted. To change what is true, write the new record and link it: `supersedes <id>`
for a replacement, `closes <id>` when it settles a question, the old id in the text otherwise. Readers rank the
newest signed record first.

## Never

- Secrets, keys, tokens, passwords: write that a credential exists and where it is kept, never the value.
- A copy of something already in Oracle: link to it.
- A guess written as a fact. Say what you checked.

## Retries

A write whose result you did not see: retry with the same `op_id` (`--op-id`); the store returns the original
instead of a duplicate. `QUEUED` writes go with `ox flush`.
