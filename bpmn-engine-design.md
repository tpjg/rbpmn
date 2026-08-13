# BPMN Engine — Design Brief & Learnings

Starting point for building a **correctness-first BPMN engine in Rust on PostgreSQL**.
This document records the reasoning behind every major decision so a fresh session can
start implementing without re-deriving them. Read fully before writing code.

## Vision

A small, embeddable BPMN execution engine, used **as a Rust library**, persisting to a
**single PostgreSQL database** (no Elasticsearch, no Kafka, no ZooKeeper, no extra
infrastructure of any kind). We deliberately support a **restricted subset of BPMN** and
reject everything else **at deploy time** — but everything we support must be *correct*
per the BPMN 2.0 spec semantics, proven by an extensive integration test suite.

Guiding principle: **it is better to loudly reject a model than to silently execute it
wrong.** The industry's mature engines (Camunda 7 lineage, Flowable) carried documented
spec deviations for over a decade (inclusive-gateway join, parallel-join token counting)
because their data models couldn't express token-level semantics. We avoid that class of
bug by (a) choosing a token-level data model from day one and (b) refusing the model
shapes that make correct semantics undecidable.

## Key learnings (why the design is the way it is)

### 1. Tokens, not "executions in activities"
Camunda 7 stores an *execution tree* — rows meaning "an execution is in activity X". It
cannot represent "N tokens on sequence flow Y", which is what the spec's operational
semantics are written in. This single representational gap is the root cause of its
documented gateway deviations. Zeebe (Camunda 8) fixed the same bugs by moving to
token-level, event-sourced state.
**Decision: our unit of runtime state is the token. A token is a row.**

### 2. The OR-join (inclusive gateway join) is the trap — refuse it
The converging inclusive gateway has *non-local* semantics: to fire, it must answer
"can any live token anywhere in the process still reach me?" — a reachability analysis
over the whole graph plus current token state, ambiguous or undecidable with loops.
Every major engine either shipped it wrong for years, documented deviations as intended
behavior, or (Zeebe) refused to ship it for ~6 years.

However: simple **local token counting is provably correct when the model is
block-structured** — every split has a matching join, branches cannot escape the block,
no tokens enter from outside, loops wrap whole blocks.
**Decision: we enforce block structure at deploy time and keep join semantics local
counting. The inclusive gateway is rejected entirely** (the equivalent portable pattern
is a parallel split with an exclusive skip-bypass in each branch — the linter message
should say exactly that).

### 3. No execution conformance suite exists — our tests ARE the assurance
There is no TCK for BPMN execution semantics. OMG never built one; conformance is
self-declared. The only assurance available to anyone is a self-owned suite of small
models with asserted outcomes.
**Decision: fixture-driven integration tests are a first-class deliverable, not an
afterthought.** Every supported element and combination gets fixtures; every *rejected*
construct gets a fixture asserting rejection with a specific rule ID.

### 4. Wait states are the transaction boundaries
A waiting token costs nothing — it's a passive row. The engine advances tokens
synchronously *inside the caller's DB transaction* from one wait state to the next
(Camunda 7's model, which is good). Only *due* work (timers, retries, unlocked work
items) is found by polling. Because the engine lives in the application's Postgres,
**process transitions can share a transaction with business writes** — a property
remote engines (Zeebe, Temporal) cannot offer. Preserve it in the API design.

### 5. History = one append-only event table
History is the thing that kills BPM installations (write amplification + unbounded
growth). Design: a single append-only `event` table is the *only* history mechanism.
"History level" = which event kinds get written (configurable per definition).
Retention = `DELETE WHERE`. This also gives replay-grade debugging.

### 6. Definitions are islands; interaction is messages only
**No call activities.** A call activity welds parent and child instance lifecycles
together (parked parent token, child→parent FK, error propagation upward, cancellation
cascading downward, version-binding headaches). Banning it keeps definitions
referentially disjoint: an instance never references another instance. Cross-process
interaction is exclusively message throw → message start/catch with correlation keys.
Intra-definition decomposition uses **embedded subprocesses** (same instance, cheap).
Cost accepted: request/reply/timeout and cancellation must be modeled explicitly.

### 7. Long-running instances: versioning and sleep
Instances are **pinned to the definition version they started on**, forever. New
deployments create new versions; old versions stay executable. Never model polling
loops (timer→check→loop = thousands of executions/year per instance); model *catching
elements* and let the outside world push messages. An explicit instance-migration API
is a later phase but design the schema so it's possible (tokens reference element IDs,
not positional indexes).

### 8. Postgres specifics learned the hard way
- Work acquisition: `SELECT ... FOR UPDATE SKIP LOCKED` — gives contention-free bulk
  acquisition (the thing Flowable needed a "global acquire lock" to approximate).
- `LISTEN/NOTIFY` to wake pollers early; polling interval is the fallback, not the
  mechanism.
- **Partial indexes** per definition on the shared tables:
  `CREATE INDEX ... ON rbpmn_work_item ((variables->>'status')) WHERE definition_id = 'orders'`.
  Two footguns: (a) the planner only uses a partial index when the query contains the
  **literal** predicate (`definition_id = 'orders'`, not `= $1`); (b) the indexed
  expression must match the query expression **exactly** (`->>` vs `@>` are different
  indexes). Generate both index DDL and query fragments from the same source — the
  definition's deploy step is the natural place.
- Single shared schema, one migration path. Schema-per-definition was considered and
  rejected; partial indexes deliver the per-definition index isolation without the
  N-schema migration fan-out. Partition/promote a single definition later only if one
  earns a genuinely different lifecycle.
- Job-ish tables (`work_item`, `token`) are churn-heavy: plan per-table autovacuum
  settings from the start.

### 9. XML handling (Rust)
- Parse BPMN XML with **roxmltree** (read-only tree, fast, no unsafe, ideal for
  extraction). We never need to *write* BPMN.
- Preserve nothing we don't understand, but **do not fail on foreign namespaces**
  (modelers sprinkle `camunda:`/`zeebe:`/`flowable:` attributes; ignore them, warn via
  a linter rule if they are semantically load-bearing, e.g. a service task whose only
  implementation binding is a foreign attribute).

### 10. Topology: any number of engine processes, one Postgres
Active-active by construction — there is no singleton component and no leader
election anywhere. Postgres is the only coordination point: steps serialize on
the instance row lock (`FOR UPDATE`), work/timer acquisition is
`FOR UPDATE SKIP LOCKED` (competing consumers, no double delivery, no
contention), deploys are advisory-locked per key and idempotent by content,
and `LISTEN/NOTIFY` wakes every node while `SKIP LOCKED` arbitrates who wins.
Run as many engine binaries as needed for HA and throughput. Replicas must
run identical environment config; startup re-validation flags a drifted
replica loudly (and is the standing argument for eventually persisting
environment declarations in the DB). The scaling ceiling remains one
Postgres — deliberately (see Non-goals).

## Supported subset (v1 target across all phases)

| Category | Supported | Explicitly rejected (linter rule) |
|---|---|---|
| Events | none start/end, message start/catch/throw, timer intermediate/boundary, error boundary, terminate end | signal, escalation, conditional, link, compensation, cancel, event subprocess (later) |
| Tasks | service task (external work item), user task, receive task | script task, business rule task, manual task (map to user task?), send task (use throw event) |
| Gateways | exclusive (split+join), parallel (split+join), event-based | **inclusive (both directions)**, complex |
| Structure | embedded subprocess, sequential loops around whole blocks | **call activity**, multi-instance (later phase), transactions/compensation |
| Other | — | lanes/pools as execution semantics (diagram-only, ignored), data objects (diagram-only) |

### Deploy-time validation = linter rules
Validation runs at deploy/initialisation and produces machine-readable diagnostics:
`{ rule_id, element_id, message, severity }`. The same rule catalogue should be
publishable as a **bpmnlint plugin** so modelers see the rules inside bpmn-io tooling
before deploying. Initial rule set:

- `no-inclusive-gateway` — with the parallel+skip-bypass rewrite hint
- `no-call-activity` — with the message start/catch rewrite hint
- `no-unsupported-element` — anything outside the whitelist
- `balanced-gateways` — every parallel split has a matching join; branch token flow
  cannot escape the split/join region (this is the rule that makes local counting
  correct; implement as a structural/region analysis on the flow graph)
- `single-start-event` (per process and per subprocess, v1 simplification)
- `conditions-feel-subset` — sequence-flow conditions must be in the FEEL subset
  (below); default flow required on exclusive splits. Fixtures must cover both
  directions: subset accepted; full-FEEL constructs (function call, arithmetic,
  range) rejected with this rule id
- `unresolved-topic` — every service task's resolved topic (see Topic binding) must
  have a registered handler or be declared for external workers; checked at deploy
  against registration state, so wiring gaps fail loudly instead of leaving
  silently-stuck tokens at runtime
- `timer-iso8601` — timer definitions must be valid ISO-8601 date/duration/cycle
- `message-has-correlation` — every message start/catch/throw references a *named*
  message in the XML; the correlation binding itself (a FEEL qualified name into
  the instance variables, e.g. `order.id`) is registered in code
  (`map_correlation`) and checked at deploy against registration state, exactly
  like `unresolved-topic` — never declared in the XML (see Registration-time
  binding)
- `no-foreign-implementation` (warn) — service task bound only via vendor attributes
- `boundary-on-supported-host` — boundary events only on tasks/subprocesses we support
- `no-implicit-split` (error) — activities must have at most one outgoing sequence flow.
  The spec gives an activity with multiple outgoing flows *parallel* split semantics
  (and *inclusive* semantics if the flows carry conditions — the OR-gateway through the
  back door). Require an explicit parallel gateway instead: explicitness keeps the
  `balanced-gateways` region analysis clean and keeps refused inclusive semantics out.
  Do NOT auto-normalize implicit splits into gateways at parse time — silently
  reinterpreting a model the author may misunderstand violates the "loudly reject"
  principle.
- `implicit-merge-after-parallel` (warn) — implicit merges (activity with multiple
  incoming flows) are *allowed*: the spec's semantics are an unambiguous uncontrolled
  XOR merge (every arriving token starts the activity; no synchronization), and they
  are ubiquitous in real diagrams. But warn when an implicit merge is reachable from a
  parallel split without an intervening parallel join — the spec asymmetry (implicit
  split = parallel, implicit merge = NOT a join) is the classic "task runs twice" trap.

## Runtime model

### Tables (single shared schema)

Every physical relation (tables, indexes, and the migration ledger
`rbpmn_migrations`) carries an **`rbpmn_` prefix**: the engine shares its
schema with the embedding application's business tables — that is the whole
point of same-transaction stepping — so generic names like `instance` are
not ours to claim. Prose below uses the short names; the DDL is prefixed.
(The migration runner is hand-rolled for the same reason: sqlx's migrator
hardcodes its `_sqlx_migrations` table, which would collide with a host
application running its own sqlx migrations in the shared schema.)

- `definition` (id, key, **version**, bpmn_xml, parsed/validated model as JSONB cache,
  deployed_at) — (key, version) unique; instances pin a definition id.
- `instance` (id, definition_id, business_key, status[active|completed|terminated],
  **variables JSONB** — the single variable document, created_at, completed_at)
- `token` (id, instance_id, scope_id, element_id or flow_id + position kind,
  state[active|waiting|parked]) — *the* runtime truth.
- `scope` (id, instance_id, parent_scope_id, element_id) — subprocess nesting; enables
  correct interruption teardown (boundary events, terminate).
- `work_item` (id, instance_id, token_id, definition_id, kind[service|user], topic,
  element_id, state[available|locked|completed|failed], retries,
  **lock_owner, lock_until**, created_at, variables_snapshot JSONB?)
  — the external-task queue. `definition_id` denormalised here on purpose: it is what
  the partial indexes predicate on.
- `timer` (id, instance_id, token_id, **due_at** timestamptz indexed, kind, payload)
- `subscription` (id, instance_id, token_id, message_name, correlation_key) —
  indexed on (message_name, correlation_key).
- `event` (append-only: id bigserial, instance_id, definition_id, kind, element_id,
  payload JSONB, at) — the only history. Consider native partitioning by
  definition_id from day one (retention differs per definition first and largest here).

### Variables
- One JSONB document per instance. The engine treats it as **opaque** except where the
  model reads it (conditions, correlation keys).
- Handler completion returns an **RFC 7386 JSON Merge Patch** applied to the instance
  document in the same transaction that advances the token. (Merge patch, not full
  replacement: concurrent parallel branches must not clobber each other's writes.
  Document the limitation: merge patch cannot delete-vs-null distinguish arrays well —
  acceptable, it's the application's document.)
- Serialization/deserialization is entirely the application's concern.

### Condition grammar: a strict FEEL subset
Conditions are a strict subset of FEEL (DMN 1.3+) — identical syntax AND semantics,
so every v1 condition remains a valid, identically-evaluating FEEL expression when
dsntk lands post-v1, and models authored by FEEL-aware tooling
(`expressionLanguage` = FEEL) parse as-is. The subset:
- expressions: `identifier op literal`, combined with `and`/`or`, parentheses
- ops: `=` `!=` `<` `<=` `>` `>=` (accept `==` on input, normalize to FEEL's `=`)
- literals: numbers, double-quoted strings, `true`/`false`, `null`
- identifiers: FEEL qualified names (`order.priority`) resolved as paths into the
  instance's JSONB variable document; a missing path evaluates to null
- nothing else: no functions, no arithmetic, no `in`/ranges, no date literals

Null semantics are **exactly FEEL's** (they must not change when dsntk swaps
in). Two *independent* rules — conflating them is what made `x != 1` true for
a missing x until the dsntk differential caught it:

1. **Against the `null` literal**: the null-check idiom, always a boolean.
   `x = null` is true iff x is missing/null; `x != null` its inverse.
2. **Against any other literal**: a type mismatch, which yields null — and
   null is a type, so a missing value is null against every non-null literal,
   **`!=` included**. `x != 1` is null (→ false), *not* true, when x is
   missing. A missing variable must never satisfy a negative condition;
   the required default flow is where such a token belongs.

Ordering comparisons with anything but a number yield null, `and`/`or` are
Kleene, and the ternary result collapses to a boolean only at the root:
null → false.

Additional strictness beyond FEEL (safe: a strict subset may reject more, it must
never evaluate differently): ordering ops (`<` `<=` `>` `>=`) require a number
literal; qualified-name segments are `[A-Za-z_][A-Za-z0-9_]*` (no spaces).

Verified, not asserted: `just feel-parity` differentials the whole subset
against dsntk over ~8k expression/document pairs. dsntk itself stays out of
the workspace — `dsntk-feel-number` binds Intel's decimal C library through
`dfp-number-sys`, so the stack cannot reach wasm32 and cannot enter
`rbpmn-model` (see "Post-v1: decisions").

Own tiny parser/evaluator in the pure core (no dsntk dependency in v1). Checked at
deploy (`conditions-feel-subset`). Rationale unchanged: decisions belong in
application code — compute outside, store the result, let the gateway read a flag.
Correlation keys use the same FEEL qualified-name syntax, registered via
`map_correlation` — the XML carries no rbpmn-specific syntax anywhere.

### Execution semantics (the correctness core)
- Pure core: `fn step(model, state, command) -> (state', effects)` — **no IO in the
  semantics**. The Postgres layer is a projection of this core: load the affected
  state, run the pure transition, write rows + events, all in one transaction.
  This is what makes property tests and exhaustive semantic tests cheap.
- Advance synchronously to the next wait state within the caller's transaction.
- Wait states: user task, receive/message catch, timer, event-based gateway,
  parallel join with missing tokens, work_item creation for service tasks
  (service task = create `work_item`, park token; completion call advances it).
- Parallel join: fires when it holds one token per incoming sequence flow **within its
  scope** (valid because `balanced-gateways` guarantees block structure). Exactly-once
  firing under concurrency is a property test target: two branches completing in
  concurrent transactions must produce exactly one continuation (row locking on the
  join's parked tokens / `SELECT FOR UPDATE` on instance-scope advance).
- Terminate end event: delete all tokens/scopes/work items/timers/subscriptions of the
  instance in one transaction; write terminal event.
- Error boundary: service-task failure past retry budget raises a named error; matched
  by error code on the boundary event of the task or nearest enclosing scope; no match
  → instance goes to a `failed`/incident state (do NOT silently swallow).
- At-least-once handler delivery is the contract; the engine guarantees exactly-once
  *state transition* (completion of an already-completed work item is a no-op returning
  a distinct result). Handlers must be idempotent; say so loudly in docs.

## Library API (sketch — refine in session)

```rust
// Core engine handle; Clone-able, owns a PgPool.
let engine = Engine::builder(pool)
    .handler("payments", HttpPostHandler::new("https://internal/payments"))
    .handler("email", my_custom_handler)          // impl ServiceTaskHandler
    .declare_topic("fulfillment")                 // pull-mode workers will poll this
    .build();

// One atomic deploy: the definition plus its bindings manifest, validated
// together (the manifest needs no definition key — it rides along with the
// definition it describes). Serializes 1:1 to the server's JSON deploy body.
let bindings = Bindings::new()
    .topic("ChargeCustomer", "payments")
    .topic("ChargeRenewal", "payments")
    .correlation("AwaitPayment", "order.id")
    .index("status");                              // optional, performance only
engine.deploy(bpmn_xml, bindings).await?;          // -> Deployment { version, diagnostics }
                                                   //    Err(Rejected{diagnostics}) on lint failure
let id = engine.start("order-process", business_key, initial_vars_json).await?;
engine.correlate("PaymentConfirmed", "order-84231", patch_json).await?; // messages, callable directly

// The engine does NOT own an HTTP server. Provided as optional, thin, example layers:
//   - axum router exposing correlate/start/complete  (feature = "http")
//   - HttpPostHandler: default ServiceTaskHandler that POSTs {instance, element, vars}
//     and applies the JSON response body as merge patch (feature = "http")

#[async_trait]
pub trait ServiceTaskHandler: Send + Sync {
    async fn execute(&self, item: WorkItem) -> Result<MergePatch, HandlerError>;
}
// Push mode: engine's worker loop acquires work_items (SKIP LOCKED) and invokes handlers.
// Pull mode (external workers / user tasks): the task API below. Both share work_item.
```

### Registration-time binding (topics & correlation — never in the XML)
BPMN files stay **100% standard-namespace**: no vendor attributes, and no rbpmn
extension attributes either. Anything that wires a model to its runtime is
registered in code and validated at deploy — the same class of guarantee a
compiler gives: a wiring gap fails loudly at deploy instead of "seeming to run"
with silently-stuck tokens. (Principle, decided explicitly: resist every future
"let's just add a hint/annotation in the XML".)

A **topic** is the capability name a work item is addressed to: the routing key
between "the engine decided this work is due" and "somebody does it". Element id
and topic answer different questions (where in the process vs. what capability),
and the relationship is many-to-one — several tasks across definitions can share
one worker.
- `map_topic(definition_key, element_id, topic)` — explicit binding, lives in code.
- Default when unmapped: **topic = element id** (zero-config simple case; the
  authoring style makes ids descriptive PascalCase anyway).
- `declare_topic(name)` — announces that out-of-process workers will poll this
  topic; the engine cannot see pull-mode consumers, so this is how they become
  "known" to the `unresolved-topic` deploy check.
- `map_correlation(definition_key, element_id, feel_name)` — binds a message
  start/catch/throw element to its correlation key, a FEEL qualified name into
  the instance variables (e.g. `order.id`). No default: every message element
  must be mapped or deploy fails (`message-has-correlation`).
- `work_item.topic` is denormalized at creation time from the binding in force.

**The deployment manifest — one atomic deploy.** Per-definition wiring
(topics, correlations, optional index declarations) is *declarative data about
one definition* and is versioned with it: instances pin the definition version,
so they must pin the wiring that was in force too. Therefore deploy is a single
atomic operation carrying both — `deploy(bpmn_xml, bindings)` in the library,
`POST /v1/definitions { "bpmn": ..., "bindings": ... }` on the server — and the
deploy-time checks (`unresolved-topic`, `message-has-correlation`) run against
exactly that pair. Never a multi-call registration dance: partially-wired
intermediate states are the "seems to run" failure mode this design exists to
kill. The Rust builder API and the HTTP endpoint converge on one
`DeploymentManifest` struct internally — a single validation path, so library
and server cannot drift. The manifest is a small JSON document that lives in
git next to its `.bpmn`: this is the same information other engines smear into
vendor XML annotations — separated cleanly from the process definition and
versioned with it.

Distinct from the manifest: **environment capabilities** (handler targets,
`declare_topic` for external workers) describe the runtime, not a definition.
In the library they live on the Engine builder; in the standalone server they
are operator configuration (handler URLs never come from request data — see
docs/http-security.md).

**The environment grows monotonically.** Registration is not a one-shot
builder ritual: more handlers and declared worker topics can be added at any
time after build (library methods; on the standalone server an operator API /
config reload), and a deploy validates against the environment **as it exists
at that moment**. Both sides are **idempotent**: re-declaring a topic is a
no-op, re-registering a handler applies the latest binding, and deploy is
idempotent by content — same key + byte-identical XML + bindings returns the
existing version (no new version row), changed content allocates the next
version. Everything is safely retryable infrastructure. Declared worker topics are
**persisted** (`rbpmn_environment_topic`): the deploys a declaration unblocks
persist, so the declaration must too — a restart or replica resumes the same
environment, converging config and API declarations (their union wins).
Handlers remain code/config by nature (they are executable bindings, not
data); handler drift is what startup re-validation still catches.

Growth got one carefully protected inverse (decided post-phase-4):
`undeclare_topic(name)` / `DELETE /v1/topics/{name}` withdraws a persisted
declaration — but is **refused (`TopicInUse`, HTTP 409, naming the
culprits) while any relevant definition still needs the topic**: the latest
version of every key plus any version with active instances, the same set
startup re-validation checks. A definition that cannot be inspected (no
longer compiles) also blocks — we only undeclare what we can *prove*
unneeded. Registered handlers deliberately do not substitute for the
declaration: they are process-local and ephemeral, and a replica without
that handler code would refuse to boot. Known, accepted limits ("we can
only check what we know"): a topic still named in config or a builder
declaration returns at the next `sync_environment`; other replicas keep it
in memory until they restart; and a deploy racing an undeclare in the
narrow window between check and delete is caught loudly by the next
startup re-validation, never silently stuck. Undeclaring an absent topic
is the idempotent no-op.

**Ordering is deliberate: environment before definitions.** Deploy is the
link step; `unresolved-topic` is its undefined-symbol error, so capabilities
must be registered before a definition that uses them deploys. Both usage
modes make this structural (the Rust builder produces the engine `deploy`
hangs off; server config is read before HTTP traffic). The ordering binds the
*declaration*, never worker liveness — `declare_topic` is a recorded promise
that pull-mode workers exist in this environment; the engine cannot and does
not verify they are running. Because definitions persist across restarts but
the environment is rebuilt from code/config at every startup, deploy-time
checking alone would miss drift (a handler removed after deploy): therefore
**startup re-validates every active definition version's topics against the
current registration state** and fails loudly with the same rule id
(phase 2, alongside the deploy check).

### Task API (phase 3, but design the table for it now)
- `get_task(topic, order, ttl) -> Option<LockedTask>` — `FOR UPDATE SKIP LOCKED`,
  sets lock_owner + lock_until = now()+ttl. `order` selects **FIFO** (default:
  `created_at` ascending, tie-broken by item number — a fair queue) or
  **LIFO** (descending — freshest-first triage), applied in the same
  acquisition query; the per-definition partial indexes serve both directions
  (btree scans backwards for free) and `created_at` is database time, so
  ordering is consistent across nodes. Honesty note: under concurrent
  consumers FIFO is fair-but-not-strict — `SKIP LOCKED` skips rows a peer is
  claiming; strict global FIFO would serialize all consumers, the wrong trade
  for a work queue. The same `order` parameter applies to
  `get_task_filtered`. **Lease model, not long locks:** base TTL is
  short (~10 min), and holders renew it while actively working. Expired locks make
  the item available again (no reaper needed — availability predicate is
  `state='available' OR lock_until < now()`).
- `extend_lock(id, owner, ttl) -> Result<Extended, LockLost>` — heartbeat: a single
  conditional UPDATE (`SET lock_until = now()+ttl WHERE id=$1 AND lock_owner=$2 AND
  lock_until > now()`). Clients renew every ~2 min against a 10 min TTL, so a task can
  be held for hours — but only while someone is demonstrably still working on it; a
  crashed client or closed browser tab frees the task within one TTL automatically.
  `LockLost` (owner mismatch or already expired) must be a distinct, typed result:
  the client's UI needs to tell the user "this task was reassigned" rather than fail
  silently. Renewal is cheap (primary-key UPDATE) but budget it in autovacuum
  expectations for `work_item` — heartbeats add steady row-version churn.
  The same lease applies to `kind=service` items claimed by external workers: workers
  heartbeat too, so a wedged worker's items return without waiting out a long lock.
- `declare_index(definition_key, field)` — **entirely optional performance
  API**; filtering and counting work without it (seq scan is correct, just
  slower). Two refinements decided during phase 4: filters evaluate the
  **owning instance's live variables** — the sketched `variables_snapshot`
  on work_item (its "?" was never resolved) is rejected, because a snapshot
  silently diverges from the one variable document, and a filter matching
  stale data is a "seems correct" bug; consequently the index lives on
  `rbpmn_instance`, and its partial predicate is **`definition_key`** (a
  literal), not the sketched `definition_id` — instances pin a definition
  *version*, and a per-version predicate would silently exclude every
  instance deployed after the index. Creates `CREATE INDEX CONCURRENTLY IF
  NOT EXISTS <deterministic-name> ON rbpmn_instance ((variables->>'<field>'))
  WHERE definition_key = '<literal>'` — deterministic name from (key,
  field), idempotent and re-runnable at startup. CONCURRENTLY runs outside
  a transaction and, interrupted, leaves an *invalid* index that
  `IF NOT EXISTS` would silently accept forever — so validity is verified
  after the build and an invalid leftover is dropped and reported loudly.
  This IS the "declared filterable fields" mechanism, exposed as an
  explicit call; the same declaration rides in the deploy manifest's
  `indexes` entries, validated before anything persists and applied after
  the commit. JSONB stays opaque: the engine never interprets variables, it
  only indexes fields the application names.
- `get_task_filtered(topic, filter, ttl)` — the filter compiler MUST emit exactly
  the indexed expression shape (`variables->>'field'` + literal definition_id) so
  declared indexes are actually used. EXPLAIN-based integration test: index usage
  for a declared field, correct results (via seq scan) for an undeclared one.
- `count_tasks(topic, filter) -> u64` — dashboard indications; same index discipline.
- `complete_task(id, owner, merge_patch)` / `fail_task(id, owner, error_code?)` —
  owner checked; completing advances the token in the same transaction.

## Testing strategy (first-class deliverable)

1. **Fixture corpus**: `tests/fixtures/**/*.bpmn`, each with a sibling scenario file
   (TOML/JSON): inputs, ordered external interactions (complete work item X with patch
   P, send message M, advance clock), and the **expected event trace** (golden log) +
   expected final variable document. One runner executes all fixtures.
2. **Rejection fixtures**: models using inclusive gateways, call activities, unbalanced
   splits, etc. — assert deploy fails with the exact rule_id.
3. **Property tests** on the pure core (proptest): parallel join fires exactly once for
   any interleaving; token count invariants (no lost/duplicated tokens across any
   transition sequence); terminate leaves zero runtime rows; merge patches from
   concurrent branch completions both land.
4. **Concurrency tests** against real Postgres (testcontainers or a CI service):
   two workers completing sibling branch work items simultaneously; N workers hammering
   `get_task` — no double-delivery, no lost items, lock TTL expiry re-delivers.
   Lease semantics: extend after expiry fails with LockLost; extend races reacquisition
   (A's lock expires, B acquires, A's late heartbeat must lose); complete_task after
   losing the lease is rejected — completion authority follows the current lease.
5. **Crash tests**: kill mid-transaction between work-item completion and token
   advance; restart; assert convergence (this is what the transactional design buys —
   make the test prove it). *Shipped:* `crates/rbpmn-engine/tests/chaos.rs`
   terminates node backends and rebuilds whole nodes under load, then asserts
   convergence the strong way — every instance drains, the fsck is clean, and
   every history still re-derives through the pure core
   (docs/stress-testing.md §5). Note the recovery bound it made explicit: a
   node killed while holding a work-item lease strands that item for one lease
   TTL, so the TTL *is* the crash-recovery window.
6. Start every phase by writing its fixtures *first*.

## Build order — start small, but start with the hard parts

**Phase 0 — Parse & reject (the linter is the product's front door)**
roxmltree-based parser → internal model (elements, flows, scopes). Whitelist +
structural validation incl. `balanced-gateways` region analysis and
`no-implicit-split` (so the semantic core may assume: every activity has ≤1 outgoing
flow; all splitting happens at gateways). Diagnostics type.
Fixtures: every rejection rule. No execution yet. *(Also: publishable rule catalogue —
keep rule IDs stable from day one.)*
Crate-structure requirement set now, needed by 0-B: parsing + validation live in a
dependency-light `engine-model` crate (no Postgres, no tokio) so it compiles to WASM.

**Phase 0-B — Linter playground (inspiration: bpmn-io/bpmnlint-playground)**
A local, static web page for visualizing our .bpmn fixtures and seeing **exactly the
diagnostics deploy would produce** — same rules, same rule IDs, same severities.
- One source of truth: compile `engine-model` to **WASM** and call it from the page.
  No JS reimplementation of rules — a second implementation would drift and defeat
  the purpose. **The bpmnlint plugin ships now, not in phase 5:** wrap
  `lint(xml) -> [{rule_id, element_id, message, severity}]` in bpmnlint's rule
  interface and publish it as part of 0-B — rule IDs are stable from day one, so
  there is nothing to wait for, and modelers get the rules inside bpmn-io tooling
  from the very first fixture.
- Build the diagram **annotation layer generically**: an overlay renderer that takes
  `[{element_id, kind, payload}]` and decorates the bpmn-js canvas. Diagnostics are
  its first consumer; live token state becomes its second (see phase 2) — same
  component, no rework.
- UI: bpmn-js (viewer or modeler) rendering the diagram; diagnostics as element
  overlays/markers (error/warn colouring on the offending element) + a clickable
  list panel that focuses the element. Editing in the page re-lints live — that
  makes the playground double as the fastest way to *author* new fixtures.
- **Fixture browser**: enumerate `tests/fixtures/**/*.bpmn` (tiny build step
  generates an index.json) with a dropdown/tree, so clicking through the entire
  corpus — including the rejection fixtures — is one keystroke. Accepted fixtures
  render clean; rejection fixtures visibly show the rule that kills them.
- Ship as `just playground` / `npm run dev` style local tooling; no server-side
  component beyond static file serving (the linter runs client-side in WASM).
- Definition of done: every fixture in the corpus renders, and the playground's
  diagnostics for each fixture are byte-identical to the deploy-time diagnostics
  asserted in the Rust tests (a CI check compares them — that's the guarantee the
  playground never lies).

**Phase 1 — Pure semantic core: control flow + gateways + service tasks**
Tokens, scopes, the `step` function. Elements: none start/end, sequence flow,
exclusive split/join (condition grammar + default flow), parallel split/join, service
task as work-item wait state, terminate end. Entirely in-memory, exhaustively tested
(fixtures + property tests). This phase is where correctness is won; take the time.

**Phase 2 — Postgres projection**
Schema + migrations (single schema). Transactional stepping over the pure core.
Work-item acquisition loop (SKIP LOCKED + LISTEN/NOTIFY), retries, error boundary →
incident. `Engine` builder, `deploy/start/complete`. HttpPostHandler example.
Concurrency + crash test suites.
Exit criterion: the playground gains an **instance inspection mode** — a dev-only
read endpoint serving an instance's `token`/`work_item`/`event` rows, rendered on
the diagram through the 0-B annotation layer (tokens on elements, parked work items,
recent events). This is the token-overlay debug view, pulled forward from phase 5:
it costs little once the annotation layer exists, and debugging phases 2–4 with it
is exactly when it pays.

**Phase 3 — Timers & messages**
`timer`/`subscription` tables, scheduler tick, correlation (`correlate()` +
optional axum ingress). Timer boundary events (interrupting first). Event-based
gateway. Fixtures incl. "years-long sleep" simulated via clock injection — the clock
is injected in the core from phase 1 precisely for this.

Timer mechanics (decided; claim ordering refined during phase 3): a timer
catch inserts its `rbpmn_timer` row in the same transaction that parks the
token, with `due_at` computed from **database time** (`clock_timestamp() +
duration` — statement time, not `now()`: `now()` is *transaction-start*
time, so a timer armed late inside a long caller transaction would be due
too early by the age of that transaction) —
node clocks never decide anything. Firing is one timer per transaction, and
the timer row's deletion commits together with the step — that is what makes
firing exactly-once. The originally sketched claim (`DELETE ... FOR UPDATE
SKIP LOCKED` first, then lock the instance) turned out to lock in the
opposite order from every other step path (completion locks the instance,
*then* deletes cancelled timer rows) — a classic AB/BA deadlock, survivable
via Postgres's detector but needless. Implemented instead with identical
invariants and no deadlock: pick the due candidate **without any lock**
(cheap on the `due_at` index), take the instance row lock (same order as
every other step), re-check the timer row still exists under that lock
(a concurrent step may have fired or cancelled it — losing the re-check just
means moving on), then step + delete the row via the `timer-fired` event in
that one transaction. Draining = repeat until nothing is due; every node
runs the same loop (competing consumers). There is **never** a per-timer
in-process wait — a sleeping timer is a passive row. After draining, each
scheduler sleeps until `SELECT min(due_at)` (cheap on the index), capped by
a fallback poll interval (~30s, configurable), and a `NOTIFY` on timer
insert (`rbpmn_timer` channel) wakes sleepers when an earlier timer appears
— polling is the safety net, not the mechanism. Nothing fires before
`due_at`; no "due soon" prefetching.

Correlation delivery contract (decided): `correlate(message, key, patch)`
delivers to **exactly one** open subscription of an **active** instance
(frozen instances keep their subscription rows for repair, but those never
match — a corpse must not block delivery to a live instance sharing its
key). No match is a loud error (HTTP 404) — a message with nowhere to go is
never dropped silently; more than one match is refused (HTTP 409) —
delivering to "one of them" would be a guess. Retrying a delivered
correlate returns the no-match error (the subscription is consumed); unlike
work items there is no closed-row no-op — callers that need blind retry
idempotency should make keys unique per message occurrence. The correlation
key **value** is evaluated from the variables when the subscription is
armed; valid keys are strings and **exact integers** (floats have no
canonical spelling across a jsonb round-trip — the same logical value would
arm two different keys). An invalid key, like a second open subscription
for the same (message, key) in one instance (every delivery would be
permanently ambiguous — `duplicate-subscription` event), freezes the
instance as an incident instead of waiting forever. Message start/throw
events stay compile-rejected: cross-definition message routing (throw →
start/catch between islands) is designed post-phase-3; external systems
deliver via `correlate()`.

Incident freeze (uniform, decided in the post-phase-3 review round): every
incident converges on one shape — the token parks at the failing element
with the `incident` wait kind, that token's in-flight arms (boundary
timers, partial event-gateway arms) are withdrawn, and the instance
freezes. Inspection always shows *where* it failed, and a future repair API
has exactly one state to resume from. Anything the freeze deliberately
keeps (a sibling branch's subscription, say) is excluded from scheduler and
correlation queries by instance status, never left to trip them.

**Phase 4 — User tasks & the task API**
`kind=user` work items; get/get-filtered/count/complete with locking + TTL;
declare_index API → generated partial indexes. Dashboard counting.

**Phase 5 — Rounding out**
Event ordering guarantees in `event` (shipped, below) and the dual license.
Retention moved *into* v1 (phase 7) rather than shipping here; the instance
migration API, cross-definition messaging and the upgrade escape hatch moved
*out* to the roadmap, each needing a design round before code. Also here: the
stress/fuzz/chaos tier (`docs/stress-testing.md`) — model generation,
state-space exploration, replay verification, the storm, and chaos runs that
kill processes and drop database connections mid-flight. It reports the
engine behaving as designed: work completes, exactly-once holds, nothing is
lost or double-executed. That is the assurance BPMN has no TCK for, and it is
now empirical rather than argued.

Event ordering (decided and shipped; cursor shape corrected in the
post-phase-5 review, twice): two guarantees and one caveat. Per instance,
ascending **`id`** is the semantic order — an instance's steps serialize on
its row lock, so each step's rows are inserted after the previous step
committed. (The first correction claimed per-instance *txid* monotonicity;
that is false, because a transaction's xid is taken at its first write,
which may precede acquiring the row lock.) The stream itself is ordered and
cursored by **`(txid, id)`**, stopping
at the **safe horizon**: only rows whose `txid` is older than every
in-progress transaction are released, so nothing can ever appear behind a
returned frontier. Neither key alone works, and the first shipped version
(id-ordered, txid-gated) was wrong: bigserial ids are assigned at insert
but transactions commit out of order, *and* a transaction's `txid` is
assigned at its **first write** — so a business transaction around an
`*_in_tx` call holds an old txid while inserting late, high-id events, and
an id-only cursor advances straight past them. Ordering by the pair is what
makes "below the horizon" mean "finished, nothing can precede this".
**The caveat:** cursor safety and per-instance order are different sort
keys and cannot both drive one pass, so an instance's events can *arrive*
out of `id` order when a long-lived caller transaction is involved —
consumers reassembling an instance's history sort by `id` rather than
trusting arrival. Autocommit callers (everything over HTTP) never trigger
it. The horizon is cluster-wide (xids are global to the PostgreSQL cluster) — a
long-running transaction anywhere, including a business transaction around
an `*_in_tx` call, delays the stream (late, never lost), which is one more
reason the "commit promptly" rule is a rule. External API callers cannot
delay it at all: a claimed task is a *lease* (a row value), never an open
transaction.
**Phase 6 — Embedded subprocesses (v1-completing).** Promoted from the v2
roadmap into v1, deliberately: the brief calls hierarchical BPMN "the
modeling style this engine exists to serve", and Method-and-Style hierarchy
(a top level of ~10 collapsed subprocesses, each expanding to its own plane)
is *unmodellable* without them. A flat-only v1 could not express the style
v1 advocates. It is also the largest remaining feature that needs **no new
external contract**: pure internal semantics, where this codebase is
strongest. Scope: the `scope` tree becomes live (it has been in the schema,
unused, since phase 2); joins count within their scope; a subprocess is a
wait state holding its children; interrupting teardown cancels everything in
a cancelled scope (tokens, work items, timers, subscriptions) in one
transaction; error boundaries on a subprocess (today a `NotYetExecutable`
pointer) become the scoped error handler that is the point of the feature;
terminate ends its own scope, not the instance. The linter already recurses
into subprocess scopes and `13-subprocess.bpmn` already sits in the accept
corpus — the groundwork was laid on purpose.

**Phase 7 — Retention (v1-completing; decided and shipped).** Urgent for a
reason that did not exist before phase 5: `/v1/events` publishes a cursor
contract, so a retention job that deletes events a tailing consumer has not
read silently breaks it. The resolution reframes the problem — you cannot
promise never to delete data someone might want, but you *can* promise that
nobody ever silently misses an event.

- **A monotonic truncation floor** is the primary mechanism: one row holding
  the highest `(txid, id)` ever deleted. Everything deleted is at or below
  it, so a cursor at or above it has provably lost nothing however scattered
  the deleted set, and a *resume* from below it fails loudly with
  `CursorTruncated` (HTTP **410 Gone**, carrying the floor to resume from).
  A zero cursor still means "from the beginning", which now reads as *from
  the oldest retained event*: a new consumer has no completeness expectation
  to violate. One row of state, **zero coordination** — third-party
  consumers over HTTP get the guarantee without registering anything. The
  floor and the page are read in **one statement**, sharing one snapshot: the
  first implementation read the floor first, in its own query, which left a
  window where a sweep committing in between produced a page silently missing
  its deleted events with the check having passed against the old floor — the
  exact silence the floor exists to break, reintroduced by the order of two
  queries.
- **A read horizon was considered and deferred**, on an argument worth
  keeping: if a registered reader's TTL is ≤ the retention age, the age floor
  already protects it for longer, so the horizon is dead weight. It earns its
  keep only under a "keep as long as the slowest consumer needs, *beyond* the
  nominal age" policy — and adding it later is purely additive, since it can
  only make truncation rarer, never change what a cursor means. The loud
  mechanism had to ship in v1 because it changes the contract; the
  optimisation did not.
- **One knob, after measuring the two-knob version.** A record retires
  whole: the instance row, its children and its events, in one transaction.
  The first implementation split it — retire the children early
  (`retain_runtime`), delete the record later (`retain_history`) — on the
  theory that those are two growth curves. The post-phase review checked, and
  they are not: the claim indexes (`rbpmn_work_item_pull`,
  `rbpmn_work_item_claim`) are *partial* on `state in ('available','locked')`,
  so closed work items were never in them, and a terminal instance's tokens,
  timers, subscriptions and scopes are already gone. The early stage
  reclaimed roughly a tenth of a record's footprint — events outnumber work
  items by an order of magnitude — in exchange for a column, a partial index,
  a guard on the hot step path, a second planner with dedup between the two,
  two extra fsck invariants, and a narrowed `AlreadyClosed` contract. With
  archive-before-delete in place, long histories belong in object storage
  rather than in Postgres anyway, which removes the last reason to keep a
  record behind after its children.
- **`rbpmn_event.instance_id` became a real foreign key** (`on delete
  cascade`) once one-stage retention made an orphan event impossible. It had
  been an unenforced reference since phase 2, deliberately, so that history
  *could* outlive its instance. Now "an event never outlives its instance" is
  not an invariant this codebase asserts and tests — it is one the database
  will not let it break, and it is what keeps "is this definition still
  referenced?" an indexed lookup on `rbpmn_instance` rather than a scan of
  the largest table in the schema. The referential-integrity check lands on
  the highest-volume insert path, which is affordable for a specific reason:
  a step already holds its instance row `FOR UPDATE`, so the check's KEY
  SHARE lock is uncontended — an index probe per event row, nothing more.
- **Two phases with no transaction across the gap**, so that
  export-before-delete (S3, a warehouse, a compliance log) is possible at
  all: `plan` → `archive` → `execute`. A sink call inside the deletion
  transaction would pin `pg_snapshot_xmin` — cluster-wide — for the duration
  of a network upload, stalling every event-stream reader: the feature that
  archives history would freeze the stream that reads it. The gap is safe
  because retention only ever selects immutable data (terminal instances,
  closed event histories). Export is at-least-once, and a sink failure
  deletes nothing — on *every* path, because `execute_retention` runs the
  sink itself rather than trusting its caller to have done so; it is public,
  and the invariant has to be a property of the code rather than of the
  calling convention. For the same reason the cross-node claim is a **lease
  row**, never a session advisory lock (which would leak forever on a
  cancelled pass) — the task API's "a lease is a row value, never an open
  transaction" rule, applied again.
- **What it will not touch**: active instances, `failed` ones at any age (an
  incident is frozen evidence and a repair target), anything younger than its
  policy, and definitions. Eligibility is evaluated **per instance**, so a
  wedged instance never blocks its neighbours' retirement — the tempting
  alternative, a global watermark at "the oldest event of any non-terminal
  instance", is one number, trivially safe, and permanently jammed by a
  single stuck instance until the disk fills. `Engine::delete_instance` is
  the explicit escape hatch for a triaged incident, and it archives too: an
  escape hatch that bypassed the audit trail would not be one.
- **Definitions and config go only by hand**, which generalises into a rule:
  *an automatic sweep is justified by unbounded growth and by nothing else*.
  Definitions grow with deployments, not throughput, so there is no growth to
  justify the risk — only the risk of turning an archive into a pile of
  element ids. `prunable_definitions` is the dry run; `delete_definition`
  refuses while anything references the version. The second review round
  found that guard accidentally void: it counts live instance rows, retention
  exists to remove them, and an archived record carries element ids but no
  BPMN — so exporting a definition's history was precisely what made the
  definition deletable. Copying the model into every archived record would
  fix it and dwarf a short record; a `retired_instances` counter on the
  definition fixes it for eight bytes, and lets the refusal state its actual
  reason.
- **A record larger than one batch is skipped, loudly, not swallowed.**
  `max_events` bounds a single record as well as the batch. The first version
  always took the oldest candidate whole "so an oversized record cannot stall
  retention", which achieved the exact opposite: the oversized record *is*
  the oldest candidate, so every pass would load its entire history into
  memory, die, and retry it forever — stalled and crash-looping at once. It
  is now skipped with a warning naming it and a count in the report, its
  neighbours retire normally (the same rule a wedged instance already gets),
  and raising the ceiling retires it with no repair step. Event bodies are
  also materialised only when a sink is registered: without one they would be
  loaded solely to be deleted, which is what turned size into an
  out-of-memory rather than a slow query.
- **Retention is opt-in twice over**: no sweeper runs unless one is started,
  and starting one means naming the default policy (`forever()` is a valid
  and explicit choice). Per-definition overrides are keyed by definition
  **key**, not version — retention is operational, not semantic, and keying
  by version would force a redeploy to change an operational knob. The
  policy row's presence, not its nullness, decides: a null column means
  *forever*, a missing row means *no override*, and conflating them (the
  first implementation did, via `coalesce`) silently deletes the history a
  key explicitly asked to keep.
- **Partitioning: no, and the axis matters.** Partition-by-definition only
  pays if a whole definition retires at once, but retention is per-instance
  and time-based, so row deletes would continue inside every partition. The
  axis that pays is range-on-time, where retention becomes `DROP PARTITION` —
  at the cost of coarse time buckets, per-instance atomicity, and complexity
  around the global `(txid, id)` index the cursor contract rests on. It is
  also *physical*, so it can be adopted later without touching a contract.
  Same call for **history level** (which event kinds get written): it changes
  the stream's *completeness* contract — a consumer could no longer tell
  "didn't happen" from "not recorded" — so it is a different feature.

**Phase 8 — Authoring & inspection surfaces (v1-completing).** An editor for
the model+manifest *pair* and a read-only single-instance inspector, both
shipped as self-contained HTML documents rather than as API clients. Detailed
in "Authoring & inspection surfaces" below, including why this is not the
modeler-and-cockpit the non-goals refuse. Last in the order because it is the
only remaining item whose value depends on the engine already being finished:
it exposes what phases 0–7 built, and adds no semantics of its own.

*(bpmnlint plugin packaging and the token-overlay debug view were pulled forward:
plugin → phase 0-B, token overlay → phase 2 exit criterion.)*

## Authoring & inspection surfaces

### Why this is not the modeler and cockpit the non-goals refuse

The original non-goal — *a modeler (use bpmn-io), a cockpit* — was about not
building a modelling **engine** and not building an operations console. Both
still hold, unchanged. What changed is that two decisions already recorded in
this brief left gaps that nothing else can fill:

1. **Wiring lives outside the XML** (see "Registration-time binding"). A
   `.bpmn` file is therefore *not deployable on its own* — it is half of a
   pair, and no bpmn-io tool knows the other half exists. Every other engine
   dodges this by smearing wiring into `camunda:`/`zeebe:` attributes, which is
   precisely what we refuse. Purity shipped without tooling is a tax the user
   pays; purity **with** an authoring surface for the pair is strictly better
   than the vendor-attribute alternative, because the manifest is reviewable
   JSON in git next to the model.
2. **The token-overlay debug view already exists** (phase 2 exit criterion) and
   has been this project's primary debugging instrument through phases 2–7. It
   lives in the playground, reachable only by a developer running
   `just playground` against a dev proxy. Turning it into a document an
   embedding application can hand to a supervisor is packaging, not new scope.

So v1 ships an **editor** (authoring the model+manifest pair) and a
**read-only inspector** (one instance, no writes). Neither is a cockpit. The
constraints below are what keep them from becoming one.

### Hard constraints (these have teeth)

- **The inspector is read-only. No buttons. Ever.** No retry, no cancel, no
  variable edit, no migration. Each of those is a designed API first (see
  "Everything still open"); a UI is never the reason one ships early.
- **No lists, no search, no pagination, no queries.** The inspector addresses
  exactly one instance, by UUID. Finding *which* instance is the embedding
  application's job — it called `start`, it holds the mapping from its own
  order/case/ticket to the returned `instanceId`.
- **rbpmn never authenticates a UI user.** The application does. No cookies, no
  sessions, no login, and no rbpmn credential ever reaches a browser.
- **Neither surface persists anything.** The editor reads and writes local
  files on the user's machine and never uploads a model. There is no draft
  store and no model repository: definitions live in the application's git
  repository and reach the engine through `deploy`, which is code.

### The document model — the decision everything else follows from

Both surfaces are **self-contained HTML documents**, not single-page
applications talking to an API. For the inspector, the data is inlined at
render time, and that single choice dissolves nearly the whole browser-security
problem:

- there is no inspector API, so there is nothing to reverse-proxy, no CORS
  question, no read-only façade, no third token scope, no prefix to configure
- the application's authorization check becomes the *only* gate — which is what
  "the application handles auth" has to mean in order to be true
- snapshot semantics stay honest: `inspect_instance` already reads inside one
  repeatable-read transaction precisely so the view cannot show a completed
  instance with live tokens. A document *is* that snapshot; a polling page
  silently re-tears it on every refresh
- the artifact is attachable — to a support ticket, an incident review, a bug
  report — and works with the database unreachable

The library boundary is therefore a **value, not an endpoint**:
`Engine::inspect_instance(uuid) -> InstanceInspection` already exists, and
rendering is a pure function over it. An axum handler is a five-line
convenience wrapper, never the primitive. Redaction then needs no feature —
an application that must not show variables to tier-1 support strips the field
from the struct before rendering. That door stays open for free, and it is the
only "redaction layer" this project will ever build.

The editor is the same shape without the data: one document, served by a
handler, or opened from disk — which works as a *consequence* of being
self-contained, not as the distribution model.

### Serving them

A new feature-gated crate, `rbpmn-ui`:

- `render_inspection(&InstanceInspection) -> String` — pure, IO-free, no axum,
  no engine, unit-testable against fixtures
- `inspector_router()` / `editor_router()` — thin conveniences for axum hosts.
  The standalone server mounts them behind its bearer; library users mount them
  behind their own middleware. Non-axum hosts reverse-proxy the standalone
  binary; a framework-neutral handler abstraction is not worth its cost.

**The page never knows its own prefix.** Assets and the one optional endpoint
resolve relative to the document's own location, so `/bpmn-inspector`,
`/admin/debug/wf` or anything else work with zero configuration. A prefix
setting is a knob that exists only to be set wrong.

### Three validation tiers

| Tier | Checks | Where |
|---|---|---|
| L1 | model-only lint | `rbpmn-model` → WASM. Shipped in phase 0-B |
| L2 | model **+ manifest**: missing correlations, phase-gated elements, resolved topics | `rbpmn-core::compile` → WASM. **New export** |
| L3 | are those topics covered by a *running* environment | server: one `GET` returning the covered-topic set |

L2 exists today only inside `deploy`, so no browser tool can see it.
`rbpmn-core`'s entire dependency set is `rbpmn-model + serde + serde_json +
thiserror`, and it compiles clean to `wasm32-unknown-unknown` (verified);
exporting `compile(xml, bindings) -> diagnostics` from `rbpmn-wasm` is what
lets the editor validate the *pair* offline.

L3 collapses to a set of topic names: the environment side is
`Engine::covered_topics()` (today `pub(crate)`), the model side is
`ExecutableProcess::service_topics()`, and the comparison is set subtraction
the page performs itself. So the endpoint returns a list of strings and **the
editor never uploads the model** — a confidential process can be validated
against a production environment without leaving the browser. A dry-run
endpoint that accepts XML cannot offer that, which is why it is rejected below.

**A consequence to enforce: the dsntk prohibition now extends from
`rbpmn-model` to `rbpmn-core`.** This is a live collision, not a hypothetical —
the post-v1 business-rule task would naturally put DMN compile support exactly
there. Decide it before the DMN spike, not during.

**Severity discipline.** `unresolved-topic` keeps its rule id *and* its error
severity. Rule ids and severities are stable public API asserted by the fixture
corpus, and no rule may be contextually downgraded because one surface has less
information than deploy does. Uncertain wiring is therefore **not** a lint
diagnostic in the editor: it is a wiring pane with three explicit states —
*bound*, *defaulted to element id*, *unknown to this server / no server
attached* — which is more informative than a diagnostic list anyway. A new
warn-level, environment-free manifest-hygiene rule stays available later; new
ids are always allowed, renames never.

### What the inspector shows

The diagram alone answers "where", which is rarely the question. The element
pane fuses three sources, all of them already in the payload:

- **static model facts** from `element.businessObject` — bpmn-js holds the full
  moddle object for every element after `importXML`, so this costs no
  dependency and no re-parse
- **runtime state** — the token and its `wait_kind`, work-item
  `state`/`retries`/`last_failure`, timer `due_at`, subscription
  `correlation_key`
- **that element's slice of the event trace**, in order

Two additions complete it:

- `InstanceInspection` gains the deployed **`bindings` manifest** — one extra
  column from `rbpmn_definition`, on a row `inspect_in` already joins. Without
  it only *instantiated* work items reveal a topic, so an unreached service
  task shows nothing; with it, model and manifest are visible resolved against
  each other. This is the inspector feature no other engine's cockpit can have,
  precisely because the manifest is deliberately not in the XML.
- a **diagnosis line** at the top: token at `charge-card`, `wait_kind =
  incident`, retries exhausted, `last_failure = "handler answered 502"`.
  Entirely derivable from today's payload, and it is the actual question the
  operator arrived with.

Variables render in full, by default. This is a supervisor/admin debug tool
over an instance the application already decided this person may see; a
field-level policy engine would be re-answering a question already answered.

Phase 6 note: tokens park inside subprocess planes, and the annotation layer's
`focus()` already switches drill-down roots — the inspector renders nested
planes rather than assuming one canvas.

### What we owe the application, and what it owes us

Full detail lands in `docs/http-security.md`; the shape:

**Ours.** Escaping is not sanitization. Declining a redaction policy (*who may
see this data*) says nothing about correctness (*does an order note containing
`</script>` render as text*). Inlining business data makes that bug class ours,
and one mistake ships to every embedder simultaneously — so it gets the corpus
treatment: hostile fixtures for `</script>`, `<!--`, U+2028/2029 and attribute
contexts, rendered and asserted.

The document also carries its own lockdown. Executable JS is **one** inline
script whose SHA-256 is known at build time and constant across every instance;
the per-instance data goes in `<script type="application/json">`, which is not
executable and can never be promoted to script. That split makes the policy a
compile-time constant we emit ourselves as
`<meta http-equiv="Content-Security-Policy">`:

```
default-src 'none'; script-src 'sha256-…'; style-src 'sha256-…';
img-src data:; connect-src 'none'; base-uri 'none'; form-action 'none'
```

`connect-src 'none'` is the guarantee worth stating out loud: **the document
cannot phone home** — no fetch, no XHR, no WebSocket, no beacon. Tested rather
than asserted, in the `just parity` tradition: a rendered document must contain
exactly one executable script, its hash must match the policy the document
carries, and no `http://` or `https://` reference may appear anywhere in the
bytes.

**Theirs.** Authenticate and authorize the viewer. Embed in
`<iframe sandbox="allow-scripts">` **without** `allow-same-origin` — the two
together let the page remove its own sandbox — so the opaque origin keeps
business data away from the application's cookies and storage. Add
`frame-ancestors` by header (meta-CSP cannot express it), plus
`Cache-Control: no-store` and `nosniff`. Never proxy `/v1` to a browser
audience: deploy is code. And the flip side of the attachable artifact — a
saved inspection document is an uncontrolled copy of business data, to be
treated like a database extract.

### Rejected alternatives

- **An inspector REST API the page calls.** Reintroduces CORS, a token in the
  browser, a third token scope, a prefix knob, and re-tears the snapshot. The
  single-document form gets all of that for free.
- **A dry-run validate endpoint accepting XML.** Uploads the user's model to
  answer a question that is set subtraction, and forfeits "the confidential
  model never leaves the browser".
- **`bpmn-js-properties-panel` in the inspector.** It is an *editing* component
  whose providers write into the moddle tree, and its vendor packs write the
  very `camunda:`/`zeebe:` attributes `no-foreign-implementation` exists to
  warn about. The inspector's pane is read-only and fuses static with runtime,
  which the stock panel cannot do.
- **Restricting the editor's palette to the supported subset.** Tempting, and
  rejected: the linter is the product's front door and teaches *why*. Someone
  who draws an inclusive gateway should meet `no-inclusive-gateway` and its
  parallel+skip-bypass rewrite hint, not silently fail to find the shape.
- **A server-side draft or model store.** Definitions live in git next to the
  code that binds them; a model repository is a CMS and contradicts
  deploy-is-code.
- **Business-key addressing.** `business_key` is nullable, unindexed and
  non-unique (re-running a process for the same order is legal), and nothing
  reads it back. If the convenience is ever wanted it is exactly one resolver
  carrying `correlate()`'s discipline — exactly one match, loud 404 for none,
  409 with the candidate ids for several — plus an index. Not in v1: the
  application already holds the UUID it was given.

## Post-v1: decisions — FEEL / DMN via dsntk
Candidate dependency: `dsntk` (DecisionToolkit, Rust, Apache-2.0/MIT, formerly `dmntk`).
Unlike BPMN, **DMN has a real TCK** — and dsntk submits: 3374/3391 passed, 0 failed,
16 not-supported (April 2026 submission). Independently verified correctness fits this
project; effectively single-maintainer, so pin versions.

**Surveyed at 0.3.0 (August 2026) — the constraint that shapes both routes.**
`dsntk-feel-number` binds Intel's decimal C library via `dfp-number-sys`
(cc-rs), and it sits under `dsntk-feel`, so *every* dsntk crate carries it.
Consequences, measured: no wasm32 (`sys/signal.h` not found — even the parser
alone fails), C FFI and `unsafe` in a tree whose core is
`#![forbid(unsafe_code)]`, 91 transitive crates for the parser and 173 for the
evaluator against `rbpmn-model`'s 4, and `dsntk-feel-evaluator` carries an
unconditional `reqwest::blocking` for FEEL's external-Java bridge (the crate
declares no features, so there is nothing to gate). **dsntk can therefore
never enter `rbpmn-model`** — it would take the linter playground and the
bpmnlint plugin with it. Native-only crates are unaffected.

Sequencing (deliberate):
1. **Business-rule task first** (clean, additive): task evaluates a deployed DMN
   artifact against the instance JSONB, result written back as merge patch; gateways
   still read flags via the tiny grammar. Preserves "decisions computed outside
   control flow, control flow reads results". New task kind + artifact type; zero
   changes to semantic core or condition grammar. `dsntk-feel` / model-evaluation
   crates only, not the whole toolkit. Unblocked: this lives in a native crate,
   so the wasm32 constraint does not bite.
2. **FEEL in sequence-flow conditions second, or never.** It deletes
   the `conditions-feel-subset` restriction and couples control-flow correctness to an external
   evaluator. If added: per-definition opt-in, tiny grammar stays the default.
   Now known to be harder than sketched: conditions are validated at deploy,
   deploy validation lives in `rbpmn-model`, and `rbpmn-model` is the WASM
   crate. So this route forces a choice — a native-only validation path
   (breaking "the playground never lies"), or keeping our parser for
   validation and using dsntk only for evaluation (two grammars, forever).
   Decide that before, not during.

Integration rules (either route):
- Parse/validate all DMN + FEEL at deploy time — new linter rules `dmn-validates`,
  `feel-parses` (same loudly-reject front door).
- DMN artifacts deployed and versioned exactly like process definitions; instances
  pin the decision version.
- FEEL is deterministic except the clock builtins: `now()`/`today()` must route
  through the injected clock or be rejected at deploy — retries/replay require
  deterministic decisions.

## Everything still open — one visible list

Ordering rule: value first, then how much *design* (not code) it still needs.
Anything with unanswered semantics stays behind anything without, because a
design round is the expensive part and the codebase is at its best when the
question is purely internal.

| Item | Why it matters | Complexity | Status |
|---|---|---|---|
| **Embedded subprocesses** | The modelling style the engine exists to serve; unmodellable today | High | **v1, phase 6 — shipped** |
| **Retention** | The failure mode that kills BPM installations; interacts with the new event cursor | Low | **v1, phase 7 — shipped** |
| **Authoring & inspection surfaces** | A `.bpmn` is half-deployable without its manifest, and nothing outside this repo knows the manifest exists; the project's own debugger is dev-only | Medium | **v1, phase 8 — next** |
| Event-stream read horizon | Hold history for a registered slow consumer *beyond* the nominal retention age | Low | Roadmap — purely additive over the phase-7 floor; see phase 7 for why the age subsumes it otherwise |
| `rbpmn_event` time partitioning | Turns retention into `DROP PARTITION` at very large scale | Medium | Roadmap — physical only, no contract change; see phase 7 |
| Non-interrupting boundary events | "Every 30 days while this runs"; rides on scope machinery | Medium | v2 — held out of v1 so it does not double phase 6's risk |
| Expression-valued timers | Deadlines from the variable document; standard `tFormalExpression`, no extension needed | Small | **Shipped** — see the roadmap entry for the rulings taken |
| Link events | Diagram hygiene once big models are common | Trivial | v2 |
| Event subprocesses | "Cancel my order at any point" without boundary spaghetti | Medium | v3 |
| Conditional events | Wake a token when the variable document satisfies a predicate | Moderate | v4 |
| Compensation + cancel + transaction subprocess | The real work; history becomes runtime state | High | v5 |
| Cross-definition messaging (message start/throw) | Model fidelity for choreography; exactly-once emission for remote callers | Medium | **Needs a design round** — buffering, dead-lettering, message-start versioning |
| Instance migration API | Long-lived instances pin their version forever; a five-year process can never get a fix | Very high | **Design only in v1**, needs its own round |
| DMN / business-rule task via dsntk | Decisions as models rather than handler code | Medium-high | Spike in flight (`feel-parity`); see the dsntk section |
| Upgrade escape hatch | Retroactively-stricter lint can refuse to boot with the deploy API unreachable | Low | Queued; matters at the first real upgrade |
| Restricted inclusive gateway | More than the Camunda 7 lineage ever shipped | Moderate | Someday; revisit when fixture discipline is mature |

Cross-definition messaging deserves one clarification, because it looks like a
back door to the banned call activity — and in one sense it is. It unlocks the
call-activity *use case* (decompose into another definition, wait for its
answer) while refusing the *mechanism* that made call activities a correctness
problem: no parent→child reference, no error propagating upward on its own, no
cancellation cascading down, no version welding. The cost, accepted openly, is
that request/reply, timeout and cancellation must be **modelled** — the
timeout is a boundary event the author drew, not implicit engine behaviour.
Referential independence between definitions survives; only the choreography
becomes visible.

## Beyond v1 — roadmap (v2, v3, …)

Nothing below is blocked by the architecture; that is by construction. The v1
primitives (token-per-row, explicit `scope` tree, append-only `event`, pure `step`
core) are the same substrate the full element set needs. Ordered easy → big.

**v2 — Non-interrupting boundary events + link events (both small).**
Non-interrupting boundaries spawn a *second* token while the host keeps
running ("every 30 days while this runs") — a genuinely different concurrency
shape from the interrupting boundaries v1 ships, which is exactly why they are
held back rather than bundled into phase 6. Link events are pure control-flow
goto: pair throw/catch by name at deploy, then it is an ordinary edge in the
internal model. Zero runtime machinery, and worth shipping the moment
hierarchical modelling makes big single-level models common.

*(Embedded subprocesses were the original v2 and moved into v1 — see phase 6.
The Method-and-Style hierarchy they enable is a natural fit for this engine:
Silver-style levels demand exactly the block discipline `balanced-gateways`
already enforces — one start, bounded ends, no flows crossing the boundary —
so the linter and the style teach the same thing. An optional warn-level
"method-and-style" lint pack, labeling conventions and end-state naming, stays
on the table as cheap goodwill for modelers who learned from Silver.)*

**Expression-valued timers (shipped).** `timeDate`, `timeDuration` and
`timeCycle` are typed `tExpression` in the BPMN XSD, not string literals — so
reading a deadline from the variable document is *standard* BPMN, needing no
vendor extension:

```xml
<bpmn:timeDuration xsi:type="bpmn:tFormalExpression">order.slaDuration</bpmn:timeDuration>
```

That is the same `tFormalExpression` mechanism `conditionExpression` already
uses, evaluated by the same FEEL-subset evaluator against the same variable
document, so the machinery is largely present. It pairs naturally with v2's
non-interrupting boundaries: "remind every N days, where N comes from the
contract" is the use case that makes both worth having. Today the spec is
literal-only — validated as ISO-8601 by the linter, resolved against database
time by the projection.

*The failure mode is the whole design, and the naive version is worse than
"never fires".* The projection resolves the spec by casting it in SQL
(`clock_timestamp() + $6::interval`). Passing an unvalidated expression result
into that means Postgres rejects the cast and **aborts the step transaction**:
the token never leaves the *previous* wait state, its work item stays open,
and the worker retries into the same failure forever — a poison pill
surfacing as a generic 500. The other classic outcome, in engines that swallow
the error instead, is a token parked at the catch event with no timer row and
nothing that will ever wake it: silent, permanent, and invisible. Both are
unacceptable here. The requirement is therefore: **resolve and validate inside
the pure core, before any SQL**, and on failure raise it the way a service
task failure is raised — an incident the instance freezes at, diagnosable by
inspection, with a repair API's single resume point. Better still if it is
*catchable* by an error boundary, so a modeller can handle "no SLA configured"
in the diagram rather than in an operator's runbook.

Two rulings to take before building it:

1. **Deploy-time validation genuinely weakens, permanently.** Today a bad
   timer spec cannot reach runtime — the compiler-validates-types property
   this engine leans on. "Is `order.slaDuration` a valid ISO-8601 duration?"
   is unknowable at deploy, because variables are one opaque JSONB document
   with no declarations. This moves a class of error from deploy to runtime,
   which is the trade the brief refuses elsewhere; it should be accepted
   explicitly, not drift in with the feature.
2. **`timer-armed`'s `Display` is stable API.** A trace presumably wants both
   the source expression and the resolved instant — but the resolved instant
   is input-dependent, which makes scenario fixtures depend on variable data
   in a way they currently do not.

*The lint becomes a warning, not an error* (Timo's call): a non-literal timer
spec is valid BPMN, and the standalone linter also serves models targeting
other engines, so erroring on it is wrong. The warning says what it can
honestly say — *this deadline is computed at runtime; rbpmn cannot tell you
ahead of time whether it will resolve to a valid duration, and if it does not,
the instance raises an incident there.* **Ordering matters:** the warning ships
*with* the runtime support, never before it. Relaxing the lint while the
compile step still refuses expression timers would only move a clear rejection
somewhere less clear, and relaxing both before the core can resolve them is
exactly the "seems to run" this project refuses. Until then the existing
`NotYetExecutable` phase pointer stands.

One case no lint can catch: a *valid but nonsensical* value — a `timeDate` in
the past, a negative duration — fires immediately. That may be exactly right
("the deadline already passed") or a bug, and nothing but the model's author
can tell which.

*What building it changed, and then changed back.* One assumption in the plan
above was wrong, and the corpus caught it immediately: the two grammars are
**not** disjoint. `P30X` is a mistyped duration *and* a syntactically valid
FEEL qualified name. The first fix was to require the spec's own marker,
`xsi:type="bpmn:tFormalExpression"`, to opt into the reference form.

That was wrong for a reason no amount of reading the XSD would have revealed:
**bpmn-moddle emits the marker for any expression object**, so every bpmn-js
modeler — this repo's own editor, Camunda Modeler — stamps it on ordinary
literal durations. Verified by round-tripping through the repo's own
bpmn-moddle: `moddle.create('bpmn:FormalExpression', {body: 'P3D'})`
serialises `<bpmn:timeDuration xsi:type="bpmn:tFormalExpression">P3D</...>`,
while a literal read from XML stays `bpmn:Expression` and round-trips clean.
Keying off the marker therefore turned `P3D` typed into a properties panel
into a variable lookup named `P3D`. A marker that tooling writes
unconditionally carries no authorial intent.

**The rule is parse order** (Timo's call): a spec that parses as ISO-8601 is a
literal; one that does not is read as a qualified name. Intuitive, and it is
what a modeller expects. The cost is accepted deliberately: a mistyped
duration shaped like a name (`P30X`, `P999999999W`) now falls through to a
warning rather than erroring, so two fixtures moved from `reject/` to
`accept/24-timer-typo-reads-as-variable.bpmn`. What keeps that honest is the
warning text — it carries the ISO-8601 complaint that *made* it fall through
("duration needs at least one component"), so an author reads why it is not a
duration and what it will be treated as instead, on one line. Component
bounds keep their error coverage through a `timeDate`, whose `-` and `:`
cannot be a qualified name.

A freeze mid-entry has to close what the entry had already opened — the
host's work item, and a subprocess's just-allocated scope. Neither was
obvious: `cancel_attachments` withdraws timers and subscriptions only, so the
first version left an `available` work item on a failed instance and a scope
row with no members whose owner had become an incident. Harmless only while
claimability requires `status = 'active'`; a repair API clearing the incident
would have handed a worker an item whose token was parked at one.

The subset is a FEEL **qualified name** (`order.slaDuration`), not general
FEEL — the same strict subset correlation keys already use, so it stays
syntactically and semantically valid when dsntk lands. `TimerSource` and
`TimerDue` are deliberately distinct types rather than one enum with a
"resolved" flag: the resolved form is what the projection stores, so an
unresolved expression cannot reach the SQL cast by construction rather than
by care. And `Event::TimerArmed`'s `Display` did not change — it shows the
resolved value, so the golden format stayed stable while the traces gained a
new sibling event, `timer-resolve-failed`, whose prose reason is deliberately
*outside* the Display format so improving the message cannot break a trace —
and therefore carried separately on `EventView::detail`, because a reason
that no read path can reach is a reason that does not exist.

**v3 — Event subprocesses (cheap-ish).** Scope-attached event handlers with
interrupting and non-interrupting starts. Reuses v2's scope/teardown machinery plus
the existing subscription kinds (message, timer, later error). The clean way to model
"cancel my order at any point" without boundary-event spaghetti.

**v4 — Conditional events (moderate).** "Wake this token when the variable document
satisfies a predicate." Clean hook exists: every variable write is a merge patch in a
known transaction — re-evaluate waiting `kind=condition` subscriptions there. The
FEEL-subset condition grammar keeps evaluation cheap and deterministic; a `subscription`
row fits as-is. Deploy rule: conditional predicates use the same grammar as
sequence-flow conditions.

**v5 — Compensation + cancel + transaction subprocess (the real work).**
Compensation needs "completed activities, in reverse completion order, per scope" —
which is literally the `event` table; handlers attach per scope; execution is
scope-local, so `balanced-gateways` is unthreatened. Required deploy rule:
compensation-enabled definitions force the relevant event kinds to be written
regardless of configured history level — compensation turns history from debugging
into runtime state. Cancel events are compensation's sibling (transaction subprocess
= scope + compensate-on-cancel) and ship in the same release.

**Maybe, someday — a restricted inclusive gateway.** Local counting semantics
(split records N activated branches; join waits for N, decremented by quiet exits)
are *provably correct* under exactly the block structure `balanced-gateways` already
enforces. A block-structured OR-gateway with correct local semantics would be more
than the Camunda 7 lineage ever shipped. Revisit only when fixture discipline is
mature; until then the parallel+skip-bypass rewrite hint stands.

Permanent exclusions remain choices, not impossibilities: unbalanced/unstructured
graphs, and call activities (referential independence between definitions is a
principle). The true cost of every roadmap step is combinatorial, not architectural:
each element multiplies the fixture corpus (compensation × multi-instance × boundary
timer …). Budget fixtures per release, not code.

## Non-goals (write them down so nobody "helpfully" adds them)
- Inclusive/complex gateways, call activities, compensation, BPEL anything
- Embedded scripting; expression languages in v1 (see "Post-v1: decisions" for the
  planned DMN/FEEL route — business-rule task, not gateway conditions)
- A modelling *engine* (we embed bpmn-io's) and an operations cockpit — no
  writes from any UI, no instance lists, no search, no scheduling views. Phase
  8's editor and read-only inspector are deliberately narrower than both; the
  constraints that keep them there are in "Authoring & inspection surfaces"
- Horizontal scale beyond one Postgres (the DB is the honest ceiling; it is high)
- History levels beyond event-kind filtering; no separate history store
