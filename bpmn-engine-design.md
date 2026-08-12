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
Event ordering guarantees in `event`, retention jobs, instance migration API
(design only). Embedded subprocesses + non-interrupting boundary timers are the
head of the post-v1 roadmap (v2, below) — the first release after v1, as
hierarchical BPMN is the modeling style this engine exists to serve.

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
Retention jobs, the instance migration API, cross-definition messaging and
the upgrade escape hatch remain open — each needs a design round first.
*(bpmnlint plugin packaging and the token-overlay debug view were pulled forward:
plugin → phase 0-B, token overlay → phase 2 exit criterion.)*

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

## Beyond v1 — roadmap (v2, v3, …)

Nothing below is blocked by the architecture; that is by construction. The v1
primitives (token-per-row, explicit `scope` tree, append-only `event`, pure `step`
core) are the same substrate the full element set needs. Ordered easy → big, except
subprocesses lead deliberately.

**v2 — Embedded subprocesses: hierarchical BPMN (Bruce Silver, Method and Style).**
First not because it is easiest but because it is the modeling style this engine
exists to serve: a top level showing the happy path as a row of collapsed
subprocesses, each expanding into its own level, recursively. This is a natural
fit — Silver-style hierarchy demands exactly the block discipline `balanced-gateways`
enforces (each level: one start, bounded ends, no flows crossing the boundary), so
the linter and the style teach the same thing. Runtime cost is the `scope` machinery
v1 already carries for boundary events and terminate: scope-per-subprocess, nested
teardown, boundary events on the subprocess itself. Consider an optional
"method-and-style" lint pack (warn-level: labeling conventions, one none-start per
level, end-state naming) — cheap goodwill for modelers who learned from Silver.
Non-interrupting boundary timers land here too (the "every 30 days while this runs"
pattern).

**v3 — Link events (trivial).** Pure control-flow goto: pair throw/catch by name at
deploy time; becomes an ordinary edge in the internal model. Zero runtime machinery.
Mostly a diagram-hygiene feature for large single-level models — worth shipping the
moment v2 makes big models common.

**v4 — Event subprocesses (cheap-ish).** Scope-attached event handlers with
interrupting and non-interrupting starts. Reuses v2's scope/teardown machinery plus
the existing subscription kinds (message, timer, later error). The clean way to model
"cancel my order at any point" without boundary-event spaghetti.

**v5 — Conditional events (moderate).** "Wake this token when the variable document
satisfies a predicate." Clean hook exists: every variable write is a merge patch in a
known transaction — re-evaluate waiting `kind=condition` subscriptions there. The
FEEL-subset condition grammar keeps evaluation cheap and deterministic; a `subscription`
row fits as-is. Deploy rule: conditional predicates use the same grammar as
sequence-flow conditions.

**v6 — Compensation + cancel + transaction subprocess (the real work).**
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
- A modeler (use bpmn-io), a cockpit (the token-overlay debug view is the 80%)
- Horizontal scale beyond one Postgres (the DB is the honest ceiling; it is high)
- History levels beyond event-kind filtering; no separate history store
