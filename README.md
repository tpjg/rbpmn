# rbpmn

A **correctness-first BPMN execution engine** in Rust on PostgreSQL. Embeddable
as a library; a small optional HTTP server for everyone else. A deliberately
**restricted subset of BPMN 2.0**, enforced at deploy time: *it is better to
loudly reject a model than to silently execute it wrong.*

The full rationale — why tokens-per-row, why no inclusive gateway, why no call
activities, why block structure — lives in [bpmn-engine-design.md](bpmn-engine-design.md).
Read it before touching the semantics.

## Workspace

| Crate / package | Purpose |
|---|---|
| `crates/rbpmn-model` | BPMN XML → internal model + the linter + the FEEL-subset parser/evaluator. Dependency-light (no IO, no async, no DB) so it compiles to WASM for the linter playground and the bpmnlint plugin. |
| `crates/rbpmn-core` | The pure semantic core: `compile` → executable model, tokens, and the `step` function. No IO — the Postgres layer projects it. |
| `crates/rbpmn-engine` | The PostgreSQL projection: transactional stepping over the core (with `*_in_tx` variants sharing the caller's transaction — process transitions commit atomically with business writes), atomic idempotent deploys, the growing persistent environment, leases, retries with backoff, incidents. |
| `crates/rbpmn-wasm` | Thin wasm-bindgen surface over rbpmn-model: `lint(xml) -> JSON`, `catalogue()`. |
| `crates/rbpmn-server` | Small standalone HTTP server wrapping the engine. Bearer-token auth, loopback-only by default. See [docs/http-security.md](docs/http-security.md). |
| `playground/` | Local linter playground (bpmn-js + WASM): fixture browser, live re-lint, diagnostics as diagram overlays. `just playground`. |
| `bpmnlint-plugin-rbpmn/` | bpmnlint plugin backed by the same WASM — rbpmn's rules inside bpmn-io tooling, zero JS reimplementation. |

Planned (per the design brief's build order): rounding out (phase 5).
Cross-definition messaging (message start/throw routing between
definitions) is designed after that; until then message start/throw events
lint clean but refuse to compile.

## Status

- [x] **Phase 0 — Parse & reject**: parser, full linter rule catalogue,
      fixture corpus (19 accept / 34 reject today — every phase adds its
      own before its code), corpus runner.
- [x] Server skeleton with the security spine and `POST /v1/definitions/lint`.
- [x] **Phase 0-B — linter playground (WASM) + bpmnlint plugin**, with a
      byte-parity check between native Rust and WASM over the whole corpus.
- [x] **Phase 1 — pure semantic core**: `compile` (re-lints + gates
      later-phase elements), tokens, the deterministic `step` function with
      golden event traces, FEEL-exact condition evaluation, RFC 7386 merge
      patch; scenario corpus + property tests (interleaving confluence,
      exactly-once joins, terminate cleanliness).
- [x] **Phase 2, milestone 1 — PostgreSQL projection**: schema, transactional
      stepping (instance-row locking; join exactly-once verified under real
      concurrent completions), atomic content-idempotent deploys with the
      bindings manifest, the monotonically growing environment with
      `unresolved-topic` enforcement + startup re-validation, retries →
      incident. Integration tests run against a local Postgres.
- [x] **Phase 2, milestone 2** — worker loop (SKIP LOCKED leases +
      LISTEN/NOTIFY), HttpPostHandler, error boundaries (matched → boundary
      path, unmatched → incident), the server's full engine API
      (deploy/start/complete/fail/topics/inspect), and the playground's
      live token-overlay instance inspection (exit criterion, verified
      end-to-end in a browser).
- [x] **Phase 3 — timers & messages**: timer intermediate catch (duration +
      date, years-long sleeps as passive rows), interrupting timer boundaries,
      message catch/receive task with registered correlations
      (`Bindings::correlation`, FEEL qualified names — never in the XML),
      the event-based gateway race, `correlate()` (+ `POST /v1/messages`:
      exactly-one delivery, loud 404/409), and the deadlock-free scheduler
      (instance-lock-first claim, `min(due_at)` sleep, `NOTIFY rbpmn_timer`,
      poll fallback; exactly-once verified under competing schedulers).
- [x] **Phase 4 — user tasks & the task API**: pull-mode `get_task` (FIFO
      default / LIFO opt-in, `SKIP LOCKED`, renewable leases — expired locks
      return without a reaper), `extend_lock` with the typed lock-lost
      result, owner-checked `complete_task`/`fail_task`, equality filters
      over the instance's **live** variables, `count_tasks`, and
      `declare_index` (partial expression indexes, also declarable in the
      deploy manifest; index usage verified by test against the real query
      path). Server: `POST /v1/tasks/{get,count}` and
      `/v1/tasks/{id}/{extend,complete,fail}`.
- [ ] Phase 5 — rounding out.

## Rule catalogue

Rule IDs are **stable public API** — fixtures, the playground and the bpmnlint
plugin all assert on them. Rules marked ⁺ are structural prerequisites added
beyond the design brief's initial list (the region analysis is only sound on
graphs that pass them).

| Rule | Severity | Meaning |
|---|---|---|
| `no-inclusive-gateway` | error | Inclusive gateways rejected entirely; rewrite as parallel split + exclusive skip-bypass per branch. |
| `no-call-activity` | error | Definitions are islands; interact via message throw → message start/catch with correlation keys. |
| `no-unsupported-element` | error | Anything outside the supported subset (script/send/manual/business-rule tasks, signals, cycles, multi-instance, …). |
| `balanced-gateways` | error | Every parallel split has a matching join; branches stay disjoint; nothing enters/escapes the region; each branch delivers exactly one token; no plain end events inside (terminate allowed). |
| `single-start-event` | error | Exactly one start event per process and per subprocess. |
| `conditions-feel-subset` | error | Conditions only on exclusive-split flows, in the strict FEEL subset (`name op literal`, `and`/`or`, parentheses); default flow required. Full-FEEL constructs (functions, arithmetic, ranges) are rejected. |
| `timer-iso8601` | error | Timer definitions must be valid ISO-8601; dates require an explicit UTC offset; component magnitudes bounded. |
| `message-has-correlation` | error | Message start/catch/throw must reference a *named* message. The correlation binding itself (a FEEL qualified name) is registered via `map_correlation` and checked at deploy. |
| `no-foreign-implementation` | warn | Service task carries vendor attributes (`camunda:`, `zeebe:`, …), which rbpmn ignores — topics are bound at registration. |
| `unresolved-topic` | error | Every service task's topic (via `map_topic`, default: element id) must have a registered handler or a declared external-worker topic. Checked at deploy against registration state — ID reserved now, enforced from phase 2. |
| `boundary-on-supported-host` | error | Boundary events only on service/user/receive tasks and subprocesses; error boundaries only where errors can originate. |
| `no-implicit-split` | error | Activities have at most one outgoing flow; splitting happens at explicit gateways. |
| `implicit-merge-after-parallel` | warn | Implicit merge receiving concurrent tokens — the "task runs twice" trap (accompanies the balanced-gateways error). |
| `bpmn-structure` ⁺ | error | Well-formedness: resolvable refs, flow cardinalities, connectivity, unique ids, error definitions. |
| `no-mixed-gateway` ⁺ | error | A gateway either splits or joins, never both. |
| `event-gateway-structure` ⁺ | error | Event gateway races ≥2 message/timer catches or receive tasks, each with exactly one incoming flow and no boundary events (the gateway itself is the race). |

### The structural rules have counterexamples, not just rationales

`cargo test -p rbpmn-core --test mutation` runs the rejected fixtures with the
lint gate off and records what actually breaks. `cross-branch-merge` produces
the Camunda-lineage bug in this engine, on demand:

```
StepError::Invariant: second token arrived at join 'pj' via flow 'f7'
  — the linter's block structure guarantee is broken
```

Others deadlock; two (`entry-into-region`, `parallel-missing-join`) execute
cleanly — block structure is a *sufficient* condition for local join counting,
so not every violation manifests. The same file mutation-fuzzes generated
models: ~99% of structural mutations are rejected, and no lint-clean mutant
has yet executed wrongly. Details in `docs/stress-testing.md` §3.

## XML purity: nothing rbpmn-specific in the BPMN

BPMN files stay **100% standard-namespace** — no rbpmn extension attributes,
no vendor attributes, ever. Anything that wires a model to its runtime is
**registered in code** and **validated at deploy**, which gives the same
class of guarantee a compiler does: a wiring gap fails loudly at deploy
instead of "seeming to run" with stuck tokens.

| Wiring | Registration API | Deploy check |
|---|---|---|
| Service-task topic | `map_topic(definition_key, element_id, topic)`; default topic = element id; `declare_topic(name)` announces pull-mode workers | `unresolved-topic` |
| Message correlation | `map_correlation(definition_key, element_id, "order.id")` — FEEL qualified name into the instance variables | `message-has-correlation` |
| Filterable fields | `declare_index(definition_key, field)` — optional, performance only | — |

Per-definition wiring deploys **atomically with the definition** as a small
JSON bindings manifest (`deploy(bpmn_xml, bindings)` in the library, one
`POST /v1/definitions` body on the server) and is versioned with it — the same
information other engines smear into vendor XML annotations, separated cleanly
and reviewable in git next to the `.bpmn`. Environment capabilities (handler
targets, `declare_topic`) are engine/server configuration, not manifest content.

Conditions inside the XML are pure FEEL (a strict subset), so they carry no
rbpmn-specific syntax either. Null follows FEEL exactly: `x = null` is the
"is it set?" test, while `x = 1` **and** `x != 1` are both false when `x` is
missing — a type mismatch is null in either direction, so an unset variable
never satisfies a negative condition. `just feel-parity` differentials the
whole subset against dsntk (DMN-TCK-verified) to keep that honest.

## Fixtures

`crates/rbpmn-model/tests/fixtures/{accept,reject}/*.bpmn`. Every fixture
embeds its expected diagnostics in a leading comment:

```xml
<!-- expect-diagnostics:
  error no-inclusive-gateway @ gateway_1
-->
```

One runner (`tests/fixtures.rs`) executes the corpus and compares exact
(severity, rule, element) sets. Start every phase by writing its fixtures
first.

## Playground & bpmnlint plugin

`just playground` opens a local page (no server-side component) with the whole
fixture corpus: accepted fixtures render clean, rejected ones show the rule
that kills them as badges on the offending elements, with a clickable
diagnostics panel. Editing the XML re-lints live through the **same WASM
build of `rbpmn-model` that deploy uses** — no JS reimplementation of any
rule. Hand-typed models without DI get an automatic layout, making the
playground the fastest way to author new fixtures (`just fixtures-di` bakes
the layout in).

**Inspecting a live instance:** run the API (`just serve`, terminal 1 — prints
the token) and the playground (`just playground`, terminal 2 →
http://localhost:5173). Paste the token once and an instance id into the
Inspect panel — or open `http://localhost:5173/#instance=<uuid>` directly
(the token is remembered per browser session, never in the URL). Tokens and
open/failed work items render as badges on the deployed diagram; the event
trace lists alongside. The playground proxies `/rbpmn` to the server, which
therefore never needs CORS.

`just e2e` drives all of this headlessly and drops screenshots of **every
fixture plus the inspection views** into `e2e/screenshots/` (gitignored) —
needs python3 with playwright installed.

`just parity` is the guarantee the playground never lies: it lints every
fixture through native Rust and through the WASM build and requires
byte-identical output, then runs the corpus through bpmnlint's own pipeline
via `bpmnlint-plugin-rbpmn`. One documented blind spot: bpmn-moddle silently
repairs duplicate ids, so `rbpmn/bpmn-structure` cannot flag them inside
moddle-based tooling — deploy (raw XML) remains the authority.

To use the rules in your own bpmn-io tooling:

```json
{ "extends": ["bpmnlint:recommended", "plugin:rbpmn/recommended"] }
```

## Developing

```sh
cargo test            # everything, including the fixture corpus
just lint             # clippy -D warnings + fmt --check
just serve            # run the HTTP server with a throwaway token
just playground       # linter playground (builds WASM first)
just parity           # Rust-vs-WASM byte parity + bpmnlint plugin test
just feel-parity      # FEEL subset differentialled against dsntk (own lockfile)
just tla              # TLA+ model check of the locking + lease protocol
```

## HTTP server (optional)

```sh
export RBPMN_API_TOKEN=$(openssl rand -hex 32)
cargo run -p rbpmn-server
curl -s -X POST localhost:7420/v1/definitions/lint \
  -H "Authorization: Bearer $RBPMN_API_TOKEN" \
  --data-binary @model.bpmn
```

Configuration is env-only: `RBPMN_BIND` (default `127.0.0.1:7420`),
`RBPMN_API_TOKEN` / `RBPMN_API_TOKEN_FILE`, `RBPMN_ALLOW_NON_LOOPBACK`,
`RBPMN_DATABASE_URL` (required), `RBPMN_TOPICS` (comma-separated declared
worker topics), `RBPMN_HTTP_HANDLERS` (`topic=url;...`), `RBPMN_WORKERS`.
Startup re-validates persisted definitions against the configured
environment and refuses to start on drift.
Security posture and roadmap: [docs/http-security.md](docs/http-security.md).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
