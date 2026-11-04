// The signer against the reference: crates/oracle-proto's golden vector must come out byte for byte.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { envelope, keyFromSeed, linkOf, publicKeyHex, signEnvelope, signingBytes, whenMs } from '../oracle.mjs';

const vector = ['..', '..', '..', 'crates', 'oracle-proto', 'testdata', 'say-v2-golden.json'].join('/');
const golden = JSON.parse(readFileSync(new URL(vector, import.meta.url), 'utf8'));

test('the golden vector: the same bytes, the same signature, the same public key', () => {
  const key = keyFromSeed(golden.seed_hex);
  assert.equal(publicKeyHex(key), golden.pubkey_hex);
  assert.equal(signingBytes(golden.envelope).toString('utf8'), golden.signing_bytes);
  assert.equal(signEnvelope(key, golden.envelope).sig, golden.envelope.sig);
});

test('a changed tag, cargo or content changes the signature (negative control)', () => {
  const key = keyFromSeed(golden.seed_hex);
  for (const change of [{ tags: ['intent:answer'] }, { cargo: '{}' }, { content: golden.envelope.content + ' ' }]) {
    assert.notEqual(signEnvelope(key, { ...golden.envelope, ...change }).sig, golden.envelope.sig, JSON.stringify(change));
  }
});

test('tags are hashed in UTF-8 byte order, not JavaScript UTF-16 order', () => {
  const key = keyFromSeed(golden.seed_hex);
  const tags = ['𝛼-astral', 'ｚ-fullwidth'];
  assert.deepEqual([...tags].sort(), tags, 'UTF-16 order puts the astral character first');
  const env = envelope(key, { channel: 'room', author: 'kin', content: 'x', tags }, 1800000000000);
  assert.deepEqual(signingBytes(env), signingBytes({ ...env, tags: [...tags].reverse() }), 'the order tags are given in does not matter');
});

test('authored_ms and prev are signed only when present, in that order', () => {
  const key = keyFromSeed(golden.seed_hex);
  const { sig, ...base } = golden.envelope;
  const prev = '0'.repeat(64);
  assert.ok(signingBytes({ ...base, authored_ms: 1, prev }).toString().endsWith(`\nauthored:1\nprev:${prev}`));
  assert.equal(signingBytes(base).toString(), golden.signing_bytes);
  assert.equal(signEnvelope(key, base).sig, sig);
  assert.equal(linkOf(sig).length, 64);
});

test('a seed must be 32 bytes of hex; a time is a date, ms or ISO', () => {
  assert.throws(() => keyFromSeed('07'), /64 hex/);
  assert.equal(whenMs('2026-11-04'), Date.parse('2026-11-04T23:59:59.999Z'));
  assert.equal(whenMs(5), 5);
  assert.throws(() => whenMs('soon'), /a time is/);
});
