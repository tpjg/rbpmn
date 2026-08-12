# rbpmn — working notes

Read `bpmn-engine-design.md` fully before changing semantics or the linter;
it records the reasoning behind every major decision (tokens-per-row, no
inclusive gateway, block structure, messages-only interaction, build order).

## Ground rules

- **Loudly reject, never silently reinterpret.** New capabilities land as
  linter rules first, with fixtures, then execution.
- **Rule IDs are stable public API.** Never rename one; add new ones. The four
  rules beyond the brief's list are documented in README.md.
- **Fixtures first.** Every phase starts with fixtures in
  `crates/rbpmn-model/tests/fixtures/{accept,reject}/`. Expected diagnostics
  are embedded in each `.bpmn` as an `expect-diagnostics:` comment; the runner
  is `tests/fixtures.rs`. Execution scenarios (golden event traces) live in
  `crates/rbpmn-core/tests/scenarios/*.json` — the `Display` format of
  `Event` is stable API, like rule IDs.
- `rbpmn-model` stays dependency-light (no IO/async/DB) — it must compile to
  WASM for the phase 0-B playground and the bpmnlint plugin.
- The engine advances tokens inside the caller's DB transaction; wait states
  are the transaction boundaries. The pure `step` core (rbpmn-core) is
  IO-free and deterministic — keep it that way; time enters as command data
  when timers land (phase 3), never from a clock.
- FEEL null semantics in `condition::eval` are FEEL-exact and **verified**, not
  asserted: `just feel-parity` differentials the subset against dsntk. Two
  separate rules — `= null`/`!= null` are the null-check idiom (boolean);
  everything else against a missing value is a type mismatch, so null, `!=`
  included. Don't re-merge those match arms; that was the bug.
- dsntk must never become a dependency of `rbpmn-model`: its number crate
  binds a C library (`dfp-number-sys`), which kills wasm32 and the whole
  playground/bpmnlint chain. `feel-parity` is outside the workspace for this
  reason.

## Commands

- `cargo test` — everything including the fixture corpus. The rbpmn-engine
  integration tests need a reachable local Postgres (they create and drop
  throwaway `rbpmn_test_*` databases; override via `RBPMN_TEST_ADMIN_URL`).
- `just lint` — clippy `-D warnings` + fmt check (keep it at zero warnings).
- `just serve` — run the HTTP server with a throwaway token (needs a local
  Postgres; provisions the rbpmn_dev database).
- `just playground` — linter playground (builds WASM, needs node + wasm-pack).
- `just e2e` — browser end-to-end with screenshots into `e2e/screenshots/`
  (gitignored); runs the full inspection stack when Postgres is reachable.
- `just parity` — MUST stay green: byte-parity of native Rust vs WASM lint
  output over the corpus, plus the bpmnlint plugin's pipeline test.
- `just tla` — TLA+ model checking of the concurrency protocol (`spec/`).
  Six configs; three are *expected* to fail — they are the counterexamples
  that prove the checks have teeth, and two of them reproduce bugs that were
  real. Needs java; the jar is pinned and checksum-verified.
- `just fixtures-di` — fixtures carry baked-in BPMN DI so they render
  everywhere; new fixtures without a `bpmndi:BPMNDiagram` section get theirs
  from this (idempotent; two reject fixtures have hand-written DI — see the
  comments in them).

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
