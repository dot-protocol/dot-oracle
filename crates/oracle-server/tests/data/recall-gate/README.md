# The recall gate

A fixed, synthetic team memory and the questions asked of it, run by `tests/recall_gate.rs` on every commit
(plan.oracle.search CI-S9). It exists so a change to ranking that makes recall worse fails CI before it ships.
The real evaluations (`evals/moments-20261103`, `evals/longmemeval`) need the live Oracle or a downloaded dataset;
CI has neither, so this set is written to be small, offline and honest about what it covers.

- `corpus.jsonl`: one record per line: `key`, `room`, `who`, `mins` (minutes after the corpus clock), `text`, and
  optional `tags`. A tag `supersedes:@<key>` names an earlier record of this file; `valid_until_mins` adds a
  `valid-until:` that has already passed when the gate runs.
- `queries.jsonl`: `q`, optional `room`, `expect` (the keys that answer it; empty means nothing in scope should),
  `cat` (what the question tests) and `arm` (`both`, or `meaning` when only a paraphrase can find it).
- `synonyms.json`: the toy embedder's idea of similar words, so the meaning arm runs without a model.
- `BASELINE.json`: the counts the gate must not fall below, per arm and per category.

Two arms run: words only (`Ranker::Keyword`, no embedder) and the default fusion with the toy embedder. Scores: an
answer first (top 1) and among the first five (top 5); for an empty `expect`, the question passes when nothing is
returned. The gate fails if any arm's count falls below `BASELINE.json`. When a change makes recall better, raise the
baseline in the same commit and say so in the CHANGELOG; lowering it is a regression and needs a stated reason.

The words are about a made-up team; nothing here is from a real Oracle.
