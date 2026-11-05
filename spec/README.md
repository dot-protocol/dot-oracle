# Security transition model

`OracleSecurity.tla` is a finite abstraction of chain growth and read authorization.
Run `python3 scripts/check-formal-model.py` with Java 21 available. The runner pins
TLC 1.7.4 by URL and SHA-256, checks the correct model, and requires each of four
mutations to fail for its named invariant. CI and release publication require this
job; logs preserve the explored-state count and counterexamples.

The checked configuration has two seats, two rooms (one private), and at most three
records. Transitions append a record, change room scopes, change private-room
membership, or read. Read authorization is evaluated at the time of the read;
revocation does not pretend to retract data previously disclosed.

| Invariant | Meaning | Negative control |
|---|---|---|
| AppendOnly | Every previous log is a prefix of the next log | Drop the first record during append |
| ChainWhole | Record position, ID and predecessor remain consecutive | Also broken by the append mutation |
| WithheldWhole | Hiding content retains every chain position and predecessor | Drop withheld records from the view |
| ScopeSafety | Every visible read was within its token's room scope | Bypass the scope check |
| PrivateSafety | Every visible private-room read had membership | Bypass the membership check |

Local TLC run: 161,793 generated states, 14,848 distinct states, maximum search depth
11, no violations. Each of the four mutations produces the intended counterexample.
TLC uses state fingerprints; these counts describe this bounded configuration,
not an unbounded theorem.

## Abstraction and proof limits

Chain identifiers/predecessors are symbolic positions. This model assumes an
authenticated append and does not model hashes, signatures, key rotation, or a
malicious server fabricating chain metadata. In particular, a withheld record is
not a cryptographically verified signed record; retaining its link does not prove
its hidden contents or authenticate an unanchored tail.

The projection models the intended intersection of room scope and membership.
It is not a proof that every Rust route implements that projection. Server
integration tests and code review must establish the implementation connection.
The model excludes crashes, persistence, concurrency inside storage, inter-owner
replication, resource bounds, confidentiality from operators, and liveness.
Those require separate models and/or implementation evidence.

Changing bounds or the transition relation requires rerunning the checker and its
negative controls. Never replace the checker with text matching alone: the runner
requires a successful completed check, or a nonzero result naming the intended
invariant for each deliberately broken variant.
