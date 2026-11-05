# Dependency security gate

CI and release publication call the same reusable workflow. It installs pinned
`cargo-audit` 0.22.2 and `cargo-deny` 0.20.2 with their lockfiles, then runs
`scripts/check-dependency-security.py`. Release publication depends on its success.

The audit fetches a fresh RustSec database with system git and prints its revision.
A fetch failure is a failure, not a clean audit. `cargo audit --deny warnings`
checks the real lockfile. `cargo deny` checks advisories, licenses, bans and sources
against `deny.toml`: no ignored advisories, no unapproved licenses or git/registry
sources, and no second native TLS stack. Existing duplicate transitive versions
remain visible warnings; wildcard dependency specifications are refused, including
internal paths, which carry the workspace version.

Both tools must also reject a metadata-only `time` 0.1.45 fixture specifically for
RUSTSEC-2020-0071. The fixture is generated under a temporary directory and never
compiled or executed. A tool error, network failure, or unrelated refusal does not
count as that negative control passing.

The public export includes the policy, workflow and scripts. Its tests check that
script paths referenced by exported workflows remain allowlisted. An export scan
alone cannot prove those workflows run successfully; exact-revision hosted receipts
remain required.

## Unsafe-code boundary

Every production crate root and build-script root forbids unsafe code. Workspace
lint inheritance also covers test and auxiliary targets. Local `allow(unsafe_code)`
cannot override `forbid`. This constrains Oracle's source, not transitive crates,
generated machine code, or the compiler. Dependency auditing and review still apply.

References: [cargo-audit](https://github.com/RustSec/rustsec/tree/main/cargo-audit),
[cargo-deny checks](https://embarkstudios.github.io/cargo-deny/checks/index.html),
[the negative-control advisory](https://rustsec.org/advisories/RUSTSEC-2020-0071.html).
