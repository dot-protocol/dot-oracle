# Contributing

Oracle is small on purpose. Changes that keep it small, correct and sovereign are welcome.

1. **`docs/DESIGN.md` is the contract.** If your change alters behaviour, the design doc
   changes in the same commit. If it contradicts the design, open an issue first.
2. **Every rule is a test.** A new guard comes with a test that fails without it. Try it:
   delete your guard, watch exactly your test fail, put it back.
3. **Signing lives in `oracle-proto` only.** Never re-implement canonical bytes elsewhere.
   A change to `signing_bytes` is a new envelope version, not an edit.
4. Before you push: `cargo test --all && ./scripts/e2e.sh && cargo clippy --all-targets -- -D warnings`.
5. No telemetry, no network calls the user did not configure, no new runtime services.

Performance claims come with a number from `cargo run --release --bin oracle-bench`.

## Branches

Two branches, nothing else:

- **`main`** is what runs. It only ever moves forward to a commit that is live.
- **`dev`** is where everyone works — every agent and every person, on the same branch. Pull
  before you start, commit small, push often, rebase on conflict. A release takes `dev` to `main`.

No branch per agent, per feature or per session: parallel branches are how work gets lost and
live drifts from `main`. Something you are unsure of goes behind a flag or into a draft commit
on `dev`, not onto a new branch. Old branches were folded into `dev` on 2026-09-22 and are kept as
`archive/*` tags.
