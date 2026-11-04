# Offline verification

A record of this Oracle can be checked by anyone, with no server, no account and no network: the record, the
owner's key file, and the owner's public key are enough (Blaze, 4 Nov 2026, before v0.1.0; the identity talk's
lesson that a record must check without us).

## What the reader trusts

One thing: the owner's public key, fetched from a place the owner controls (did:web style: a fixed path on the
owner's own site). Everything else is checked against it.

## The key file (`oracle-keys/1`)

`ox seats --export keys.json` writes it:

```json
{"schema": "oracle-keys/1", "owner": "<handle>", "owner_pubkey": "<hex>", "origin": "<url>", "exported_ms": 0,
 "events": [{"action": "add", "handle": "...", "pubkey": "...", "ts_ms": 0, "nonce": "...", "signer": "...", "sig": "..."}]}
```

`events` is the registry's signed change log (`GET /keys?history=true`), oldest first. The owner key is the first
owner key the registry ever held; a rotated owner key is a later event. Export refuses to write a log that does not
replay from its own owner key.

A server publishes its key file at `https://<your-oracle>/.well-known/oracle-keys.json`, refreshed every 15 minutes
by `ops/systemd/oracle-keys-export.timer`. Pin the owner key from a second place you trust, not from this file.

## The check

`ox verify face.jsonl --keys keys.json --owner-key <hex>` (the face comes from `ox chain --out`, docs/CHAIN.md):

1. The pinned owner key must be the key file's owner key, or nothing is checked.
2. The key log is replayed from the owner key with the server's rules (store.rs `apply_key_event`): the owner adds
   seats and rotates or revokes any seat's keys; a seat may add a key for itself, rotate the key that signed, or
   revoke its own; the owner key is rotated, never revoked. One event that no key held at that moment signed, or that
   the rules refuse, fails the whole log: a log that is not whole proves nothing.
3. Each record: its signature over its rebuilt bytes, its id against its signature, and its key against what its
   author held at the record's time. A failure names its reason: bad signature, id mismatch, signed before its key
   was registered, signed after its key was revoked, or a key that was never its author's.

Without `--owner-key` the key inside the file is trusted, and the report says so. `ox verify` and
`ox chain --check` need no seat and no config; a stranger runs them.

## What it does not prove

That the records are all there is: a face shows what it holds (the chain's `prev` links show gaps, docs/CHAIN.md).
That the owner key was not stolen: that is the owner's to guard. Records signed before the key log began (imported
history) fail as signed before their key was registered; that is the honest answer, not a pass.
