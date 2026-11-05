# Reflect: observations refined, not overwritten

Status: 4 Nov 2026. This is the competitor map's reflection row: Hindsight's observations, Letta's sleep-time agents and
Cognee's improve. Memories pile up one by one. Reflection reads the ones that came since the last reflection and writes
what the team now holds true about each subject they speak of.

An Oracle has one reflector, not one per agent (Blaze, 4 Nov 2026: "every agent should not have their own nightly
reflection ... there should only be one, Pythia, which would be doing it for all of oracle"). On ours it is Pythia's seat.

## What happens

1. `ox reflect --room memory`, run as the reflector, reads every seat's memories in that room that came after the last
   reflection, oldest first, and leaves out reflections. A memory is a record tagged `memory:decision`, `memory:lesson`,
   `memory:preference` or `memory:commitment` (what `ox remember` and `ox capture` write); acks, replies and State heads
   are not, though the room's `kind=memory` search returns them too. Each memory goes with the seat that wrote it. It also reads the
   reflector's current observations: every reflection of its own that no later reflection supersedes. Two new memories at
   least are needed; with fewer it says so and writes nothing. `--room a,b` reflects on each room on its own, so an
   observation stays in the room its memories came from and nothing private moves to another room. `--room all` reads
   every room in one pass and still reflects on each room on its own; its first run reads the last day only.
   `--except a,b` leaves out authors that write memories but are not minds (automated feeds).
2. It sends them to `POST /reflect` in batches of 24 (`--limit`), at most four a run (`--batches`), which uses the same configured model endpoint as `/extract`; that endpoint can be remote (docs/CAPTURE.md). Every line the
   secret scan matches is dropped first. The answer holds at most five observations. Each one:
   - is about one subject in a few words;
   - is two or three sentences on what is true now and what changed;
   - cites only the memories it was given, and at least one;
   - is about something other than the room itself (a subject that is only the room's name is dropped by `ox reflect`);
   - names, in `refines`, the current observation it updates, or nothing.

   Anything else is dropped. The server stores nothing.
3. The reflector signs each observation as its own memory in the same room, with these tags:
   - `reflection`
   - `observation:<subject>`
   - one `ref:<id>` for each memory it rests on, up to twelve

   Its cargo is `reflect/1`: about, cites, refines and model. An observation that refines an earlier one also carries
   `supersedes:<that id>`. The earlier observation stays, read whole by id, and ranks under its successor in recall. That
   is what "refined, not overwritten" means here, and every observation can be checked against the memories it cites.
4. The reflector keeps the last memory it read in `<ox home>/reflect/<room>.json` after each batch, so the next run starts
   after it and nothing is skipped.

## Sleep-time

On core it runs once a night for the whole Oracle: `ops/systemd/oracle-reflect.timer`, 22:00 UTC (03:30 IST), runs
`ox --home /var/lib/pythia/ox reflect --room all --except brain,world,estate,hustler-book,hustler-desk` as Pythia at
low priority (on 4 Nov 2026, about 500 of the 536 memories were in #memory and the rest in a dozen seat rooms), with `Persistent=true` so a missed night
runs at the next start. It needs the server's model (`ORACLE_EXTRACT_URL`; without one each run is refused and changes
nothing). `oracle up` installs no reflection: an agent seat never reflects on its own. By hand, `ox reflect --dry-run`
prints what it would write and writes nothing.

Measured on core with real memories: one seat's 24 newest memories took 5.5 minutes on qwen3:8b with six threads, and
gave one observation that joined two of them correctly.

## Limits, said plainly

- A small model on a CPU writes these observations. They are only as good as it is, and they never replace the memories
  they cite: recall finds both.
- One run reads at most 96 new memories a room (four batches of 24). For one room, what is left waits for the next
  night, oldest first. With `--room all` there is one place for every room, so what is left over in a room (past 96, or
  a lone memory) is not reflected on; the memories themselves stay and recall finds them.
