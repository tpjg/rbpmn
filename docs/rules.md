# Rule catalogue

Every rule rbpmn enforces, with the reasoning. Rule ids are **stable
public API** — fixtures, the playground and the bpmnlint plugin all assert
on them, so ids are added, never renamed. Rules marked ⁺ go beyond the
design brief's initial list.

The linter is the front door: a model that lints clean and compiles against
its manifest is one the engine will run to quiescence. See
[../bpmn-engine-design.md](../bpmn-engine-design.md) for why the subset is
drawn where it is.

## Model rules

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
| `boundary-on-supported-host` | error | Boundary events only on service/user/receive tasks and subprocesses; error boundaries only on service tasks and subprocesses — a user task can fail, through the task API, and a catch-all on an embedded subprocess around it is how that failure is caught. Never on a business rule task: its decision is answered inside the transaction that starts it, so a boundary there is armed and cancelled in one step and can never fire. |
| `boundary-side-path` ⁺ | error | A non-interrupting boundary spawns a *second* token beside its host's, and no block-structure proof covers it — it entered through no split. So its path must be a **side path**: disjoint from everything else in the scope, ending at its own plain end event. A terminate end does not discharge it (it cancels the whole scope instead of consuming the token), and neither does an end reachable only through a boundary's handler — every boundary is optional, so a timer that may not fire and a catch-all that may not catch are both ends the side token may never reach. The end that discharges a side path is the one its own sequence flows reach. It may not rejoin the flow after the host (the rest would run twice) or reach a parallel join (which would collect a second token on one incoming flow), and it may not carry a parallel block of its own: the boundary can fire again while an earlier side token is still inside it, and both activations run in the host's scope — wrap the block in an embedded subprocess, which gives each activation its own scope. For "remind, then wait again", use an interrupting boundary and a loop. |
| `side-path-message-arm` ⁺ | warn | A message arm (catch, receive task, message boundary) on a side path is armed once per activation of its non-interrupting boundary, and an earlier activation's arm may still be open: unless each activation changes the correlation key (a delivery patch can), the second arm freezes the instance — `duplicate-subscription`, loud, at arm time. Including an arm inside an embedded subprocess on that path, at any depth: a subprocess gives each activation its own *scope*, which is what makes a parallel block there safe, but a subscription is keyed by (message, key) across the whole instance and a scope narrows it by nothing. |
| `side-path-failure-escapes` ⁺ | warn | A service task, user task or subprocess on a side path whose failure nothing *on the side path* catches. An error walks outward through enclosing scopes and never sideways, so the host's own boundaries do not reach its side path; uncaught, the failure freezes the whole instance, or — with a catch-all further out — tears down the scope around the host, stopping the flow the side path exists to leave alone. The remedy is a catch-all (an error boundary with no `errorRef`) on the failing activity, or on an embedded subprocess on the side path around it, which is the only option for a user task. A catch-all on a host that may not carry one does not silence this warning: the refusal and the advice are said together, so a single pass names both the shape and its fix. Decision, timer and correlation incidents are not errors and no boundary catches them, so this rule does not speak for them. |
| `ambiguous-message-arm` ⁺ | error | Two message arms for the same message *and* the same correlation binding that can be live at once — two message boundaries on one host, a receive task and its own boundary, a subprocess boundary and a catch inside it, and a non-interrupting message boundary with an arm for the same pair on its own side path (it re-arms and spawns the side token in one step, so the *first* delivery freezes). Every delivery would be ambiguous, so deploy refuses it. Decided at L2 (`check_deployable`), because with different bindings both arms are legitimate and only the manifest knows. |
| `no-implicit-split` | error | Activities have at most one outgoing flow; splitting happens at explicit gateways. |
| `implicit-merge-after-parallel` | warn | Implicit merge receiving concurrent tokens — the "task runs twice" trap (accompanies the balanced-gateways error). |
| `bpmn-structure` ⁺ | error | Well-formedness: resolvable refs, flow cardinalities, connectivity, unique ids, error definitions. An error boundary with no `errorRef` is a **catch-all**: it catches any error, coded or not, after any exact code on the same activity. A *present* `errorRef` must resolve to an error with a code — a typo is refused, never read as "catch everything". At most one boundary per code, and one catch-all, per activity. |
| `no-mixed-gateway` ⁺ | error | A gateway either splits or joins, never both. |
| `event-gateway-structure` ⁺ | error | Event gateway races ≥2 message/timer catches or receive tasks, each with exactly one incoming flow and no boundary events (the gateway itself is the race). |

DMN rules apply to the decision artifacts a deployment bundles, and are
implemented in `rbpmn-dmn` — the one crate where dsntk is allowed
([docs/dmn.md](dmn.md)). They report the same `Diagnostic` type, so a
decision error and a model error read the same way.

| Rule | Severity | Meaning |
|---|---|---|
| `dmn-validates` ⁺ | error | The artifact is DMN and its decision logic builds. A decision that cannot be compiled cannot be deployed. |
| `feel-parses` ⁺ | error | Every FEEL expression in the artifact parses — literal expressions, decision-table entries, item-definition constraints and the rest. dsntk is the authority here: FEEL names may contain spaces and operators, so parsing needs the model's own scope. |
| `feel-deterministic` ⁺ | error | No `now()`/`today()` (dsntk answers them from the *node's* local timezone) and no external Java or PMML functions. Time enters a decision as an input, never from a clock. Deliberately conservative: it errs toward refusing. |
| `decision-has-binding` ⁺ | error | A business-rule task's decision binding lives in the manifest, never in the XML, and must be well-formed: a decision name, and a FEEL qualified name for where the answer lands. |
| `unresolved-decision` ⁺ | error | Every bound decision names exactly one invocable in the bundled DMN. Unlike `unresolved-topic` this needs no environment — decisions travel *inside* the deployment, so the verdict is complete offline. Ambiguity is refused rather than resolved by picking. |
| `config-binds-task` ⁺ | error | A manifest config entry is a JSON object keyed by a service or user task — the elements that produce a work item to deliver it on. Stricter than a stale key in the other manifest groups, which is only an editor warning: those have a default, so an override for an element that is not there overrides nothing, while config has none and an entry nothing delivers is wiring that silently never arrives. |
| `retry-policy-binds-task` ⁺ | error | A manifest retry policy binds a service task (by element id) or a topic some service task resolves to, and sets at least one member in range: `attempts` 1..=1000 (total handler calls, not calls after the first), a `backoff` that is an ISO-8601 duration of fixed positive length no longer than the ten-year ceiling a retry gap is capped at, a `multiplier` 1..=10. User tasks have no handler to back off from — they fail only through the task API, on the engine's default budget — and business-rule tasks produce no work item, so neither can carry one. Same strictness and the same reason as `config-binds-task`: no default, so an entry that binds nothing is a policy an operator believes is in force and is not. |


## The structural rules have counterexamples, not just rationales

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
has yet executed wrongly. Details in `stress-testing.md` §3.
