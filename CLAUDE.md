# rbpmn — working notes

Read `bpmn-engine-design.md` fully before changing semantics or the linter;
it records the reasoning behind every major decision (tokens-per-row, no
inclusive gateway, block structure, messages-only interaction, build order).

## Ground rules

- **Loudly reject, never silently reinterpret.** New capabilities land as
  linter rules first, with fixtures, then execution.
- **Rule IDs are stable public API.** Never rename one; add new ones. The
  rules beyond the brief's list are marked ⁺ in README.md's catalogue.
- **Fixtures first.** Every phase starts with fixtures in
  `crates/rbpmn-model/tests/fixtures/{accept,reject}/`. Expected diagnostics
  are embedded in each `.bpmn` as an `expect-diagnostics:` comment; the runner
  is `tests/fixtures.rs`. Execution scenarios (golden event traces) live in
  `crates/rbpmn-core/tests/scenarios/*.json` — the `Display` format of
  `Event` is stable API, like rule IDs.
- `rbpmn-model` **and `rbpmn-core`** stay dependency-light (no IO/async/DB)
  — both must compile to wasm32. model powers the playground and the bpmnlint
  plugin; core powers the editor's L2 check (`check_deployable`). The dsntk
  prohibition below therefore applies to `rbpmn-core` too. That was once
  flagged here as a live collision, because a business-rule task would
  naturally put DMN compile support exactly there; it is **decided**
  (`docs/dmn.md`, D1). `rbpmn-core` defines a `DecisionValidator` trait and
  takes a `&dyn` — `rbpmn-dmn` implements it, and both the engine and the
  editor pass the same implementation. Neither the trait nor the diagnostics
  cost `rbpmn-core` a dependency. Keep new DMN work behind that seam.
- **One verdict, one implementation.** `rbpmn_core::check_deployable(xml,
  bindings, decisions, validator)` is everything `deploy` decides without a
  database: parse, one-process, the DMN artifacts, decision bindings, lint,
  compile-against-manifest, resolved topics. `Engine::deploy` calls it and then
  does the environment link; the editor calls it through WASM and does the link
  against a fetched topic set. Don't grow a second copy of any of those steps —
  `just parity` compares both WASM exports against native over the whole corpus
  precisely so a surface cannot drift into reporting a different verdict than
  deploy will. Note what is *not* in the environment link: decisions travel
  inside the deployment, so `unresolved-decision` is decidable offline while
  `unresolved-topic` is not.
- The engine advances tokens inside the caller's DB transaction; wait states
  are the transaction boundaries. The pure `step` core (rbpmn-core) is
  IO-free and deterministic — keep it that way. Time and decisions both enter
  as **command data**, never from a clock or an evaluator: the core parks at a
  business-rule task and says what it needs, the projection evaluates inside
  the same transaction, and the answer re-enters as
  `Command::CompleteDecision`. That is what lets `chaos.rs` re-derive every
  history through a core that cannot evaluate anything.
  `WaitKind::Decision` is the one wait state that must **not** survive a
  step — persistence refuses to write one, and a freeze takes any pending
  decision with it. A token left parked on one made the engine answer a
  decision on a `Failed` instance and roll back the transaction recording the
  freeze; that was the bug.
- FEEL null semantics in `condition::eval` are FEEL-exact and **verified**, not
  asserted: `just feel-parity` differentials the subset against dsntk. Two
  separate rules — `= null`/`!= null` are the null-check idiom (boolean);
  everything else against a missing value is a type mismatch, so null, `!=`
  included. Don't re-merge those match arms; that was the bug.
- Retention never deletes silently and never deletes what does not grow.
  A record retires **whole** (one age, one transaction) — the two-stage
  version was built, measured and collapsed; don't reintroduce it without
  numbers. The truncation floor is monotonic and keeps the `/v1/events`
  cursor contract honest (`CursorTruncated` → HTTP 410), and the floor and
  the page must be read in **one statement**: two queries leave a window in
  which a sweep commits between them and the page comes back silently short.
  The archive call must stay **outside** every transaction, because
  `pg_snapshot_xmin` is cluster-wide and an open transaction stalls the whole
  event stream. In the policy table a null column means *forever* and a
  missing row means *no override* — never `coalesce` the two; that was the
  bug. Durations are stored as bigint seconds, so "forever" is `None`, never
  a huge `Duration` (`as i64` wraps negative → a cutoff in the future).
- Timer specs parse **literal first**: valid ISO-8601 is a literal, anything
  else is read as a FEEL qualified name from the variable document. Do *not*
  reintroduce `xsi:type="bpmn:tFormalExpression"` as the discriminator —
  bpmn-moddle stamps it on every expression object, so every bpmn-js modeler
  writes it on ordinary literals, and keying off it turns `P3D` into a
  variable named `P3D`. That was the bug. The cost (a mistyped duration
  shaped like a name warns instead of erroring) is paid by the warning text,
  which carries the ISO-8601 complaint that made it fall through.
- **dsntk lives in `rbpmn-dmn` and nothing upstream of it may depend on it.**
  Not `rbpmn-model`, not `rbpmn-core`. Upstream it would kill wasm32 and with
  it the playground, the bpmnlint plugin and the editor. `feel-parity` and
  `feel-number-parity` are outside the workspace for the same reason: the
  latter links the C library, which must never reach this workspace's
  lockfile, and both pull ~170 crates that have no business in it.
- **dsntk comes from the fork, by git rev, and there is no
  `[patch.crates-io]` anywhere.** The two things rbpmn needs from dsntk are
  *features* of `github.com/tpjg/dsntk`, not substitutions:
  - `dsntk-feel-number/use-fastnum` (its default) — a pure-Rust decimal128
    replacing Intel's C library. This is what gets dsntk to wasm32 at all. It
    is **verified, not asserted**: `just number-parity` differentials it
    against the library it replaces (26 300 comparisons, three named deviation
    classes, anything outside them fails) and `just dmn-tck` runs the DMN TCK
    against published dsntk and against the fork, comparing case by case. Both
    are owed by any fork bump, plus a re-read of `docs/dmn.md`'s gates.
  - `dsntk-feel-evaluator/java-bridge`, left **off** — FEEL's external Java
    function bridge, an HTTP POST to a JVM on localhost. **A decision must not
    call out — not to Java, not to anything else.** (Timo's explicit call, and
    the same shape as XML purity: the capability is *removed*, not disabled.)
    `feel-deterministic` refuses the external functions at deploy; the absent
    feature removes the ability at runtime. Do not enable it to fix a compile
    error.
  This replaced two shim crates plus a workspace-global patch, and the reason
  is not tidiness: Cargo honours `[patch]` **only from the workspace root of
  the build being run**, so every application depending on rbpmn had to repeat
  it or get a silently weaker build. A feature travels down the dependency
  graph; a patch does not. A build script in `rbpmn-dmn` used to prove the
  patch was in effect — it is gone with the patch, and `just no-dmn` now
  asserts the same two properties on the resolved tree instead.
  - The rev is written down in four places (`crates/rbpmn-dmn`,
    `feel-number-parity`, `dmn-wasm-probe`, `dmn-tck/run.sh`). **`just
    dsntk-rev` checks they agree**, and `just number-parity` depends on it,
    because a differential run against a rev nobody ships is green and
    meaningless. `feel-parity` is deliberately outside that check — it is
    allowed to lag while a rev is being evaluated.
  - Feature unification cuts both ways here: any crate in the graph asking for
    `java-bridge` restores an HTTP client for everyone. That is what the new
    `just no-dmn` assertions catch, and they were verified by switching it on
    and watching them fail.
- **DMN is on by default, and the off switch must stay real.** `docs/dmn.md`
  D9 records why it flipped: a definition plus its manifest is a fully
  executable flow, and a decision is part of that definition. The feature
  still turns off, and `just no-dmn` is the only thing keeping "optional" a
  fact — it asserts the dependency graph in **both** directions and builds the
  `#[cfg(not(feature = "dmn"))]` arms. Cargo unifies features per package
  across the whole graph, so a single dependency edge that takes the defaults
  switches `dmn` back on for everything; that has now gone wrong twice (a
  self dev-dependency, and a server that did not opt out of the engine's
  defaults), and both times every other check stayed green.
- **One MSRV, 1.94**, declared once in the root manifest and inherited
  everywhere. It is fastnum's floor, reached through the fork's
  `dsntk-feel-number`, and DMN is in the default build — so it is the floor for
  everything. A per-crate split was tried (model/core/wasm holding 1.91 so the
  wasm32 surfaces stayed buildable without dsntk) and **removed**: it only ever
  helped a downstream consumer of those crates standalone, and the machinery to
  keep it honest — a second toolchain, its own recipe, its own CI task — cost
  more than that was worth. Don't reintroduce it without such a consumer.
- **Declared indexes carry a scope, and nothing ever drops one.**
  `definition` (partial on `definition_key`, the default, byte-identical to
  what shipped before) or `shared` (one index per field across definitions,
  partial on `(variables->>'f') is not null`, named from the field alone so N
  definitions converge on one index). A shared declaration asserts the field
  means the same thing everywhere — rbpmn cannot check that and does not
  pretend to; a cross-definition scope conflict is a `tracing::warn!`, never a
  `Diagnostic`, because it is an operator fact about *other* definitions that
  no offline surface can reproduce. Nothing drops a declared index of either
  scope — not `delete_definition`, not retention, not dropping the field from
  a manifest — deliberately, because a shared index belongs to no definition;
  `declared_indexes()` is the read-only audit that keeps orphans visible.
  **Index builds are serialized by a `pg_try_advisory_lock` poll, keyed by the
  *table*.** Do not "simplify" it to a blocking lock: `CREATE INDEX
  CONCURRENTLY` waits for every concurrent snapshot, and a session blocked on
  a lock is a session holding one — both the blocking-lock and the no-lock
  versions were reproduced as real Postgres deadlocks, and the no-lock one is
  a bug that predates scopes (two definitions deploying at once both index
  `rbpmn_instance`). The session must be **idle** between attempts; that is
  what lets the holder's build drain.
- **The published views (`rbpmn_v_definition`,
  `rbpmn_v_definition_decision`, `rbpmn_v_instance`, `rbpmn_v_work_item`,
  `rbpmn_v_timer`, `rbpmn_v_subscription` — one per wait state, plus the
  instance and what it was deployed from) are public API and must stay plain
  inlinable projections.** A 0..N artifact set does not get folded in with an
  aggregate; it gets a projection of its own (`rbpmn_v_definition_decision`)
  or stays a documented query (subscription ambiguity), because an aggregate
  stops the view being inlinable. No WHERE, no volatile
  function, and above all not `security_barrier` — a barrier view refuses to push `variables->>'f' = $1`
  below itself (`jsonb ->>` is not leakproof), which would strand every
  declared index beneath a full scan. Columns may be added, never removed or
  repurposed. It is not a tenancy boundary and does no row filtering: an
  application's own predicate belongs in the application's own query, which is
  the whole reason the surface is SQL rather than an API. `find_by_shared_index`
  is the no-SQL convenience beside it and is explicitly not a search
  primitive — its limit lands before any caller-side filter. `queue_depths`
  deliberately does not repeat that shape: its key set is a bound argument and
  it has no limit at all, and timers get no typed call at all.
- **`rbpmn_v_timer` publishes what is ARMED, and carries no `overdue`
  column.** Deliberate, and the asymmetry with `claimable` is the reasoning:
  `claimable` encodes a rule a caller re-deriving would get wrong, `overdue`
  would be `due_at < now()` and nothing else — and a boolean cannot express
  the range queries ("due in the next hour") the raw column can, off the same
  index. `instance_status` **is** a column, because separating "the scheduler
  is behind" from "the instance is frozen" is the operational question and
  should not cost a second join. A `due_at` in the past means due-and-not-yet
  fired, never late-by-definition; and no view can see a node's in-process
  deferral set, so none of them claims to say what fires next.
- **Ask a view for the soonest deadline with `order by due_at limit 1`, never
  `min(due_at)`.** The aggregate-to-index-scan rewrite is refused across a
  join, before indexes are considered, so `min()` plans a hash join over two
  sequential scans — measured, 6 buffers against 733. This is the same finding
  `Engine::next_due_in` records for the scheduler's own query; it survives the
  view, and `Engine::NEXT_DEADLINE_SQL` writes the right shape out.
- **`rbpmn_v_subscription` is searched by `correlation_key` alone, and the
  correlate index cannot serve that.** `rbpmn_subscription_correlate` is
  `(message_name, correlation_key)`; a predicate on the second column has no
  leading equality. Migration 0017 adds `rbpmn_subscription_by_key`. Do not
  delete it as redundant because a local EXPLAIN shows skip scan picking up the
  correlate index anyway. Two reasons, both needed: skip scan is PostgreSQL 18
  and development here runs 18 while CI runs 15 and the claimed floor is 13 —
  so below 18 there is no index path for a key-only predicate at all; and even
  on 18 it seeks once per distinct message name, so the cost scales with the
  deployment's model portfolio. Measured on 60 000 subscriptions: 24 buffers
  at 4 message names, 394 at 400, against 3 through the explicit index either
  way. Ambiguity (the 409) is deliberately a documented query
  and not a column: it needs an aggregate, and an aggregate would stop the
  view being inlinable.
- **`rbpmn_v_work_item.claimable` is the claim predicate, not a guess at it.**
  `CLAIMABLE` in `lib.rs` is the one source, and it is written **total**
  (`lock_until is not null`) rather than merely correct-in-a-WHERE, because
  the view *projects* it: a three-valued boolean would put an item in neither
  bucket of a dashboard that thinks it split the world in two. Do **not**
  "fix" that by projecting `coalesce(…, false)` — measured, COALESCE is opaque
  to the planner's predicate prover, `state in ('available','locked')` can no
  longer be proved, and the depth query becomes a parallel sequential scan of
  every work item in the system. Migration 0015 carries the same text because
  a migration cannot read a Rust const; they are held together by
  `the_view_and_the_claim_predicate_cannot_drift`, a behavioural differential
  over a corpus with every state, lease and backoff in it — not a textual
  comparison. `claimable` needs `i.status = 'active'`, which is why the view
  joins instances: an instance frozen on an incident keeps its work items and
  none of them may be handed out.
- The UI documents inline business data into HTML, which makes **escaping our
  problem** — one mistake ships to every embedder at once. The rule:
  `escape_json_for_html` for the data block, `textContent` (never
  `innerHTML`) everywhere in the JS. Escaping is not sanitization and rbpmn
  does none: the inspector shows the whole variable document by design, and
  who may see it is the application's call. The hostile-payload corpus is in
  `crates/rbpmn-ui/tests/documents.rs`.
- **Diagram export is always the light palette, and never restyles the live
  canvas.** bpmn-js bakes stroke/fill as SVG *attributes* at construction
  (`ui/src/shared/theme.js` says why CSS cannot reach them), so exporting the
  visible canvas exports its theme — and a dark export is near-invisible on
  paper, which is the one job the button has. `svg-export.js` renders through
  a second, detached viewer with `LIGHT_DIAGRAM`. Do **not** "simplify" it to
  flipping the live canvas: that means re-constructing the modeler, which
  `remountForTheme`'s own comment records as costing the undo history — fair
  once when the OS flips at sunset, not on every export. The detached host
  must be off-screen but **laid out**, never `display: none`: `saveSVG`
  measures with `getBBox()`, which reports zeros on an unrendered SVG and
  yields an empty-looking file. The print stylesheets are the other half and
  fix a different problem (chrome eating the page); they cannot fix the dark
  palette, and say so.
- The inspector is **read-only, forever**. No retry, no cancel, no variable
  edit, no migration, no lists, no search. Every one of those is a designed
  API first; a UI is never the reason one ships early. The pressure to add a
  button will come from the element pane — that is the spot to hold.
- **Benchmarks are a separate track and never gate on absolute numbers**
  (`benchmarks/`). Two rules with teeth. (a) A feature must not land because a
  benchmark axis wanted it: the three-history-level matrix is *wired and
  loudly refused* because per-definition event-kind filtering is a roadmap
  item, and shipping it here would invert "linter rules first, with fixtures".
  (b) The only gate is the IO-free core suite, against a baseline recorded on
  **that same machine**, with that machine's measured noise added to the
  threshold — a flat threshold fired twice on identical code before the noise
  term existed. Those baselines live in gitignored `benchmarks/.baselines/`
  and are **never committed**: a baseline describes one machine, and a
  committed one invites comparing against another's. `benchmarks/results/` is
  gitignored for the same reason — every file is stamped with the machine
  that produced it, so a committed result is one laptop's numbers presented
  as the project's. Both are the only copy there is; `just cleanup`
  deliberately removes neither. Also load-bearing: `just bench` starts from an empty database
  and runs `ANALYZE` before measuring, because without it the claim path's
  plan flips (measured 20 vs 175 instances/sec) and the suite measures when
  autovacuum last ran instead of the engine.

## Commands

`.github/workflows/ci.yml` is GitHub Actions CI: one step per command below,
in the order a developer would run them, split across two jobs by what setup
they need (`build` = Postgres + browser, `differential` = C toolchain).
`just ui` is a step of its own and must stay first — everything that builds
`rbpmn-ui` needs it. Two deliberate omissions:
benchmarks (`just lint` compiles the harness via `--workspace`, so it cannot
rot, but nothing runs it) and `just dmn-tck` (it fetches the TCK, dsntk's
source and a third-party runner from the network — the gate for a dsntk bump,
not a per-commit check). CI is the backstop; the point of the table in README's
"Developing" is knowing which command a change owes *before* pushing.

- `cargo test` — everything including the fixture corpus. The rbpmn-engine
  integration tests need a reachable local Postgres (they create and drop
  throwaway `rbpmn_test_*` databases; override via `RBPMN_TEST_ADMIN_URL`).
- `just lint` — clippy `-D warnings` + fmt check (keep it at zero warnings).
- `just serve` — run the HTTP server with a throwaway token (needs a local
  Postgres; provisions the rbpmn_dev database).
- `just playground` — linter playground (builds WASM, needs node + wasm-pack).
- `just e2e` — browser end-to-end with screenshots into `e2e/screenshots/`
  (gitignored); runs the full inspection stack when Postgres is reachable.
- `just parity` — MUST stay green: byte-parity of native Rust vs WASM over the
  corpus for **both** exports (`lint` and `check_deployable`), plus the
  bpmnlint plugin's pipeline test. Both sides are built `--features dmn`
  explicitly even though it is the default, so they cannot become different
  builds comparing different validators.
- `just no-dmn` — the DMN seam, built and asserted in both directions. See the
  ground rule above; this is not optional paperwork, it is the check that
  stops "optional" rotting into a claim.
- `just number-parity` — the fork's `use-fastnum` decimal against the C
  library it replaces, both as `dsntk-feel-number` from two different sources.
  Runs `just dsntk-rev` first. Lives outside the workspace and cannot be
  reached from it, so this stays owed by any fork bump. (The upstream
  acceptance corpus now travels *with* the fork: `cargo test` inside it runs
  191 assertions on both backends.)
- `just dsntk-rev` — the four places the fork's rev is written down all name
  the same one. Cheap, and a dependency of `number-parity` rather than a
  chore, because a differential against a rev nobody ships proves nothing.
- `just dmn-test` — `rbpmn-dmn`'s own tests. `cargo test` runs these too now
  that it is a default member; this is the fast loop while working in it.
- `just dmn-tck` — the DMN TCK twice, against dsntk 0.3.0 as published and
  against the fork rbpmn ships, compared case by case. The gate for a fork
  bump. Fetches third-party source; not on CI. Note the comparison now carries
  two variables rather than one (the decimal *and* 0.3.0 → 0.3.1-dev): a
  `[patch]` can no longer express the swap, because the fork's version does not
  satisfy the `^0.3.0` its own siblings request — which the recipe's own
  assertions caught rather than hid.
- `just dmn-wasm-probe` — Gate 0b: the whole DMN stack compiling *and
  evaluating* inside a real WebAssembly VM, built through wasm-pack exactly as
  `rbpmn-wasm` is.
- `just feel-parity` — the FEEL subset differentialled against dsntk over ~8k
  expression/document pairs. Outside the workspace (it links the C library).
- `just ui` — build the two UI documents into `crates/rbpmn-ui/assets/`.
  **Bootstrap step: run it once after cloning**, or `cargo build` fails with a
  build.rs message telling you to. The bundles are compile output and are
  gitignored like every other artifact here (`playground/src/wasm/`,
  `bpmnlint-plugin-rbpmn/wasm/`) — committing build results was tried and
  reverted; don't reintroduce it. The editor embeds the linter compiled from
  rbpmn-{model,core,wasm}, so **touching a rule owes a `just ui`** or the
  document you serve carries a stale validator. Nothing checks that for you.
- `just ui-test` — the UI's pure modules under node, no browser, no build
  artifacts. `just e2e-ui` drives both documents in a real browser: from
  `file://`, and then — when Postgres is up — against a real server behind an
  auth-injecting proxy. Both halves earn their keep, and the served one
  skips itself when Postgres or its ports are unavailable: right for a
  developer, wrong for CI, which sets `RBPMN_E2E_REQUIRE_SERVED=1` to turn
  that skip into a failure. It went green on the `file://` half alone once
  already. The CSP is only actually
  enforced in a browser, and the served half is the only place the editor's
  own fetch happens: it caught a `connect-src 'none'` policy that blocked the
  editor's own button, and a URL that resolved one path segment short.
- `just demo` — the same served stack, left running with two clickable links
  and an instance deliberately frozen on an incident. The proxy is part of the
  demonstration: UI routes are behind the bearer, browsers cannot send it on a
  navigation, and supplying it is the embedding application's job.
- `just tla` — TLA+ model checking of the concurrency protocol (`spec/`).
  Nineteen configs; twelve are *expected* to fail, each matched against the
  specific violation it demonstrates (a spec that stops parsing must not read
  as "fails as expected"). Three reproduce bugs that were real. The lock order
  is checked at two arities. `Lease.tla` models the process withdrawing a
  leased item (`Cancel`) and `BoundaryExit.tla` the correlate-vs-complete race
  on one token; both came with message boundaries, and `Lease`'s old
  "only by its holder" property turned out never to have been true of the
  shipped engine — a model with no action for an actor proves nothing about
  it. Needs java; the jar is pinned and
  checksum-verified.
- `just fixtures-di` — fixtures carry baked-in BPMN DI so they render
  everywhere; new fixtures without a `bpmndi:BPMNDiagram` section get theirs
  from this (idempotent; two reject fixtures have hand-written DI — see the
  comments in them).
- `just cleanup` — **destructive**: drops every `rbpmn_*` database (including
  the `rbpmn_test_*` throwaways a panicked integration test leaves behind for
  inspection) and removes all build output. `just ui` is required afterwards.
  Keeps `benchmarks/.baselines/` — machine-local, and deleting it silently
  disarms the micro gate.
- `just bench` — the benchmark suite (`benchmarks/README.md`). Needs the local
  Postgres, **not** Docker; `just bench-compose` is the opt-in pinned-server
  variant. `just bench-micro` is the pure-core criterion suite plus the only
  benchmark permitted to fail a build; `just bench-baseline` re-records that
  machine's baseline; `just bench-report` renders `results/*.json`.
  `rbpmn-bench` is a workspace member but **not a default member**, so
  `cargo test` never builds it — `just lint` uses `--workspace` precisely so
  it is still linted.

## The specs are hand-written and will not tell you they drifted

`spec/` models the concurrency protocol: the lock order, the work-item lease,
and the scheduler's timer claim racing scope teardown. The corpus-driven
tests adapt to a new phase by themselves; **a hand-written model does not**,
and nothing fails when it goes stale. Phase 6 proved that — `spec/` was not
touched, and the conclusion that scopes changed nothing was an argument, not
a check. **DMN proved it again**: the claim path grew an in-transaction
decision evaluation, `just tla` was run and stayed green, and `spec/` was not
re-read until a review asked. (The answer, once read: no new lock enters the
order, but the claim transaction can now roll back *after* its re-check
passed, which `TimerTeardown.tla`'s `Abort` action now models. Running the
checker is not the same as re-reading the model.)

So: **touching any of these means re-reading `spec/` and re-running
`just tla`**, not just keeping `cargo test` green.

- lock acquisition order anywhere (`runtime.rs`, `scheduler.rs`, `tasks.rs`)
- the work-item lease: TTL, renewal, ownership, the lease epoch a claim
  mints (only a claim mints one — a renewal continues the same lease), the
  voluntary hand-back (`release_task`), the `guard_lease` predicate
- the scheduler's claim path (`try_fire`) — pick, NOWAIT, re-check, and the
  in-transaction decision evaluation that can now abort a claim already made
- `correlate_in_tx`'s resolve → lock → re-check (`runtime.rs`): the re-check
  confirms *this* subscription row is still in the rehydrated state, never
  "some open subscription" (`spec/BoundaryExit.tla`, `LateCallsAreTyped` —
  `BoundaryExit_AnyRowRecheck.cfg` is the loosened form failing), and like the
  timer claim it sees the row, never its token (`SubscriptionTeardown.cfg`)
- what scope teardown reaps (`step.rs::tear_down_scope`) — specifically that
  a reaped token's arms are withdrawn *with* it
- retention's plan/archive/execute split, the DUE re-check under the row
  lock, or how the truncation floor is advanced (`retention.rs`)

Each of those sites carries a comment naming the spec and the property. Two
distinctions the models make that prose kept blurring, worth knowing before
editing: deadlock freedom comes from the lock **order**, not from `NOWAIT`
(which buys throughput); and the scheduler's re-check confirms a timer *row*
survived, never that its *token* did — that half is teardown's invariant.

## Conventions

- **XML purity is a principle (Timo's explicit call — resist eroding it).**
  BPMN files are 100% standard-namespace: no rbpmn extension attributes, no
  vendor attributes, ever. It is always tempting to "just add a hint in the
  XML" — don't. Wiring lives in code at registration time — one `Bindings`
  value (`topic`, `correlation`, `decision`, `index`/`shared_index`), plus
  `declare_topic` and `declare_index` for the *environment* half — and is
  validated at deploy like a compiler validates types: fail early, never
  "seems to run". This held
  through DMN, which was the strongest pull yet: a business-rule task's
  decision name and result path are a `Bindings::decision` call, not an
  attribute, and the DMN artifacts travel in the deploy bundle beside the
  BPMN rather than being referenced from inside it.
- Conditions are a **strict FEEL subset** — identical syntax and semantics to
  the full language, which is why every condition written before dsntk landed
  is still valid now that it has. `condition::eval` remains rbpmn's own: it
  runs in `rbpmn-model`, which must reach wasm32 and must not depend on dsntk.
  The two are kept honest against each other by `just feel-parity`. Correlation
  keys use FEEL qualified names too (`order.id`), registered — not in the XML.
- Server config is env-only (`RBPMN_BIND`, `RBPMN_API_TOKEN[_FILE]`,
  `RBPMN_ALLOW_NON_LOOPBACK`); secrets never come from CLI args.
  Security posture: `docs/http-security.md`.
