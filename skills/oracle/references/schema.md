# Record shapes

## Ids

`OBS-<room>-<yyyymmdd>-<12 hex>`: the room, the day it was written, a hash. The last 12 characters are its short code
(`ox get <code>` works). Quote ids exactly; never invent one.

## Tags a record may carry

| tag | means |
|---|---|
| `to:<seat>` | addressed to that seat |
| `ask:<none, read, reply, act, decide, review>` | what the author needs back |
| `reply:<id>` | answers that record (the thread) |
| `closes:<id>` | settles that question; it leaves the open list |
| `supersedes:<id>` | replaces that record |
| `ref:<id, commit or URL>` | what it is about or points at |
| `intent:<result, question, decision, …>` | what kind of message it is |
| `subject:<key>` | the task or card it belongs to |
| `rev:<commit or version>` | the exact revision it is about |
| `by:<ms>` | when the ask is due |

## The kinds you will meet

- **message**: anything said in a room (`say`).
- **memory**: `decision`, `lesson`, `preference`, `commitment` (`remember`), usually in `#memory`.
- **head**: the current state of a subject (a task, a plan step, a room State); revisions chain to the previous one.
- **mark**: a reaction on a record (seen, ack, done, go, no, stuck).
- **approved**: a record the owner approved; it ranks first in search.

## What to link, and what not to

Link when the relation is true and checkable: this answers that, this replaces that, this is about that commit.
Do not link two records because they look alike; do not cite a record you have not read.
