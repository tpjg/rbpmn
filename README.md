# rbpmn

A **correctness-first BPMN execution engine** in Rust on PostgreSQL. Embeddable
as a library; a small optional HTTP server for everyone else. A deliberately
**restricted subset of BPMN 2.0**, enforced at deploy time: *it is better to
loudly reject a model than to silently execute it wrong.*

The full rationale — why tokens-per-row, why no inclusive gateway, why no call
activities, why block structure — lives in [bpmn-engine-design.md](bpmn-engine-design.md).
Read it before touching the semantics.

**PostgreSQL 18 or newer is recommended.** Older versions are correct but
slower on some read paths; 13 is the floor the schema needs.

## Workspace

| Crate / package | Purpose |
|---|---|
| `crates/rbpmn-model` | BPMN XML → internal model + the linter + the FEEL-subset parser/evaluator. Dependency-light (no IO, no async, no DB) so it compiles to WASM for the linter playground and the bpmnlint plugin. |
| `crates/rbpmn-core` | The pure semantic core: `compile` → executable model, tokens, and the `step` function. No IO — the Postgres layer projects it. |
| `crates/rbpmn-engine` | The PostgreSQL projection: transactional stepping over the core (with `*_in_tx` variants sharing the caller's transaction — process transitions commit atomically with business writes), atomic idempotent deploys, the growing persistent environment, leases, retries with backoff, incidents. |
| `crates/rbpmn-dmn` | Decisions: DMN validation and FEEL evaluation over dsntk — from [a fork](https://github.com/tpjg/dsntk) whose defaults are a pure-Rust decimal128 in place of Intel's C library and no HTTP client for FEEL's external-Java bridge. **The one crate where dsntk is allowed** — nothing upstream of it may depend on it, which is what keeps `rbpmn-model` and `rbpmn-core` on wasm32. See [docs/dmn.md](docs/dmn.md). |
| `crates/rbpmn-wasm` | Thin wasm-bindgen surface: `lint(xml)` (model only), `check_deployable(xml, bindings, decisions)` (model, manifest **and** bundled DMN — everything deploy decides without a database), `evaluate_decision(...)`, `catalogue()`. |
| `crates/rbpmn-server` | Small standalone HTTP server wrapping the engine. Bearer-token auth, loopback-only by default. See [docs/http-security.md](docs/http-security.md). |
| `crates/rbpmn-ui` | The two UI documents as self-contained HTML: a read-only instance inspector (`render_inspection`, a pure function over an `InstanceInspection`) and the model+manifest editor. No API, no credentials in the browser. |
| `playground/` | Local linter playground (bpmn-js + WASM): fixture browser, live re-lint, diagnostics as diagram overlays. `just playground`. |
| `ui/` | Source for the two documents; `just ui` builds them into `crates/rbpmn-ui/assets/` (build output, gitignored — a one-time bootstrap after cloning). |
| `bpmnlint-plugin-rbpmn/` | bpmnlint plugin backed by the same WASM — rbpmn's rules inside bpmn-io tooling, zero JS reimplementation. |

v1 is phases 0–7: everything below plus **embedded subprocesses** (phase 6)
and **retention** (phase 7). Subprocesses were promoted out of
the v2 roadmap deliberately — hierarchical modelling is the style this
engine exists to serve, and it is unmodellable without them. Everything
else, including cross-definition messaging (message start/throw between
definitions, which lint clean today but refuse to compile) and the instance
migration API, is listed with its status in the design brief's
[open-items table](bpmn-engine-design.md#everything-still-open--one-visible-list).

**Decisions (DMN) landed after that plan and changed it** — the brief listed
them as post-v1, and they are in the default build now. A workflow definition
plus its bindings manifest is meant to be a *fully executable* flow, and a
decision turned out to be part of that definition rather than an add-on to it.
[docs/dmn.md](docs/dmn.md) is the record: what was decided, what was measured,
and what deviates.

## Status

- [x] **Phase 0 — Parse & reject**: parser, full linter rule catalogue,
      fixture corpus (27 accept / 34 reject today — every phase adds its
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
      result (carrying the item's state, so a frontend can tell *reassigned*
      from *withdrawn by the process*), owner-checked
      `complete_task`/`fail_task`/`release_task` (the
      third exit from a claim: hand it back undecided, claimable again at
      once instead of after the lease runs out — scoped to the claim's lease
      epoch, so a retried release cannot free the claim that replaced it),
      equality filters over the instance's **live** variables, `count_tasks`,
      and
      `declare_index` (partial expression indexes, also declarable in the
      deploy manifest; index usage verified by test against the real query
      path). Server: `POST /v1/tasks/{get,count}` and
      `/v1/tasks/{id}/{extend,release,complete,fail}`. A claim is not a
      promise the process will wait: when the process withdraws a claimed
      task — an interrupting boundary fired, the instance terminated — the
      holder's `complete` answers `alreadyClosed` with `state: "cancelled"`
      and **its patch is not applied**, so an application that wants the
      holder's decision kept must keep it itself. Nothing is pushed to the
      holder either: it learns at its next heartbeat, which makes the
      renewal interval the detection bound.
- [x] **Phase 5 — rounding out**: the event-stream tailing contract
      (`read_events` / `GET /v1/events`, ordered and cursored by
      `(txid, id)` behind a safe horizon so a cursor cannot miss an event),
      MIT/Apache-2.0 dual licensing, and the stress/fuzz/chaos tier
      ([docs/stress-testing.md](docs/stress-testing.md)) — model generation,
      state-space exploration, replay verification, the storm, and chaos
      runs that kill processes and sever database connections mid-flight,
      reporting exactly-once and clean completion throughout. BPMN has no
      execution TCK; this is the assurance in its place.
- [x] **Phase 6 — embedded subprocesses**: the live scope tree (one runtime
      scope per entry, so two loop iterations never see each other's joins),
      scope-local join counting, subprocess-as-wait-state, interrupting
      teardown of a whole scope subtree in one transaction, **error
      boundaries on a subprocess** — an error propagates outward to the
      nearest enclosing handler — and scope-local terminate (ends the
      subprocess, not the instance). Hierarchical modelling, the style the
      engine exists to serve, is now executable.
- [x] Phase 7 — **retention**. One age per definition; a record retires
      whole — instance row, children and events, in one transaction, after
      the archive sink (if any) has a complete copy. A **monotonic truncation
      floor** keeps the phase-5 cursor contract honest: everything deleted is
      at or below it, so a cursor above it has provably lost nothing, and a
      resume from below it fails with `CursorTruncated` (HTTP 410, carrying
      the floor as fields) instead of silently skipping the gap. A pass is
      `plan` → `archive` → `execute` with **no transaction across the
      archive**, which is what makes export-before-delete possible without
      stalling every stream reader in the cluster; a sink failure deletes
      nothing, on every path. `rbpmn_event.instance_id` became a real foreign
      key here, so "an event never outlives its instance" is enforced rather
      than asserted. Active instances, failed ones (frozen evidence, at any
      age) and definitions are never swept — the last only ever by hand, via
      `delete_definition`. Opt-in twice over: no sweeper unless you start one,
      and `RetentionPolicy::forever()` is a valid choice.
- [x] Phase 8 — **authoring & inspection surfaces**. Two self-contained HTML
      documents, neither of which is a cockpit. The **inspector** renders one
      instance read-only, with its data inlined rather than fetched — so
      there is no API to protect and the embedding application's
      authorization check is the only gate. It fuses static model facts,
      runtime rows and each element's slice of the trace, opens with a
      *diagnosis* rather than a diagram ("Incident at `charge` — retry budget
      exhausted — handler answered 502"), and shows the deployed **bindings
      manifest**, which is the only place the wiring of an element the token
      never reached can be recovered from. The **editor** authors the
      model+manifest pair together: bpmn-js for the diagram, a hand-written
      properties pane restricted to standard BPMN (no vendor providers, so
      XML purity holds by construction), a wiring pane for the manifest, and
      live L1+L2 validation from the same code deploy runs, compiled to
      wasm32. Its one optional server call fetches the **covered topic set**
      for `unresolved-topic` — a list of names, so the model never leaves the
      browser. Each document carries its own CSP, narrowed to what it
      actually does: the inspector — the one holding business data — gets
      `connect-src 'none'` and so cannot phone home at all, while the editor
      gets `'self'` for that single call and carries no instance data to
      leak. See [docs/http-security.md](docs/http-security.md) for what the
      embedding application still owes its users.
- [x] Phase 9 — **decisions (DMN + FEEL)**, on by default. A business-rule
      task parks, the projection evaluates inside the same transaction, and
      the answer re-enters the pure core as command data — so a replay reads
      the recorded answer instead of running an evaluator, and the core still
      cannot evaluate anything. DMN artifacts travel *inside* the deployment
      (`{bpmn, bindings, decisions}`, one content hash over all three), which
      is what makes `unresolved-decision` decidable with no environment at
      all: unlike a topic, a decision cannot be missing at deploy time
      because it is in the bundle. The wiring stays out of the XML like every
      other binding. Under it, **dsntk** — 100k lines, DMN 1.3/1.4/1.5, the
      DMN TCK — reached wasm32 by replacing its decimal128, which binds a C
      library, with a pure-Rust one; that substitution is *verified* against
      the library it replaces over 26 300 differential comparisons and 3 391
      TCK cases, not argued. Its HTTP client is replaced by one that refuses:
      a decision must not call out, to Java or anything else. The editor
      authors the whole bundle — dmn-js for the decision, and the same
      validator deploy runs, in the browser, offline.
      [docs/dmn.md](docs/dmn.md) has the decisions, the gates and the
      measured deviations.
- [x] Phase 10 — **boundary events beyond v1**, in slices. *Interrupting
      message boundary events* on user, service and receive tasks and on
      embedded subprocesses: a message correlated to an instance parked at a
      task withdraws the task and takes the boundary path; a holder who had
      the task claimed gets `alreadyClosed` with `state: "cancelled"` and its
      patch is never applied, and `lockLost` now carries the item's state so a
      frontend can tell *withdrawn by the process* from *reassigned*. Then
      *non-interrupting* boundary events — message, and single-shot timers —
      which start a sibling token while the host keeps running, under the new
      rule `boundary-side-path` (a side path ends at its own end event and
      never merges back: it would run the continuation twice, or deliver a
      second token to a join). And repeating timers — `timeCycle` on a
      non-interrupting boundary, `R[n]/P…` or anchored
      `R[n]/<datetime>/P…` with a fixed-length period — where every
      occurrence steps from the *previous due* (a late scheduler never drifts
      the schedule), an occurrence missed while the engine was down is
      skipped rather than replayed (an outage is not a backlog), and the
      anchor fixes the phase rather than replaying the past; "every 7 days
      while waiting for payment, add a late fee" is one boundary now, not a
      loop.
      [docs/design/boundary-messages.md](docs/design/boundary-messages.md)
      is the record — including the two things building it found that the
      design had not: a timer boundary on a business-rule task was a dead arm
      lint accepted, and a re-arming boundary makes the explorer's state space
      infinite without a bound.

## Rule catalogue

Rule IDs are **stable public API** — fixtures, the playground and the bpmnlint
plugin all assert on them. Rules marked ⁺ are structural prerequisites added
beyond the design brief's initial list (the region analysis is only sound on
graphs that pass them).

| Rule | Severity | Meaning |
|---|---|---|
| `no-inclusive-gateway` | error | Inclusive gateways rejected entirely; rewrite as parallel split + exclusive skip-bypass per branch. |
| `no-call-activity` | error | Definitions are islands; interact via message throw → message start/catch with correlation keys. |
| `no-unsupported-element` | error | Anything outside the supported subset (script/send/manual/abstract tasks, call activities, signals, multi-instance, a `timeCycle` anywhere but on a non-interrupting boundary, …). |
| `balanced-gateways` | error | Every parallel split has a matching join; branches stay disjoint; nothing enters/escapes the region; each branch delivers exactly one token; no plain end events inside (terminate allowed). |
| `single-start-event` | error | Exactly one start event per process and per subprocess. |
| `conditions-feel-subset` | error | Conditions only on exclusive-split flows, in the strict FEEL subset (`name op literal`, `and`/`or`, parentheses); default flow required. Full-FEEL constructs (functions, arithmetic, ranges) are rejected. |
| `timer-iso8601` | error | Timer definitions must be valid ISO-8601 — dates require an explicit UTC offset, component magnitudes bounded; a `timeCycle` is `R[n]/P…` or `R[n]/<datetime>/P…` with a fixed-length period (weeks, days, hours, minutes, seconds — never months or years; at least one minute, at most a million repeats), `R0` and the `/end` forms refused — **or**, failing that, a FEEL qualified name naming the deadline in the variable document. Text that is neither is the error. Parse order is the only discriminator: `xsi:type="bpmn:tFormalExpression"` is deliberately ignored, because bpmn-moddle stamps it on every expression object and so every bpmn-js modeler writes it on ordinary literals. |
| `timer-expression`⁺ | warn | A timer whose deadline is read from the variable document cannot be validated ahead of time — if it does not resolve to a valid ISO-8601 value at arm time, that element raises an incident rather than firing. |
| `message-has-correlation` | error | Message start/catch/throw events, receive tasks and message **boundary** events must reference a *named* message. The correlation binding itself (a FEEL qualified name) is registered via `Bindings::correlation`, keyed by the element's own id — for a boundary, the boundary's id, never its host's — and checked at deploy. |
| `no-foreign-implementation` | warn | Service task carries vendor attributes (`camunda:`, `zeebe:`, …), which rbpmn ignores — topics are bound at registration. |
| `unresolved-topic` | error | Every service task's topic (via `Bindings::topic`, default: element id) must have a registered handler or a declared external-worker topic. Checked at deploy against registration state, so `lint(xml)` alone cannot decide it. |
| `boundary-on-supported-host` | error | Boundary events only on service/user/receive tasks and subprocesses; error boundaries only where errors can originate. Never on a business rule task: its decision is answered inside the transaction that starts it, so a boundary there is armed and cancelled in one step and can never fire. |
| `boundary-side-path` ⁺ | error | A non-interrupting boundary spawns a *second* token beside its host's, and no block-structure proof covers it — it entered through no split. So its path must be a **side path**: disjoint from everything else in the scope, ending at its own plain end event (a terminate end is allowed too). It may not rejoin the flow after the host (the rest would run twice) or reach a parallel join (which would collect a second token on one incoming flow), and it may not carry a parallel block of its own: the boundary can fire again while an earlier side token is still inside it, and both activations run in the host's scope — wrap the block in an embedded subprocess, which gives each activation its own scope. For "remind, then wait again", use an interrupting boundary and a loop. |
| `side-path-message-arm` ⁺ | warn | A message arm (catch, receive task, message boundary) on a side path is armed once per activation of its non-interrupting boundary, and an earlier activation's arm may still be open: unless each activation changes the correlation key (a delivery patch can), the second arm freezes the instance — `duplicate-subscription`, loud, at arm time. Including an arm inside an embedded subprocess on that path, at any depth: a subprocess gives each activation its own *scope*, which is what makes a parallel block there safe, but a subscription is keyed by (message, key) across the whole instance and a scope narrows it by nothing. |
| `ambiguous-message-arm` ⁺ | error | Two message arms for the same message *and* the same correlation binding that can be live at once — two message boundaries on one host, a receive task and its own boundary, a subprocess boundary and a catch inside it, and a non-interrupting message boundary with an arm for the same pair on its own side path (it re-arms and spawns the side token in one step, so the *first* delivery freezes). Every delivery would be ambiguous, so deploy refuses it. Decided at L2 (`check_deployable`), because with different bindings both arms are legitimate and only the manifest knows. |
| `no-implicit-split` | error | Activities have at most one outgoing flow; splitting happens at explicit gateways. |
| `implicit-merge-after-parallel` | warn | Implicit merge receiving concurrent tokens — the "task runs twice" trap (accompanies the balanced-gateways error). |
| `bpmn-structure` ⁺ | error | Well-formedness: resolvable refs, flow cardinalities, connectivity, unique ids, error definitions. |
| `no-mixed-gateway` ⁺ | error | A gateway either splits or joins, never both. |
| `event-gateway-structure` ⁺ | error | Event gateway races ≥2 message/timer catches or receive tasks, each with exactly one incoming flow and no boundary events (the gateway itself is the race). |

DMN rules apply to the decision artifacts a deployment bundles, and are
implemented in `rbpmn-dmn` — the one crate where dsntk is allowed
([docs/dmn.md](docs/dmn.md)). They report the same `Diagnostic` type, so a
decision error and a model error read the same way.

| Rule | Severity | Meaning |
|---|---|---|
| `dmn-validates` ⁺ | error | The artifact is DMN and its decision logic builds. A decision that cannot be compiled cannot be deployed. |
| `feel-parses` ⁺ | error | Every FEEL expression in the artifact parses — literal expressions, decision-table entries, item-definition constraints and the rest. dsntk is the authority here: FEEL names may contain spaces and operators, so parsing needs the model's own scope. |
| `feel-deterministic` ⁺ | error | No `now()`/`today()` (dsntk answers them from the *node's* local timezone) and no external Java or PMML functions. Time enters a decision as an input, never from a clock. Deliberately conservative: it errs toward refusing. |
| `decision-has-binding` ⁺ | error | A business-rule task's decision binding lives in the manifest, never in the XML, and must be well-formed: a decision name, and a FEEL qualified name for where the answer lands. |
| `unresolved-decision` ⁺ | error | Every bound decision names exactly one invocable in the bundled DMN. Unlike `unresolved-topic` this needs no environment — decisions travel *inside* the deployment, so the verdict is complete offline. Ambiguity is refused rather than resolved by picking. |
| `config-binds-task` ⁺ | error | A manifest config entry is a JSON object keyed by a service or user task — the elements that produce a work item to deliver it on. Stricter than a stale key in the other manifest groups, which is only an editor warning: those have a default, so an override for an element that is not there overrides nothing, while config has none and an entry nothing delivers is wiring that silently never arrives. |

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

All of it goes through one `Bindings` value — a fluent builder in the library,
the same struct deserialized from the deploy body's `bindings` JSON on the
server. Two syntaxes, one manifest, one validation path.

| Wiring | Registration API | Deploy check |
|---|---|---|
| Service-task topic | `Bindings::topic(element_id, topic)`; default topic = element id. `declare_topic(name)` announces pull-mode workers, and is *environment* rather than manifest | `unresolved-topic` |
| Message correlation | `Bindings::correlation(element_id, "order.id")` — FEEL qualified name into the instance variables; a message boundary event is bound by its **own** id, not its host's | `message-has-correlation` |
| Decision | `Bindings::decision(element_id, decision_name, "order.discount")` — which decision a business-rule task invokes, and where its answer lands | `decision-has-binding`, `unresolved-decision` |
| Filterable fields | `Bindings::index(field)` (this definition) or `Bindings::shared_index(field)` (across definitions) — optional, performance only | — |
| Task config | `Bindings::config(element_id, json!({"template": "warning_first"}))` — free JSON delivered beside the variables on every work item that element produces, never interpreted | `config-binds-task` |

Per-definition wiring deploys **atomically with the definition** as a small
JSON bindings manifest (`deploy(bpmn_xml, bindings)` in the library, one
`POST /v1/definitions` body on the server) and is versioned with it — the same
information other engines smear into vendor XML annotations, separated cleanly
and reviewable in git next to the `.bpmn`. Environment capabilities (handler
targets, `declare_topic`) are engine/server configuration, not manifest content.

### Config is model content, not runtime configuration

`Bindings::config` is the one manifest group whose value is the application's
own, so it is the one that attracts things which do not belong in a model. The
manifest is inside `content_hash` and an instance is pinned to the version it
started on, which means **changing config is a deploy, by construction**, and
running instances keep the config they started with.

One deciding question: **must this change with a deploy?**

- **Yes** — a document template, a form name, a threshold that is part of what
  the model *does*. That is config, and hashing it is the point: two
  installations on the same `content_hash` are running the same model, letters
  included.
- **No** — an endpoint URL, a credential, a per-environment switch. Those are
  not model content and must not be in the manifest, because the only way to
  change a hashed manifest is to deploy. They belong to the environment half,
  or to the application's own store.

For that second case the mechanism is unchanged and fully supported: every
claimed task carries `definition_id` and `definition_version`, the exact
version the instance is pinned to, and an application resolves whatever it
likes against that pair. Config did not replace it — the two answer different
questions, and `docs/design/task-config.md` (D7) is the long form.

rbpmn never interprets a config value at any depth: no FEEL, no interpolation,
no defaults per topic. The instance variables travel beside it on the same work
item; composing the two is the handler's job.

### Declared indexes have a scope

`Bindings::index(field)` builds a partial expression index over one
definition's instances, predicated on its `definition_key`. That is exactly
what the engine's own `TaskFilter` needs — it always carries a definition key,
and the literal is what lets the planner prove the predicate — and it keeps
the index as small as the definition it serves.

`Bindings::shared_index(field)` builds **one** index per field, across every
definition that declares it, for the other query real applications have:
resolving a business identifier — an order number, a customer reference, an
external case id — to whichever instance carries it, without knowing which
workflow or which deployment that is. Postgres can prove a partial index's
predicate only from an equality against a constant, so `definition_key =
any($1)` cannot use the definition-scoped indexes at all; measured, it plans
as a bitmap scan on the definition-key index with the hoisted field demoted to
a recheck filter. The recourse without a scope is to unroll one
`definition_key = $n` branch per key — tolerable at three definitions,
untenable at a hundred, and a hundred catalogue entries over one identical
expression.

A shared index is partial on `(variables->>'field') is not null`, which costs
nothing and saves a lot: an equality against the expression is strict and so
implies `is not null`, keeping the index usable, while instances of
definitions that never carry the field stay out of it entirely.

**What rbpmn cannot check.** A shared declaration asserts that the field name
means the *same thing* in every definition that declares it. `variables` is
opaque to the engine by design; nothing here verifies that, and nothing can.
It is the application's contract, and declaring `shared` is the application
asserting it. Where one definition declares a field `shared` and another
declares it `definition`-scoped, deploy logs a warning — not an error, because
two indexes over one expression is a legitimate choice (a `TaskFilter` served
only by a shared index degrades to a `BitmapAnd`), and not a diagnostic,
because it is an operator fact about other deployed definitions that no
offline surface can see.

Both spellings live in the manifest, and the string form is unchanged:

```json
{ "indexes": ["channel", { "field": "order_no", "scope": "shared" }] }
```

An unknown scope is refused at deploy, naming the valid ones — never
defaulted. So is one manifest declaring the same field at both scopes.

**Nothing ever drops a declared index**, of either scope: not
`delete_definition`, not retention, and not removing the field from a
manifest. That is deliberate — a shared index belongs to no single definition,
so one going away says nothing about whether another still needs it, and
reference counting would race a concurrent deploy. `Engine::declared_indexes()`
is the read-only audit that makes leftovers visible (an entry with an empty
`declared_by` is an orphan, safe to drop by hand); `declared_index_name` and
`shared_index_name` locate them.

**Migration.** Existing deployments are untouched: their per-definition
indexes keep their names, their DDL and their content hashes, so a redeploy
does not even allocate a new version. Adopting `shared` is a manifest edit and
a redeploy. The old index is not removed — if the definition still declares
the field definition-scoped, it still has it.

### The published read surface

Applications legitimately need to *join* rbpmn's state against their own rows.
A result set of "our tenancy, our ordering, rbpmn's instances" is a SQL join,
and no API returning data instead of SQL does it as well. The answer to "stop
reading my schema" is not to stop reading; it is to publish the surface. So
rbpmn publishes three views as **public API**, on the same footing as rule ids
and the `Display` format of `Event`, one per thing an instance can be doing:

| view | the question it answers |
|---|---|
| `rbpmn_v_definition` | what is deployed, at which version, and from which artifacts |
| `rbpmn_v_definition_decision` | the DMN artifacts a version was deployed with |
| `rbpmn_v_instance` | what is running, and what does it hold |
| `rbpmn_v_work_item` | what is waiting to be worked, and how deep is each queue |
| `rbpmn_v_timer` | when does this next happen |
| `rbpmn_v_subscription` | what is waiting on this business identifier |

The last three are the three things an instance can be *waiting* on — a
worker, a clock, a message — so between them there is no wait state an
application has to read an undocumented table to see. The first two are what
is deployed rather than what is happening: the question behind them is
reconciliation, "is the model running here the one in git?", which is
`content_hash` against a hash of the bundle.

They compose on `instance_id`, so a statement can group deadlines and queue
depths by an application's own dimensions at once. The constants `rbpmn_engine::{DEFINITION_VIEW, DEFINITION_DECISION_VIEW,
INSTANCE_VIEW, WORK_ITEM_VIEW, TIMER_VIEW, SUBSCRIPTION_VIEW}` name them, so a rename is a compile error for
callers rather than a runtime surprise.

**One contract, for all three.** Columns may be added; none will be removed or
repurposed. Each is deliberately a **plain inlinable projection** — no `WHERE`,
no `LIMIT`, no `DISTINCT`, no `ORDER BY`, no aggregate, no volatile function
(`now()` is STABLE, which is what makes time-dependent columns legal), and
explicitly **not** `security_barrier`. A barrier view refuses to push an
outside predicate below itself unless the operators are leakproof, and
`jsonb ->>` is not one, so every declared variable index would sit unused
beneath a full scan. Each has an EXPLAIN-based test asserting the *plan*
through the view, not just its shape.

**None of them is a tenancy boundary.** They do no row filtering and rbpmn
manages no grants: what a connection can see, it can see. An application that
needs a boundary expresses it in its own query, which is the whole reason this
surface is SQL.

**And none of them is a claim.** They are read models: a value is true when it
was measured. A depth of five does not reserve five items, an armed timer is
not a promise about when it fires, and the only way to *hold* work is
`get_task`.

#### `rbpmn_v_definition` and `rbpmn_v_definition_decision`

| column | |
|---|---|
| `id` | the definition id `deploy` returns |
| `key`, `version` | the stable pair everything else joins on |
| `content_hash` | sha256 of the bundle — deploy's own idempotency key |
| `deployed_at` | when this version landed |
| `bpmn_xml`, `bindings` | the artifacts that are 1:1 with a definition |
| `retired_instances` | why `delete_definition` may refuse a version that looks unreferenced |

The DMN artifacts are 0..N, so folding them in would need an `array_agg` and
that would stop the view being an inlinable projection — the same reason
`rbpmn_v_subscription` leaves ambiguity to a query. They get
`rbpmn_v_definition_decision` (`definition_id`, `definition_key`,
`definition_version`, `ordinal`, `dmn_xml`), joinable either way. **Read them
ordered by `ordinal`:** artifacts may import one another, so deployment order
is part of the deployment.

⚠️ `bpmn_xml` and `dmn_xml` are whole documents. `select *` here pulls every
model in the installation across the wire — name your columns, and reach for
the XML only when the answer *is* the model. The deployment inventory asked
for most often is `Engine::DEPLOYED_NOW_SQL`, which selects no XML at all:

```sql
select distinct on (key) key, version, content_hash, deployed_at
  from rbpmn_v_definition order by key, version desc
```

No index was added for these. Definitions are bounded by deploys rather than
by throughput — a few versions per process — so there is no scan worth
preventing, and the primary key plus the `(key, version)` unique index already
serve both "this exact version" and "the latest of this key".

#### `rbpmn_v_instance`

| column | |
|---|---|
| `id` | instance id |
| `definition_key`, `definition_version` | the stable coordinates; instances pin a version |
| `business_key` | as passed to `start` — nullable, non-unique, unindexed |
| `status` | `active` / `completed` / `terminated` / `failed` |
| `variables` | the whole live variable document |
| `created_at`, `completed_at` | |

#### `rbpmn_v_work_item`

The question is a triage screen's first paint: *for every queue this user can
work, how many items are waiting right now?* `count_tasks(topic, filter)`
answers it one queue at a time, so a dashboard covering T topics across D
definitions cost T×D round trips. Here it is one statement.

| column | |
|---|---|
| `id`, `instance_id`, `item_no` | identity; `(instance_id, item_no)` is the stable pair |
| `definition_key`, `definition_version`, `element_id` | where in which model |
| `topic`, `kind` | the queue, and `user` / `service` |
| `state` | `available` / `locked` / `completed` / `cancelled` / `failed` |
| **`claimable`** | **would `get_task` hand this out right now** |
| **`in_progress`** | held under a lease that has not expired |
| `lock_owner`, `lock_until` | who holds it, until when |
| `retry_at`, `retries`, `failures`, `last_failure` | why it is stuck |
| `created_at` | |

**`claimable` is computed by the engine**, and that is the point of the
column. It is not `state = 'available'`: it accounts for a lapsed lease
(claimable again), a live lease (not), retry backoff not yet due (not), closed
states (never), and an instance frozen on an incident (never — which is why
the view joins instances). A dashboard whose depths disagree with what
`get_task` hands out is worse than no dashboard, so it is the same expression
the claim path uses, and a test differentials the two row for row.

`in_progress` is about the **lease alone**, so `waiting + in_progress` is not
"every open item" — work belonging to a frozen instance is in neither bucket.
That gap is information, not an omission.

#### `rbpmn_v_timer`

The question is *when does this next happen?* — a renewal date, a payment
reminder, an escalation that has not fired yet.

| column | |
|---|---|
| `instance_id`, `timer_no` | identity |
| `definition_key`, `definition_version`, `element_id` | where in which model |
| `due_at` | the instant it is armed for |
| `due_kind` | `duration` / `date` / `cycle` |
| **`due_spec`** | **the literal or variable path the arm resolved from** |
| `remaining` | fires left on a cycle, including this one; null when unbounded and on non-cycles |
| `instance_status` | so "scheduler behind" and "instance frozen" are one query apart |
| `created_at` | when it was armed |

`due_spec` is the load-bearing column. An operator asking *why is it due
then* needs the source of the instant, not the instant — and for a cycle it
carries the period too, inside the repetition (`R/P7D`).

**A row is what is armed, not a promise about when it fires.** A `due_at` in
the past means "due and not yet fired", not "late": the scheduler runs on its
own cadence and fires at most one timer per pass. Whether it takes this one
next also depends on the instance being active (hence `instance_status`) and
on a node's transient, in-process deferral set, which no view can see.

**For a cycle the row is the next occurrence, never the series.** Firing
deletes the row and inserts the next in the same transaction, so there is
exactly one row per armed cycle and an application rendering a date never has
to guess which one it has.

There is deliberately **no `overdue` boolean.** It would be legal, but unlike
`claimable` it encodes no rule — it is `due_at < now()` and nothing else.
Compare `due_at` directly, which also gets you the range queries a boolean
cannot express ("due in the next hour", "due before this invoice date") from
the same index.

⚠️ **Ask for the soonest deadline with `order by due_at limit 1`, never
`min(due_at)`.** The aggregate-to-index-scan rewrite is refused across a join,
before indexes are considered, so `min()` plans a hash join over two
sequential scans. Measured on a 50 000-instance probe: 6 buffers against 733.
`Engine::next_due_in` carries the same finding for the scheduler's own query,
and `Engine::NEXT_DEADLINE_SQL` is the shape written out.

#### `rbpmn_v_subscription`

The question arrives in one shape: someone quotes a business identifier — an
order number, a ticket reference — and asks what is waiting on it. Nobody asks
by instance id; if that were known the answer would already be at hand.

| column | |
|---|---|
| `instance_id`, `subscription_no` | identity |
| `definition_key`, `definition_version`, `element_id` | where in which model |
| `message_name` | the message it is armed for |
| **`correlation_key`** | **the business identifier it is waiting on** |
| `instance_status` | whether `correlate` is answering for it |
| `created_at` | when it was armed |

**The delivery rule, because the view cannot be read correctly without it.**
`correlate` matches on (`message_name`, `correlation_key`) among **active
instances only**, and then: exactly one match delivers; none is
`NoSubscription` (404); two or more is `AmbiguousCorrelation` (409), refused
rather than delivered to an arbitrary one.

An incident-frozen instance keeps its subscriptions, and they neither answer
for a key nor block delivery to a live instance sharing it. That is what
`instance_status` is for — one column between "nothing is waiting" and "the
thing waiting is frozen", which are opposite answers to give someone.

There is no `deliverable` boolean, for the reason `rbpmn_v_timer` has no
`overdue`: it would be `instance_status = 'active'` and nothing else.

The one fact that is *not* derivable from a single row is ambiguity — seeing
it needs an aggregate over the table, which would stop this being an inlinable
projection. So it is a query, and it is the one to run after a 409
(`Engine::AMBIGUOUS_CORRELATIONS_SQL`):

```sql
select message_name, correlation_key, count(*), array_agg(instance_id)
  from rbpmn_v_subscription
 where instance_status = 'active'
 group by 1, 2 having count(*) > 1
```

⚠️ **Search by `correlation_key` and the engine's own correlate index will
not serve you well** — `rbpmn_subscription_correlate` is `(message_name,
correlation_key)`, so a key-only predicate has no leading equality to seek on.
Skip scan gives it one — on PostgreSQL 18 and up — by seeking once per
distinct message name, so the cost grows with your model portfolio; below 18
there is no index path for that predicate at all. Migration 0017 adds
`rbpmn_subscription_by_key` for exactly this. Measured on 60 000
subscriptions: 24 buffers at 4 distinct message names, 394 at 400, against 3
through the explicit index either way.

#### SQL, or a typed call?

Use **SQL against the views** whenever the answer involves your own data:
joining your rows, filtering by your tenancy, grouping by your dimensions,
ordering by your rules. That is a join, and it is why the surface is SQL.

Use a **typed call** when you want ids or counts and no join:

- `Engine::queue_depths(definition_keys)` — the dashboard query, busiest
  first. The key set is an argument bound into the statement, and there is
  deliberately **no limit at all**, so nothing is truncated before your filter
  can compose with it. An empty slice matches no keys and returns no rows —
  plain SQL set semantics.
- `Engine::find_by_shared_index(field, value, limit)` — index-backed by
  construction; it refuses outright rather than sequential-scanning when no
  shared index for the field exists.

Timers and subscriptions get no typed call at all; what they get instead is
their query written out — `Engine::NEXT_DEADLINE_SQL`,
`Engine::WAITING_ON_KEY_SQL`, `Engine::AMBIGUOUS_CORRELATIONS_SQL` — because
these are queries with no rule in them that an application wants joined to its
own row anyway.

**The caution that decides between them:** `find_by_shared_index` applies its
limit *in the database, before you see anything*. An application that then
filters the result — by tenant, by permission, by anything — is filtering a
page that was already truncated, and can silently miss rows it was entitled
to. A call that bounds before you can filter is the wrong tool for a filtered
result set; express the filter in SQL against the view, where your predicate
and the bound compose in the right order. `queue_depths` takes no limit for
exactly this reason.

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

## Benchmarks

`benchmarks/` is a separate track from the tests: models, data, a hardware
spec and one command, in the repository, so a number can be reproduced rather
than believed. `just bench` runs the lifecycle suite against the local
Postgres — no Docker — and writes a result file carrying the git sha, the
seed, every Postgres setting, the hardware, the deployed manifest and the
scenario's own statement of what it does *not* measure.

```sh
just bench            # the lifecycle suite (7 scenarios), writes results/
just bench-micro      # pure-core criterion suite + this machine's regression gate
just bench-report     # render results/*.json into a markdown table, grouped by host
```

Nothing here gates CI on an absolute number, and `cargo test` never builds the
harness. The one exception is `bench-micro`, which compares the IO-free core
against a baseline recorded on the same machine — never committed, because it
describes that machine — with its own measured noise folded into the
threshold — and prints the smallest slowdown it
can actually detect, rather than implying it is watching more closely than it
is. Details, and an explicit list of what these numbers exclude (no network,
no handler work, no cross-engine comparison), in
[benchmarks/README.md](benchmarks/README.md).

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

## The editor and the inspector

Two self-contained HTML documents — one stylesheet, one script, no
subresources. `just ui-dist` writes both to `ui/dist/`; open `editor.html`
straight from disk and it works, which is the point.

**To see them running for real: `just demo`.** It brings up Postgres and
`rbpmn-server`, deploys a model, starts an instance, and drives it into an
incident — `Charge card` kept answering 502 and raised `GATEWAY_TIMEOUT`,
which the error boundary (listening for `PAYMENT_FAILED`) does not catch, so
the retry budget ran out and the instance froze. Then it prints two links and
waits:

```
inspector   http://localhost:8099/ui/inspect/<uuid>
editor      http://localhost:8099/ui/editor
```

The proxy on 8099 is part of the demonstration, not scaffolding. rbpmn's UI
routes sit behind the same bearer as `/v1`, and a browser cannot send that
header on a top-level navigation — so the demo runs the smallest honest
version of the embedding application: forty lines that add the header. A real
one would authenticate the user first and decide whether they may see that
instance at all.

**The editor** authors a deployment, meaning the pair: the `.bpmn` and its
bindings manifest. rbpmn keeps every runtime binding out of the XML, so a
model on its own is half a deployment and no bpmn-io tool knows the other half
exists. Open either file, edit the diagram and the wiring side by side, save
both back to disk. Validation is live and is the engine's own code compiled to
wasm32 — the linter (L1) *and* compile-against-manifest (L2), so a missing
correlation binding is caught here rather than at deploy. The remaining check,
`unresolved-topic`, needs a running environment: press **Check against
server** and the editor fetches the covered topic **names** and does the
comparison locally. Your model is never uploaded, so a confidential process
can be validated against production.

It opens on a worked example rather than an empty canvas: the same deployment
`just demo` runs, model and manifest and decision table together, because the
first thing worth showing is that all three are one artifact. The decision is
live — the try-it pane evaluates it with the engine's own evaluator, offline,
because a deployment's DMN travels inside it. **New** clears that down to a
deployable skeleton when you want to model your own.

### Getting a diagram out, for a document or a printer

Two ways, and they are not the same thing.

**Export SVG** (editor toolbar) writes the diagram as a standalone `.svg` next
to where you would save the `.bpmn` — vector, so it scales into a report, a
slide or a printed page at any size. Three decisions worth knowing about:

- **Always light**, whatever theme the editor is wearing. bpmn-js paints
  strokes and fills as SVG *attributes* chosen when the canvas is built, so a
  dark-mode canvas would export a dark-mode diagram — `#c9cfda` strokes are
  near-invisible on white paper, which is precisely what the button is for. A
  document is not a screen.
- **Rendered by a second, detached viewer**, not by restyling the canvas in
  front of you. Restyling works, and the editor already owns the machinery for
  it (the OS flipping to dark at sunset rebuilds the canvas), but that rebuild
  costs the undo history. That is a fair price once at sunset; it is not a
  price to pay silently on every export. The visible canvas is untouched.
- **The whole model, not the viewport**, tightly cropped, with its own white
  background — so zoom and scroll position do not change what you get.

Diagnostics do not come along, which is the point: the badges are HTML
overlays that live outside the SVG, and the element highlighting is a CSS
class the file carries without the stylesheet that colours it. What lands in
the document is the model, not the review of it.

**Printing** (both documents) has a stylesheet that hides the chrome — the
toolbar, and the whole side column of diagnostics, wiring and raw documents
that was otherwise taking more of the page than the diagram. The inspector
keeps its heading, instance id and diagnosis, because those are what a
printout is *for*, and keeps the canvas annotations, because there the badges
are the content.

Its limits, stated rather than discovered: the canvas is sized to its
container, so what prints is the current zoom and scroll rather than
necessarily the whole model; and printing in dark mode prints a dark diagram,
for the same reason the export exists — no stylesheet can reach an SVG
attribute, and half-overriding it loses every arrowhead. For a document, use
Export SVG.

**The inspector** shows one instance, read-only, addressed by UUID. Its data
is baked into the document rather than fetched, so it has no API to secure,
works with the database unreachable, and can be attached to a support ticket.
It opens with a diagnosis line, marks tokens and parked work on the diagram,
and its element pane fuses the model, the deployed manifest and the runtime
rows — including for elements the token has not reached, whose wiring exists
nowhere else the reader can see.

Finding *which* instance is the application's job: it called `start` and holds
the mapping from its own order or ticket to the id it was given. There are
deliberately no lists, no search and no buttons that change anything.

Mount them in your own axum app behind your own middleware:

```rust
Router::new()
    .nest("/bpmn-editor", rbpmn_ui::editor_router())
    .merge(rbpmn_ui::editor_slash_redirect("/bpmn-editor"))
    .nest("/bpmn-editor/api", rbpmn_ui::environment_router().with_state(engine.clone()))
    .nest("/bpmn-inspector", rbpmn_ui::inspector_router().with_state(engine))
    .layer(your_authentication)
```

Or skip the routers entirely — the primitive is a pure function, and an
application that must redact something edits the value first:

```rust
let inspection = engine.inspect_instance(id).await?;
let html = rbpmn_ui::render_inspection(&inspection);
```

**rbpmn authenticates nobody.** Read
[docs/http-security.md](docs/http-security.md) before exposing either document
to a human: the sandboxed iframe, `frame-ancestors` and never proxying `/v1`
to a browser audience are all the application's job.

## Developing

`.build.yml` runs these on sourcehut, one CI task per command, so a red build
names the discipline that broke rather than "tests failed". Two exceptions,
both deliberate and both stated in the table below: the **benchmarks** are a
separate track that never gates on absolute numbers, and a shared builder is
the worst machine to measure on; and **`just dmn-tck`** fetches the DMN TCK,
dsntk's source and a third-party runner from the network, which makes it the
gate for a dsntk version bump rather than a per-commit check.

CI is the backstop, not the workflow: knowing which command a change *owes*
is what keeps the loop short, because every one of these guards something
`cargo test` structurally cannot see.

**Always, before committing:**

| | |
|---|---|
| `cargo test` | everything including the fixture corpus. Needs a local Postgres for the engine's integration tests. |
| `just lint` | clippy `-D warnings` across the whole workspace + `cargo fmt --check`. |

**Owed by what you touched** — these guard things `cargo test` cannot see:

| If you changed | Run | Because |
|---|---|---|
| a linter rule, `rbpmn-model`, `rbpmn-core` | `just ui` | the editor embeds the linter; without it the document you serve validates against yesterday's rules, and nothing checks that for you |
| the dsntk fork's rev | `just number-parity` | its decimal against the C library it replaces, 26 300 comparisons. Runs `just dsntk-rev` first, because a differential against a rev nobody ships proves nothing. Lives outside the workspace and cannot be reached from it |
| the `dmn` feature, or anything behind it | `just no-dmn` | DMN is on by default; this is the only thing keeping "optional" a fact rather than a claim, and it asserts the dependency graph in *both* directions |
| anything WASM-facing | `just parity` | byte-parity of native Rust vs WASM over the corpus, for both exports, plus the bpmnlint plugin |
| lock order, the work-item lease, the scheduler's claim, scope teardown, retention | `just tla` | the specs are hand-written and will not tell you they drifted |
| the FEEL subset / `condition::eval` | `just feel-parity` | differential against dsntk over ~8k expression/document pairs |
| the two UI documents | `just ui-test`, `just e2e-ui` | the pure modules under node, then both documents in a real browser (the only place the CSP is enforced) |
| a fixture without DI | `just fixtures-di` | so it renders in bpmn-js and any standard modeler |
| the dsntk fork's rev | `just dmn-tck` | the DMN TCK twice — against dsntk as published, and against the fork we ship — compared case by case. Not on CI: it fetches the TCK, dsntk's source and a third-party runner |

**Occasionally, to catch performance regressions** — local only, never on CI
(a separate track — see [benchmarks/README.md](benchmarks/README.md); none of
it gates on absolute numbers):

| | When | Cost |
|---|---|---|
| `just bench-micro` | after touching the semantic core | ~10 min. The only benchmark that can fail: pure-core suite vs *this machine's* baseline, with that machine's measured noise in the threshold. A machine with no baseline reports and passes — record one with `just bench-baseline`. |
| `just bench` | before a release, or after touching the claim/step/persist paths | ~3 min. Seven lifecycle scenarios. `results/` is gitignored — there is no baseline set in the repo, so comparing releases means keeping your own runs, from the same machine. |
| `just bench-population` | after touching anything a large parked population meets — the scheduler, claims, retention, indexes | ~45 min to a million instances. This is the one that found the three engine issues fixed in migration 0008 and the scheduler's sleep query. |
| `just bench-report` | when quoting numbers | renders `results/*.json` grouped by host. |

**Utility:**

| | |
|---|---|
| `just serve` / `just demo` | the HTTP server with a throwaway token / a live demo with an instance frozen on an incident |
| `just playground` | fixture browser + live lint |
| `just e2e` | every fixture rendered in a browser, plus the full inspection stack |
| `just bench-check` | lint every benchmark model against its manifest (no database) |
| `just cleanup` | **destructive**: drops every `rbpmn_*` database (including the `rbpmn_test_*` throwaways a panicked test leaves for inspection) and removes all build output — `target/` alone is typically tens of GB. Keeps `benchmarks/.baselines/`, which is machine-local and costs ten minutes to re-record. |

**Bootstrap:** `just ui` before the first `cargo build`, because the UI
bundles are compile output and are gitignored like every other artifact here.
`rbpmn-ui`'s build.rs says so if you forget. It needs node and wasm-pack —
already prerequisites for `just playground` and `just parity`.

The editor embeds the linter compiled from `rbpmn-model`/`rbpmn-core`, so
changing a rule means running `just ui` again; otherwise the document you
serve validates against yesterday's rules.

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
worker topics), `RBPMN_HTTP_HANDLERS` (`topic=url;...`), `RBPMN_WORKERS`,
and `RBPMN_RETAIN` (retention age in days; unset means no sweeper runs at
all — nothing is even scanned).
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
