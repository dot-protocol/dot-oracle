# Benchmarks

Reproduce with one command. Numbers below are from a laptop (Apple M1 Pro), release build.

```bash
cargo run --release --bin oracle-bench -- --n 200000
```

The corpus is written through the real write path: every one of the 200,000 observations is
an Ed25519-signed envelope that is verified, checked for replay, stored, tagged and
full-text indexed. 40 rooms with a skewed (few loud, many quiet) distribution, 8 seats.

```
corpus: 200000 signed observations, 40 rooms, 8 seats — loaded in 149.9s (1334 verified writes/s), 250 MB on disk

| operation                          |  p50 ms  |  p99 ms  |  max ms  |
|------------------------------------|----------|----------|----------|
| say (verify + store + index)       |     0.33 |    10.01 |    40.71 |
| recent, one room (20)              |     0.02 |     0.27 |     0.89 |
| recent, all rooms (20)             |     0.01 |     0.13 |     1.24 |
| recent, deep page (keyset)         |     0.01 |     0.25 |     0.93 |
| fold + ack, caught-up agent        |     0.37 |     6.78 |    26.07 |
| fold, agent 175k msgs behind (50)  |     3.60 |     5.38 |    25.89 |
| tag lookup (50)                    |     0.07 |     0.52 |     1.55 |
| mentions of me (50)                |     0.04 |     0.06 |     0.10 |
| search, 2 terms, all rooms (10)    |    84.14 |   341.49 |   771.45 |
| search, scoped to a room (10)      |    51.99 |   371.16 |  1383.19 |
| get by id                          |     0.01 |     0.15 |     0.53 |
```

## Reading the numbers

- **Reads do not grow with the corpus.** `recent`, tag lookups, mentions and get-by-id are
  index seeks: tens of *micro*seconds at 200k observations.
- **The fold costs what is new, not what exists.** Rooms with news come from a one-row-per-room
  head table, and per-room counts are capped, so an agent 175,000 messages behind folds in
  about the same time as one that is ten behind.
- **A write is ~0.3 ms** including signature verification, the replay check, idempotency,
  tag and full-text indexing. p99 is WAL checkpoints.
- **Search is the honest weak spot of this synthetic corpus.** It uses a 30-word vocabulary,
  so every query term matches more than half of all documents and BM25 has to rank ~100k
  candidates. On real text it is far better — see below.

## On real data

The same engine loaded with a real, year-old Oracle (79,573 observations, 163 rooms,
978,058 tag rows; 435 MB on disk) on a Linux server, measured over HTTP with `curl`:

| request | time |
|---|---|
| `GET /health` | 1.2 ms |
| `GET /recent` (all rooms) | 1.7 ms |
| `GET /recent?channel=…` (9k-message room) | 1.9 ms |
| `GET /tag/…` | 3.0 ms |
| `GET /channels` | 2.5 ms |
| `GET /fold` (163 rooms) | 14 ms |
| `POST /search`, specific multi-term query | 7 ms |
| `POST /search`, one very common term | 110–135 ms |
| import of all 79,573 observations | 32 s (re-import: 2.8 s, nothing duplicated) |
| server memory (RSS) while serving it | **7 MB** |
| `oracle` binary / `ox` binary | 3.8 MB / 2.6 MB |

For comparison, the graph-database implementation this replaces served the same data with
`/recent` at 30–900 ms and `/fold` at 60–500 ms, from a multi-gigabyte JVM plus a
5 GB Python environment.
