# Oracle threat model

Scope: the Rust server, CLI/MCP client, capture/extraction, replication, and public
release. This is an implementation-oriented model, not a penetration-test report
or a claim of protection from a compromised operating system. Each row names the
boundary and the executable evidence to inspect. Passing a listed test establishes
its exercised cases, not universal correctness.

## Assets and principals

Protect seat signing keys, bearer tokens, private records and transcripts, room
membership and scope tables, signed history and its attribution, the availability
of the core, and the integrity of public source and release artifacts.

Principals are the machine owner, Oracle operator, registered seats, public seats,
anonymous clients where enabled, other replica operators, extraction models, and
build/release infrastructure. An agent is a principal with granted capabilities;
its generated text is not an owner decision. A signature establishes signed bytes
and key attribution, not truth, consent, or permission to execute instructions.

## Trust boundaries and evidence

| Boundary and attacker | Mitigation and evidence | Limit or outstanding work |
|---|---|---|
| Seat key: network attacker changes an envelope or impersonates its author | Canonical signing/verification in `oracle-proto`; `tests/author_chain.rs`, `tests/sync.rs` tampered-entry cases, and end-to-end signature refusals | The endpoint holding the key is trusted. A stolen key can sign; registry rotation/history and revocation cannot erase disclosures already made. |
| Bearer token: anonymous/public/read-only client requests a stronger capability | HTTP principal/scope checks; `tests/room_view.rs` scope intersection and public-seat tests, `tests/public_release.rs`, `scripts/e2e.sh` refusal controls | A stolen token inherits its current capabilities. Operators must protect transport and token storage. No claim that every configured URL enforces TLS. |
| Private rooms: an authenticated non-member attempts reads through direct, aggregate, search, or chain routes | Fail-closed private-room table and membership checks; `tests/private_rooms.rs`, `tests/entity_brief.rs`, `tests/room_view.rs` | Server/operator sees plaintext. Membership removal cannot retract copies already read. Withheld chain links reveal author/position/time/link metadata; they are not verified record signatures. |
| Original write versus retry: another seat or room reuses an operation ID | Original-author/room authorization and idempotent receipt checks; `tests/public_release.rs` concurrent writes, retries, quotas and reopen | This is bounded local concurrency evidence, not global capacity or crash-recovery proof. |
| Local capture: another OS user reads queued unfiltered text or redirects a capture directory | Unix directories 0700, files 0600, durable writes, directory-symlink rejection; `capture::tests::captures_at_the_same_time_keep_separate_private_files` and `capture_refuses_a_redirected_directory` | **Windows ACL isolation is unverified.** Inherited user-directory ACLs are not an explicit verified restriction. Same-user malware/root is outside this mitigation; historical exposure before tightening is not repaired. |
| Extraction/reflect: remote Oracle or configured model receives private text; model emits misleading memories | Destination behavior documented in `CAPTURE.md`; server secret scan and candidate validation in `extract.rs`; extracted provenance remains visible | Text reaches Oracle before filtering. Endpoint locality is not enforced; secret filtering does not remove all private information. Public-user capture consent is an open release gate. Model output is untrusted interpretation. |
| Sync: hostile peer tampers with, duplicates, or fabricates replication rows | `/sync/*` requires admin scope; verified native envelopes and quarantine in replication; `tests/sync.rs` tamper, unknown-key, duplicate-operation, partition and convergence cases | Current replication is an administrative trust boundary. **Do not treat it as least-privilege sharing between independent owners.** Per-owner selective disclosure and revocation across replicas remain unverified; a receiving operator can retain plaintext. |
| Public export: private history, paths or records accidentally ship | Fresh allowlisted export of committed files, scrubbing and required scanners; `ops/public-export/test_export.py` planted-secret, excluded-file and missing-scanner controls | A clean scan is not proof no sensitive information exists. Exported source must also build and its referenced workflow inputs must be present. Never open the private repository's history. |
| Dependency/release supply chain: malicious update, known vulnerable crate, replaced action or asset | Lockfile; dependency policy; pinned release actions/compiler; minimal job permissions; checksum, provenance and Linux comparison gates (`RELEASE-PROVENANCE.md`) | A signature is not proof of safe source or an uncompromised builder. Actual hosted attestations and comparisons are required. Other platform/package reproducibility is not established. |
| Availability: oversized/malformed requests or expensive authenticated work | Request/parser bounds, quota tests and bounded work execution; `tests/public_release.rs` and existing HTTP budget tests | Parser fuzz coverage, long-running corpora, adversarial capacity measurements and recovery drills remain separate evidence requirements. No eight-billion-user capacity claim. |

Test paths above are relative to `crates/oracle-server/` unless explicitly prefixed.
Capture tests live in `crates/ox/src/capture.rs`.

## Formal and operational evidence still owed

Machine-checked models must state their finite scope and assumptions, and include
a deliberately broken invariant that the checker rejects. They do not prove the
Rust implementation refines the model. Kani harnesses must exercise real core
functions with explicit bounds; proof of a simplified substitute is insufficient.
Fuzzing must enumerate network parsers, retain corpus/crashes, and record actual
nightly execution. Zero crashes in a bounded run is not proof of parser safety.

The release checklist must distinguish implemented checks, local passes, exact-head
CI passes, deployed behavior, and independent review. Unverified gaps above remain
visible until specific evidence closes them; none is waived by this document.
