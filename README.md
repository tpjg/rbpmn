# rbpmn

A **correctness-first BPMN execution engine** in Rust on PostgreSQL. Embeddable
as a library; a small optional HTTP server for everyone else. A deliberately
**restricted subset of BPMN 2.0**, enforced at deploy time: *it is better to
loudly reject a model than to silently execute it wrong.*

The full rationale — why tokens-per-row, why no inclusive gateway, why no call
activities, why block structure — lives in [bpmn-engine-design.md](bpmn-engine-design.md).
Read it before touching the semantics.

## Workspace

| Crate | Purpose |
|---|---|
| `crates/rbpmn-model` | BPMN XML → internal model + the linter. Dependency-light (no IO, no async, no DB) so it compiles to WASM for the linter playground and the bpmnlint plugin. |
| `crates/rbpmn-server` | Small standalone HTTP server wrapping the engine. Bearer-token auth, loopback-only by default. See [docs/http-security.md](docs/http-security.md). |

Planned (per the design brief's build order): `rbpmn-core` (pure semantic
`step` function, phase 1), the PostgreSQL projection + `Engine` API (phase 2),
timers/messages (phase 3), the task API (phase 4).

## Status

- [x] **Phase 0 — Parse & reject**: parser, full linter rule catalogue,
      fixture corpus (15 accept / 31 reject), corpus runner.
- [x] Server skeleton with the security spine and `POST /v1/definitions/lint`.
- [ ] Phase 0-B — linter playground (WASM) + bpmnlint plugin.
- [ ] Phase 1 — pure semantic core (tokens, scopes, `step`).
- [ ] Phase 2 — PostgreSQL projection, `Engine` builder, work items.
- [ ] Phase 3 — timers & messages. Phase 4 — task API. Phase 5 — rounding out.

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
| `conditions-are-trivial` | error | Conditions only on exclusive-split flows, matching the tiny grammar; default flow required. |
| `timer-iso8601` | error | Timer definitions must be valid ISO-8601; dates require an explicit UTC offset. |
| `message-has-correlation` | error | Message start/catch/throw must reference a named message and declare `rbpmn:correlationKey`. |
| `no-foreign-implementation` | warn | Service task bound only via vendor attributes (`camunda:`, `zeebe:`, …), which rbpmn ignores. |
| `boundary-on-supported-host` | error | Boundary events only on service/user/receive tasks and subprocesses; error boundaries only where errors can originate. |
| `no-implicit-split` | error | Activities have at most one outgoing flow; splitting happens at explicit gateways. |
| `implicit-merge-after-parallel` | warn | Implicit merge receiving concurrent tokens — the "task runs twice" trap (accompanies the balanced-gateways error). |
| `bpmn-structure` ⁺ | error | Well-formedness: resolvable refs, flow cardinalities, connectivity, unique ids, error definitions. |
| `no-mixed-gateway` ⁺ | error | A gateway either splits or joins, never both. |
| `event-gateway-structure` ⁺ | error | Event gateway races ≥2 message/timer catches or receive tasks, each with exactly one incoming flow. |
| `service-task-topic` ⁺ | error | Service tasks declare their work-item topic via `rbpmn:topic`. |

## rbpmn extension attributes

Namespace: `https://rbpmn.dev/schema/1.0` (conventionally `xmlns:rbpmn`).

| Attribute | On | Meaning |
|---|---|---|
| `rbpmn:topic` | `serviceTask` | Work-item topic that handlers/external workers subscribe to. |
| `rbpmn:correlationKey` | message start/catch/throw events, `receiveTask` | JSON pointer into the instance variable document, e.g. `/orderId`. |

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

## Developing

```sh
cargo test            # everything, including the fixture corpus
just lint             # clippy -D warnings + fmt --check
just serve            # run the HTTP server with a throwaway token
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
`RBPMN_API_TOKEN` / `RBPMN_API_TOKEN_FILE`, `RBPMN_ALLOW_NON_LOOPBACK`.
Security posture and roadmap: [docs/http-security.md](docs/http-security.md).
