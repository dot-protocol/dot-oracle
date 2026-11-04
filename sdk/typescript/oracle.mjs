// @dot-protocol/oracle: Oracle from TypeScript or JavaScript. Signed writes, recall, records, threads and the
// pre-flight check, over the REST API (docs/openapi.yaml). No dependencies: node:crypto signs oracle-say/2 the way
// crates/oracle-proto does, and the golden vector (crates/oracle-proto/testdata/say-v2-golden.json) is its test.
// crates/oracle-proto stays the reference; scripts/e2e.sh has the real server verify what this signs.
import { createHash, createPrivateKey, createPublicKey, randomBytes, sign as ed25519 } from 'node:crypto';
import { readFileSync } from 'node:fs';

export const SAY_DOMAIN = 'oracle-say/2';
export const KINDS = Object.freeze(['say', 'state', 'memory']);
export const MEMORY_KINDS = Object.freeze(['decision', 'lesson', 'preference', 'commitment']);

const sha = s => createHash('sha256').update(s, 'utf8').digest('hex');
const randomHex = n => randomBytes(n).toString('hex');
// Tags are hashed in BYTE order. JavaScript's default sort is UTF-16 order, which differs beyond the Basic
// Multilingual Plane, so compare the UTF-8 bytes (Rust's String order).
const byteOrder = (a, b) => Buffer.compare(Buffer.from(a, 'utf8'), Buffer.from(b, 'utf8'));
const PKCS8_ED25519 = Buffer.from('302e020100300506032b657004220420', 'hex');

/** A 32-byte Ed25519 seed, as 64 hex digits, made a private key. */
export function keyFromSeed(seedHex) {
  const s = String(seedHex).trim();
  if (!/^[0-9a-fA-F]{64}$/.test(s)) throw new TypeError('a seed is 64 hex digits (32 bytes)');
  return createPrivateKey({ key: Buffer.concat([PKCS8_ED25519, Buffer.from(s, 'hex')]), format: 'der', type: 'pkcs8' });
}

/** The 32-byte public key of a private key, as hex: what `oracle seat add-key --pubkey` takes. */
export function publicKeyHex(key) {
  return createPublicKey(key).export({ format: 'der', type: 'spki' }).subarray(-32).toString('hex');
}

/** The exact bytes that are signed (docs/openapi.yaml, Envelope). Content, cargo and tags are covered by hash. */
export function signingBytes(env) {
  const tags = [...(env.tags ?? [])].sort(byteOrder);
  let s = [SAY_DOMAIN, env.channel, env.author, env.kind, env.ts_ms, env.nonce, env.client_op,
    sha(env.content), sha(env.cargo ?? ''), sha(tags.join('\n'))].join('\n');
  // Only an envelope that carries them signs them: every envelope without one keeps its exact bytes.
  if (env.authored_ms !== undefined) s += `\nauthored:${env.authored_ms}`;
  if (env.prev !== undefined) s += `\nprev:${env.prev}`;
  return Buffer.from(s, 'utf8');
}

/** Sign an envelope as it is: `sig` is replaced. */
export function signEnvelope(key, env) {
  const { sig: _old, ...rest } = env;
  return { ...rest, sig: ed25519(null, signingBytes(rest), key).toString('hex') };
}

/** Build and sign a new envelope. `nowMs` is injected so tests control time. */
export function envelope(key, { channel, author, kind = 'say', content, cargo, tags = [], prev, authoredMs }, nowMs = Date.now()) {
  if (!KINDS.includes(kind)) throw new TypeError(`kind is one of ${KINDS.join(', ')}`);
  if (typeof content !== 'string') throw new TypeError('content is a string');
  const env = { channel, author, kind, ts_ms: nowMs, nonce: randomHex(16), client_op: randomHex(16), content };
  if (cargo !== undefined) env.cargo = typeof cargo === 'string' ? cargo : JSON.stringify(cargo);
  if (tags.length) env.tags = [...tags];
  if (authoredMs !== undefined) env.authored_ms = authoredMs;
  if (prev !== undefined) env.prev = prev;
  return signEnvelope(key, env);
}

/** Re-sign for a retry: a fresh clock and nonce, the SAME client_op, so a write that did arrive is not stored twice. */
export function resign(key, env, nowMs = Date.now()) {
  return signEnvelope(key, { ...env, ts_ms: nowMs, nonce: randomHex(16) });
}

/** The chain link to a signed write: SHA-256 of its hex signature (docs/CHAIN.md). */
export const linkOf = sigHex => sha(sigHex);

/** A refusal: nothing was stored. `status` is the HTTP status, `error` the server's word, `detail` its sentence. */
export class OracleError extends Error {
  constructor(status, error, detail) {
    super(`${status} ${error}${detail ? `: ${detail}` : ''}`);
    this.name = 'OracleError';
    this.status = status;
    this.error = error;
    this.detail = detail;
  }
}

/** Unix ms from a Date, unix ms, or an ISO date (a date alone means the end of that day, UTC). */
export function whenMs(v) {
  if (v instanceof Date) return v.getTime();
  if (typeof v === 'number' && Number.isSafeInteger(v)) return v;
  if (typeof v === 'string' && /^\d{4}-\d{2}-\d{2}$/.test(v)) return Date.parse(`${v}T23:59:59.999Z`);
  const ms = typeof v === 'string' ? Date.parse(v) : NaN;
  if (!Number.isFinite(ms)) throw new TypeError('a time is a Date, unix ms, or an ISO date');
  return ms;
}

const q = params => {
  const u = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== null && v !== '') u.set(k, String(v));
  const s = u.toString();
  return s ? `?${s}` : '';
};

export class Oracle {
  /**
   * @param {object} o
   * @param {string} o.url     the Oracle, e.g. http://127.0.0.1:8893
   * @param {string} o.token   a bearer token minted by `oracle token new <seat>`, bound to one seat
   * @param {string} [o.seat]  the seat the token is bound to; read from /whoami when left out
   * @param {string} [o.seed]  the seat's Ed25519 seed (64 hex); needed only to write
   * @param {import('node:crypto').KeyObject} [o.key] the seat's private key, instead of a seed
   */
  constructor({ url, token, seat, seed, key, fetch: fetchImpl = globalThis.fetch, timeoutMs = 30000 } = {}) {
    if (!url) throw new TypeError('url is required');
    if (!token) throw new TypeError('token is required');
    this.url = String(url).replace(/\/+$/, '');
    this.token = token;
    this.seat = seat;
    this.key = key ?? (seed ? keyFromSeed(seed) : undefined);
    this.fetch = fetchImpl;
    this.timeoutMs = timeoutMs;
    this._features = undefined;
  }

  /** ORACLE_URL, ORACLE_TOKEN (or ORACLE_TOKEN_FILE), ORACLE_SEAT, ORACLE_SEED (or ORACLE_SEED_FILE). */
  static fromEnv(env = process.env) {
    const file = p => (p ? readFileSync(p, 'utf8').trim() : undefined);
    return new Oracle({ url: env.ORACLE_URL, token: env.ORACLE_TOKEN || file(env.ORACLE_TOKEN_FILE), seat: env.ORACLE_SEAT,
      seed: env.ORACLE_SEED || file(env.ORACLE_SEED_FILE) });
  }

  /** One request. A non-2xx answer throws OracleError; nothing was stored. */
  async request(method, path, body) {
    const res = await this.fetch(this.url + path, {
      method,
      headers: { Authorization: `Bearer ${this.token}`, ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}) },
      body: body !== undefined ? JSON.stringify(body) : undefined,
      signal: AbortSignal.timeout(this.timeoutMs),
    });
    const text = await res.text();
    let data;
    try { data = text ? JSON.parse(text) : {}; } catch { data = { error: 'not_json', detail: text.slice(0, 200) }; }
    if (!res.ok) throw new OracleError(res.status, data.error ?? 'refused', data.detail);
    return data;
  }

  /** The seat this token is bound to, its keys, and what this server understands (features). */
  async whoami() {
    const w = await this.request('GET', '/whoami');
    this.seat ??= w.handle;
    this._features = new Set(Array.isArray(w.features) ? w.features : []);
    return w;
  }

  async features() {
    if (!this._features) await this.whoami();
    return this._features;
  }

  /**
   * Write one signed record and return its receipt ({ ok, id, seq, duplicate, signed }). On a server that keeps
   * author chains it is signed over the head the server reports, and again over a newer head when another device
   * wrote first: the same client_op, so it is one write (crates/ox post_write).
   */
  async write({ kind = 'say', room, content, cargo, tags = [] }) {
    if (!this.key) throw new TypeError('writing needs the seat key: pass seed or key');
    if (!room) throw new TypeError('room is required');
    const author = this.seat ?? (await this.whoami()).handle;
    const chains = (await this.features()).has('prev');
    let env = envelope(this.key, { channel: room, author, kind, content, cargo, tags });
    for (let i = 0; i < 4; i++) {
      if (chains) {
        const head = await this.request('GET', `/chain/${encodeURIComponent(author)}/head`).then(h => h.head, () => undefined);
        if (typeof head === 'string') env = resign(this.key, { ...env, prev: head });
      }
      try {
        return await this.request('POST', '/observations', env);
      } catch (e) {
        if (chains && e instanceof OracleError && e.status === 409 && /chain head/.test(`${e.detail ?? ''} ${e.error}`)) continue;
        throw e;
      }
    }
    throw new OracleError(409, 'conflict', 'the chain head kept moving under four tries; NOT written');
  }

  /**
   * Say something in a room. `replyTo` threads it, `to` addresses seats (their inbox), `intent` and `ask` say what
   * it is and what it asks for, `supersedes` replaces an earlier record, `validUntil` ends it at a time.
   */
  async say(room, text, { replyTo, to = [], intent, ask, supersedes, validUntil, tags = [], cargo } = {}) {
    const t = [...tags];
    if (replyTo) t.push(`reply:${replyTo}`);
    for (const h of [].concat(to)) t.push(`to:${String(h).replace(/^@/, '')}`);
    if (intent) t.push(`intent:${intent}`);
    if (ask) t.push(`ask:${ask}`);
    for (const id of [].concat(supersedes ?? [])) t.push(`supersedes:${id}`);
    if (validUntil !== undefined) t.push(`valid-until:${whenMs(validUntil)}`);
    return this.write({ kind: 'say', room, content: text, cargo, tags: t });
  }

  /** Durable knowledge: a decision, lesson, preference or commitment (never status). */
  async remember(text, { kind, room = 'memory', validUntil } = {}) {
    if (!MEMORY_KINDS.includes(kind)) throw new TypeError(`kind is one of ${MEMORY_KINDS.join(', ')}`);
    const tags = [`memory:${kind}`];
    if (validUntil !== undefined) tags.push(`valid-until:${whenMs(validUntil)}`);
    return this.write({ kind: 'memory', room, content: text, tags });
  }

  /**
   * Ranked recall, the search every seat runs (docs/SEARCH.md): words and meaning fused, approved readings first,
   * a replaced record under its successor. `asOf` reads Oracle as it stood then.
   */
  recall(query, { room, limit = 10, asOf, rerank, byRoom } = {}) {
    return this.request('GET', `/search${q({ q: query, channel: room, limit, as_of: asOf === undefined ? undefined : whenMs(asOf),
      rerank: rerank === false ? 'off' : undefined, by_room: byRoom ? 'true' : undefined })}`);
  }

  /** Can Oracle answer this before you recall? answerable/1: yes, partial or no, and what is missing. */
  answerable(question, { room } = {}) {
    return this.request('GET', `/answerable${q({ q: question, room })}`);
  }

  /** One record, whole, by its OBS id or its code. `envelope: true` adds the signed envelope. */
  get(id, { envelope: withEnvelope = false } = {}) {
    return this.request('GET', `/search${q({ id, include: withEnvelope ? 'envelope' : undefined })}`);
  }

  /** A thread: its root first, the rest newest first, paged with `before`. */
  thread(root, { limit, before, includeCargo } = {}) {
    return this.request('GET', `/threads/${encodeURIComponent(root)}${q({ limit, before, include_cargo: includeCargo ? 'true' : undefined })}`);
  }

  /** A room's messages, newest first, paged with `before`. */
  messages(room, { limit, before } = {}) {
    return this.request('GET', `/rooms/${encodeURIComponent(room)}/messages${q({ limit, before })}`);
  }

  /** Open asks (or another state), every room this seat reads. */
  asks({ state = 'open' } = {}) {
    return this.request('GET', `/asks${q({ state })}`);
  }
}

export default Oracle;
