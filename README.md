# rbpmn

A **correctness-first BPMN 2.0 execution engine** in Rust on PostgreSQL.
Embeddable as a library; a small optional HTTP server for everyone else.

It runs a deliberately **restricted subset** of BPMN, enforced at deploy time:
*it is better to loudly reject a model than to silently execute it wrong.* No
inclusive gateways, no call activities, block-structured concurrency only. A
model that deploys is one the engine can run to quiescence — the linter is the
front door, not a pass you can skip.

Two things follow from that and shape everything else. **The BPMN stays 100%
standard-namespace**: every runtime binding lives in a manifest beside the
model, never in a vendor attribute inside it. And **the engine steps inside
your transaction**, so a process transition and the business write that caused
it commit together or not at all.

Requires **PostgreSQL 13+**; 18 or newer is recommended (older versions are
correct, but slower on some read paths).

- [bpmn-engine-design.md](bpmn-engine-design.md) — why the subset is drawn
  where it is, and everything still open, in one table.
- [docs/rules.md](docs/rules.md) — the full rule catalogue.
- [docs/read-surface.md](docs/read-surface.md) — the published SQL views.
- [docs/http-security.md](docs/http-security.md) — before exposing anything to
  a browser.

## Quick start

```rust
use rbpmn_engine::{Bindings, Engine, GetTaskOptions, WorkerOptions};
use serde_json::json;
use std::sync::Arc;

let pool = rbpmn_engine::connect(&std::env::var("DATABASE_URL")?).await?;
let engine = Engine::builder(pool)
    .handler("payments", Arc::new(ChargeCard))   // push-mode service worker
    .declare_topic("review-queue")               // pull-mode: a human, or an external worker
    .build();
engine.migrate().await?;

// A deployment is the model *and* its wiring, deployed atomically and
// versioned together.
engine.deploy(
    &std::fs::read_to_string("order.bpmn")?,
    &Bindings::new()
        .topic("charge", "payments")
        .topic("review", "review-queue")
        .config("charge", json!({ "gateway": "acquirer-a" })),
).await?;

engine.start("order", None, json!({ "order": { "total": 129.95 } })).await?;
tokio::spawn({
    let engine = engine.clone();
    async move { engine.run_worker(WorkerOptions::default()).await }
});
```

A `ServiceTaskHandler` is one method returning a boxed future; what it returns
is an RFC 7386 merge patch applied to the instance variables. Delivery is
at-least-once and the state transition exactly-once, so handlers must be
idempotent.

Human work is pulled rather than pushed:

```rust
let task = engine.get_task("review-queue", &GetTaskOptions::new("alice")).await?.unwrap();
engine.complete_task(task.id, "alice", json!({ "approved": true })).await?;
```

Every step-like call has an `*_in_tx` variant taking your transaction, which is
how a process transition and a business write become one commit.

## Status

Everything through embedded subprocesses, boundary events (interrupting,
non-interrupting, cyclic timers), DMN decisions, message correlation, timers,
incidents and retention is implemented and covered by the corpus. The known
gaps, each with its status in the design brief's
[open-items table](bpmn-engine-design.md#everything-still-open--one-visible-list):
cross-definition messaging (message start/throw between definitions lint clean
but refuse to compile) and the instance migration API.

## Workspace

| Crate / package | Purpose |
|---|---|
| `crates/rbpmn-model` | BPMN XML → model, the linter, the FEEL-subset parser and evaluator. No IO, no async, no DB — so it reaches wasm32. |
| `crates/rbpmn-core` | The pure semantic core: `compile` → executable model, tokens, and the deterministic `step`. No IO; the Postgres layer projects it. |
| `crates/rbpmn-engine` | The PostgreSQL projection: transactional stepping, atomic idempotent deploys, the growing environment, leases, retries, incidents, retention. |
| `crates/rbpmn-dmn` | DMN validation and FEEL evaluation over [a dsntk fork](https://github.com/tpjg/dsntk) — pure-Rust decimal128 in place of Intel's C library, and no HTTP client for FEEL's external-Java bridge. The one crate where dsntk is allowed; nothing upstream of it may depend on it, which is what keeps model and core on wasm32. [docs/dmn.md](docs/dmn.md). |
| `crates/rbpmn-wasm` | wasm-bindgen surface: `lint`, `check_deployable`, `evaluate_decision`, `catalogue`. |
| `crates/rbpmn-server` | Standalone HTTP server. Bearer auth, loopback-only by default. |
| `crates/rbpmn-ui` | The two UI documents as self-contained HTML: a read-only inspector, and the model+manifest editor. |
| `ui/`, `playground/`, `bpmnlint-plugin-rbpmn/` | Sources for those documents; the linter playground; rbpmn's rules inside bpmn-io tooling, backed by the same WASM. |

## XML purity: the wiring is not in the model

BPMN files carry no rbpmn attributes and no vendor attributes, ever. Everything
that wires a model to its runtime goes through one `Bindings` value — a fluent
builder in the library, the same struct deserialized from the deploy body's
`bindings` JSON on the server. It deploys atomically with the model and is
versioned and content-hashed with it, so it is reviewable in git next to the
`.bpmn` instead of smeared through vendor annotations inside it.

| Wiring | API | Checked by |
|---|---|---|
| Service-task topic | `Bindings::topic(element, topic)`; default is the element id | `unresolved-topic` |
| Message correlation | `Bindings::correlation(element, "order.id")` — a FEEL qualified name into the variables | `message-has-correlation` |
| Decision | `Bindings::decision(element, name, "order.discount")` — which decision, and where the answer lands | `decision-has-binding`, `unresolved-decision` |
| Task config | `Bindings::config(element, json!({…}))` — free JSON delivered beside the variables, never interpreted | `config-binds-task` |
| Filterable fields | `Bindings::index(field)` / `shared_index(field)` — optional, performance only | — |

`declare_topic` and `declare_index` are the *environment* half: engine
configuration, not manifest content.

**Config is model content, not runtime configuration.** It is inside
`content_hash` and pinned with the instance, so changing it is a deploy by
construction. One deciding question: *must this change with a deploy?* A
document template, yes. An endpoint URL or a credential, no — those belong to
the environment, or to your own store, keyed by the `definition_id` and
`definition_version` every claimed task carries.
[docs/design/task-config.md](docs/design/task-config.md) is the long form.

## Reading rbpmn's state

Applications need to *join* rbpmn's state against their own rows, and no API
returning data instead of SQL does that as well. So the surface is published
rather than hidden: six views, public API on the same footing as rule ids, with
columns added but never removed or repurposed.

| View | The question it answers |
|---|---|
| `rbpmn_v_definition` | what is deployed, at which version, from which artifacts |
| `rbpmn_v_definition_decision` | the DMN artifacts a version was deployed with |
| `rbpmn_v_instance` | what is running, and what does it hold |
| `rbpmn_v_work_item` | what is waiting to be worked, and how deep is each queue |
| `rbpmn_v_timer` | when does this next happen |
| `rbpmn_v_subscription` | what is waiting on this business identifier |

The last three are the three things an instance can be waiting on — a worker, a
clock, a message — so no wait state needs an undocumented table to see. None of
them is a tenancy boundary and none is a claim: they are read models, true when
measured, and the only way to *hold* work is `get_task`.
[docs/read-surface.md](docs/read-surface.md) has the columns, the plans and the
contract.

## The editor, the inspector, the playground

Two self-contained HTML documents — one stylesheet, one script, no
subresources. `just ui-dist` writes both to `ui/dist/`; open `editor.html`
straight from disk and it works.

**The editor** authors a deployment, meaning the pair: the `.bpmn` and its
manifest. Validation is live and is the engine's own code compiled to wasm32 —
the linter *and* compile-against-manifest — so a missing correlation binding is
caught here, not at deploy. The one check needing a server,
`unresolved-topic`, fetches the covered topic *names* and compares locally:
your model is never uploaded, so a confidential process can be validated
against production. Export SVG writes the diagram out for a document, always in
the light palette.

**The inspector** is read-only, forever, and opens with a *diagnosis* rather
than a diagram — "Incident at `charge` — retry budget exhausted — handler
answered 502". Its element pane fuses the model, the deployed manifest and the
runtime, which is the only place the wiring of an element the token never
reached can be recovered from.

**`just demo`** brings both up against a real server with an instance frozen on
an incident, and prints two links. The auth proxy it runs is part of the
demonstration: UI routes sit behind the same bearer as `/v1`, browsers cannot
send that header on a navigation, and supplying it is the embedding
application's job.

**`just playground`** is the fixture corpus with live re-lint through the same
WASM build deploy uses. To use the rules in your own bpmn-io tooling:

```json
{ "extends": ["bpmnlint:recommended", "plugin:rbpmn/recommended"] }
```

## Developing

`.github/workflows/ci.yml` runs these on GitHub Actions, one step per command,
so a red build names the discipline that broke rather than "tests failed". CI
is the backstop, not the workflow: knowing which command a change *owes* is
what keeps the loop short, because each guards something `cargo test`
structurally cannot see.

**Bootstrap:** `just ui` before the first `cargo build` — the UI bundles are
compile output and gitignored like every other artifact here. Needs node and
wasm-pack.

**Always, before committing:** `cargo test` (needs a local Postgres for the
engine's integration tests) and `just lint` (clippy `-D warnings` + fmt check).

**Owed by what you touched:**

| If you changed | Run | Because |
|---|---|---|
| a linter rule, `rbpmn-model`, `rbpmn-core` | `just ui` | the editor embeds the linter; without it the document you serve validates against yesterday's rules, and nothing checks that for you |
| anything WASM-facing | `just parity` | byte-parity of native Rust against WASM over the corpus, both exports, plus the bpmnlint plugin |
| the `dmn` feature, or anything behind it | `just no-dmn` | DMN is on by default; this is the only thing keeping "optional" a fact, and it asserts the dependency graph in *both* directions |
| lock order, the work-item lease, the scheduler's claim, scope teardown, retention | `just tla` | the specs are hand-written and will not tell you they drifted |
| the FEEL subset, `condition::eval` | `just feel-parity` | differential against dsntk over ~8k expression/document pairs |
| the two UI documents | `just ui-test`, `just e2e-ui` | the pure modules under node, then both documents in a real browser — the only place the CSP is enforced |
| a fixture without DI | `just fixtures-di` | so it renders in bpmn-js and any standard modeler |
| the dsntk fork's rev | `just number-parity`, then `just dmn-tck` | its decimal against the C library it replaces (26 300 comparisons), then the DMN TCK against published dsntk and against the fork, case by case. `dmn-tck` is not on CI: it fetches the TCK, dsntk's source and a third-party runner |

**Fixtures come first.** Every phase starts with them, in
`crates/rbpmn-model/tests/fixtures/{accept,reject}/`, each embedding its
expected diagnostics in a leading comment:

```xml
<!-- expect-diagnostics:
  error no-inclusive-gateway @ gateway_1
-->
```

**Benchmarks are a separate track** and never gate on absolute numbers.
`just bench` runs the lifecycle suite; `just bench-micro` compares the IO-free
core against *this machine's* baseline and is the only benchmark that can fail
a build. [benchmarks/README.md](benchmarks/README.md) says what they exclude.

**Utility:** `just serve` (the server with a throwaway token), `just e2e`
(every fixture in a browser, plus the inspection stack), `just cleanup`
(**destructive**: drops every `rbpmn_*` database and removes all build output).

## HTTP server (optional)

```sh
export RBPMN_API_TOKEN=$(openssl rand -hex 32)
cargo run -p rbpmn-server
curl -s -X POST localhost:7420/v1/definitions/lint \
  -H "Authorization: Bearer $RBPMN_API_TOKEN" \
  --data-binary @model.bpmn
```

Configuration is env-only and secrets never come from CLI args: `RBPMN_BIND`
(default `127.0.0.1:7420`), `RBPMN_API_TOKEN` / `RBPMN_API_TOKEN_FILE`,
`RBPMN_ALLOW_NON_LOOPBACK`, `RBPMN_DATABASE_URL` (required), `RBPMN_TOPICS`,
`RBPMN_HTTP_HANDLERS` (`topic=url;…`), `RBPMN_WORKERS`, `RBPMN_RETAIN`
(retention age in days; unset means no sweeper runs at all).

Startup re-validates persisted definitions against the configured environment
and refuses to start on drift. **rbpmn authenticates nobody** — read
[docs/http-security.md](docs/http-security.md) before exposing either UI
document to a human.

## License

Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or MIT
([LICENSE-MIT](LICENSE-MIT)), at your option. Contributions are dual licensed
as above unless you state otherwise.
