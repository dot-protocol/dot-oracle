# Release provenance

The release workflow pins third-party actions to complete commit IDs. Updating a
pin is a reviewed dependency change. Compilation and install tests receive only
`contents: read`; checkouts do not leave credentials in Git configuration.

Only the tag-gated publish job receives `contents: write`, `id-token: write`, and
`attestations: write`. After all platform smoke tests and the Linux reproducibility
gate pass, it downloads this run's
artifacts and copies the installers from this revision. Before uploading any
release assets, `actions/attest` signs provenance for every file in `dist/`,
including archives, checksum files, and installers. An attestation failure stops
publication. It is not an optional best-effort step.

For a downloaded file, verify the expected repository and signing workflow:

```sh
gh attestation verify oracle-x86_64-unknown-linux-musl.tar.gz \
  --repo dot-protocol/dot-oracle \
  --signer-workflow dot-protocol/dot-oracle/.github/workflows/release.yml
```

Inspect the verified source revision and compare it with the intended release.
A checksum alone does not establish who produced a file. A valid attestation
associates its digest with a workflow execution; it does not prove the program is
safe, the builders were uncompromised, or a second build produces identical bytes.

GitHub supports attestations for public repositories, and for private repositories
on Enterprise Cloud. An unsupported private staging repository must not disable
the gate to manufacture a passing release. See the upstream
[action documentation](https://github.com/actions/attest).

## Remaining gates

This change supplies the workflow, not evidence of an executed release. A tag run
must create an attestation and an independent download must verify it before
provenance is reported as runtime-verified. No SLSA level is claimed.

The Linux x86-64 gate uses Rust 1.96.0, two fresh source paths, two independent
target directories, a fixed source timestamp, and remapped source paths. Both
builds must match each other and the binaries inside the release archive. Evidence
records the source revision, compiler version, and all three sets of digests.
The build script uses the committed revision, not uncommitted working files.

Until the gate executes successfully, reproducibility is unverified. Even a pass
covers only Linux x86-64 binaries in this workflow: it is not independent-builder
verification or a byte-for-byte package guarantee. Runner images and system
packages still receive updates; archive metadata is not normalized. The other
four targets still need reproducibility gates. Attestation does not substitute
for any of these tests.
