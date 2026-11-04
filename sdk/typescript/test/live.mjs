// Against a running Oracle: node test/live.mjs, with ORACLE_URL, ORACLE_TOKEN (or _FILE), ORACLE_SEED (or _FILE),
// and ORACLE_ROOM (a scratch room). scripts/e2e.sh runs it on a fresh server; it writes, so never point it at a real one.
import assert from 'node:assert/strict';
import { Oracle, OracleError } from '../oracle.mjs';

const o = Oracle.fromEnv();
const room = process.env.ORACLE_ROOM || 'sdk';
const me = await o.whoami();
assert.ok(me.handle, 'whoami names the seat');

const word = 'sdkprobe' + Date.now().toString(36);
const first = await o.say(room, `the TypeScript SDK writes a signed record ${word}`, { intent: 'result' });
assert.equal(first.ok, true);
assert.match(first.id, /^OBS-/);

const got = await o.get(first.id);
const rec = got.record ?? got;
assert.equal(rec.author, me.handle);
assert.ok(String(rec.content).includes(word), 'the record reads back whole');

const reply = await o.say(room, `and a reply to it ${word}`, { replyTo: first.id });
const th = await o.thread(first.id);
const ids = [th.root?.id, ...(th.items ?? []).map(i => i.id)];
assert.ok(ids.includes(reply.id), 'the reply is in the thread');

const later = await o.say(room, `the corrected record ${word}`, { supersedes: first.id });
const found = await o.recall(word, { room, limit: 10 });
const hits = found.hits ?? found.items ?? found;
assert.ok(Array.isArray(hits) && hits.some(h => h.id === later.id), 'recall finds it');
const old = hits.find(h => h.id === first.id);
if (old) assert.equal(old.superseded_by, later.id, 'the replaced record names its successor');

const mem = await o.remember(`the SDK remembers a lesson ${word}`, { kind: 'lesson', room });
assert.match(mem.id, /^OBS-/);
await assert.rejects(() => o.remember('status line', { kind: 'status' }), /kind is one of/);

const a = await o.answerable(`${word} reply`, { room });
assert.ok(['yes', 'partial', 'no'].includes(a.verdict), 'answerable gives a verdict');

// Negative control: a token bound to this seat cannot sign as another.
const forged = new Oracle({ url: o.url, token: o.token, seat: me.handle + 'x', key: o.key });
forged._features = new Set();
await assert.rejects(() => forged.say(room, 'not mine'), e => e instanceof OracleError && e.status === 403);
console.log(`signed:ok  the TypeScript SDK wrote, read, threaded, superseded, recalled and remembered as ${me.handle}; a forged author was refused`);
