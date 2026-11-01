#!/usr/bin/env node
// A second implementation of the oracle-say/2 envelope, in ~40 lines of Node with no
// dependencies — for gateways that cannot link the Rust crate (a web app signing for its
// logged-in user). crates/oracle-proto stays the reference; scripts/e2e.sh proves this file
// agrees with it by having the real server verify what it signs.
//
//   node examples/sign.mjs <url> <token> <seed-hex-file> <author> <room> <text> [tag ...]
import { createHash, createPrivateKey, randomBytes, sign } from 'node:crypto';
import { readFileSync } from 'node:fs';

const sha = s => createHash('sha256').update(s, 'utf8').digest('hex');

/** A 32-byte Ed25519 seed (hex) as a key object. */
export const keyFromSeed = seedHex =>
  createPrivateKey({ key: Buffer.concat([Buffer.from('302e020100300506032b657004220420', 'hex'), Buffer.from(seedHex.trim(), 'hex')]), format: 'der', type: 'pkcs8' });

/** Build and sign an envelope. `kind` is say | state | memory; `cargo` a string or undefined. */
export function envelope(key, { channel, author, kind = 'say', content, cargo, tags = [] }, nowMs = Date.now()) {
  const env = { channel, author, kind, ts_ms: nowMs, nonce: randomBytes(16).toString('hex'), client_op: randomBytes(16).toString('hex'), content };
  if (cargo !== undefined) env.cargo = cargo;
  if (tags.length) env.tags = tags;
  // Tags are hashed in BYTE order. JavaScript's default sort is UTF-16 order, which differs
  // beyond the Basic Multilingual Plane — compare the UTF-8 bytes.
  const sorted = [...tags].sort((a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b)));
  const bytes = ['oracle-say/2', channel, author, kind, nowMs, env.nonce, env.client_op, sha(content), sha(cargo ?? ''), sha(sorted.join('\n'))].join('\n');
  env.sig = sign(null, Buffer.from(bytes, 'utf8'), key).toString('hex');
  return env;
}

const ROUTE = { say: '/observations', state: '/observations', memory: '/observations' }; // one write door; the kind is in the envelope

if (import.meta.url === `file://${process.argv[1]}`) {
  const [url, token, seedFile, author, channel, content, ...tags] = process.argv.slice(2);
  const env = envelope(keyFromSeed(readFileSync(seedFile, 'utf8')), { channel, author, content, tags });
  const res = await fetch(url.replace(/\/$/, '') + ROUTE[env.kind], { method: 'POST', headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' }, body: JSON.stringify(env) });
  const body = await res.json();
  console.log(res.ok ? `signed:ok  ${body.id}` : `refused (${res.status}): ${body.detail}`);
  process.exit(res.ok ? 0 : 1);
}
