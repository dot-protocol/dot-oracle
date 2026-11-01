// Generates tests/fixtures/cards-parity.json: edge-case observations, and what the AXXIS web board makes of them.
//   AXXIS_SRC=<the AXXIS web source> node --experimental-strip-types --no-warnings ops/cards_golden.mjs
// The web board (src/lib/project-board.ts readProjectBoard, with src/lib/room-task-revisions.mjs and the task contract) is the reference:
// oracle-core's GET /cards must fold exactly the same cards (crates/oracle-server/tests/cards.rs reads this file). Regenerate only when
// the web rules change, and say so in the commit.
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';

const AXXIS = process.env.AXXIS_SRC;
if (!AXXIS) { console.error('AXXIS_SRC must name the AXXIS web source'); process.exit(2); }
const {readProjectBoard} = await import(path.join(AXXIS, 'src/lib/project-board.ts'));
const {fromCore} = await import(path.join(AXXIS, 'src/lib/room-task-revisions.mjs'));

const T0 = Date.UTC(2026, 8, 20, 9, 0, 0);
const obs = [];
let n = 0;
const rev = (channel, o) => ({schema: 'room-task/1', channel, id: 'x', title: 'A card', group: 'Run reliably', impact: 'high', owner: 'rocky', status: 'queue', delivery: 'not_live', summary: 'What it is', acceptance: 'How we know', evidence: [], dependsOn: [], parents: [], ...o});
const face = (channel, r) => '[' + channel[0].toUpperCase() + channel.slice(1) + ' task:' + r.id + '] ' + r.status + ': ' + r.title;
/** A native (envelope 2) row: the revision is the cargo. */
function native(channel, minutes, r, extra = {}) {
  const id = extra.id || 'OBS-' + channel + '-20260920-' + String(++n).padStart(12, '0');
  obs.push({seq: n, id, channel, author: extra.author || r.owner || 'rocky', kind: 'say', content: extra.content ?? face(channel, r), cargo: extra.cargo === undefined ? JSON.stringify(r) : extra.cargo, tags: extra.tags || ['schema:room-task/1'], created_ms: T0 + minutes * 60000, pubkey: '', sig: '', envelope: 2});
  return id;
}
/** An imported (envelope 1) row: the revision rides inside an evidence envelope, and the row keeps the proof tags it had. */
function imported(channel, minutes, r, tags = ['from:emmy', 'signed:ok', 'cargo-signed:ok']) {
  const id = 'OBS-' + channel + '-20260914-' + String(++n).padStart(18, '0');
  obs.push({seq: n, id, channel, author: 'emmy', kind: 'say', content: face(channel, r), cargo: JSON.stringify({source: 'graph', cargo: JSON.stringify(r)}), tags, created_ms: T0 + minutes * 60000, pubkey: '', sig: '', envelope: 1});
  return id;
}

// ---- room alpha (native rows)
{
  const c = 'alpha';
  const e1 = native(c, 1, rev(c, {id: 'a-basic', title: 'Basic card', status: 'queue'}));
  const e2 = native(c, 2, rev(c, {id: 'a-basic', title: 'Basic card', status: 'in_progress', parents: [e1]}));
  native(c, 3, rev(c, {id: 'a-basic', title: 'Basic card', status: 'review', parents: [e2], handoff: 'changed X · test: Y · gotcha: Z', unblock: 'dinesh: review it', reviewer: 'dinesh', priority: 'P1', delivery: 'preview', evidence: ['abc1234']}));
  const d1 = native(c, 4, rev(c, {id: 'a-done', title: 'Done card', status: 'queue', impact: 'critical'}));
  native(c, 5, rev(c, {id: 'a-done', title: 'Done card', status: 'done', impact: 'critical', evidence: ['deadbee'], parents: [d1], delivery: 'live'}));
  const k0 = native(c, 6, rev(c, {id: 'a-conflict', title: 'Two heads', status: 'queue'}));
  native(c, 7, rev(c, {id: 'a-conflict', title: 'Two heads', status: 'in_progress', parents: [k0], owner: 'rocky'}));
  native(c, 8, rev(c, {id: 'a-conflict', title: 'Two heads', status: 'review', parents: [k0], owner: 'dinesh', handoff: 'h', unblock: 'rocky: pick one'}));
  native(c, 9, rev(c, {id: 'a-old', title: 'Old way', status: 'queue', supersededBy: 'a-new'}));
  native(c, 10, rev(c, {id: 'a-new', title: 'New way', status: 'in_progress', impact: 'medium'}));
  native(c, 11, rev(c, {id: 'a-dep', title: 'Needs a-basic', status: 'in_progress', dependsOn: ['a-basic']}));
  native(c, 12, rev(c, {id: 'a-miss', title: 'Needs a ghost', status: 'queue', dependsOn: ['ghost-card']}));
  native(c, 13, rev(c, {id: 'a-cyc1', title: 'Cycle one', status: 'queue', dependsOn: ['a-cyc2']}));
  native(c, 14, rev(c, {id: 'a-cyc2', title: 'Cycle two', status: 'queue', dependsOn: ['a-cyc1']}));
  const b0 = native(c, 15, rev(c, {id: 'a-bad', title: 'Later update is malformed', status: 'in_progress'}));
  native(c, 16, rev(c, {id: 'a-bad', title: 'Later update is malformed', status: 'review', parents: [b0], group: 'Room UX'}));
  native(c, 17, rev(c, {id: 'a-orphan', title: 'Parent is missing', status: 'review', parents: ['OBS-alpha-20000920-000000000404'], handoff: 'h', unblock: 'rocky: x'}));
  native(c, 18, rev(c, {id: 'a-noev', title: 'Done without evidence', status: 'done'}));
  // a batch: one message, two cards
  const r1 = rev(c, {id: 'a-b1', title: 'Batch one', status: 'in_progress'}), r2 = rev(c, {id: 'a-b2', title: 'Batch two', status: 'queue'});
  native(c, 19, r1, {content: '2 card moves: a-b1, a-b2', cargo: JSON.stringify({schema: 'room-task-event/1', channel: c, moves: [{task: 'a-b1', to: 'in_progress', revision: r1}, {task: 'a-b2', to: 'queue', revision: r2}]}), tags: ['schema:room-task-event/1']});
  // a batch that is refused whole: a move that names another card than its revision
  const r3 = rev(c, {id: 'a-b3', title: 'Batch three', status: 'queue'});
  native(c, 20, r3, {content: '1 card move: a-b3', cargo: JSON.stringify({schema: 'room-task-event/1', channel: c, moves: [{task: 'a-other', to: 'queue', revision: r3}]}), tags: ['schema:room-task-event/1']});
  // a task face with no usable cargo
  native(c, 21, rev(c, {id: 'a-nocargo'}), {content: '[Alpha task:a-nocargo] queue: no cargo', cargo: null});
  // ordinary speech, and a face of another room: neither is a card
  native(c, 22, rev(c, {}), {content: 'just talking about a card', cargo: null, tags: []});
  native(c, 23, rev(c, {id: 'a-wrongroom'}), {content: '[Beta task:a-wrongroom] queue: another room', cargo: JSON.stringify(rev('beta', {id: 'a-wrongroom'})), tags: []});
}
// ---- room beta (imported rows, face-only rows, the same card id as in alpha)
{
  const c = 'beta';
  imported(c, 1, rev(c, {id: 'b-imp', title: 'Imported card', status: 'queue', owner: 'emmy'}));
  imported(c, 2, rev(c, {id: 'b-imp2', title: 'Imported without proof', status: 'queue'}), ['from:emmy']);
  imported(c, 3, rev(c, {id: 'b-imp3', title: 'Imported with a signature but no cargo signature', status: 'queue'}), ['from:emmy', 'signed:ok']);
  native(c, 4, rev(c, {id: 'a-basic', title: 'Same id as alpha, its own card', status: 'in_progress', owner: 'kepler'}), {tags: []});
  const m = native(c, 5, rev(c, {id: 'b-face', title: 'Face only', status: 'queue', owner: 'bobby-core'}), {tags: []});
  native(c, 6, rev(c, {id: 'b-face', title: 'Face only', status: 'blocked', owner: 'bobby-core', parents: [m], unblock: 'blaze: your call', summary: 'why it is blocked'}), {tags: []});
  native(c, 7, rev(c, {id: 'b-prio', title: 'Bad priority', status: 'queue', priority: 'P9'}), {tags: []});
  native(c, 8, rev(c, {id: 'b-med', title: 'Historical med impact', status: 'queue', impact: 'med'}), {tags: []});
}

// CARDS_OBS=<file.json> CARDS_OUT=<file.json>: fold real rows (an array of store observations) instead of the built ones,
// and write only the golden there. That is how the live #rocky and #hustler boards were compared (tests/cards.rs, live_boards_match_the_web_board).
if (process.env.CARDS_OBS) { obs.length = 0; obs.push(...JSON.parse(fs.readFileSync(process.env.CARDS_OBS, 'utf8'))); }

// ---- what the web board makes of each room
const rooms = [...new Set(obs.map(o => o.channel))];
const graph = obs.map(o => ({...fromCore(o)}));
function fakeGet(channel) {
  const prefix = channel[0].toUpperCase() + channel.slice(1);
  const mine = graph.filter(g => g.channel === channel);
  return async (p) => {
    if (p.startsWith('/channel/' + channel + '/events')) {
      const list = mine.filter(g => g.statement.startsWith('[' + prefix + ' task:'));
      return {schema: 'channel-events/1', channel, prefix: '[' + prefix + ' task:', observations: list, count: list.length, has_more: false, complete: true, cursor: null};
    }
    if (p.startsWith('/channel/' + channel + '/export')) return {observations: mine.filter(g => g.tags.includes('schema:room-task-event/1')), truncated: false};
    throw new Error('unexpected read ' + p);
  };
}
const pick = (t) => ({id: t.id, title: t.title, status: t.status, effective_status: t.effectiveStatus, owner: t.owner, group: t.group, impact: t.impact, priority: t.priority ?? null, delivery: t.delivery, summary: t.summary, acceptance: t.acceptance, evidence: t.evidence, depends_on: t.dependsOn, unblock: t.unblock ?? null, handoff: t.handoff ?? null, reviewer: t.reviewer ?? null, superseded_by: t.supersededBy ?? null, conflict: !!t.conflict, heads: t.heads, blocking: t.blockingIds ?? [], issues: t.dependencyIssues ?? [], event: t.eventId, at: t.at});
const golden = {};
for (const room of rooms) {
  const b = await readProjectBoard(room, fakeGet(room));
  golden[room] = {
    cards: b.tasks.map(pick),
    superseded: b.superseded.map(pick),
    invalid: b.invalidUpdates.map(u => ({event: u.eventId, task: u.taskId, at: u.at})).sort((x, y) => (x.event + x.task).localeCompare(y.event + y.task)),
    resolved_invalid: b.resolvedInvalidUpdates.map(u => ({event: u.eventId, task: u.taskId})).sort((x, y) => (x.event + x.task).localeCompare(y.event + y.task)),
  };
}
let sha = 'unknown';
try { sha = execFileSync('git', ['-C', AXXIS, 'log', '-1', '--format=%h', '--', 'src/lib/project-board.ts', 'src/lib/room-task-revisions.mjs', 'public/vendor/axxis-room-contract.js'], {encoding: 'utf8'}).trim(); } catch {}
const out = {_: 'Generated by ops/cards_golden.mjs from the AXXIS web board (src/lib/project-board.ts). Do not edit by hand.', reference: 'AXXIS ' + sha, observations: obs, golden};
if (process.env.CARDS_OUT) {
  fs.writeFileSync(process.env.CARDS_OUT, JSON.stringify({golden}));
  console.log('rooms', rooms.join(','), '· observations', obs.length, '· cards', rooms.map(r => r + ':' + golden[r].cards.length + '+' + golden[r].superseded.length + ' superseded, ' + golden[r].invalid.length + ' invalid').join(' · '));
  process.exit(0);
}
fs.writeFileSync(new URL('../tests/fixtures/cards-parity.json', import.meta.url), JSON.stringify(out, null, 1) + '\n');
console.log('rooms', rooms.join(','), '· observations', obs.length, '· cards', rooms.map(r => r + ':' + golden[r].cards.length + '+' + golden[r].superseded.length + ' superseded, ' + golden[r].invalid.length + ' invalid').join(' · '));
