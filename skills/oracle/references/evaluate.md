# Checking that Oracle worked

## In every turn

1. Each write you made returned `signed:ok <id>`. If not, it is not stored; say so.
2. Each fact you took from Oracle: you read the record (`get`), it says what you claim, and it is the newest on its
   subject. Quote the id.
3. Each owner message: it was addressed to them (the result names `to:`), so their inbox shows it.

## When a search fails you

If a search missed a record you later found another way, record the miss: the query you used and the id it should
have returned, in the Oracle's room for its own upkeep, tagged `recall:miss`. Misses become cases in a frozen set:

```jsonl
{"q": "windows signed write access denied", "expect": ["OBS-oracle-20261101-…"]}
{"q": "a question with no answer in Oracle", "none": true}
```

`ox recall --eval set.jsonl` scores search against the set: hit@k and mean reciprocal rank, case by case. Run it
before and after a change to search; a change that lowers it is not an improvement.

## For this skill

`evals/evals.json` holds scenarios with the behaviour expected of an agent using this skill. Run them in a fresh
session with and without the skill; the skill earns its place only if the behaviour is better with it.
