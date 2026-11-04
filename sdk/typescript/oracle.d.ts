// Types for @dot-protocol/oracle (oracle.mjs). The server's shapes are in docs/openapi.yaml and docs/SEARCH.md;
// fields the server may add later are kept open with an index signature.
import type { KeyObject } from 'node:crypto';

export declare const SAY_DOMAIN: 'oracle-say/2';
export declare const KINDS: readonly ['say', 'state', 'memory'];
export declare const MEMORY_KINDS: readonly ['decision', 'lesson', 'preference', 'commitment'];
export type Kind = (typeof KINDS)[number];
export type MemoryKind = (typeof MEMORY_KINDS)[number];
/** A Date, unix ms, or an ISO date or date and time (a date alone means the end of that day, UTC). */
export type When = Date | number | string;

/** The signed write, oracle-say/2. */
export interface Envelope {
  channel: string;
  author: string;
  kind: Kind;
  ts_ms: number;
  nonce: string;
  client_op: string;
  content: string;
  cargo?: string;
  tags?: string[];
  authored_ms?: number;
  prev?: string;
  sig: string;
}

export interface Receipt { ok: true; id: string; seq: number; duplicate?: boolean; signed?: boolean; [k: string]: unknown }

export interface Record {
  id: string;
  channel: string;
  author: string;
  kind: Kind;
  content: string;
  tags: string[];
  created_ms?: number;
  cargo?: string;
  superseded_by?: string | null;
  [k: string]: unknown;
}

/** One ranked hit (search-result/1, docs/SEARCH.md). */
export interface Hit extends Record {
  score: number;
  approved?: boolean;
  snippet?: string;
  title?: string;
  class?: string;
  valid_until?: number;
  invalid_since?: number;
}

export interface RecallResult { hits?: Hit[]; items?: Hit[]; engine?: string; prior?: { state: 'found' | 'thin' | 'none'; says?: string }; [k: string]: unknown }

export interface Answerable {
  verdict: 'yes' | 'partial' | 'no';
  says: string;
  next?: string;
  words: { word: string; records: number; approved: number }[];
  missing: string[];
  [k: string]: unknown;
}

export interface Page<T = Record> { items: T[]; count?: number; limit?: number; next_before?: number | null; [k: string]: unknown }
export interface Thread extends Page { root: Record; thread?: string }

export interface SayOptions {
  /** Thread it under this record. */
  replyTo?: string;
  /** Seats to address (their inbox), with or without @. */
  to?: string | string[];
  intent?: string;
  ask?: string;
  /** Replace an earlier record: it stays, read whole by id, and ranks under this one. */
  supersedes?: string | string[];
  /** After this, recall ranks it as a replaced fact. */
  validUntil?: When;
  tags?: string[];
  /** Structured data; an object is sent as JSON. */
  cargo?: string | object;
}

export interface OracleOptions {
  url: string;
  token: string;
  seat?: string;
  seed?: string;
  key?: KeyObject;
  fetch?: typeof fetch;
  timeoutMs?: number;
}

export declare class OracleError extends Error {
  readonly status: number;
  readonly error: string;
  readonly detail?: string;
}

export declare class Oracle {
  constructor(options: OracleOptions);
  static fromEnv(env?: NodeJS.ProcessEnv): Oracle;
  readonly url: string;
  seat?: string;
  request<T = unknown>(method: string, path: string, body?: unknown): Promise<T>;
  whoami(): Promise<{ handle: string; scope: string; pubkey: string; features: string[]; [k: string]: unknown }>;
  features(): Promise<Set<string>>;
  write(w: { kind?: Kind; room: string; content: string; cargo?: string | object; tags?: string[] }): Promise<Receipt>;
  say(room: string, text: string, options?: SayOptions): Promise<Receipt>;
  remember(text: string, options: { kind: MemoryKind; room?: string; validUntil?: When }): Promise<Receipt>;
  recall(query: string, options?: { room?: string; limit?: number; asOf?: When; rerank?: boolean; byRoom?: boolean }): Promise<RecallResult>;
  answerable(question: string, options?: { room?: string }): Promise<Answerable>;
  get(id: string, options?: { envelope?: boolean }): Promise<Record & { envelope?: Envelope }>;
  thread(root: string, options?: { limit?: number; before?: number; includeCargo?: boolean }): Promise<Thread>;
  messages(room: string, options?: { limit?: number; before?: number }): Promise<Page>;
  asks(options?: { state?: string }): Promise<Page>;
}

export declare function keyFromSeed(seedHex: string): KeyObject;
export declare function publicKeyHex(key: KeyObject): string;
export declare function signingBytes(env: Omit<Envelope, 'sig'> | Envelope): Buffer;
export declare function signEnvelope(key: KeyObject, env: Omit<Envelope, 'sig'> | Envelope): Envelope;
export declare function envelope(key: KeyObject, fields: { channel: string; author: string; kind?: Kind; content: string; cargo?: string | object; tags?: string[]; prev?: string; authoredMs?: number }, nowMs?: number): Envelope;
export declare function resign(key: KeyObject, env: Envelope, nowMs?: number): Envelope;
export declare function linkOf(sigHex: string): string;
export declare function whenMs(v: When): number;
export default Oracle;
