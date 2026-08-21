//! Transactional stepping: instance rows are locked (`FOR UPDATE`), the core
//! state is rebuilt from rows, the pure step runs, and rows + events are
//! written — all in one transaction. Concurrent completions on one instance
//! serialize on the row lock, which is what makes the parallel join's
//! exactly-once firing hold under concurrency.
//!
//! Every operation exists in two forms: the pool-owning convenience
//! (`start`, `complete_work_item`, `fail_work_item`) that opens and commits
//! its own transaction, and the `*_in_tx` variant taking the **caller's**
//! connection — the design's flagship property that a process transition can
//! share a transaction with business writes. The `_in_tx` forms never
//! commit; the caller does (and must, promptly — the instance row is locked
//! until then). Remote (HTTP) callers cannot share a transaction by nature;
//! their contract is per-call atomicity plus idempotent retries.

use crate::{Completion, Correlation, Engine, EngineError, FailOutcome, StartedInstance};
use rbpmn_core::{
    Bindings, Command, Counters, Event, ExecutableProcess, InstanceState, InstanceStatus, ScopeId,
    ScopeState, SubscriptionId, SubscriptionState, TimerDue, TimerId, TimerState, Token, TokenId,
    WaitKind, WorkItemId, WorkItemState, WorkKind, step,
};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// The definition an instance is pinned to, carried through the step path.
/// All three fields are immutable for the instance's whole life — it never
/// migrates — which is why they are denormalised onto its rows rather than
/// re-read from `rbpmn_definition` (migration 0011).
pub(crate) struct DefinitionRef {
    id: Uuid,
    key: String,
    version: i32,
}

impl DefinitionRef {
    pub(crate) fn new(id: Uuid, key: String, version: i32) -> Self {
        DefinitionRef { id, key, version }
    }

    /// Only the decision path needs this; without the feature nothing reads
    /// it, and an unused method is a warning in a workspace that keeps zero.
    #[cfg(feature = "dmn")]
    pub(crate) fn id(&self) -> Uuid {
        self.id
    }
}

/// Work-item lifecycle states as stored in `rbpmn_work_item.state` (and
/// mirrored by its CHECK constraint). Rust-side comparisons go through
/// these; SQL text spells them out where the planner needs literals.
pub(crate) mod item_state {
    pub const AVAILABLE: &str = "available";
    pub const LOCKED: &str = "locked";
    pub const COMPLETED: &str = "completed";
    pub const CANCELLED: &str = "cancelled";
    pub const FAILED: &str = "failed";
}

/// Options for [`Engine::fail_work_item`].
#[derive(Debug, Clone, Default)]
pub struct FailOptions {
    /// Error code for boundary matching once the retry budget is exhausted.
    pub error_code: Option<String>,
    /// Human-readable failure reason, recorded on the work item
    /// (`last_failure`) and in the retry events — what makes an incident
    /// diagnosable from stored state.
    pub detail: Option<String>,
    /// Lease identity. Failing a *live-leased* item requires its owner;
    /// ownerless calls on a live lease are refused (`ItemLeased`) so an HTTP
    /// fail cannot yank an item out from under a running worker.
    pub owner: Option<String>,
}

impl Engine {
    pub async fn start(
        &self,
        key: &str,
        business_key: Option<&str>,
        variables: serde_json::Value,
    ) -> Result<StartedInstance, EngineError> {
        let mut tx = self.pool().begin().await?;
        let started = self
            .start_in_tx(&mut tx, key, business_key, variables)
            .await?;
        tx.commit().await?;
        Ok(started)
    }

    /// [`Engine::start`] inside the caller's transaction.
    pub async fn start_in_tx(
        &self,
        tx: &mut PgConnection,
        key: &str,
        business_key: Option<&str>,
        variables: serde_json::Value,
    ) -> Result<StartedInstance, EngineError> {
        reject_nul(&variables)?;
        require_object(&variables, "initial variables")?;
        if let Some(bk) = business_key {
            reject_nul_text(bk, "business key")?;
        }
        let row = sqlx::query(
            "select id, version from rbpmn_definition \
             where key = $1 order by version desc limit 1",
        )
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| EngineError::UnknownDefinition(key.to_string()))?;
        let definition_id: Uuid = row.get("id");

        // Latest version at start, pinned from here on: the instance keeps
        // this pair for life, and every row it writes carries it.
        let definition = DefinitionRef {
            id: definition_id,
            key: key.to_string(),
            version: row.get("version"),
        };
        let proc = compiled_process(self, &mut *tx, definition_id, key).await?;

        let instance_id: Uuid = sqlx::query(
            "insert into rbpmn_instance \
             (definition_id, definition_key, definition_version, business_key, \
              status, variables) \
             values ($1, $2, $3, $4, 'active', 'null'::jsonb) returning id",
        )
        .bind(definition.id)
        .bind(key)
        .bind(definition.version)
        .bind(business_key)
        .fetch_one(&mut *tx)
        .await?
        .get("id");

        let mut state = InstanceState::new();
        let events = step_answering_decisions(
            self,
            tx,
            &proc,
            &definition,
            &mut state,
            Command::Start { variables },
        )
        .await?;
        persist_step(tx, &proc, &definition, instance_id, &state, &events).await?;

        Ok(StartedInstance {
            id: instance_id,
            events,
        })
    }

    /// Ownerless completion: refused while another holder's lease is live
    /// (`ItemLeased`) — a stray call must not discard a working claimant's
    /// result. Claimed tasks are completed through [`Engine::complete_task`]
    /// with the lease identity.
    pub async fn complete_work_item(
        &self,
        work_item: Uuid,
        patch: serde_json::Value,
    ) -> Result<Completion, EngineError> {
        let mut tx = self.pool().begin().await?;
        let completion = self
            .complete_work_item_in_tx(&mut tx, work_item, None, patch)
            .await?;
        tx.commit().await?;
        Ok(completion)
    }

    /// [`Engine::complete_work_item`] inside the caller's transaction, with
    /// the caller's lease identity (`None` = ownerless: only unleased items
    /// may be completed).
    pub async fn complete_work_item_in_tx(
        &self,
        tx: &mut PgConnection,
        work_item: Uuid,
        owner: Option<&str>,
        patch: serde_json::Value,
    ) -> Result<Completion, EngineError> {
        reject_nul(&patch)?;
        require_object(&patch, "completion patch")?;
        let item = sqlx::query("select instance_id, item_no from rbpmn_work_item where id = $1")
            .bind(work_item)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(EngineError::UnknownWorkItem(work_item))?;
        let instance_id: Uuid = item.get("instance_id");
        let item_no: i64 = item.get("item_no");

        // Lock the instance first: every step on an instance serializes
        // here, in the same order engine-wide (instance row, then item row
        // — the one order that can never deadlock the scheduler or a fail).
        let (definition, proc, mut state) = load_instance(self, &mut *tx, instance_id).await?;
        let item_state = guard_lease(&mut *tx, instance_id, item_no, owner, work_item).await?;

        // The idempotent no-op comes before every other gate: a retried,
        // already-committed completion must converge even if a sibling
        // branch has since raised an incident.
        if item_state != item_state::AVAILABLE && item_state != item_state::LOCKED {
            return Ok(Completion::AlreadyClosed { state: item_state });
        }
        if state.status == InstanceStatus::Failed {
            return Err(EngineError::IncidentOpen(instance_id));
        }
        if state.status != InstanceStatus::Active {
            return Err(EngineError::InstanceNotActive(
                instance_id,
                status_to_db(state.status).to_string(),
            ));
        }

        let events = step_answering_decisions(
            self,
            tx,
            &proc,
            &definition,
            &mut state,
            Command::CompleteWorkItem {
                id: WorkItemId(item_no as u64),
                patch,
            },
        )
        .await?;
        persist_step(tx, &proc, &definition, instance_id, &state, &events).await?;
        Ok(Completion::Advanced(events))
    }

    /// Deliver a message to the single open subscription matching
    /// `(message, correlation key value)`, applying the merge patch in the
    /// same step that advances the token. Exactly one subscription must
    /// match: none is [`EngineError::NoSubscription`] (the message has
    /// nowhere to go — loudly, never dropped), several is
    /// [`EngineError::AmbiguousCorrelation`] (delivering to "one of them"
    /// would be a guess).
    pub async fn correlate(
        &self,
        message: &str,
        key: &str,
        patch: serde_json::Value,
    ) -> Result<Correlation, EngineError> {
        let mut tx = self.pool().begin().await?;
        let correlation = self.correlate_in_tx(&mut tx, message, key, patch).await?;
        tx.commit().await?;
        Ok(correlation)
    }

    /// [`Engine::correlate`] inside the caller's transaction.
    ///
    /// The whole delivery path — resolve without a lock, take the instance
    /// row, re-check *this* subscription under it — is model checked;
    /// `just tla` must stay green if its shape changes.
    ///
    /// * **The re-check is what makes a late message typed**
    ///   (`spec/BoundaryExit.tla`, `LateCallsAreTyped`). Delivery and the
    ///   host's own completion are the two exits from one wait, and exactly
    ///   one may take it: completion withdraws the arm inside its
    ///   transaction (`ArmDiesWithTheWait`), and this re-check answers the
    ///   loser 404 *before* the core is reached.
    ///   `BoundaryExit_NoRecheck.cfg` drops it and TLC walks a message into
    ///   `step` on a closed task. `BoundaryExit_AnyRowRecheck.cfg` is the
    ///   sharper one: it loosens the predicate below to "some subscription
    ///   is still open" rather than *this* `sub_id`, which is exactly the
    ///   plausible-looking edit a message boundary makes available — two
    ///   arms on one token — and TLC shows it lets a withdrawn arm's message
    ///   through. `StepError::UnknownSubscription` would catch it as an
    ///   internal error; the contract is a 404, not a 500.
    /// * **The re-check confirms the row, never its token**
    ///   (`spec/TimerTeardown.tla` under `spec/SubscriptionTeardown.cfg`,
    ///   which binds the module's arm rows to subscriptions). That second
    ///   half is scope teardown's invariant — a reaped token's arms are
    ///   withdrawn *with* it, in `Advancer::tear_down_scope`.
    pub async fn correlate_in_tx(
        &self,
        tx: &mut PgConnection,
        message: &str,
        key: &str,
        patch: serde_json::Value,
    ) -> Result<Correlation, EngineError> {
        reject_nul(&patch)?;
        require_object(&patch, "message patch")?;
        reject_nul_text(message, "message name")?;
        reject_nul_text(key, "correlation key")?;
        // Resolve without a lock, then lock the instance (the same order as
        // every step path) and re-check the subscription under it. Only
        // *active* instances count: an incident-frozen instance keeps its
        // subscription rows (frozen for repair), and those must not block
        // delivery to a live instance sharing the key — or answer for a key
        // that otherwise has no destination.
        let matches = sqlx::query(
            "select s.instance_id, s.subscription_no from rbpmn_subscription s \
             join rbpmn_instance i on i.id = s.instance_id \
             where s.message_name = $1 and s.correlation_key = $2 \
               and i.status = 'active' limit 2",
        )
        .bind(message)
        .bind(key)
        .fetch_all(&mut *tx)
        .await?;
        let row = match matches.as_slice() {
            [] => {
                return Err(EngineError::NoSubscription {
                    message: message.to_string(),
                    key: key.to_string(),
                });
            }
            [row] => row,
            _ => {
                return Err(EngineError::AmbiguousCorrelation {
                    message: message.to_string(),
                    key: key.to_string(),
                });
            }
        };
        let instance_id: Uuid = row.get("instance_id");
        let subscription_no: i64 = row.get("subscription_no");

        let (definition, proc, mut state) = load_instance(self, &mut *tx, instance_id).await?;
        if state.status == InstanceStatus::Failed {
            return Err(EngineError::IncidentOpen(instance_id));
        }
        if state.status != InstanceStatus::Active {
            return Err(EngineError::InstanceNotActive(
                instance_id,
                status_to_db(state.status).to_string(),
            ));
        }
        // A concurrent step (boundary timer, terminate, another delivery)
        // may have withdrawn it between resolve and lock.
        let sub_id = SubscriptionId(subscription_no as u64);
        if !state.subscriptions().any(|(id, _)| id == sub_id) {
            return Err(EngineError::NoSubscription {
                message: message.to_string(),
                key: key.to_string(),
            });
        }

        let events = step_answering_decisions(
            self,
            tx,
            &proc,
            &definition,
            &mut state,
            Command::DeliverMessage { id: sub_id, patch },
        )
        .await?;
        persist_step(tx, &proc, &definition, instance_id, &state, &events).await?;
        Ok(Correlation {
            instance_id,
            events,
        })
    }

    /// Handler failure: spend one retry (with exponential backoff before the
    /// item becomes claimable again), or — budget exhausted — raise the named
    /// error into the core: a matching error boundary takes its path; no
    /// match freezes the instance in the incident state.
    pub async fn fail_work_item(
        &self,
        work_item: Uuid,
        options: &FailOptions,
    ) -> Result<FailOutcome, EngineError> {
        let mut tx = self.pool().begin().await?;
        let outcome = self
            .fail_work_item_in_tx(&mut tx, work_item, options)
            .await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// [`Engine::fail_work_item`] inside the caller's transaction.
    pub async fn fail_work_item_in_tx(
        &self,
        tx: &mut PgConnection,
        work_item: Uuid,
        options: &FailOptions,
    ) -> Result<FailOutcome, EngineError> {
        // Scrubbed, not rejected: a NUL in a handler's failure message must
        // not wedge the fail path itself into an abort loop.
        let options = &FailOptions {
            error_code: options.error_code.as_deref().map(scrub_nul),
            detail: options.detail.as_deref().map(scrub_nul),
            owner: options.owner.clone(),
        };
        let item = sqlx::query("select instance_id, item_no from rbpmn_work_item where id = $1")
            .bind(work_item)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(EngineError::UnknownWorkItem(work_item))?;
        let instance_id: Uuid = item.get("instance_id");
        let item_no: i64 = item.get("item_no");

        let (definition, proc, mut state) = load_instance(self, &mut *tx, instance_id).await?;
        let item_state = guard_lease(
            &mut *tx,
            instance_id,
            item_no,
            options.owner.as_deref(),
            work_item,
        )
        .await?;
        if item_state != item_state::AVAILABLE && item_state != item_state::LOCKED {
            // The idempotent no-op, mirroring completion.
            return Ok(FailOutcome::AlreadyClosed { state: item_state });
        }
        if state.status == InstanceStatus::Failed {
            return Err(EngineError::IncidentOpen(instance_id));
        }
        if state.status != InstanceStatus::Active {
            return Err(EngineError::InstanceNotActive(
                instance_id,
                status_to_db(state.status).to_string(),
            ));
        }

        // SET expressions see pre-update values: the backoff exponent uses
        // the failure count before this failure.
        let row = sqlx::query(
            "update rbpmn_work_item set retries = retries - 1, failures = failures + 1, \
             state = 'available', lock_owner = null, lock_until = null, \
             retry_at = clock_timestamp() + \
               make_interval(secs => $3 * power(3, least(failures, 20))), \
             last_failure = coalesce($4, last_failure) \
             where instance_id = $1 and item_no = $2 \
               and state in ('available', 'locked') \
             returning retries, element_id, topic",
        )
        .bind(instance_id)
        .bind(item_no)
        .bind(self.retry_backoff().as_secs_f64())
        .bind(options.detail.as_deref())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(EngineError::UnknownWorkItem(work_item))?;
        let retries: i32 = row.get("retries");
        let element_id: String = row.get("element_id");
        let topic: String = row.get("topic");

        let outcome = if retries > 0 {
            insert_engine_event(
                &mut *tx,
                &definition,
                instance_id,
                "work-item-retrying",
                Some(element_id.as_str()),
                serde_json::json!({
                    "kind": "work-item-retrying",
                    "element": element_id,
                    "retriesLeft": retries,
                    "message": options.detail,
                }),
            )
            .await?;
            notify_work(&mut *tx, &topic).await?;
            FailOutcome::Retrying {
                retries_left: retries,
            }
        } else {
            let events = step_answering_decisions(
                self,
                tx,
                &proc,
                &definition,
                &mut state,
                Command::RaiseError {
                    id: WorkItemId(item_no as u64),
                    code: options.error_code.clone(),
                },
            )
            .await?;
            persist_step(tx, &proc, &definition, instance_id, &state, &events).await?;
            if events
                .iter()
                .any(|e| matches!(e, Event::IncidentRaised { .. }))
            {
                FailOutcome::IncidentRaised
            } else {
                FailOutcome::ErrorCaught(events)
            }
        };
        Ok(outcome)
    }
}

fn status_to_db(status: InstanceStatus) -> &'static str {
    match status {
        InstanceStatus::Created | InstanceStatus::Active => "active",
        InstanceStatus::Completed => "completed",
        InstanceStatus::Terminated => "terminated",
        InstanceStatus::Failed => "failed",
    }
}

fn status_from_db(status: &str) -> Result<InstanceStatus, EngineError> {
    match status {
        "active" => Ok(InstanceStatus::Active),
        "completed" => Ok(InstanceStatus::Completed),
        "terminated" => Ok(InstanceStatus::Terminated),
        "failed" => Ok(InstanceStatus::Failed),
        other => Err(internal(format!("unknown instance status '{other}'"))),
    }
}

fn work_kind_from_db(kind: &str) -> Result<WorkKind, EngineError> {
    match kind {
        "service" => Ok(WorkKind::Service),
        "user" => Ok(WorkKind::User),
        other => Err(internal(format!("unknown work item kind '{other}'"))),
    }
}

fn internal(message: String) -> EngineError {
    EngineError::Compile(rbpmn_core::CompileError::Internal(message))
}

/// RFC 7386 replaces the whole target when the patch is not an object — for
/// the variables document that means one stray scalar destroys every
/// variable. Reject loudly at the boundary; the same rule guards initial
/// variables (the document must *be* an object).
fn require_object(value: &serde_json::Value, what: &str) -> Result<(), EngineError> {
    if !value.is_object() {
        return Err(EngineError::InvalidVariables(format!(
            "{what} must be a JSON object (a non-object RFC 7386 merge patch \
             would replace the entire variables document)"
        )));
    }
    Ok(())
}

/// The one rendering for a public timestamp: RFC 3339, UTC, microsecond
/// precision, database time. Used by the event stream and the retention
/// archive, which are documented as the same contract, so the format cannot
/// drift between them. (`inspect.rs`'s `due_at` deliberately renders to the
/// second — a timer's due time is a schedule, not a stream position — and is
/// left alone rather than folded in here.)
pub(crate) fn ts(column: &str, alias: &str) -> String {
    format!(
        "to_char({column} at time zone 'UTC', \'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"\') as {alias}"
    )
}

/// Text parameters bound into queries hit the same NUL limitation as jsonb;
/// reject at the boundary so it is a 400, not a transaction-poisoning 500.
pub(crate) fn reject_nul_text(value: &str, what: &str) -> Result<(), EngineError> {
    if value.contains('\u{0}') {
        return Err(EngineError::InvalidVariables(format!(
            "{what} must not contain \\u0000 (PostgreSQL cannot store it)"
        )));
    }
    Ok(())
}

/// Diagnostic text (failure details, error codes) is scrubbed, not rejected:
/// refusing the *fail* path over a NUL byte would loop the failure forever —
/// the exact wedge the fail path exists to resolve.
fn scrub_nul(value: &str) -> String {
    value.replace('\u{0}', "\u{fffd}")
}

/// PostgreSQL jsonb cannot represent NUL in strings; reject it loudly at the
/// boundary instead of poisoning the step transaction forever.
fn reject_nul(value: &serde_json::Value) -> Result<(), EngineError> {
    fn has_nul(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::String(s) => s.contains('\u{0}'),
            serde_json::Value::Array(a) => a.iter().any(has_nul),
            serde_json::Value::Object(m) => {
                m.iter().any(|(k, v)| k.contains('\u{0}') || has_nul(v))
            }
            _ => false,
        }
    }
    if has_nul(value) {
        return Err(EngineError::InvalidVariables(
            "strings must not contain \\u0000 (PostgreSQL jsonb cannot store it)".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn compile_row(row: &PgRow, key: &str) -> Result<ExecutableProcess, EngineError> {
    // A manifest that stopped deserializing is corruption or an unmigrated
    // schema change — loudly reject, never silently run with empty bindings.
    let bindings: Bindings = serde_json::from_value(row.get::<serde_json::Value, _>("bindings"))
        .map_err(|e| {
            internal(format!(
                "stored bindings manifest does not deserialize: {e}"
            ))
        })?;
    let defs = rbpmn_model::parse(&row.get::<String, _>("bpmn_xml"))
        .map_err(|e| internal(e.to_string()))?;
    Ok(ExecutableProcess::compile(&defs, key, &bindings)?)
}

/// The one lease gate: taken under the instance lock (every caller locks
/// the instance first — a single lock order engine-wide, model checked in
/// `spec/LockOrder.tla`), reading the item row FOR UPDATE so a concurrent
/// claim cannot slip between check and mutation. Live foreign leases are
/// refused; closed items pass through (the caller answers its idempotent
/// AlreadyClosed). Returns the item's state.
///
/// This is where completion authority is decided, and `spec/Lease.tla`
/// checks what that has to mean: a worker cannot observe its own expiry, so
/// two workers really can both *believe* they hold one item
/// (`Lease_DoubleBelief.cfg` proves that state is reachable). Safety comes
/// from every mutation being conditional on owner-and-not-expired here —
/// belief is never authority. Re-run `just tla` if this predicate changes.
pub(crate) async fn guard_lease(
    tx: &mut PgConnection,
    instance_id: Uuid,
    item_no: i64,
    owner: Option<&str>,
    work_item: Uuid,
) -> Result<String, EngineError> {
    let row = sqlx::query(
        "select state, lock_owner, \
         (lock_until is not null and lock_until > now()) as lease_live \
         from rbpmn_work_item where instance_id = $1 and item_no = $2 for update",
    )
    .bind(instance_id)
    .bind(item_no)
    .fetch_one(&mut *tx)
    .await?;
    let state: String = row.get("state");
    if state == item_state::LOCKED
        && row.get::<bool, _>("lease_live")
        && owner != row.get::<Option<String>, _>("lock_owner").as_deref()
    {
        return Err(EngineError::ItemLeased(work_item));
    }
    Ok(state)
}

/// The definition compile cache: definitions are immutable (insert-only,
/// content-hashed, unique (key, version)), so a compiled process is cached
/// forever by definition id. On a hit the whole-XML fetch and O(model)
/// parse+compile are skipped — this runs inside the held instance lock on
/// every step-like operation, so it is the hottest path there is.
pub(crate) async fn compiled_process(
    engine: &Engine,
    tx: &mut PgConnection,
    definition_id: Uuid,
    key: &str,
) -> Result<std::sync::Arc<ExecutableProcess>, EngineError> {
    if let Some(proc) = engine.cached_process(definition_id) {
        return Ok(proc);
    }
    let row = sqlx::query("select bpmn_xml, bindings from rbpmn_definition where id = $1")
        .bind(definition_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| internal(format!("definition {definition_id} has no row")))?;
    let proc = std::sync::Arc::new(compile_row(&row, key)?);
    engine.cache_process(definition_id, proc.clone());
    Ok(proc)
}

/// Locks the instance row and rebuilds the quiescent core state from rows —
/// rows are the runtime truth, this is their inverse. Every mapping is
/// exhaustive: an unknown status/kind/wait is an error, never a guess.
pub(crate) async fn load_instance(
    engine: &Engine,
    tx: &mut PgConnection,
    instance_id: Uuid,
) -> Result<
    (
        DefinitionRef,
        std::sync::Arc<ExecutableProcess>,
        InstanceState,
    ),
    EngineError,
> {
    load_instance_nowait(engine, tx, instance_id, false)
        .await?
        // Only the NOWAIT variant can report lock-busy; a blocking load
        // waits instead. Surfaced as an error rather than a panic: this is
        // the hot step path, inside a held transaction.
        .ok_or_else(|| internal("blocking instance load reported lock-busy".to_string()))
}

/// [`load_instance`] with an optional `FOR UPDATE NOWAIT`: `Ok(None)` when
/// someone else holds the instance row lock — for callers with other work
/// to do (the scheduler must not park its whole drain loop behind one
/// long-running caller transaction).
pub(crate) async fn load_instance_nowait(
    engine: &Engine,
    tx: &mut PgConnection,
    instance_id: Uuid,
    nowait: bool,
) -> Result<
    Option<(
        DefinitionRef,
        std::sync::Arc<ExecutableProcess>,
        InstanceState,
    )>,
    EngineError,
> {
    let sql = format!(
        "select i.definition_id, i.definition_key, i.definition_version, \
                i.status, i.variables, \
                i.next_token, i.next_work_item, i.next_timer, i.next_subscription, \
                i.next_scope \
         from rbpmn_instance i where i.id = $1 for update{}",
        if nowait { " nowait" } else { "" }
    );
    let inst = match sqlx::query(&sql)
        .bind(instance_id)
        .fetch_one(&mut *tx)
        .await
    {
        Ok(row) => row,
        // 55P03 lock_not_available: the NOWAIT caller moves on.
        Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("55P03") => {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };

    let key: String = inst.get("definition_key");
    let definition = DefinitionRef {
        id: inst.get("definition_id"),
        key: key.clone(),
        version: inst.get("definition_version"),
    };
    let proc = compiled_process(engine, &mut *tx, definition.id, &key).await?;
    let status = status_from_db(&inst.get::<String, _>("status"))?;

    let mut timers = Vec::new();
    for row in sqlx::query(
        "select timer_no, token_no, element_id, due_kind, due_spec, remaining \
         from rbpmn_timer where instance_id = $1 order by timer_no",
    )
    .bind(instance_id)
    .fetch_all(&mut *tx)
    .await?
    {
        let element_id: String = row.get("element_id");
        let spec: String = row.get("due_spec");
        let due = match row.get::<String, _>("due_kind").as_str() {
            "duration" => TimerDue::Duration(spec),
            "date" => TimerDue::Date(spec),
            "cycle" => TimerDue::Cycle(spec),
            other => return Err(internal(format!("unknown timer due kind '{other}'"))),
        };
        // A cycle's fire count is positive or absent — the core drops the
        // timer rather than re-arming at zero, and the column checks it.
        // Clamping a corrupt value to zero would arm an occurrence that can
        // never fire and say nothing; reject the row instead.
        let remaining = match row.get::<Option<i32>, _>("remaining") {
            Some(r) if r > 0 => Some(r as u32),
            Some(r) => {
                return Err(internal(format!(
                    "timer '{element_id}' has remaining = {r}; a cycle's fire \
                     count is positive, and every other kind's is absent"
                )));
            }
            None => None,
        };
        timers.push((
            TimerId(row.get::<i64, _>("timer_no") as u64),
            TimerState {
                element: proc.node_by_id(&element_id).ok_or_else(|| {
                    internal(format!("timer references unknown element '{element_id}'"))
                })?,
                token: TokenId(row.get::<i64, _>("token_no") as u64),
                due,
                remaining,
            },
        ));
    }

    let mut subscriptions = Vec::new();
    for row in sqlx::query(
        "select subscription_no, token_no, element_id, message_name, correlation_key \
         from rbpmn_subscription where instance_id = $1 order by subscription_no",
    )
    .bind(instance_id)
    .fetch_all(&mut *tx)
    .await?
    {
        let element_id: String = row.get("element_id");
        subscriptions.push((
            SubscriptionId(row.get::<i64, _>("subscription_no") as u64),
            SubscriptionState {
                element: proc.node_by_id(&element_id).ok_or_else(|| {
                    internal(format!(
                        "subscription references unknown element '{element_id}'"
                    ))
                })?,
                token: TokenId(row.get::<i64, _>("token_no") as u64),
                message: row.get("message_name"),
                key: row.get("correlation_key"),
            },
        ));
    }

    let mut scopes = Vec::new();
    for row in sqlx::query(
        "select scope_no, parent_scope_no, element_id, token_no \
         from rbpmn_scope where instance_id = $1 order by scope_no",
    )
    .bind(instance_id)
    .fetch_all(&mut *tx)
    .await?
    {
        let element_id: String = row.get("element_id");
        scopes.push((
            ScopeId(row.get::<i64, _>("scope_no") as u64),
            ScopeState {
                element: proc.node_by_id(&element_id).ok_or_else(|| {
                    internal(format!("scope references unknown element '{element_id}'"))
                })?,
                parent: ScopeId(row.get::<i64, _>("parent_scope_no") as u64),
                token: TokenId(row.get::<i64, _>("token_no") as u64),
            },
        ));
    }

    let mut tokens = Vec::new();
    for row in sqlx::query(
        "select token_no, element_id, wait_kind, arrived_via, work_item_no, scope_no \
         from rbpmn_token where instance_id = $1 order by token_no",
    )
    .bind(instance_id)
    .fetch_all(&mut *tx)
    .await?
    {
        let element_id: String = row.get("element_id");
        let node = proc
            .node_by_id(&element_id)
            .ok_or_else(|| internal(format!("token references unknown element '{element_id}'")))?;
        let token_no = TokenId(row.get::<i64, _>("token_no") as u64);
        let wait = match row.get::<String, _>("wait_kind").as_str() {
            "join" => {
                let flow_id: String = row.get("arrived_via");
                WaitKind::Join {
                    arrived_via: proc.flow_by_id(&flow_id).ok_or_else(|| {
                        internal(format!("token references unknown flow '{flow_id}'"))
                    })?,
                }
            }
            "work_item" => WaitKind::WorkItem(WorkItemId(row.get::<i64, _>("work_item_no") as u64)),
            // A token waiting on its own timer/subscription is matched by
            // `(token_no, element_id)`, never by token alone: a boundary arm
            // sits on its *host's* token, so a receive task with a message
            // boundary has two subscription rows on one token. Resolving by
            // token alone would take the lowest subscription_no — the host's
            // today, only because `enter` arms the host before its boundaries.
            // That is arm order standing in for intent, and it breaks silently
            // the day anything re-arms or reorders. A token sits at exactly one
            // element and a boundary's id is never its host's, so the
            // element-qualified match is unique by construction — the fsck
            // asserts that ("a message-waiting token has exactly one
            // subscription at its own element"). The timer arm hosts nothing
            // today, but carries the same predicate rather than a comment
            // explaining why it needn't.
            "timer" => WaitKind::Timer(
                timers
                    .iter()
                    .find(|(_, t)| t.token == token_no && t.element == node)
                    .map(|(id, _)| *id)
                    .ok_or_else(|| {
                        internal(format!(
                            "token {token_no:?} waits on a timer that has no row"
                        ))
                    })?,
            ),
            "message" => WaitKind::Message(
                subscriptions
                    .iter()
                    .find(|(_, s)| s.token == token_no && s.element == node)
                    .map(|(id, _)| *id)
                    .ok_or_else(|| {
                        internal(format!(
                            "token {token_no:?} waits on a subscription that has no row"
                        ))
                    })?,
            ),
            "event_gateway" => WaitKind::EventGateway,
            "incident" => WaitKind::Incident,
            // A token parked at a subprocess waits on the scope it opened —
            // the one whose parked token is this one.
            "scope" => WaitKind::Scope(
                scopes
                    .iter()
                    .find(|(_, sc)| sc.token == token_no)
                    .map(|(id, _)| *id)
                    .ok_or_else(|| {
                        internal(format!("token {token_no:?} waits on a scope with no row"))
                    })?,
            ),
            other => return Err(internal(format!("unknown token wait kind '{other}'"))),
        };
        tokens.push((
            token_no,
            Token {
                node,
                scope: ScopeId(row.get::<i64, _>("scope_no") as u64),
                wait,
            },
        ));
    }

    let mut work_items = Vec::new();
    for row in sqlx::query(
        "select item_no, token_no, element_id, kind, topic from rbpmn_work_item \
         where instance_id = $1 and state in ('available', 'locked') order by item_no",
    )
    .bind(instance_id)
    .fetch_all(&mut *tx)
    .await?
    {
        let element_id: String = row.get("element_id");
        work_items.push((
            WorkItemId(row.get::<i64, _>("item_no") as u64),
            WorkItemState {
                element: proc.node_by_id(&element_id).ok_or_else(|| {
                    internal(format!(
                        "work item references unknown element '{element_id}'"
                    ))
                })?,
                token: TokenId(row.get::<i64, _>("token_no") as u64),
                kind: work_kind_from_db(&row.get::<String, _>("kind"))?,
                topic: row.get("topic"),
                open: true,
            },
        ));
    }

    let state = InstanceState::rehydrate(
        status,
        inst.get("variables"),
        tokens,
        work_items,
        timers,
        subscriptions,
        scopes,
        Counters {
            next_token: inst.get::<i64, _>("next_token") as u64,
            next_work_item: inst.get::<i64, _>("next_work_item") as u64,
            next_timer: inst.get::<i64, _>("next_timer") as u64,
            next_subscription: inst.get::<i64, _>("next_subscription") as u64,
            next_scope: inst.get::<i64, _>("next_scope") as u64,
        },
    );
    Ok(Some((definition, proc, state)))
}

/// Projects a completed step: instance columns, token snapshot, work-item
/// transitions from the events, and the append-only event rows. Timer and
/// subscription rows follow the events too — armed rows insert (with
/// `due_at` resolved from **database time**), fired/received/cancelled rows
/// delete, in the same transaction as the step that decided it.
pub(crate) async fn persist_step(
    tx: &mut PgConnection,
    proc: &ExecutableProcess,
    definition: &DefinitionRef,
    instance_id: Uuid,
    state: &InstanceState,
    events: &[Event],
) -> Result<(), EngineError> {
    let status = status_to_db(state.status);
    let counters = state.counters();
    sqlx::query(
        "update rbpmn_instance set status = $2, variables = $3, next_token = $4, \
         next_work_item = $5, next_timer = $6, next_subscription = $7, \
         next_scope = $8, \
         completed_at = case when $2 in ('completed', 'terminated') \
         then now() else completed_at end where id = $1",
    )
    .bind(instance_id)
    .bind(status)
    .bind(&state.variables)
    .bind(counters.next_token as i64)
    .bind(counters.next_work_item as i64)
    .bind(counters.next_timer as i64)
    .bind(counters.next_subscription as i64)
    .bind(counters.next_scope as i64)
    .execute(&mut *tx)
    .await?;

    // Scopes are a snapshot like tokens: few per instance, and wholesale
    // replacement keeps the projection trivially correct under teardown.
    sqlx::query("delete from rbpmn_scope where instance_id = $1")
        .bind(instance_id)
        .execute(&mut *tx)
        .await?;
    // One multi-row insert, like the token snapshot below: round-trips here
    // happen inside the held instance lock, and a deeply nested model has
    // one scope per level.
    let mut scope_nos: Vec<i64> = Vec::new();
    let mut scope_parents: Vec<i64> = Vec::new();
    let mut scope_elements: Vec<&str> = Vec::new();
    let mut scope_tokens: Vec<i64> = Vec::new();
    for (id, scope) in state.scopes() {
        scope_nos.push(id.0 as i64);
        scope_parents.push(scope.parent.0 as i64);
        scope_elements.push(proc.node_id(scope.element));
        scope_tokens.push(scope.token.0 as i64);
    }
    if !scope_nos.is_empty() {
        sqlx::query(
            "insert into rbpmn_scope \
             (instance_id, scope_no, parent_scope_no, element_id, token_no) \
             select $1, s.no, s.parent, s.el, s.tok \
             from unnest($2::bigint[], $3::bigint[], $4::text[], $5::bigint[]) \
               as s(no, parent, el, tok)",
        )
        .bind(instance_id)
        .bind(&scope_nos)
        .bind(&scope_parents)
        .bind(&scope_elements)
        .bind(&scope_tokens)
        .execute(&mut *tx)
        .await?;
    }

    // Token rows are a snapshot of the quiescent state (small per instance;
    // wholesale replace keeps the projection trivially correct). One delete
    // plus one multi-row insert — round-trips inside the held instance lock
    // are the cost that matters here.
    sqlx::query("delete from rbpmn_token where instance_id = $1")
        .bind(instance_id)
        .execute(&mut *tx)
        .await?;
    let mut token_nos: Vec<i64> = Vec::new();
    let mut token_scopes: Vec<i64> = Vec::new();
    let mut token_elements: Vec<&str> = Vec::new();
    let mut wait_kinds: Vec<&str> = Vec::new();
    let mut arrived_vias: Vec<Option<String>> = Vec::new();
    let mut work_item_nos: Vec<Option<i64>> = Vec::new();
    for (id, token) in state.tokens() {
        let (wait_kind, arrived_via, work_item_no) = match &token.wait {
            WaitKind::Join { arrived_via } => {
                ("join", Some(proc.flow(*arrived_via).id.clone()), None)
            }
            WaitKind::WorkItem(item) => ("work_item", None, Some(item.0 as i64)),
            WaitKind::Timer(_) => ("timer", None, None),
            WaitKind::Message(_) => ("message", None, None),
            WaitKind::EventGateway => ("event_gateway", None, None),
            WaitKind::Incident => ("incident", None, None),
            WaitKind::Scope(_) => ("scope", None, None),
            // A decision is answered inside the transaction that asks for
            // it, so no token should reach persistence still holding one.
            // When one does, a step path forgot to drain.
            //
            // Third of three gates, and the only one that produces a usable
            // message. Removing this arm does not compile — the match is
            // exhaustive on purpose — and `rbpmn_token`'s `wait_kind` CHECK
            // constraint has no `'decision'` member, so the insert would be
            // refused even if some path invented a string for it. Aborting
            // here costs the caller one failed operation and leaves the
            // instance exactly as it was, while naming the token and element
            // instead of surfacing a constraint violation. (Not
            // hypothetical: the scheduler's timer path was such a caller.)
            WaitKind::Decision => {
                return Err(internal(format!(
                    "token {} at '{}' reached persistence still awaiting a decision — \
                     a step path advanced a token without answering it",
                    id.0,
                    proc.node_id(token.node)
                )));
            }
        };
        token_scopes.push(token.scope.0 as i64);
        token_nos.push(id.0 as i64);
        token_elements.push(proc.node_id(token.node));
        wait_kinds.push(wait_kind);
        arrived_vias.push(arrived_via);
        work_item_nos.push(work_item_no);
    }
    if !token_nos.is_empty() {
        sqlx::query(
            "insert into rbpmn_token \
             (instance_id, token_no, element_id, wait_kind, arrived_via, work_item_no, scope_no) \
             select $1, t.no, t.el, t.wk, t.via, t.wi, t.sc \
             from unnest($2::bigint[], $3::text[], $4::text[], $5::text[], $6::bigint[], \
                         $7::bigint[]) as t(no, el, wk, via, wi, sc)",
        )
        .bind(instance_id)
        .bind(&token_nos)
        .bind(&token_elements)
        .bind(&wait_kinds)
        .bind(&arrived_vias)
        .bind(&work_item_nos)
        .bind(&token_scopes)
        .execute(&mut *tx)
        .await?;
    }

    let mut event_kinds: Vec<String> = Vec::new();
    let mut event_elements: Vec<Option<String>> = Vec::new();
    let mut event_payloads: Vec<serde_json::Value> = Vec::new();
    let mut armed_timer = false;
    // The due instant of every timer row this step deleted, in epoch seconds:
    // a cycle's re-arm steps from the due of the occurrence it continues, and
    // the core emits `timer-fired` before the `timer-armed` that continues it,
    // so the value is always here by the time it is needed.
    let mut deleted_due: std::collections::BTreeMap<TimerId, f64> =
        std::collections::BTreeMap::new();
    for event in events {
        match event {
            Event::WorkItemCreated {
                id,
                element,
                work_kind,
                topic,
            } => {
                let token_no = state
                    .work_items()
                    .find(|(wid, _)| wid == id)
                    .map(|(_, w)| w.token.0 as i64)
                    .ok_or_else(|| internal("created work item missing from state".into()))?;
                sqlx::query(
                    "insert into rbpmn_work_item \
                     (instance_id, item_no, definition_id, definition_key, \
                      definition_version, token_no, \
                      kind, topic, element_id, state) \
                     values ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'available')",
                )
                .bind(instance_id)
                .bind(id.0 as i64)
                .bind(definition.id)
                .bind(&definition.key)
                .bind(definition.version)
                .bind(token_no)
                .bind(work_kind.to_string())
                .bind(topic)
                .bind(element)
                .execute(&mut *tx)
                .await?;
                if *work_kind == WorkKind::Service {
                    notify_work(tx, topic).await?;
                }
            }
            Event::WorkItemCompleted { id, .. } => {
                set_work_item_state(tx, instance_id, id.0 as i64, item_state::COMPLETED).await?;
            }
            Event::WorkItemCancelled { id, .. } => {
                set_work_item_state(tx, instance_id, id.0 as i64, item_state::CANCELLED).await?;
            }
            Event::WorkItemFailed { id, .. } => {
                set_work_item_state(tx, instance_id, id.0 as i64, item_state::FAILED).await?;
            }
            Event::TimerArmed {
                id,
                element,
                due,
                token,
                continues,
                remaining,
            } => {
                // A cycle's re-arm steps from the due of the occurrence it
                // continues, which this step deleted a few events ago.
                let previous = match continues {
                    Some(prev) => Some(deleted_due.get(prev).copied().ok_or_else(|| {
                        internal(format!(
                            "cycle '{element}' continues timer {} which this step did not fire",
                            prev.0
                        ))
                    })?),
                    None => None,
                };
                insert_timer(
                    tx,
                    instance_id,
                    *id,
                    token.0 as i64,
                    element,
                    due,
                    *remaining,
                    previous,
                )
                .await?;
                armed_timer = true;
            }
            Event::TimerFired { id, .. } | Event::TimerCancelled { id, .. } => {
                // Fired: the delete commits with the step — exactly-once. The
                // due comes back for a cycle's re-arm to step from.
                let due: Option<f64> = sqlx::query_scalar(
                    "delete from rbpmn_timer where instance_id = $1 and timer_no = $2 \
                     returning extract(epoch from due_at)::float8",
                )
                .bind(instance_id)
                .bind(id.0 as i64)
                .fetch_optional(&mut *tx)
                .await?;
                if let Some(due) = due {
                    deleted_due.insert(*id, due);
                }
            }
            Event::MessageSubscribed {
                id,
                element,
                message,
                key,
                token,
            } => {
                let token_no = token.0 as i64;
                sqlx::query(
                    "insert into rbpmn_subscription \
                     (instance_id, subscription_no, token_no, element_id, \
                      message_name, correlation_key) \
                     values ($1, $2, $3, $4, $5, $6)",
                )
                .bind(instance_id)
                .bind(id.0 as i64)
                .bind(token_no)
                .bind(element)
                .bind(message)
                .bind(key)
                .execute(&mut *tx)
                .await?;
            }
            Event::MessageReceived { id, .. } | Event::SubscriptionCancelled { id, .. } => {
                sqlx::query(
                    "delete from rbpmn_subscription \
                     where instance_id = $1 and subscription_no = $2",
                )
                .bind(instance_id)
                .bind(id.0 as i64)
                .execute(&mut *tx)
                .await?;
            }
            // Trace/history-only events project no row deltas. Exhaustive
            // on purpose: a new event variant must be classified here
            // deliberately — a wildcard would let a delta-bearing variant
            // compile straight into silent database drift.
            Event::InstanceStarted { .. }
            | Event::ElementStarted { .. }
            | Event::ElementCompleted { .. }
            | Event::FlowTaken { .. }
            | Event::VariablesPatched { .. }
            | Event::IncidentRaised { .. }
            | Event::CorrelationFailed { .. }
            // Recorded, never projected: the freeze that follows is what
            // changes rows. These carry the *reason* an operator needs.
            | Event::TimerResolveFailed { .. }
            | Event::DuplicateSubscription { .. }
            // The decision pair changes no rows of its own: the token move is
            // carried by ElementStarted/Completed, and the answer is *not*
            // carried by `VariablesPatched` — a decision writes by replacement
            // rather than by merge patch, so it emits none. `DecisionEvaluated`
            // is therefore the only record that the variable document changed,
            // which is precisely why it must stay in the history.
            | Event::DecisionRequested { .. }
            | Event::DecisionEvaluated { .. }
            | Event::InstanceCompleted
            | Event::InstanceTerminated => {}
        }
        let payload = serde_json::to_value(event).expect("event serializes");
        let kind = payload
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("unknown")
            .to_string();
        let element = payload
            .get("element")
            .and_then(|e| e.as_str())
            .map(str::to_string);
        event_kinds.push(kind);
        event_elements.push(element);
        event_payloads.push(payload);
    }
    if armed_timer {
        // Wake sleeping schedulers once per step: a new timer may be due
        // sooner than the min(due_at) they went to sleep on (delivered on
        // commit).
        sqlx::query("select pg_notify('rbpmn_timer', '')")
            .execute(&mut *tx)
            .await?;
    }
    if !event_kinds.is_empty() {
        // One append for the whole step. unnest preserves array order and
        // bigserial ids are assigned row by row within the statement, so
        // per-instance id order stays the emission order.
        sqlx::query(
            "insert into rbpmn_event \
             (instance_id, definition_id, definition_key, kind, element_id, payload) \
             select $1, $2, $3, e.kind, e.element, e.payload \
             from unnest($4::text[], $5::text[], $6::jsonb[]) as e(kind, element, payload)",
        )
        .bind(instance_id)
        .bind(definition.id)
        .bind(&definition.key)
        .bind(&event_kinds)
        .bind(&event_elements)
        .bind(&event_payloads)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// The one insert for `rbpmn_timer`, whatever kind of timer it is — one
/// column list, one `due_at` expression with a branch per kind, so the next
/// column cannot be added to the duration path and forgotten on the cycle
/// path (which far fewer scenarios arm).
///
/// `due_at` is database time, the design's clock authority. A duration is
/// `clock_timestamp() + interval` and a date casts; both are the stable
/// text-input casts PostgreSQL will not fold at plan time, which is what
/// keeps the untaken branches of the CASE harmless. A cycle is computed in
/// **epoch seconds**, because lint made its period fixed-length and
/// `timestamptz + interval '1 day'` is a calendar day in the session's time
/// zone — which `P1D` in a cycle is not, across a daylight-saving change:
///
/// * a re-arm (`previous_due` is the fired occurrence's due): the grid of
///   *that* due — `previous_due + period · k` — at the first occurrence at or
///   after now. The grid is the previous due's, never the time the fire
///   happened to run, so a scheduler an hour late on a weekly cycle still
///   re-arms at previous due + 7 d and the schedule does not drift. `k` is at
///   least 1, so a re-arm is always in the future: an engine that was down
///   for a day on an `R/PT15M` boundary re-arms at the next quarter hour, not
///   96 times back to back. Occurrences missed while it was down are
///   **skipped, never replayed** — and because a bounded `R<n>` counts
///   *fires*, skipping one costs it nothing;
/// * a first arm with an anchor: the anchor fixes the *phase* — the first
///   occurrence at or after now, a future anchor being itself the first due.
///   Occurrences already in the past are never replayed: a definition
///   outlives its anchor;
/// * a first arm without one: now plus the period.
#[allow(clippy::too_many_arguments)]
async fn insert_timer(
    tx: &mut PgConnection,
    instance_id: Uuid,
    id: TimerId,
    token_no: i64,
    element: &str,
    due: &TimerDue,
    remaining: Option<u32>,
    previous_due: Option<f64>,
) -> Result<(), EngineError> {
    let (due_kind, spec) = match due {
        TimerDue::Duration(s) => ("duration", s),
        TimerDue::Date(s) => ("date", s),
        TimerDue::Cycle(s) => ("cycle", s),
    };
    // Validated at lint (a literal) or at arm time (a variable) with this
    // same function; failing here means a row the core never produced.
    let (period, anchor) = match due {
        TimerDue::Cycle(text) => {
            let parts = rbpmn_model::iso8601::split_cycle(text).map_err(|e| {
                internal(format!("cycle '{text}' on '{element}' is not valid: {e}"))
            })?;
            (parts.period_seconds, parts.anchor)
        }
        _ => (0.0, None),
    };
    sqlx::query(
        "insert into rbpmn_timer \
         (instance_id, timer_no, token_no, element_id, due_kind, due_spec, remaining, due_at) \
         values ($1, $2, $3, $4, $5, $6, $7, case \
           when $5 = 'duration' then clock_timestamp() + $6::interval \
           when $5 = 'date' then $6::timestamptz \
           when $8::float8 is not null then to_timestamp( \
             $8::float8 + $9::float8 * greatest(1, ceil( \
               (extract(epoch from clock_timestamp()) - $8::float8) / $9::float8))) \
           when $10::timestamptz is not null then to_timestamp( \
             extract(epoch from $10::timestamptz) \
             + $9::float8 * ceil(greatest(0, extract(epoch from clock_timestamp()) \
                                          - extract(epoch from $10::timestamptz)) / $9::float8)) \
           else to_timestamp(extract(epoch from clock_timestamp()) + $9::float8) \
         end)",
    )
    .bind(instance_id)
    .bind(id.0 as i64)
    .bind(token_no)
    .bind(element)
    .bind(due_kind)
    .bind(spec)
    .bind(remaining.map(|r| r as i32))
    .bind(previous_due)
    .bind(period)
    .bind(anchor.as_deref())
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Wakes worker loops (delivered on commit; SKIP LOCKED arbitrates who wins).
async fn notify_work(tx: &mut PgConnection, topic: &str) -> Result<(), EngineError> {
    sqlx::query("select pg_notify('rbpmn_work', $1)")
        .bind(topic)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

async fn set_work_item_state(
    tx: &mut PgConnection,
    instance_id: Uuid,
    item_no: i64,
    to: &str,
) -> Result<(), EngineError> {
    sqlx::query("update rbpmn_work_item set state = $3 where instance_id = $1 and item_no = $2")
        .bind(instance_id)
        .bind(item_no)
        .bind(to)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Append an engine-generated event (one the pure core did not emit).
/// `element` is `None` for instance-level events — a stored NULL, not an
/// empty string, so consumers can tell "no element" from "some element".
pub(crate) async fn insert_engine_event(
    tx: &mut PgConnection,
    definition: &DefinitionRef,
    instance_id: Uuid,
    kind: &str,
    element: Option<&str>,
    payload: serde_json::Value,
) -> Result<(), EngineError> {
    sqlx::query(
        "insert into rbpmn_event (instance_id, definition_id, definition_key, kind, element_id, payload) \
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(instance_id)
    .bind(definition.id)
    .bind(&definition.key)
    .bind(kind)
    .bind(element)
    .bind(payload)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// The decision artifacts of a definition version, compiled and cached.
///
/// Same shape as [`compiled_process`], keyed the same way and for the same
/// reason: a deployed version is immutable, so this never needs invalidating.
#[cfg(feature = "dmn")]
pub(crate) async fn compiled_decisions(
    engine: &Engine,
    tx: &mut PgConnection,
    definition_id: Uuid,
) -> Result<std::sync::Arc<rbpmn_dmn::Decisions>, EngineError> {
    if let Some(decisions) = engine.cached_decisions(definition_id) {
        return Ok(decisions);
    }
    let artifacts: Vec<String> = sqlx::query_scalar(
        "select dmn_xml from rbpmn_definition_decision \
         where definition_id = $1 order by ordinal",
    )
    .bind(definition_id)
    .fetch_all(&mut *tx)
    .await?;
    // Deploy validated these, and startup re-validation re-checks them, so a
    // failure here means the binary changed under a stored definition. That
    // is an internal error rather than a user one: it must be loud, and it
    // must not look like the decision merely answered nothing.
    let compiled = rbpmn_dmn::Decisions::compile(&artifacts).map_err(|diagnostics| {
        internal(format!(
            "definition {definition_id} has decision artifacts this build cannot compile \
             (deploy accepted them, so the binary or dsntk changed): {}",
            diagnostics
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ))
    })?;
    let compiled = std::sync::Arc::new(compiled);
    engine.cache_decisions(definition_id, compiled.clone());
    Ok(compiled)
}

/// Run one step, then answer any decision it asked for — inside the same
/// transaction, before anything is persisted.
///
/// This is the projection half of the design in `docs/dmn.md`, D3: the pure
/// core parks at a business-rule task and says what it needs; evaluation
/// happens here, where dsntk is allowed; the answer re-enters as command data
/// so a replay never evaluates anything.
///
/// The loop matters. A decision's answer can advance the token straight into
/// *another* business-rule task, so draining until none is pending is what
/// makes a chain of decisions one transaction rather than a wedge.
pub(crate) async fn step_answering_decisions(
    engine: &Engine,
    tx: &mut PgConnection,
    proc: &ExecutableProcess,
    definition: &DefinitionRef,
    state: &mut rbpmn_core::InstanceState,
    command: Command,
) -> Result<Vec<Event>, EngineError> {
    let mut events = step(proc, state, command)?;
    loop {
        // Any token that is waiting — there can be **several**. The core parks
        // a branch on its decision and carries on with the rest of the step,
        // so a parallel split into two business-rule tasks parks both in one
        // `step`. (This comment used to claim the opposite and reason from it;
        // the loop was right and the reason was wrong.) Answering one can also
        // advance a token straight into another business-rule task, so
        // draining until none is pending is what makes a chain of decisions
        // one transaction rather than a wedge.
        let Some(token) = state
            .tokens()
            .find(|(_, t)| t.wait == rbpmn_core::WaitKind::Decision)
            .map(|(id, _)| id)
        else {
            return Ok(events);
        };
        let (answer, reason) = answer_decision(engine, tx, proc, definition, state, token).await?;
        events.extend(step(
            proc,
            state,
            Command::CompleteDecision {
                token,
                answer,
                reason,
            },
        )?);
    }
}

#[cfg(not(feature = "dmn"))]
async fn answer_decision(
    _engine: &Engine,
    _tx: &mut PgConnection,
    _proc: &ExecutableProcess,
    _definition: &DefinitionRef,
    _state: &rbpmn_core::InstanceState,
    _token: rbpmn_core::TokenId,
) -> Result<Answer, EngineError> {
    // Unreachable in practice: deploy refuses a bundle carrying decisions
    // when this feature is off, so no definition with a business-rule task
    // can exist here. Freezing rather than panicking keeps the promise that
    // an engine never takes an instance down for a wiring problem.
    Ok(frozen("this engine was built without DMN support"))
}

#[cfg(feature = "dmn")]
async fn answer_decision(
    engine: &Engine,
    tx: &mut PgConnection,
    proc: &ExecutableProcess,
    definition: &DefinitionRef,
    state: &rbpmn_core::InstanceState,
    token: rbpmn_core::TokenId,
) -> Result<Answer, EngineError> {
    use rbpmn_dmn::Outcome;

    let Some((_, parked)) = state.tokens().find(|(id, _)| *id == token) else {
        return Ok(frozen("the token waiting on this decision is gone"));
    };
    let rbpmn_core::ExecKind::BusinessRule { decision, .. } = &proc.node(parked.node).kind else {
        return Ok(frozen(
            "the token waits on a decision at a node that is not one",
        ));
    };
    let decisions = compiled_decisions(engine, tx, definition.id()).await?;
    let Some(invocable) = decisions
        .invocables()
        .iter()
        .find(|i| &i.name == decision)
        .cloned()
    else {
        // Deploy's `unresolved-decision` prevents this; if it ever happens the
        // instance freezes rather than guessing.
        return Ok(frozen(&format!(
            "no decision named {decision:?} in this definition's DMN"
        )));
    };

    match decisions.evaluate(&invocable, &state.variables) {
        Outcome::Value(value) => Ok((Some(value), None)),
        // A null is an answer (docs/dmn.md, "What P1 measured"): dsntk cannot
        // tell a legal "no rule matched" from a broken evaluation, so neither
        // can this. It is written as JSON null and the token continues; a
        // modeller who wants that to be an error models it, with a gateway on
        // `result = null`.
        // The reason rides along even though the token continues: it is the
        // only thing separating "no rule matched" from "the input was the
        // wrong type", and an operator reading the history needs it.
        Outcome::Null { reason } => Ok((
            Some(serde_json::Value::Null),
            reason.as_deref().map(one_line),
        )),
        // This one *is* unambiguous: a value the variable document cannot
        // hold. Dropping it silently would be worse than freezing — and
        // freezing without saying why is a dead end for whoever finds it.
        Outcome::Unrepresentable(why) => Ok(frozen(&why)),
    }
}

/// What an evaluation hands back: the answer, and the evaluator's prose about
/// it. `None` for the answer is the freeze.
type Answer = (Option<serde_json::Value>, Option<String>);

fn frozen(reason: &str) -> Answer {
    (None, Some(one_line(reason)))
}

/// dsntk's messages are multi-line and occasionally long; an event payload is
/// read in a table cell. Collapse the whitespace and cap the length — the
/// point is to name the failure, not to reproduce a stack trace.
fn one_line(reason: &str) -> String {
    const LIMIT: usize = 500;
    let mut text: String = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > LIMIT {
        text = text.chars().take(LIMIT).collect::<String>() + "…";
    }
    text
}
