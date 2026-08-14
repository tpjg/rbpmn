# rbpmn benchmarks

Models, data, hardware spec and one command, in one repository.

No published BPMN benchmark manages all four. Flowable's and Camunda's ship
models and code but leave the machine to prose; BenchFlow (USI Lugano) had the
best methodology of any of them and was never packaged as a reusable artifact.
The gap is normally hard to close because a BPM benchmark has an
infrastructure diagram behind it — Elasticsearch, a broker, a cluster. This
engine's entire stack is one Postgres, so "hardware spec" collapses to a
documented machine and a documented server, and both fit in this directory.

```
just bench                 # the lifecycle suite, writes results/
just bench SCENARIO        # one scenario
just bench-population      # park a large cohort, probe standing cost at each size
just bench-micro           # pure-core criterion suite + regression gate (fast)
just bench-baseline        # re-record this machine's micro baseline (manual)
just bench-report          # render results/*.json into a markdown table
```

Needs the local Postgres this repository already assumes (`just serve`, the
engine's integration tests) and nothing else. **No Docker.**
`benchmarks/compose.yml` is optional — `just bench-compose` — for when you
want the server itself pinned rather than whatever your machine runs.

---

## The rule this track exists under

**Benchmarks never gate CI on an absolute number.** Absolute numbers belong to
a machine; a build that fails because a laptop was warm teaches people to
ignore builds. `cargo test` does not build this crate at all — `rbpmn-bench`
is a workspace member but not a *default* member.

There is exactly one exception, and it is fenced in four ways: `just
bench-micro` compares the **pure-core** suite (no database, no IO, no clock)
against a baseline recorded **on the same machine**, with a threshold that
includes **that machine's own measured noise**, and a machine with no baseline
reports and passes. See "The gate" below for what that buys and what it does
not.

---

## Two families

### A. Lifecycle — the headline metric

One iteration is a **whole instance lifecycle**: start → work items become
available → claimed (a push handler, or `get_task` for user tasks) →
completed with a merge patch → instance reaches a terminal state. Instance
creation alone is the number vendors quote and it is not a workload.

Where a scenario has user tasks, the **query** is inside the measured path:
`count_tasks` for the dashboard indication and a filtered `get_task` for the
claim. Finding work is a real cost, and leaving it out is how a task-list
benchmark flatters itself.

Headline: **completed instances per second**, plus p50/p95/p99 instance
latency, reported next to the worker counts, the connection-pool size and the
history volume each instance produced.

| Scenario | Shape | What it isolates |
|---|---|---|
| `linear-5-service` | 5 sequential service tasks | the floor cost of an instance: five transactional steps, five claims |
| `parallel-4` | split → 4 branches → join | fan-out, local join counting, four completions serializing on one instance row lock |
| `exclusive-chain` | 5 exclusive split/join stages | FEEL-subset condition evaluation against the variable document (both branches carry a task, so the work is constant and only the routing varies) |
| `usertask-inbox` | 1 service + 1 user task | the task API round trip, including `count_tasks` and a filtered claim against a declared index |
| `message-wait` | service → message catch → service | arming a subscription and `correlate`'s indexed delivery |
| `timer-short` | service → 100 ms timer → service | the scheduler's claim path: pick off the `due_at` index, lock the instance, re-check, fire and delete in one transaction |
| `mixed-typical` | 17 elements: routing, subprocess, parallel pair, human approval with an SLA timer, archive | the closest thing here to a customer-shaped workload |

Each scenario's TOML states, in prose that is copied verbatim into every
result file, what it measures and what it does **not**. Read those before
quoting anything: `just bench-report` prints them under each table, and
`rbpmn-bench list` prints them on their own.

### B. Population — standing cost, not throughput

`just bench-population` asks the question the rest of the suite cannot: with
a large cohort parked and doing nothing, what does everything *still* cost?

That is the shape of a long-running deployment. Year-long flows at 2M
instances/year mean **0.06 instances per second and 2 000 000 live
instances** — the rate is a non-event and the population is everything. A
drain benchmark that finishes in two seconds never had a million rows to walk
past.

Two scenarios, both a five-step flow that parks its cohort on a long wait:
`population-timer` (a `P1Y` timer) and `population-message` (an open
subscription). The suite builds to each configured size in turn and probes
there, so the output is a **curve, not a point** — the question is not "is
this fast" but "does this grow with the population", and only the second one
matters when the population is going to be a million either way.

Probed at each size: an empty worker poll, a claim-and-complete, starting an
instance, `count_tasks`, an `/v1/events` page, opening the inspector on an
old instance, the admin "which definitions are in use" query, plus
`timer_next_due` / `timer_fire` or `correlate` depending on what the cohort
waits on. Each is timed individually and reported as a distribution.

Two honest notes on method. The build is parallel and its rate is reported
but is *not* a headline — it is setup. And `timer_fire` forces a handful of
due dates rather than waiting a year; the claim, the instance lock, the
re-check, the step and the row delete are all real, and the storm figure
(how long to drain N simultaneous timers) is extrapolated from the per-fire
rate and labelled as such.

**What it found immediately.** `Engine::next_due_in` — the scheduler's sleep
computation, run on every idle cycle of every node — computes
`min(t.due_at)` by *joining* `rbpmn_timer` to `rbpmn_instance` to filter on
`status = 'active'`. The join defeats the index-min shortcut. Measured at
10 000 armed timers:

| query | plan | time |
|---|---|---|
| `next_due_in` as shipped | hash join over two sequential scans | **7.4 ms** |
| `min(due_at)` on the timer table alone | index-only scan + limit | **0.022 ms** |

336× at ten thousand, and linear in the population. See "Findings" below.

### C. Pattern micro-benchmarks — per-construct cost

**Pure core** (`benches/core_constructs.rs`, criterion): one `step`
transition per construct — a token crossing a sequence flow, an exclusive
split, a parallel split of N, a join of N, scope entry and exit, condition
evaluation. No database, no IO, no clock. This is the layer the gate watches
and the early-warning system for the semantic core: the split benchmarks at
widths 2/4/8/16 are there so an accidental O(n²) in gateway handling shows up
as a curve rather than as a throughput number sagging six months later.

**Persisted** (`rbpmn-bench micro-persisted`): the same constructs including
the rows they cause, run sequentially on one connection so the number is the
*latency* a construct adds rather than how many fit on a machine. Reported,
never gated.

Both read a construct's cost as the **difference against a baseline shape**,
not as an absolute. `exclusive-split` against `sequence-flow` is one gateway
evaluation; `parallel-join/8` against `parallel-join/2` is what widening a
join costs.

Models for both come from **the fixture corpus's own generator**
(`crates/rbpmn-core/tests/modelgen`), included rather than copied. It is
already this repository's independent second implementation of "what block
structure means"; a benchmark that hand-rolled its own emitter would drift,
and a construct the engine supports would end up with no cost number.

---

## Generate / execute / monitor

Copied from Flowable's structure, because it exists to measure a *saturated*
system rather than a ramp-up.

- **generate** parks N instances at their first wait state with the workers
  switched off. Deterministic: the variable document for instance *i* is a
  pure function of (seed, run id, i), recorded in the result.
- **execute** starts the workers, drains the backlog, and measures.
- **monitor** runs in **its own process**, sampling every N seconds:
  throughput, backlog depth (total and per topic), latency percentiles
  computed in the database, `pg_stat_database` counters, per-table sizes,
  sequential vs index scans, dead tuples on `work_item` and `token`, and
  connection counts by state. Separate because a sampler sharing a runtime
  with the load reports the runtime's scheduling delays as the database's
  latency, and one sharing the pool takes a connection from the thing it is
  measuring.

The three are separately invocable, and the split is real: `generate` on one
invocation and `execute` on another works, because the backlog is in Postgres
and the correlation keys are derived rather than remembered.

```
rbpmn-bench generate linear-5-service --instances 5000 --run-id nightly
rbpmn-bench execute  linear-5-service --instances 5000 --run-id nightly
```

**`steady`** (`just bench-steady`) is the other question: open-loop arrivals
at a fixed rate, for latency under load rather than saturation throughput. An
arrival that cannot be issued on time is **recorded, never absorbed** — a
closed loop that quietly slows down reports a rate it never ran at. Both
modes record whether backpressure occurred; in saturation mode the answer is
"not applicable, and here is why", not a bare `false`.

### Two latencies, deliberately not comparable

- `latency_kind: "arrival"` — `completed_at − created_at`, the instance's own
  lifetime. Steady mode. This is the number people assume they are reading.
- `latency_kind: "drain"` — `completed_at − <drain start>`, queue-inclusive.
  Saturation mode. In a drain of 1000 parked instances, most of an instance's
  latency is waiting its turn, and p50 lands near half the total drain.

Both are computed **in the database from database timestamps**, so no client
clock enters a latency figure. The measured duration comes from database time
too — the drain ends when the last instance committed, not when a poll
happened to notice.

---

## Reproducibility

Every run writes `results/<scenario>-<mode>-<date>-<host-id>.json` containing:

- the rbpmn git sha, and whether the checkout was **dirty**
- the scenario TOML's SHA-256, every model file's SHA-256, `tuning.sql`'s
  SHA-256, and the RNG seed
- the **deployed bindings manifest** — the other half of a definition, and the
  half no other engine's benchmark can show you, because in every other engine
  it is smeared into vendor XML attributes
- the full Postgres picture: version, the curated settings list, *everything*
  whose source is not the built-in default, and the per-table storage
  parameters
- the hardware, detected and declared, and where they disagree
- harness configuration: worker counts, pool size, warmup and measured
  instance counts, monitor interval, arrival rate
- the raw measurements: per-instance latencies (sorted, capped at 20 000),
  not only the aggregate
- the scenario's own statement of what it does and does not measure

The mode is in the filename because `saturation` and `steady` are different
measurements of the same scenario, and without it the second one written on a
given day would silently replace the first. Re-running the same mode does
replace it — that is correct, and the run says `(replaced)` when it happens.

### Starting conditions, and why they are not "cheating"

`just bench` starts from an **empty database** and runs `ANALYZE` after
parking the backlog. Both are recorded in the result file
(`fresh_database`, `analyze_before_execute`), and both have a reason.

*Fresh*, because a benchmark whose numbers depend on how many previous runs
were left lying around is not a benchmark. `--no-fresh` keeps the data.

*ANALYZE*, because without it this suite mostly measures **when autovacuum
last ran**. The claim path joins `rbpmn_work_item` to `rbpmn_instance` and
filters on `i.status = 'active'`; when the planner's statistics were last
collected on an idle system, `status` looks 100% `completed`, so it estimates
that no instance is active, drives the nested loop from `rbpmn_instance`, and
bitmap-scans work items per instance — O(active instances) per claim, on the
hot path. Measured on one database, same code, same parked backlog of 300
`mixed-typical` instances, one `ANALYZE` the only difference:

| statistics | throughput |
|---|---|
| stale — `status` = `{completed}` at frequency 1.00 | **20.6 instances/sec** |
| current — `{completed 0.89, active 0.11}` | **175.4 instances/sec** |

That is not a benchmarking artifact to be hidden; it is a real hazard for any
deployment whose instance table goes quiet and then takes a burst, and it
reproduces on demand:

```
psql -c 'analyze rbpmn_instance, rbpmn_work_item'    # while idle
rbpmn-bench generate mixed-typical --instances 300 --run-id repro --no-fresh
rbpmn-bench execute  mixed-typical --instances 300 --run-id repro --no-analyze
```

(`--no-analyze` alone will not do it: a fresh database has no statistics to be
stale, and the planner's defaults happen to choose the right plan.)

### Same host, by default

The harness and Postgres run on one machine, because that is how this engine
is deployed: a Rust library inside an application, stepping tokens inside the
application's own transactions, against the application's own database. A
remote database is supported and recorded (`postgres.local = false`) — never
mix the two in one comparison, since the per-transaction round trip dominates.

### The database is guarded

The harness starts hundreds of thousands of instances and rewrites per-table
autovacuum settings, so it refuses to run against a database whose name does
not contain `bench`. `--allow-any-database` overrides it, and the refusal
tells you so.

### hardware.md

A filled-in template, not prose: CPU, cores, RAM, **disk type**, local or
remote Postgres. The harness detects what it can and cross-checks; the disk is
the one field no program can determine and the one most likely to explain a
factor of ten. An unfilled template does not block a run — it records a
warning in the result and prints it, because "we do not know what disk this
ran on" has to travel with the number.

---

## The history axis — the one thing this suite cannot yet measure

History write volume is the single biggest performance lever in this design,
and the brief for this track asked every scenario to run at three history
levels: events off, instance-level, full.

**Only `full` exists.** Per-definition event-kind filtering is a roadmap item,
deliberately not shipped: it changes the event stream's *completeness*
contract, because a consumer could no longer tell "did not happen" from "was
not recorded" (`bpmn-engine-design.md`, phase 7, and the note at the top of
`rbpmn-core/src/event.rs`). Shipping it as a side effect of wanting a
benchmark axis would be exactly backwards for a project whose ground rule is
that capabilities land as linter rules and fixtures first.

So the axis is **wired and refused**. `history = "instance"` or `"off"` in a
scenario TOML is an error naming the missing feature, not a silent fallback to
`full`. When the feature lands, the axis is already there.

What the benchmark *can* honestly say about the lever today is in every result
file: `events_written`, `events_per_instance` and `event_bytes_per_instance`,
and `bench-report` prints events-per-instance next to every throughput figure.
The persisted micro-benchmarks give the same number per construct. That
measures the size of the lever without pretending to pull it.

---

## The gate, and what it can actually see

`just bench-micro` runs the pure-core suite and compares it against this
machine's baseline. A benchmark counts as regressed when it is slower than the
threshold (25% by default) **plus that benchmark's recorded noise on this
machine**.

**Baselines are never committed.** They live in `benchmarks/.baselines/`,
which is gitignored, one file per host id. That is not tidiness: the gate
folds a machine's own measured noise into its threshold, so a baseline
describes one machine and nothing else, and a committed one would be a
standing invitation to compare against numbers from someone else's laptop —
the single mistake this whole track exists to prevent. Record your own:
`just bench-baseline`. A machine without one reports and passes.

The noise term is not caution, it is the difference between a gate and a coin
toss. The first version compared point estimates against a flat 25%; run
twice against **identical code** it reported two regressions and a spread of
−29% to +68%. On an Apple Silicon laptop a 1 µs benchmark bounces between
performance and efficiency cores, and criterion's median absolute deviation
comes out roughly equal to the median.

So the gate prints, per benchmark, the smallest slowdown it can actually
detect there, and a summary line naming the worst. On a quiet CI box that
number is small and the gate is tight; on a busy one it may be over 100%, and
the gate says so instead of implying it is watching.

That range is not hypothetical. These benchmarks were first recorded on a
machine that turned out to be running 32 stray CPU spin loops left behind by
an unrelated session:

| | busy machine | same machine, idle |
|---|---|---|
| `instance/linear-5` | 13 552 ns | **1 867 ns** |
| `condition/eval` | 226 ns | **43 ns** |
| smallest detectable regression | 129% | **33%** |

Seven times, on identical code. Nothing about the engine changed; the
machine did. That is the entire argument for per-machine baselines, for
recording conditions in every result file, and for the paragraph below about
never comparing across hosts — and it is why `uptime` before `just bench` is
a reasonable habit. What it reliably catches
is the class it exists for: an accidental O(n²), an allocation added to the
hot path of `step`, a clone that crept into the advancer. Those are multiples,
not percents.

Recording is **manual** (`just bench-baseline`). A baseline that re-recorded
itself would ratchet a regression in one accepted percent at a time.

---

## Interpreting the numbers

**Against different hardware.** Don't. Compare a scenario against itself on
one machine, or compare scenarios against each other on one machine — that is
what the matrix is for (`parallel-4` against `linear-5-service` is the cost of
the split and join; `exclusive-chain` against `linear-5-service` is the cost
of the gateways). `bench-report` groups tables by host id and refuses to
imply anything else.

**What is not in these numbers, in every scenario:**

- **No network latency.** Handlers are in-process and return immediately; the
  pull-mode workers are in the same process as the engine. A real handler that
  calls a payment API is dominated by that call.
- **No handler work.** The handler returns its merge patch and nothing else,
  so this is engine cost with the business logic set to zero.
- **No multi-node.** One process, one Postgres. The engine is active-active by
  construction, but this suite does not measure it.
- **No history levels.** Every event is written; see above.
- **No think time.** The "user" completes a task the instant it is claimed.

**Cross-engine comparison is invalid.** Do not put these numbers next to
published Camunda or Flowable figures. Different workload shapes, different
hardware, different history settings, different definitions of "instance",
and in most cases a different definition of *done* — several published figures
measure instance creation, which is the cheapest part of a lifecycle. A
comparison that survives all of that would need the other engines run here,
on this machine, on these models, and that is explicitly out of scope. If you
publish a headline number from this suite, publish it with its conditions
attached; the result file carries them so that is easy.

---

## Layout

```
benchmarks/
  README.md        # this file
  compose.yml      # OPTIONAL pinned+tuned Postgres (just bench-compose)
  tuning.sql       # per-table autovacuum settings, applied and hashed per run
  hardware.md      # filled-in template, parsed into every result
  models/          # the .bpmn used here — NOT the tests/fixtures corpus
  scenarios/       # one TOML per benchmark
  results/         # committed result JSON
  .baselines/      # per-machine micro baselines — GITIGNORED, never committed
  src/             # the harness (rbpmn-bench)
  benches/         # the pure-core criterion suite
```

The models live here, not in `crates/rbpmn-model/tests/fixtures/`, on
purpose: the fixture corpus is the *specification*, and it should grow for
semantic reasons rather than performance ones. They are ordinary models
otherwise — `rbpmn-bench check` lints and compiles every one of them against
its manifest with no database and no Docker, which is the fast check that a
benchmark model is still a model this engine would deploy.

## Findings

### The push worker's claim sorts the whole backlog to return one row

This is the big one, and it is on the engine's default execution path.

`worker.rs` claims with `w.topic = any($1)` — it passes the set of topics it
has handlers for. `tasks.rs::get_task` claims with `w.topic = $1`, a single
topic. Same table, same index, same predicates otherwise. Measured against
~87 000 claimable work items, with a **single-element** array in the first
case:

| claim | plan | time |
|---|---|---|
| `topic = any(array[…])` (push worker) | parallel sequential scan + **sort of the entire claimable set** | **~30 ms** |
| `topic = 'literal'` (pull API) | index scan on `rbpmn_work_item_pull`, stops at the first row | **0.18 ms** |

~170×. The cause is that an index on `(topic, created_at, item_no)` scanned
with `topic = ANY(…)` cannot guarantee output globally ordered by
`(created_at, item_no)` — Postgres treats it as several index searches whose
concatenation is unordered — so it cannot use the index to satisfy the
`ORDER BY`, and falls back to scanning and sorting everything claimable in
order to take `LIMIT 1`. Note that the array having one element does not
save it; the plan is chosen for the general case.

The consequence is that the push worker's cost per claim grows with the
**backlog**, not with the work. It is invisible at small backlogs — the whole
rest of this suite never sees it, because a drain of 1000 instances never has
more than a few thousand claimable rows — and it is the dominant cost as soon
as a backlog is large.

**It is why this suite cannot currently build a million-instance population
using the engine's own push worker.** At 100 000 instances the claim was
~30 ms and spilling sort buffers to disk (`IO/BuffileWrite`); extrapolated to
a 900 000-row backlog it is hours. That is a benchmark limitation *and* a
finding: the same shape is what a production system meets after any outage
that lets work accumulate.

Directions, none of them free, all of them design decisions:

- Bind a single topic when there is only one handled topic. Trivial, and it
  fixes the common case completely — but it is a special case, not a fix.
- Claim per topic (a query each, round-robin or first-hit). Loses global FIFO
  *across* topics; the design brief already documents FIFO as
  "fair-but-not-strict" under concurrent consumers, so this may be within the
  contract already — but it is a contract question, not an optimisation.
- Keep one query and drop the cross-topic ordering guarantee. Same question,
  stated more honestly.

### The scheduler's sleep computation was linear in the armed population — fixed

`Engine::next_due_in` computed `min(due_at)` by *joining* `rbpmn_timer` to
`rbpmn_instance` to filter on `status = 'active'`. The join defeats
Postgres's MIN→index transformation, which rewrites `min(col)` into
`order by col limit 1` internally and then requires the resulting path to
**be** an IndexPath; across a join the best path is a nested loop
*containing* an index scan, so the transformation is abandoned and the
aggregate is computed over the whole join.

No index or foreign key was missing — `rbpmn_timer_due`, both primary keys
and the FK were all already there, and the fast plan uses exactly them. The
optimization is refused before indexes are considered.

The fix is to write the transformation by hand: `order by t.due_at limit 1`,
same predicates, same value (`due_at` is NOT NULL). Measured on this
scenario, identical ladder, the only change being that query:

| population | 10 000 | 100 000 | 1 000 000 | growth |
|---|---:|---:|---:|---:|
| `min()` over the join | 2.228 ms | 17.335 ms | **390.955 ms** | 175× |
| `order by … limit 1` | 0.104 ms | 0.123 ms | **0.206 ms** | 2.0× |

**1 898× at a million armed timers**, and the curve is flat. It matters more
than "once per sleep" suggests, because `NOTIFY rbpmn_timer` wakes every
sleeping scheduler whenever any timer is armed, so the old cost was
arm-rate × nodes × 391 ms.

Every predicate was preserved — status filter, deferral exclusion — so the
drain's eligibility rules and the sleep's remain identical, which is the
agreement `scheduler.rs` documents from three separate busy-spin bugs. The
sleep query is now the same query `drain_due_timers` issues, minus the
`due_at <= now()` bound and with `limit 1`. `cargo test -p rbpmn-engine`
(90 tests) and `just tla` (11 configs) both pass unchanged.

The cost is honest rather than free: it is O(k) in timers walked before one
belongs to a live, non-deferred instance — normally 1, but a block of
soonest-due timers all belonging to frozen instances is walked past on every
call. `min()` was O(n) unconditionally.

### Calibration: what run-to-run variance looks like at a million

Worth knowing before reading any single number above. Comparing the two full
ladders — same code except that one query, same machine, same ladder — the
probes that *cannot* be affected by the change still moved:

| probe @ 1M | before | after |
|---|---:|---:|
| `claim_empty` | 1.242 ms | 1.292 ms |
| `count_tasks` | 1.213 ms | 1.263 ms |
| `event_page` | 0.430 ms | 0.469 ms |
| `start_instance` | 1.320 ms | **3.667 ms** |
| `inspect_instance` | 0.938 ms | **2.661 ms** |

Most are within a few percent; two moved by ~2.8×, in the direction that
would look like a regression if you were hunting for one. Nothing in the
rewrite touches instance creation or inspection — that is build-order,
cache-state and autovacuum-timing variance at a 4.5 GB working set. **Read
only large moves as signal**, which is exactly why the headline above is
quoted as three orders of magnitude and not as a percentage.

## Two harness bugs worth knowing about

Both were found by the benchmark measuring itself, and both are the reason
some numbers in `results/` are not comparable with numbers taken before them.

1. **The drain-progress poll was O(database).** It counted this pass's
   instances with `business_key like '<run>:%'` — a sequential scan, because
   `business_key` is nullable, unindexed and non-unique by design — every
   20 ms. A scenario measured 200 instances/sec on a fresh database and 27 on
   one holding a few thousand. It now polls on `(definition_key, status)`,
   which is indexed, and does the exact per-phase accounting once at the end.
2. **The definition key was re-derived per instance**, re-parsing the whole
   BPMN document on every `start`.
3. **The population build went through the push worker**, so it inherited the
   claim-sort problem above and could not reach a million — it was managing
   ~500 completions/sec and spilling sort buffers. The build now completes
   work items **by id**, which runs the identical transactional step and
   only changes how the item is chosen. The build rate reported in a
   population result is therefore not a claim-path number and must not be
   read as one.
4. **A probe claimed work items and never completed them.** The default lease
   is ten minutes, so the next build phase in the population ladder sat
   waiting for leases to expire — it read as a build running at 13
   instances/sec instead of 1500. Probes now complete what they claim, which
   also advances those instances into the cohort; the population is
   re-counted after probing rather than assumed.

Neither was an engine bug. Both are the reason this file says to read the
harness configuration in a result before trusting the result.
