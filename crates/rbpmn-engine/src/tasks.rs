//! The pull-mode task API: external workers and user-task frontends claim
//! work by topic (`FOR UPDATE SKIP LOCKED`), hold it under a short renewable
//! lease, and complete/fail it owner-checked. Pull and push mode share
//! `rbpmn_work_item` — same rows, same lease model, same completion path.
//!
//! Ordering is FIFO by default (`created_at` ascending, tie-broken by item
//! number — a fair queue; `created_at` is database time, so it is consistent
//! across nodes) or LIFO opt-in (freshest-first triage). Honesty note from
//! the design brief: under concurrent consumers FIFO is fair-but-not-strict
//! — `SKIP LOCKED` skips rows a peer is claiming; strict global FIFO would
//! serialize all consumers, the wrong trade for a work queue.
//!
//! Filters compare fields of the owning instance's **live** variables — the
//! single variable document, never a snapshot that could silently diverge.
//!
//! The lease protocol — claim, renew, expire, release, complete — is model
//! checked in `spec/Lease.tla`: no double delivery, exactly-once completion
//! under at-least-once delivery, completion authority following the current
//! lease rather than the holder's belief, a live lease freed by nobody but
//! its own holder, a release freeing only the lease it named even when the
//! client retries it, and nothing ever stranded. Changing the
//! TTL/renewal/ownership rules here means re-running `just tla`.
//! The filter compiler emits exactly the expression shape that
//! [`Engine::declare_index`] indexes (`variables->>'field'` with a literal
//! `definition_key` predicate), so declared indexes actually serve the
//! query; undeclared fields stay correct via sequential scan, just slower.
//!
//! A declaration also carries a *scope*. The definition-scoped index above is
//! what `TaskFilter` needs; [`Engine::declare_shared_index`] is the same
//! expression without the key predicate, for the lookup that spans
//! definitions (`crate::instances`). Both build through one path, because the
//! interesting part — an interrupted `CONCURRENTLY` build's corpse, and the
//! try-lock that keeps two builds off one table — is the same for both.

use crate::{Completion, Engine, EngineError, FailOptions, FailOutcome};
use rbpmn_core::{IndexDeclaration, IndexScope};
use sqlx::Row;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskOrder {
    /// Oldest first (`created_at` ascending, tie-broken by item number).
    #[default]
    Fifo,
    /// Freshest first — triage mode.
    Lifo,
}

/// Equality filters over the owning instance's live variables, ANDed.
/// Definition-scoped on purpose: fields mean something per definition, and
/// the literal `definition_key` is what lets the planner use the partial
/// indexes `declare_index` creates.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskFilter {
    definition_key: String,
    fields: BTreeMap<String, String>,
}

impl TaskFilter {
    pub fn new(definition_key: impl Into<String>) -> Self {
        TaskFilter {
            definition_key: definition_key.into(),
            fields: BTreeMap::new(),
        }
    }

    /// Require `variables->>'field' = value` (text comparison of the JSON
    /// text form, exactly what the declared index indexes).
    pub fn field(mut self, field: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.insert(field.into(), value.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct GetTaskOptions {
    /// Lease holder identity, recorded in `lock_owner`; completion and
    /// failure of a live-leased task require it.
    pub owner: String,
    /// Lease TTL; renew via [`Engine::extend_lock`] while working.
    pub ttl: Duration,
    pub order: TaskOrder,
    pub filter: Option<TaskFilter>,
}

impl GetTaskOptions {
    pub fn new(owner: impl Into<String>) -> Self {
        GetTaskOptions {
            owner: owner.into(),
            ttl: Duration::from_secs(600),
            order: TaskOrder::Fifo,
            filter: None,
        }
    }
}

/// A claimed task: everything a worker or task UI needs to act on it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedTask {
    pub id: Uuid,
    pub instance_id: Uuid,
    pub definition_key: String,
    /// The definition the owning instance is pinned to. An instance never
    /// migrates, so this is the version it will run to completion, not
    /// `max(version)` of the key: version-pinned per-task metadata on the
    /// embedding side must resolve against exactly this pair. Both are
    /// columns of the claimed row (migration 0011), written when the item
    /// was created and immutable after — the claim reads them, it does not
    /// look them up.
    pub definition_id: Uuid,
    pub definition_version: i32,
    pub element_id: String,
    pub topic: String,
    pub kind: String,
    /// The instance's live variables as of the claim.
    pub variables: serde_json::Value,
    /// Lease deadline (RFC 3339 UTC, database time). Renew before it.
    pub lock_until: String,
    /// Which claim this is — the item's lease epoch, bumped by every claim
    /// and by nothing else (a renewal continues the same one). Hand it back
    /// to [`Engine::release_task`]: that is what makes a release safe to
    /// retry, because a replayed request names an epoch that is gone.
    /// Opaque outside the engine, and the same idea as an ETag.
    pub lease_no: i64,
}

/// The typed heartbeat result: a client whose lease was lost must be able to
/// tell its user what happened — never fail silently, and never with the
/// wrong story.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockExtension {
    Extended {
        until: String,
    },
    /// The lease is not ours (any more). `state` is the work item's current
    /// `state` column, and the distinction it exists for is *withdrawn or
    /// not*:
    ///
    /// * `cancelled` — the **process** withdrew the item: an interrupting
    ///   boundary fired, the instance terminated, an enclosing scope was torn
    ///   down. There is nothing to go back to. "The ticket was paid", not
    ///   "your task was reassigned" (docs/design/boundary-messages.md §1.3).
    /// * `completed` / `failed` — the item was already decided, by a peer or
    ///   by an earlier call of your own.
    /// * `locked` / `available` — the claim is simply not yours any more.
    ///   Deliberately *not* "reassigned": a lease that lapsed with nobody
    ///   reclaiming it still reads `locked`, with `lock_owner` still naming
    ///   the caller, so all this says is "claim it again if you still want
    ///   it". Telling lapsed from reassigned would need the lease columns,
    ///   and they are not in this answer on purpose — a heartbeat's job is
    ///   to say whether to keep working, not to narrate the queue.
    Lost {
        state: String,
    },
}

/// The typed release result. Handing a task back is not the same as never
/// having held it: a frontend that releases on "close without deciding"
/// must be able to tell "it is back on the queue" from "your lease was
/// already gone and someone else may hold it now".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Released {
    /// The row was still locked in our name; the task is `available` again
    /// and the next claim can take it.
    Released,
    /// Nothing was handed back — the lease had been reassigned, the request
    /// named an epoch that is no longer current (a replay, or a claim the
    /// caller has since replaced with a newer one), or the task is not
    /// locked at all any more. Like [`LockExtension::Lost`], not an error:
    /// the client tells its user the task moved on, and a retry that reports
    /// this has done no harm — which is the whole point of the epoch.
    ///
    /// `state` carries the same vocabulary as [`LockExtension::Lost`], and
    /// separates the same thing: `cancelled` is the process having withdrawn
    /// the item (nothing to hand back, and nothing to re-claim),
    /// `completed`/`failed` is an item already decided, and
    /// `locked`/`available` is a claim that is no longer yours — lapsed or
    /// taken by someone else, which this answer does not distinguish.
    Lost { state: String },
}

/// The state column of a work item, read in a statement of its own — what
/// [`LockExtension::Lost`] and [`Released::Lost`] report. Called only after
/// an attempt matched no row, and deliberately *after*: a fresh snapshot can
/// only be more current than the attempt's (see [`Engine::extend_lock`] for
/// why folding it into the attempt's statement reports a stale state).
/// `None` — no such row at all — is [`EngineError::UnknownWorkItem`], the
/// same answer completion and failure give.
async fn current_item_state(engine: &Engine, task: Uuid) -> Result<String, EngineError> {
    sqlx::query_scalar("select state from rbpmn_work_item where id = $1")
        .bind(task)
        .fetch_optional(engine.pool())
        .await?
        .ok_or(EngineError::UnknownWorkItem(task))
}

/// A lease must be plausible: zero would mint a lock expired at birth (two
/// owners on one task moments later), and an absurd TTL turns into a
/// Postgres interval error surfaced as a 500. Reject both at the boundary.
fn validate_ttl(ttl: Duration) -> Result<(), EngineError> {
    const MIN_TTL: Duration = Duration::from_millis(10);
    const MAX_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
    if ttl < MIN_TTL || ttl > MAX_TTL {
        return Err(EngineError::InvalidVariables(format!(
            "lease ttl must be between 10ms and 30 days, got {ttl:?}"
        )));
    }
    Ok(())
}

/// Field names embed in SQL as literals (the planner needs the literal to
/// match the index expression), so they are validated hard — same segment
/// grammar as FEEL qualified names.
pub(crate) fn validate_field(field: &str) -> Result<(), EngineError> {
    // One grammar source: a field is a single-segment FEEL qualified name.
    let ok = matches!(
        rbpmn_model::condition::parse_qname(field),
        Ok(path) if path.len() == 1
    );
    if !ok {
        return Err(EngineError::InvalidVariables(format!(
            "'{field}' is not a valid filter/index field name \
             ([A-Za-z_][A-Za-z0-9_]*)"
        )));
    }
    Ok(())
}

/// Up-front validation for a manifest index entry (deploy calls this before
/// anything persists).
///
/// The definition key is checked only for the scope that *embeds* it: a
/// shared index's DDL carries no definition key at all, and rejecting a
/// deploy over a literal that never reaches SQL would be strictness pointed
/// at the wrong thing. The key is still validated wherever it is embedded —
/// `declare_index` and `compile_filter` both do it at their own call sites.
pub(crate) fn validate_index_declaration(
    key: &str,
    decl: &IndexDeclaration,
) -> Result<(), EngineError> {
    if decl.scope == IndexScope::Definition {
        validate_definition_key(key)?;
    }
    validate_field(&decl.field)
}

/// Whole-manifest validation: every entry, plus the one contradiction a
/// single manifest can state — the same field declared at both scopes.
///
/// Two indexes over one expression is a legitimate *deployment* (a shared
/// index serves the cross-definition lookup; a definition-scoped one serves
/// `TaskFilter` strictly better), which is why the cross-definition case is a
/// warning. But one manifest saying both about one field says nothing rbpmn
/// can act on, so it is refused rather than resolved.
pub(crate) fn validate_index_declarations(
    key: &str,
    indexes: &std::collections::BTreeSet<IndexDeclaration>,
) -> Result<(), EngineError> {
    for decl in indexes {
        validate_index_declaration(key, decl)?;
    }
    // Entries sort by field first, so a contradiction is always adjacent.
    for pair in indexes.iter().collect::<Vec<_>>().windows(2) {
        if pair[0].field == pair[1].field {
            return Err(EngineError::InvalidVariables(format!(
                "index field '{}' is declared at scope '{}' and '{}' in one \
                 manifest — a manifest must say one thing about a field",
                pair[0].field,
                pair[0].scope.as_str(),
                pair[1].scope.as_str(),
            )));
        }
    }
    Ok(())
}

/// Definition keys embed in SQL as literals too (partial-index predicate).
/// BPMN ids are XML NCNames; we accept the safe subset and reject the rest.
pub(crate) fn validate_definition_key(key: &str) -> Result<(), EngineError> {
    let ok = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if !ok {
        return Err(EngineError::InvalidVariables(format!(
            "'{key}' is not a safe definition key for filtering/indexing \
             ([A-Za-z0-9_.-]+)"
        )));
    }
    Ok(())
}

/// Compiles a filter to SQL predicates over the instance alias `i`. Emits
/// EXACTLY the declared-index expression shape: literal definition_key,
/// literal field name, parameterized value.
fn compile_filter(
    filter: &TaskFilter,
    args: &mut Vec<String>,
    first_param: usize,
) -> Result<String, EngineError> {
    validate_definition_key(&filter.definition_key)?;
    let mut sql = format!(" and i.definition_key = '{}'", filter.definition_key);
    for (field, value) in &filter.fields {
        validate_field(field)?;
        crate::runtime::reject_nul_text(value, "filter value")?;
        write!(
            sql,
            " and i.variables->>'{field}' = ${}",
            first_param + args.len()
        )
        .expect("write to string");
        args.push(value.clone());
    }
    Ok(sql)
}

impl Engine {
    /// Claim the next task on `topic` (any kind — pull-mode service workers
    /// and user-task frontends share this API). `None` when nothing is
    /// claimable right now. Single-statement claim: atomic, `SKIP LOCKED`
    /// arbitrates competing consumers, expired leases count as available.
    pub async fn get_task(
        &self,
        topic: &str,
        options: &GetTaskOptions,
    ) -> Result<Option<LockedTask>, EngineError> {
        crate::runtime::reject_nul_text(topic, "topic")?;
        crate::runtime::reject_nul_text(&options.owner, "owner")?;
        validate_ttl(options.ttl)?;
        let direction = match options.order {
            TaskOrder::Fifo => "asc",
            TaskOrder::Lifo => "desc",
        };
        let mut args: Vec<String> = Vec::new();
        let filter_sql = match &options.filter {
            Some(filter) => compile_filter(filter, &mut args, 4)?,
            None => String::new(),
        };
        let sql = format!(
            "update rbpmn_work_item set state = 'locked', lock_owner = $2, \
             lock_until = now() + make_interval(secs => $3), \
             lease_no = lease_no + 1 \
             where id = (select w.id from rbpmn_work_item w \
                join rbpmn_instance i on i.id = w.instance_id \
                where w.topic = $1 and {claimable}{filter_sql} \
                order by w.created_at {direction}, w.item_no {direction} \
                limit 1 for update of w skip locked) \
             returning id, instance_id, definition_key, definition_id, \
               definition_version, element_id, topic, kind, lease_no, \
               to_char(lock_until at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 as lock_until, \
               (select variables from rbpmn_instance i2 \
                 where i2.id = rbpmn_work_item.instance_id) as variables",
            claimable = crate::CLAIMABLE,
        );
        let mut query = sqlx::query(&sql)
            .bind(topic)
            .bind(&options.owner)
            .bind(options.ttl.as_secs_f64());
        for value in &args {
            query = query.bind(value);
        }
        let Some(row) = query.fetch_optional(self.pool()).await? else {
            return Ok(None);
        };
        // Claim and variables read are ONE statement (the subquery in the
        // RETURNING list): a step committing between two statements can no
        // longer hand back post-claim variables — one claim, one snapshot.
        let instance_id: Uuid = row.get("instance_id");
        let variables: serde_json::Value = row.get("variables");
        Ok(Some(LockedTask {
            id: row.get("id"),
            instance_id,
            definition_key: row.get("definition_key"),
            definition_id: row.get("definition_id"),
            definition_version: row.get("definition_version"),
            element_id: row.get("element_id"),
            topic: row.get("topic"),
            kind: row.get("kind"),
            variables,
            lock_until: row.get("lock_until"),
            lease_no: row.get("lease_no"),
        }))
    }

    /// How many tasks on `topic` are claimable right now (dashboard
    /// indications). Same predicates and filter shape as [`Engine::get_task`].
    pub async fn count_tasks(
        &self,
        topic: &str,
        filter: Option<&TaskFilter>,
    ) -> Result<u64, EngineError> {
        crate::runtime::reject_nul_text(topic, "topic")?;
        let mut args: Vec<String> = Vec::new();
        let filter_sql = match filter {
            Some(filter) => compile_filter(filter, &mut args, 2)?,
            None => String::new(),
        };
        let sql = format!(
            "select count(*) from rbpmn_work_item w \
             join rbpmn_instance i on i.id = w.instance_id \
             where w.topic = $1 and {claimable}{filter_sql}",
            claimable = crate::CLAIMABLE,
        );
        let mut query = sqlx::query(&sql).bind(topic);
        for value in &args {
            query = query.bind(value);
        }
        let count: i64 = query.fetch_one(self.pool()).await?.get(0);
        Ok(count as u64)
    }

    /// Heartbeat: extend the lease while demonstrably still working. A lost
    /// lease (owner mismatch, expiry, the item closed or withdrawn) is a
    /// typed result, not an error, and it carries the item's `state` so the
    /// client can tell its user *which* of those happened — see
    /// [`LockExtension::Lost`].
    ///
    /// The `state = 'locked'` predicate is what turns a withdrawn item into
    /// a loss: cancellation writes the state column and nothing else (a
    /// lease is a row value and the process does not take it), so the lease
    /// columns on a cancelled row still name their holder. The state column
    /// is therefore the only honest thing to report.
    ///
    /// **Two statements, deliberately — do not "optimise" this into one.**
    /// The obvious single statement is a CTE (`with attempt as (update …
    /// returning …) select …, (select state from rbpmn_work_item where id =
    /// $1)`), and it reports the *wrong* state exactly when it matters.
    /// Under READ COMMITTED an `UPDATE` re-evaluates its predicate against
    /// the latest committed row version when it blocks on a concurrent
    /// writer (EvalPlanQual), while every plain sub-select in the same
    /// statement reads that statement's snapshot. So when a boundary's
    /// cancellation commits between the snapshot and the update, the update
    /// correctly matches nothing and the sub-select still says `locked` —
    /// "your task was reassigned" for an item the process withdrew, which is
    /// the one answer this field exists to get right.
    ///
    /// A second, plain statement takes a fresh snapshot, so what it reports
    /// is at least as current as the attempt: a transition slipping between
    /// the two can only make the answer *more* current, never stale. No
    /// transaction and no `FOR SHARE` around the pair — a lease is a row
    /// value, and locking it to read it would be the thing this API is not.
    ///
    /// A task id that matches nothing at all is [`EngineError::UnknownWorkItem`]
    /// (404), not a loss — the same answer [`Engine::complete_task`] and
    /// [`Engine::fail_task`] already give for an unknown id, and a
    /// deliberate alignment: "your lease is gone" is a claim about a task
    /// that exists.
    pub async fn extend_lock(
        &self,
        task: Uuid,
        owner: &str,
        ttl: Duration,
    ) -> Result<LockExtension, EngineError> {
        crate::runtime::reject_nul_text(owner, "owner")?;
        validate_ttl(ttl)?;
        let row = sqlx::query(
            "update rbpmn_work_item set lock_until = now() + make_interval(secs => $3) \
             where id = $1 and lock_owner = $2 and state = 'locked' and lock_until > now() \
             returning to_char(lock_until at time zone 'UTC', \
               'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as lock_until",
        )
        .bind(task)
        .bind(owner)
        .bind(ttl.as_secs_f64())
        .fetch_optional(self.pool())
        .await?;
        match row {
            Some(row) => Ok(LockExtension::Extended {
                until: row.get("lock_until"),
            }),
            None => Ok(LockExtension::Lost {
                state: current_item_state(self, task).await?,
            }),
        }
    }

    /// Hand a claimed task back to the queue without deciding it — the
    /// third exit from a claim, beside [`Engine::complete_task`] and
    /// [`Engine::fail_task`]. A user opened the task and closed it again;
    /// nothing happened, and the next claim should be able to take it
    /// immediately rather than waiting out the lease.
    ///
    /// Scoped to one claim: `lease` is the [`LockedTask::lease_no`] the claim
    /// handed back. That is what makes this verb safe to retry, and it is
    /// the only verb in the task API that needed the help — `extend_lock`
    /// carries `lock_until > now()` and a repeated completion converges on
    /// `AlreadyClosed`, but a replayed release guarded by owner alone could
    /// land on a *later* claim by the same owner and free a live lease. FIFO
    /// makes that the likely case rather than a remote one: the item just
    /// released is the oldest, so it is what the same client's next claim
    /// returns.
    ///
    /// The owner is checked too, though the epoch alone already identifies
    /// the claim. It is not redundant defence: epochs are small integers and
    /// therefore guessable, so dropping the owner would let anyone holding a
    /// task id end a stranger's claim.
    ///
    /// A no-op once the claim has moved on — by reassignment or by the
    /// caller's own later claim — so neither can be unlocked from under its
    /// holder. It restores claimability, by a different row state than expiry does —
    /// expiry leaves `locked` with a stale owner and a past deadline, this
    /// writes `available` with both nulled, and only `CLAIMABLE` reads them
    /// as the same thing (the `rbpmn_work_item_acquire` partial index does
    /// not: a released item is in it, an expired one is not). What the two
    /// share is that neither is a history event, which is why this stays
    /// silent: push workers pick a released item up the same way they pick
    /// up an expired lease — on their next poll. This is the same statement
    /// the push worker runs when it must drop a claim it cannot serve.
    ///
    /// Expiry alone is not checked, deliberately: an expired lease nobody
    /// reclaimed is already claimable, so releasing it only tidies the stale
    /// `lock_owner` off the row. Once someone *has* reclaimed it, the owner
    /// no longer matches and the release is the no-op it must be — the guard
    /// that matters is ownership, not the clock.
    ///
    /// Both halves of the guard are load-bearing, and `spec/Lease.tla`
    /// checks each with its own counterexample config:
    ///
    /// * the owner, by `LiveLeaseEndsOnlyByItsHolderOrTheProcess` — a live
    ///   lease ends by the clock, by its own holder's hand, or by the
    ///   process withdrawing the item, and by no *other worker*. (The
    ///   property was `LiveLeaseEndsOnlyByItsHolder` until the message
    ///   boundary round: terminate and the interrupting timer boundary have
    ///   cancelled leased items since phase 3, so the stronger name was
    ///   never true of the shipped engine — it held vacuously over an actor
    ///   `Lease.tla` had no action for.) Every other route back to the queue
    ///   already excludes a live holder (a claim needs `CLAIMABLE`, which
    ///   means the lease lapsed; a completion or failure needs
    ///   `guard_lease`), so without the owner check this would be the one
    ///   action able to free an item out from under a worker still working
    ///   on it (`Lease_UncheckedRelease.cfg`).
    /// * the epoch, by `ReleaseFreesOnlyTheLeaseItNamed` — a release that
    ///   lands names the lease that is actually current. The model issues
    ///   stale requests to check it (`Lease_EpochlessRelease.cfg`); before
    ///   the epoch existed, neither the model nor a test could see the
    ///   difference between a replay and a fresh release.
    ///
    /// A hand-back that lands on nothing reports the item's `state`, read
    /// the way [`Engine::extend_lock`] reads it and for the same reasons —
    /// a second plain statement rather than a sub-select sharing the
    /// update's snapshot (see there; the reasoning is the load-bearing part,
    /// not the shape). An unknown id is [`EngineError::UnknownWorkItem`]
    /// rather than a loss.
    pub async fn release_task(
        &self,
        task: Uuid,
        owner: &str,
        lease: i64,
    ) -> Result<Released, EngineError> {
        crate::runtime::reject_nul_text(owner, "owner")?;
        let released = sqlx::query(
            "update rbpmn_work_item set state = 'available', lock_owner = null, \
             lock_until = null where id = $1 and lock_owner = $2 and lease_no = $3 \
             and state = 'locked'",
        )
        .bind(task)
        .bind(owner)
        .bind(lease)
        .execute(self.pool())
        .await?
        .rows_affected()
            > 0;
        if released {
            return Ok(Released::Released);
        }
        Ok(Released::Lost {
            state: current_item_state(self, task).await?,
        })
    }

    /// Owner-checked completion: refuses while another holder's lease is
    /// live (`ItemLeased`), otherwise identical to
    /// [`Engine::complete_work_item`] — including the idempotent
    /// `AlreadyClosed` no-op, so a retried completion converges. The lease
    /// guard runs *under the instance lock* inside `complete_work_item_in_tx`
    /// — the one lock order engine-wide (instance row, then item row).
    pub async fn complete_task(
        &self,
        task: Uuid,
        owner: &str,
        patch: serde_json::Value,
    ) -> Result<Completion, EngineError> {
        let mut tx = self.pool().begin().await?;
        let completion = self
            .complete_work_item_in_tx(&mut tx, task, Some(owner), patch)
            .await?;
        tx.commit().await?;
        Ok(completion)
    }

    /// Owner-checked failure — [`Engine::fail_work_item`] with the caller's
    /// lease identity filled in.
    pub async fn fail_task(
        &self,
        task: Uuid,
        owner: &str,
        error_code: Option<String>,
        detail: Option<String>,
    ) -> Result<FailOutcome, EngineError> {
        self.fail_work_item(
            task,
            &FailOptions {
                error_code,
                detail,
                owner: Some(owner.to_string()),
            },
        )
        .await
    }
}

/// Deterministic, collision-proof index name from (definition key, field):
/// idempotent, re-runnable at startup, and always carrying a short hash of
/// the exact pair — sanitization alone is ambiguous (("a.b","c") and
/// ("a","b_c") both flatten to a_b_c, and IF NOT EXISTS would silently keep
/// whichever index got there first while the validity probe vouched for
/// it). Public so operators and tests can locate the index.
pub fn declared_index_name(definition_key: &str, field: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(definition_key.as_bytes());
    hasher.update([0]);
    hasher.update(field.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    // Both halves are sanitized to ASCII before truncation: this function
    // is public, so it must not panic on a multi-byte char landing across
    // the cut (nor emit a non-identifier).
    let ascii = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    };
    // Postgres identifiers cap at 63 bytes; the hash always survives.
    let mut readable = format!("rbpmn_vix_{}_{}", ascii(definition_key), ascii(field));
    readable.truncate(63 - 9);
    format!("{readable}_{}", &digest[..8])
}

/// Deterministic index name for a **shared** declaration: derived from the
/// field alone, so every definition declaring it converges on one index and
/// `IF NOT EXISTS` makes that convergence idempotent by construction — no
/// reference counting and no registry. Public for the same reason
/// [`declared_index_name`] is: it is how an operator locates the index, and
/// (since nothing ever drops one automatically) the only way to remove it.
pub fn shared_index_name(field: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    // Domain tag. 0xFF is outside `validate_definition_key`'s alphabet, so no
    // (key, field) pair can hash to a shared digest — the two namespaces
    // cannot collide even before the differing readable prefix.
    hasher.update([0xff]);
    hasher.update(b"shared");
    hasher.update([0]);
    hasher.update(field.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let ascii = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    };
    // `rbpmn_vixs_` rather than `rbpmn_vix_`: the definition-scoped form
    // always has `_` immediately after `vix`, so the two stay readable apart.
    let mut readable = format!("rbpmn_vixs_{}", ascii(field));
    readable.truncate(63 - 9);
    format!("{readable}_{}", &digest[..8])
}

/// Advisory-lock class for declared-index builds. The **two-int32** form is a
/// different lock space from the single-bigint form deploy uses
/// (`pg_advisory_xact_lock(hashtext(key))`), so a definition key can never
/// hash into a spurious wait on an index build.
const ADVISORY_CLASS_INDEX_BUILD: i32 = 0x7262_7831; // 'rbx1'

/// One slot for every declared index on `rbpmn_instance`. Not per index name:
/// concurrent `CREATE INDEX CONCURRENTLY` on one *table* deadlock whether or
/// not they name the same index, so the slot has to be the table.
const ADVISORY_KEY_INSTANCE_INDEXES: i32 = 1;

/// How long to keep trying for the build slot. Generous: a concurrent build on
/// a large table is minutes of honest work, and giving up early would turn a
/// slow deploy into a failed one.
const INDEX_BUILD_SLOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

impl Engine {
    /// Declare that the application filters/counts tasks of `definition_key`
    /// by this variables field: creates the partial expression index the
    /// filter compiler's shape matches. **Entirely optional performance
    /// API** — filtering works without it (sequential scan is correct, just
    /// slower). Idempotent; safe to re-run at startup next to handler
    /// registration. Predicated on `definition_key`, which is stable across
    /// definition versions.
    ///
    /// Built `CONCURRENTLY` (no lock on running work), which cannot run
    /// inside a transaction and, on failure, leaves an *invalid* index
    /// behind that `IF NOT EXISTS` would silently accept forever — so
    /// validity is verified after the build and an invalid leftover is
    /// dropped and reported loudly instead.
    ///
    /// The cross-definition counterpart is [`Engine::declare_shared_index`].
    pub async fn declare_index(
        &self,
        definition_key: &str,
        field: &str,
    ) -> Result<(), EngineError> {
        validate_definition_key(definition_key)?;
        validate_field(field)?;
        self.build_declared_index(
            &declared_index_name(definition_key, field),
            field,
            &format!("definition_key = '{definition_key}'"),
        )
        .await
    }

    /// Declare a variables field indexed **across every definition** that
    /// declares it — the lookup that resolves a business identifier to
    /// whichever instance carries it, without knowing which workflow or which
    /// deployment that is.
    ///
    /// Partial on `(variables->>'field') is not null`, which costs nothing:
    /// an equality against the expression is a strict operator clause and so
    /// implies `IS NOT NULL`, so the planner still uses the index — while
    /// instances of definitions that never carry the field stay out of it
    /// entirely. (Measured: 60 154 entries against 100 000, same plan.)
    ///
    /// **This asserts a contract rbpmn cannot check** — that the field means
    /// the same thing in every definition declaring it. See
    /// [`rbpmn_core::IndexScope::Shared`].
    pub async fn declare_shared_index(&self, field: &str) -> Result<(), EngineError> {
        validate_field(field)?;
        self.build_declared_index(
            &shared_index_name(field),
            field,
            &format!("(variables->>'{field}') is not null"),
        )
        .await
    }

    /// The one build path both scopes take: `CREATE INDEX CONCURRENTLY`, the
    /// validity probe, and the loud recovery of an interrupted build's corpse.
    ///
    /// **Serialized across sessions by a try-lock, and the try is the whole
    /// point.** Two `CREATE INDEX CONCURRENTLY` on one table deadlock: each
    /// waits for the other's snapshot to drain while the other waits for the
    /// table's ShareUpdateExclusive lock. That is not a shared-scope problem —
    /// rbpmn already had it, because two definitions deploying at once both
    /// index `rbpmn_instance` — the shared scope only makes it routine.
    ///
    /// A *blocking* advisory lock does not fix it; it was tried, and it just
    /// moves the cycle onto the advisory lock, because the waiter holds a
    /// snapshot while it waits and the holder's build waits for that snapshot.
    /// Postgres reported both shapes as real deadlocks. A **try**-lock cannot
    /// be an edge in a wait-for cycle at all — the same argument that keeps
    /// the scheduler's `pg_try_advisory_xact_lock` out of `LockOrder` — because
    /// between attempts this session is idle and holding nothing, so the
    /// holder's build can drain and finish.
    ///
    /// The key is the **table**, not the index: two different indexes on
    /// `rbpmn_instance` deadlock exactly as two builds of one index do.
    ///
    /// Recorded in `spec/README.md`'s lock inventory as "declared index
    /// build", argued rather than modelled for the same reason the
    /// scheduler's try-advisory is.
    async fn build_declared_index(
        &self,
        name: &str,
        field: &str,
        predicate: &str,
    ) -> Result<(), EngineError> {
        let mut conn = self.pool().acquire().await?;
        let deadline = std::time::Instant::now() + INDEX_BUILD_SLOT_TIMEOUT;
        let mut backoff = Duration::from_millis(20);
        loop {
            let got: bool = sqlx::query_scalar("select pg_try_advisory_lock($1, $2)")
                .bind(ADVISORY_CLASS_INDEX_BUILD)
                .bind(ADVISORY_KEY_INSTANCE_INDEXES)
                .fetch_one(&mut *conn)
                .await?;
            if got {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(EngineError::InvalidVariables(format!(
                    "timed out waiting to build index '{name}': another index \
                     build on rbpmn_instance has held the slot for longer than \
                     {INDEX_BUILD_SLOT_TIMEOUT:?}"
                )));
            }
            // Idle between attempts, deliberately: a session holding a
            // snapshot here is what stalls the holder's build.
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_millis(500));
        }
        let built = build_index(&mut conn, name, field, predicate).await;
        // Release on every path — a session lock left on a pooled connection
        // would outlive the call and stall every later build. The build's own
        // error wins if both fail; a connection that dies releases it anyway.
        let released = sqlx::query("select pg_advisory_unlock($1, $2)")
            .bind(ADVISORY_CLASS_INDEX_BUILD)
            .bind(ADVISORY_KEY_INSTANCE_INDEXES)
            .execute(&mut *conn)
            .await;
        built.and(released.map(|_| ()).map_err(EngineError::from))
    }
}

async fn build_index(
    conn: &mut sqlx::PgConnection,
    name: &str,
    field: &str,
    predicate: &str,
) -> Result<(), EngineError> {
    sqlx::query(&format!(
        "create index concurrently if not exists {name} on rbpmn_instance \
         ((variables->>'{field}')) where {predicate}"
    ))
    .execute(&mut *conn)
    .await?;
    let valid: Option<bool> = sqlx::query_scalar(
        "select i.indisvalid from pg_class c \
         join pg_index i on i.indexrelid = c.oid where c.relname = $1",
    )
    .bind(name)
    .fetch_optional(&mut *conn)
    .await?;
    match valid {
        Some(true) => Ok(()),
        Some(false) => {
            // A previously interrupted concurrent build: drop the
            // corpse so the next call can rebuild, and say so.
            sqlx::query(&format!("drop index concurrently if exists {name}"))
                .execute(&mut *conn)
                .await?;
            Err(EngineError::InvalidVariables(format!(
                "index '{name}' was left invalid by an interrupted build; \
                 it has been dropped — declare the index again"
            )))
        }
        None => Err(EngineError::InvalidVariables(format!(
            "index '{name}' disappeared during creation"
        ))),
    }
}

/// One declared index, as the catalogue and the live manifests jointly
/// describe it. See [`Engine::declared_indexes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredIndex {
    /// The catalogue name — [`declared_index_name`] or [`shared_index_name`].
    pub name: String,
    /// The field it indexes, when a relevant definition still declares it.
    /// `None` for an orphan: the name carries a one-way hash and nothing in
    /// rbpmn records the declaration, so a manifest entry that goes away takes
    /// the field name with it.
    pub field: Option<String>,
    pub scope: Option<IndexScope>,
    /// `key vN` for every relevant definition whose manifest declares it.
    /// Empty means orphaned — nothing deployed asks for this index any more.
    pub declared_by: Vec<String>,
    /// Present in the catalogue. `false` alongside a non-empty `declared_by`
    /// is a declaration that never got built — a deploy that failed after its
    /// commit, fixed by redeploying.
    pub present: bool,
    /// Present *and* usable. `false` is an interrupted `CREATE INDEX
    /// CONCURRENTLY`'s corpse; declaring it again drops and rebuilds it.
    pub valid: bool,
}

/// Every `rbpmn_vix*` index on `rbpmn_instance`, matched against what the
/// relevant definitions' manifests actually declare.
///
/// Read-only, and the answer to a question the engine otherwise cannot be
/// asked: **rbpmn never drops a declared index**, of either scope. Not on
/// [`Engine::delete_definition`], not under retention, and not when a field
/// disappears from a manifest — `apply_manifest_indexes` only ever creates.
/// That is deliberate for the shared scope, where a definition going away
/// says nothing about whether another still needs the index, and where
/// reference counting would be racy against a concurrent deploy. It also
/// means orphans accumulate silently, so this is how an operator sees them:
/// anything with an empty `declared_by` is safe to drop by hand.
impl Engine {
    pub async fn declared_indexes(&self) -> Result<Vec<DeclaredIndex>, EngineError> {
        use std::collections::BTreeMap;

        let mut expected: BTreeMap<String, DeclaredIndex> = BTreeMap::new();
        for row in sqlx::query(crate::deploy::RELEVANT_DEFINITIONS)
            .fetch_all(self.pool())
            .await?
        {
            let key: String = row.get("key");
            let version: i32 = row.get("version");
            let Ok(bindings) = serde_json::from_value::<rbpmn_core::Bindings>(row.get("bindings"))
            else {
                continue;
            };
            for decl in &bindings.indexes {
                let name = match decl.scope {
                    IndexScope::Definition => declared_index_name(&key, &decl.field),
                    IndexScope::Shared => shared_index_name(&decl.field),
                };
                expected
                    .entry(name.clone())
                    .or_insert_with(|| DeclaredIndex {
                        name,
                        field: Some(decl.field.clone()),
                        scope: Some(decl.scope),
                        declared_by: Vec::new(),
                        present: false,
                        valid: false,
                    })
                    .declared_by
                    .push(format!("{key} v{version}"));
            }
        }

        for row in sqlx::query(
            "select c.relname, i.indisvalid from pg_class c \
             join pg_index i on i.indexrelid = c.oid \
             join pg_class t on t.oid = i.indrelid \
             where t.relname = 'rbpmn_instance' \
               and (c.relname like 'rbpmn\\_vix\\_%' or c.relname like 'rbpmn\\_vixs\\_%')",
        )
        .fetch_all(self.pool())
        .await?
        {
            let name: String = row.get("relname");
            let valid: bool = row.get("indisvalid");
            let entry = expected
                .entry(name.clone())
                .or_insert_with(|| DeclaredIndex {
                    name,
                    field: None,
                    scope: None,
                    declared_by: Vec::new(),
                    present: false,
                    valid: false,
                });
            entry.present = true;
            entry.valid = valid;
        }

        Ok(expected.into_values().collect())
    }
}
