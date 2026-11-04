# @dot-protocol/oracle: TypeScript and JavaScript

Oracle from Node 20 or later. You can write signed records, recall, read a record whole, read a thread, and ask
whether Oracle can answer something before you search. It has no dependencies: `node:crypto` signs oracle-say/2
exactly as `crates/oracle-proto` does.

```js
import { Oracle } from '@dot-protocol/oracle';

const oracle = new Oracle({
  url: 'http://127.0.0.1:8893',
  token: process.env.ORACLE_TOKEN, // `oracle token new <seat>`
  seed: process.env.ORACLE_SEED,   // the seat's Ed25519 seed; only needed to write
});

const { id } = await oracle.say('planning', 'We ship on Friday.', { intent: 'decision', to: '@blaze' });
await oracle.say('planning', 'Friday holds.', { replyTo: id });
await oracle.remember('Release from dev, never from a branch.', { kind: 'lesson' });

const found = await oracle.recall('when do we ship', { room: 'planning' });
const check = await oracle.answerable('when do we ship'); // yes | partial | no, before you recall
const record = await oracle.get(id);                      // whole, by id
const thread = await oracle.thread(id);
```

`Oracle.fromEnv()` reads these environment variables:
- `ORACLE_URL`
- `ORACLE_TOKEN` or `ORACLE_TOKEN_FILE`
- `ORACLE_SEAT`
- `ORACLE_SEED` or `ORACLE_SEED_FILE`

## What it does

| call | REST | notes |
|---|---|---|
| `say(room, text, {replyTo, to, intent, ask, supersedes, validUntil, tags, cargo})` | `POST /observations` | A signed write. `supersedes` replaces a record: the old one stays and ranks under the new one. |
| `remember(text, {kind, room, validUntil})` | `POST /observations` | `kind` is one of decision, lesson, preference or commitment. |
| `write({kind, room, content, cargo, tags})` | `POST /observations` | Any envelope kind: say, state or memory. |
| `recall(query, {room, limit, asOf, rerank, byRoom})` | `GET /search?q=` | Ranked recall, the same search every seat runs (docs/SEARCH.md). |
| `answerable(question, {room})` | `GET /answerable` | answerable/1 |
| `get(id, {envelope})` | `GET /search?id=` | One record whole; `envelope: true` adds the signed envelope. |
| `thread(root, …)`, `messages(room, …)`, `asks()` | `/threads/{root}`, `/rooms/{room}/messages`, `/asks` | Paged with `before`. |

On a server that keeps author chains (`/whoami` lists the `prev` feature), each write is signed over the chain head
the server reports. If another device wrote first, the write is signed again over the new head, keeping the same
`client_op`, so it is stored once.

A refusal throws `OracleError` with `status`, `error` and `detail`. When it throws, nothing was stored.

The building blocks are exported too: `envelope`, `signEnvelope`, `signingBytes`, `resign`, `keyFromSeed`,
`publicKeyHex` and `linkOf`. They are for a gateway that signs for its own users.

## Tests

- `npm test` checks the signer against the golden vector in `crates/oracle-proto/testdata/say-v2-golden.json`, with
  negative controls.
- `node test/live.mjs` runs against a fresh server. It writes, so never point it at a real Oracle; `scripts/e2e.sh` runs
  it. It proves the whole client against the real server: write, read back, thread, supersede, recall, remember,
  answerable, and a forged author refused.
