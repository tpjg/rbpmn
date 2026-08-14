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
- `rbpmn-model` **and now `rbpmn-core`** stay dependency-light (no IO/async/DB)
  — both must compile to wasm32. model powers the playground and the bpmnlint
  plugin; core powers the editor's L2 check (`check_deployable`). This makes
  the dsntk prohibition below apply to `rbpmn-core` too, which is a live
  collision: the post-v1 business-rule task would naturally put DMN compile
  support exactly there. Decide that before the spike, not during.
- **One verdict, one implementation.** `rbpmn_core::check_deployable` is
  everything `deploy` decides without a database (parse, one-process, lint,
  compile-against-manifest, resolved topics); `Engine::deploy` calls it and
  then does the environment link, and the editor calls it through WASM and
  does the link against a fetched topic set. Don't grow a second copy of any
  of those steps — `just parity` compares both WASM exports against native
  over the whole corpus precisely so a surface cannot drift into reporting a
  different verdict than deploy will.
- The engine advances tokens inside the caller's DB transaction; wait states
  are the transaction boundaries. The pure `step` core (rbpmn-core) is
  IO-free and deterministic — keep it that way; time enters as command data
  when timers land (phase 3), never from a clock.
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
- dsntk must never become a dependency of `rbpmn-model` **or `rbpmn-core`**:
  its number crate binds a C library (`dfp-number-sys`), which kills wasm32
  and with it the playground, the bpmnlint plugin and the editor.
  `feel-parity` is outside the workspace for this reason.
- The UI documents inline business data into HTML, which makes **escaping our
  problem** — one mistake ships to every embedder at once. The rule:
  `escape_json_for_html` for the data block, `textContent` (never
  `innerHTML`) everywhere in the JS. Escaping is not sanitization and rbpmn
  does none: the inspector shows the whole variable document by design, and
  who may see it is the application's call. The hostile-payload corpus is in
  `crates/rbpmn-ui/tests/documents.rs`.
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

`.build.yml` is sourcehut CI: one task per command below, in the order a
developer would run them. `just ui` is a task of its own and must stay first
— everything that builds `rbpmn-ui` needs it. Benchmarks are **not** on CI by
design (`just lint` compiles the harness via `--workspace`, so it cannot rot,
but nothing runs it). CI is the backstop; the point of the table in README's
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
  bpmnlint plugin's pipeline test.
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
  Eleven configs; six are *expected* to fail, each matched against the
  specific violation it demonstrates (a spec that stops parsing must not read
  as "fails as expected"). Two reproduce bugs that were real. The lock order
  is checked at two arities. Needs java; the jar is pinned and
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
a check.

So: **touching any of these means re-reading `spec/` and re-running
`just tla`**, not just keeping `cargo test` green.

- lock acquisition order anywhere (`runtime.rs`, `scheduler.rs`, `tasks.rs`)
- the work-item lease: TTL, renewal, ownership, the `guard_lease` predicate
- the scheduler's claim path (`try_fire`) — pick, NOWAIT, re-check
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
  XML" — don't. Wiring lives in code at registration time (`map_topic`,
  `map_correlation`, `declare_topic`, `declare_index`) and is validated at
  deploy like a compiler validates types: fail early, never "seems to run".
- Conditions are a **strict FEEL subset** (identical syntax and semantics, so
  they stay valid when dsntk lands post-v1). Correlation keys use FEEL
  qualified names too (`order.id`), registered — not in the XML.
- Server config is env-only (`RBPMN_BIND`, `RBPMN_API_TOKEN[_FILE]`,
  `RBPMN_ALLOW_NON_LOOPBACK`); secrets never come from CLI args.
  Security posture: `docs/http-security.md`.
