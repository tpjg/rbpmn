# Task config — design round

**Status: slice 1 landed (the manifest group and `config-binds-task`).**
The slices at the bottom are the staging; the decisions above them are why.

This round covers **re-usable tasks that carry a little configuration** — one
handler on one topic, invoked from many call sites, each call site configured
differently. It was read against `crates/rbpmn-core/src/compile.rs`
(`Bindings`, `IndexDeclaration`, topic resolution), `check.rs`
(`decision_bindings`, the L2 verdict), `crates/rbpmn-engine/src/deploy.rs`
(the content hash), `runtime.rs` (`compiled_process`, the definition cache,
work-item creation), `tasks.rs` (`LockedTask`, the claim statement),
`worker.rs` (push-mode `WorkItem`), migrations 0001/0015/0018,
`ui/src/editor/manifest.js` and `docs/dmn.md` (D5, the precedent this follows).

---

## The motivating case, in one paragraph

An application has one service task implementation — "send a message to the
citizen" — invoked from a dozen places across several definitions, each
needing a different document template. Today the only wiring an element can
carry is its topic, so either every template gets its own topic and its own
registered handler (an environment that grows with the *content*, not with the
capability), or the handler resolves the template itself from something
outside rbpmn, keyed by the element. Other engines answer this inside the XML:
Camunda's `zeebe:taskHeaders`, Flowable's `flowable:field`. Those are exactly
the right *feature* and exactly the wrong *place* for rbpmn, which is 100%
standard-namespace by an explicit call that has survived every pull on it so
far, DMN included.

The manifest is where this already lives. `Bindings` maps element → topic,
element → correlation, element → decision. Element → config is the same shape.

## Four things reading the code turned up

**1. The content hash is the whole argument, and it already works this way.**
`deploy` hashes `bpmn_xml`, then the serialized `Bindings`, then each DMN
artifact length-prefixed (`deploy.rs:120–132`). Anything in `Bindings` is
inside "two installations on the same hash are running the same model".
Anything in an application-owned sidecar is not. For a value that decides
which document a citizen receives, that difference is the point of asking
rbpmn for the feature at all rather than building it beside rbpmn.

**2. Resolving metadata application-side is a supported, documented
mechanism today — and it stays one.** `LockedTask` carries `definition_id` and
`definition_version` (`tasks.rs:114`) with a doc comment that says
version-pinned per-task metadata on the embedding side must resolve against
exactly that pair. This round does **not** deprecate that. See D7.

**3. Push mode cannot do it.** `WorkItem` (`lib.rs:170`) carries
`definition_key`, `element_id`, `topic` and `variables` — no `definition_id`,
no version. A push handler resolving version-pinned metadata has to guess
`max(version)` of the key, which is wrong for every instance pinned to an
older one. That is a correctness gap independent of this feature, and it is
repaired here because the same change touches the same struct.

**4. There is already a definition cache, and it exists for exactly the
reason config can use it.** `compiled_process` (`runtime.rs:634`) caches a
compiled process forever by `definition_id`, *because definitions are
immutable, insert-only and content-hashed*. Config is definition data with
those same properties. It needs no table and no column.

---

## Decisions

### D1 — config is model content, not runtime configuration

Config lives in the manifest, so it is content-hashed, versioned, and pinned
with the instance. **Changing it is a deploy, by construction**, and running
instances keep the config of the version they are pinned to.

That is right for the motivating case: which document a citizen receives is
part of what the model *does*, and an installation claiming to run a given
model should be running given templates. It is wrong for anything that must
differ per environment or change without a deploy — endpoint URLs,
credentials, feature flags, per-tenant switches. Those belong to the
environment half (`declare_topic`, handler registration, env vars) or to the
application's own store, reached through D7.

One deciding question, and it should be in the README next to the manifest
table: **must this change with a deploy?** Yes → manifest config. No → it must
not go in the manifest, because the only way to change a hashed manifest is to
deploy.

This is the project's "content belongs in the repo" rule pointed at the
manifest. A `.bindings.json` is a file: greppable, diffable, reviewable, next
to the `.bpmn` it wires. Deploys are cheap.

### D2 — a fifth manifest group, keyed by element id

```json
{ "topics":  { "send_warning": "send_message" },
  "config":  { "send_warning": { "template": "warning_first" } } }
```

`Bindings::config: BTreeMap<String, serde_json::Value>`, beside `topics`,
`correlations`, `indexes` and `decisions`. Fluent builder
`Bindings::config(element_id, value)`; the same struct deserializes from the
HTTP deploy body. Two syntaxes, one manifest, one validation path — unchanged.

**Rejected: folding config into the topic binding**
(`"topics": {"send_warning": {"topic": "send_message", "config": {…}}}`).
It reads as "one wiring per element", which is tempting, but it changes the
serialized bytes of every existing manifest (D3), it needs the whole
dual-spelling apparatus `IndexDeclaration` already carries once, and it
couples two things that are independent — an element's topic has a default,
its config does not.

### D3 — it must not serialize when empty

`deploy` hashes `serde_json::to_value(bindings).to_string()`, and today that
is `{"topics":{},"correlations":{},"indexes":[…],"decisions":{}}` — empty
groups included, asserted byte for byte by
`definition_scoped_manifests_serialize_byte_for_byte` (`compile.rs:1363`).

A plain `#[serde(default)]` field would add `"config":{}` to every manifest in
existence, change every `content_hash`, and — because redeploy-at-startup is a
documented and deliberately idempotent pattern — silently allocate a new
version of every definition in every installation on first boot after the
upgrade.

So: `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]`. The
existing byte-for-byte test then proves hash stability without a new test
being written for it. (`decisions` did not do this when it landed. That is the
history, not the specification.)

### D4 — entries are JSON objects; rbpmn never looks inside

Each entry is a JSON **object**. Free-form within: keys and values are the
application's, and rbpmn passes them through — never interprets, never
resolves, never evaluates, at any depth.

Object rather than any JSON value, though a bare string would serve the
one-value case: an object leaves room to add a key later without changing the
shape, and anyone needing a single value can spell it `{"template": "…"}`.
That is typing, not interpretation.

**The object rule is a diagnostic, not a type.** The field is
`BTreeMap<String, serde_json::Value>` and the shape is checked by
`config-binds-task` (D5). Making it `serde_json::Map` would make a non-object
unrepresentable, but at two costs: the fluent builder would stop taking
`json!({…})`, which is the spelling every call site wants; and the JSON path
would fail as a deserialization error — `bindingsError`, a byte offset, no
element — where the fluent path failed as a diagnostic. Two syntaxes, one
manifest, **one validation path** is the older promise, and it wins.
`DecisionBinding` sets the precedent: its well-formedness is a diagnostic too,
not a type.

**No size limit.** Deliberate — a limit would be a number nobody has measured
against a real case, and the cost lands where it is visible (the definition
row, the compile cache, the deploy body) rather than silently.

### D5 — `config-binds-task` ⁺ (error): the key must name a task that
produces a work item

One rule id, two clauses — the entry is a JSON object (D4), and its key names
a service task or a user task. An element id that is not in the model at all
is the second clause's other half. Both clauses on one entry report both
diagnostics: two defects are two things to fix.

`decision-has-binding` is the precedent for one id covering a binding's shape
and its resolution.
Service *and* user: both produce work items, both are claimed through the same
API, and a user task's config ("which form does this render") is the same
feature as a service task's. Business-rule tasks, receive tasks, gateways,
events and boundaries are not.

**Why config is stricter than the other groups, which stay lenient.** A stale
key in `topics`, `correlations` or `decisions` is not a deploy error today,
deliberately — `orphanedBindings` (`ui/src/editor/manifest.js:205`) warns
about it in the editor with the comment "an unused entry binds nothing". That
leniency is sound *because a topic has a default*: `topics` is an override
table, and an override for an element that is not there overrides nothing.

Config has no default. Its only meaning is delivery, so a config entry that is
never delivered is not inert — it is the feature failing silently, on a value
that decides what a citizen receives. And it has a failure the other groups
cannot have: a key naming an element that **exists but is not a task** — a
gateway, a business-rule task, a boundary. The editor's orphan check cannot
see that one (the element is present), and the modeller can see the element on
the canvas and will reasonably believe it is wired.

This round does **not** revisit the leniency of the other three groups.
Doing so would break the deploy of anyone carrying a stale key today, for a
defect that is genuinely inert there.

New rule id, nothing renamed. Error severity. Reported through
`check_deployable`, so the editor's L2 pane, the playground and `deploy` all
get it from one implementation and `just parity` covers it.

### D6 — validated in the core, delivered from the engine's cache

**The core validates and does not carry it.** `config-binds-task` is decided
in `rbpmn-core` — it needs only the node list, which `compile` has. But config
does **not** enter `ExecutableProcess`: the pure core never reads it, and
`chaos.rs`, the explorer and the model generator should not be dragging
business JSON through a state space. Core decides the verdict; the engine
delivers the value.

**The engine caches the manifest beside the compiled process.** `Engine`
already caches `Arc<ExecutableProcess>` by `definition_id` forever
(`runtime.rs:634`). It caches `Arc<Bindings>` in the same place and by the
same key, populated on the same miss. A claim resolves config by
`(definition_id, element_id)` in process, with no SQL, no column and no
storage.

The alternatives, and why they lose:

| | cost |
|---|---|
| **A** correlated subquery at claim off `rbpmn_definition.bindings` | an extra subquery and a full detoast of `bindings` in the hottest statement there is, to read one key |
| **B** a `config jsonb` column on `rbpmn_work_item`, written at creation | the same JSON in two places that can disagree; a copy per item, forever, through retention and into every archive; a migration |
| **C** in-process off the definition cache | needs `definition_id` returned by the push-mode claim, and a definition read on a cold miss |

B is the one to avoid on principle, not only on cost: two copies of one fact
is what "one verdict, one implementation" exists to prevent. C reuses a cache
that is already justified by exactly the property config relies on.

Cold-miss handling: the definition is immutable and referenced by the work
item, and `delete_definition` refuses a version in use, so a miss that cannot
read the row is corruption — `CorruptManifest`, the same as the inspector's
(`inspect.rs:131`), never a silent `None`.

**Not a column on `rbpmn_v_work_item`.** Config is definition data and is
already published in `rbpmn_v_definition.bindings`. Joining a third table into
that view would put the grouped-depth plan the tests assert at risk, for a
value the caller can join to itself.

**Not in the event stream.** Recoverable from `definition_id`, and the
definition version is retained for as long as archived history needs it —
that is what `retired_instances` is for.

### D7 — the sidecar mechanism is kept, not deprecated

`LockedTask::definition_id` / `definition_version` stay exactly what their
doc comment says they are: the pinned pair an application resolves its own
per-task metadata against. `WorkItem` **gains** them, so push mode can do it
for the first time.

The two are a choice, not a migration:

- **Manifest config** — for wiring that is model content. Content-hashed,
  version-pinned, changes with a deploy.
- **Application-side resolution against `(definition_id, definition_version)`**
  — for what is not: per-environment values, things that change without a
  deploy, payloads too large or too dynamic to want inside a content hash,
  data owned by another team's store.

Documenting both, with D1's deciding question between them, is the point. A
deprecation notice here would push per-environment configuration into the
hash, which is the failure this document most wants to prevent.

### D8 — no interpolation, no defaults, no inheritance

Named as non-goals so they do not creep in:

- **No FEEL, no interpolation.** `{"template": "= ticket.kind"}` is a string
  whose first character is `=`. The instance variables travel beside the
  config on the same `WorkItem`; composing them is the handler's job. The
  moment rbpmn evaluates config it is interpreting it, and D4 is gone.
- **No per-topic defaults.** One handler, many configured call sites, keyed
  per element. A default per topic is imaginable and nobody has asked for it;
  it is known-and-waiting, not designed.
- **Not merged into `variables`.** Variables are mutable instance state,
  patched by handlers through merge-patch. Config is immutable definition
  data. Delivering it as a separate field is what keeps that distinction
  legible at the call site.

---

## What each surface owes

**`rbpmn-core`.** The `config` group on `Bindings` with D3's serialization,
the fluent builder, and `config-binds-task` in the L2 path. The rule id in
`rbpmn-model`'s `rule` module and `CATALOGUE`, where every id lives regardless
of which crate enforces it.

**`rbpmn-engine`.** `Arc<Bindings>` in the definition cache; `config:
Option<serde_json::Value>` on `LockedTask` and on `WorkItem`; `definition_id`
and `definition_version` on `WorkItem`.

**`rbpmn-server`.** `/tasks/get` gains `config` in its response — free, since
`LockedTask` is `Serialize` with camelCase.

**`rbpmn-wasm` / playground / bpmnlint plugin.** Nothing beyond a rebuild:
the rule is in the core, so both exports get it and `just parity` compares
them.

**The editor.** `parseManifest` currently *throws* on an unknown manifest key,
so an editor build that has not learned `config` refuses a manifest carrying
it. That is safe rather than destructive, but it means the editor must learn
the group in the same release as the engine, not a later one. Then a JSON box
in the wiring pane for the selected task. `just ui` is owed, and so is
`just ui-test` and `just e2e-ui`.

**The inspector.** The element pane fuses model, manifest and runtime; config
is manifest, and "what was this task configured with when it ran" is the
question an operator asks when a citizen received the wrong letter. Read-only,
like everything else there.

**Docs.** README's manifest table gains a row and the catalogue gains the
rule. D1's deciding question goes next to the manifest table, and D7's two
mechanisms with it.

**`spec/`.** Read, and expected to need nothing: config is immutable
definition data resolved after a claim, in process, under no lock. No new lock
enters the order, the lease is untouched, the scheduler's claim path is
untouched. The claim path is on the mandatory re-read list, so this is
recorded as a conclusion reached by reading the models, not by `just tla`
staying green.

---

## Slices

### Slice 1 — the manifest and the verdict (no delivery)

Linter rules first, with fixtures, then execution — as always.

- `Bindings::config`, D3's serialization, the fluent builder taking
  `impl Into<serde_json::Value>`.
- `rule::CONFIG_BINDS_TASK` + `CATALOGUE` entry.
- `config-binds-task` in `check_deployable`, beside `decision_bindings`.
- Fixtures through the `.bindings.json` sidecar convention, plus unit tests in
  the shape of `check.rs`'s decision-binding tests.
- README: manifest table row, catalogue row, D1's deciding question.

Owes: `cargo test`, `just lint`, `just parity`.

### Slice 2 — delivery

- `Arc<Bindings>` in the definition cache, resolved by
  `(definition_id, element_id)`.
- `LockedTask::config`; `WorkItem::config` plus `definition_id` and
  `definition_version`.
- `/tasks/get` response.

The test that earns the feature: deploy v1 with `{"template":"warning_first"}`,
start an instance, deploy v2 with `warning_second`, claim the pinned
instance's item, assert `warning_first`. Everything else about this feature is
plumbing; that assertion is the reason it is in the manifest rather than
beside it.

Owes: `cargo test` (needs Postgres), `just lint`, and a `just bench` sanity
pass on the claim path — not as a gate, but because the claim path's plan is
sensitive enough to have surprised this project before.

### Slice 3 — the surfaces

- Editor: `config` in `parseManifest` / `serializeManifest` /
  `orphanedBindings`, and a JSON box in the wiring pane.
- Inspector: config in the element pane.
- `just ui`, `just ui-test`, `just e2e-ui`.

---

## Test plan

| What | Where |
|---|---|
| Empty config does not change the serialized manifest | the existing byte-for-byte test in `compile.rs` |
| A config entry round-trips, and a non-object entry is refused | `compile.rs` unit tests |
| `config-binds-task` fires for a missing element, a gateway, a business-rule task | `check.rs` unit tests + a reject fixture with a sidecar |
| A service task and a user task both accept config | accept fixture with a sidecar |
| Config reaches a pull claim and a push handler | `crates/rbpmn-engine/tests/engine.rs` |
| A pinned instance keeps its version's config after a newer deploy | `engine.rs` — the assertion the feature exists for |
| Native and WASM agree on the new rule | `just parity`, over the corpus |
| The editor round-trips a config manifest byte for byte | `just ui-test` |

---

## Known warts, stated up front

- **The manifest is no longer bounded by short strings.** Every other group
  holds identifiers; config holds whatever the application puts there, and it
  rides in the definition row, the deploy body and the compile cache. No limit
  by D4. If this ever hurts, the number will be measurable rather than
  guessed.
- **Half-strict manifest.** After D5, a stale key is an error in `config` and
  a silent no-op in the other three groups. The asymmetry is principled (a
  default versus no default) but it is still an asymmetry, and the catalogue
  entry has to carry the reason or it reads as an oversight.
- **Config is invisible in the runtime tables.** Answering "what was this item
  configured with" means joining `rbpmn_v_definition.bindings` on the
  definition — deliberate under D6, and the inspector's element pane is what
  makes it reachable without writing that join by hand.
