//! The timer scheduler: every node runs the same loop (competing
//! consumers), draining due timers one per transaction, then sleeping until
//! `min(due_at)` — capped by a fallback poll interval — with `LISTEN
//! rbpmn_timer` waking sleepers early when a sooner timer is armed. A
//! sleeping timer is a passive row; there is never a per-timer in-process
//! wait, and nothing fires before `due_at` (database time, the only clock).
//!
//! Firing locks the instance row **first** — the same order as every other
//! step path (completions, failures, correlation), so scheduler and
//! completion transactions can never deadlock. The due-timer candidate is
//! picked without a lock and re-checked under the instance lock; the timer
//! row's delete commits together with the step, which is what makes firing
//! exactly-once: whoever loses the re-check simply moves on.
//!
//! **Liveness invariant (learned the hard way, three times).** A drain pass
//! can decline to fire for several unrelated reasons: nothing is due, an
//! instance is deferred, a peer replica holds the advisory lock, a caller
//! holds the instance row, the timer vanished under us. Earlier versions
//! collapsed all of them into `false` and then let the *sleep* re-derive
//! "is there work?" from a second query — whose eligibility rules drifted
//! from the drain's, and every drift was a busy-spin bug. Now:
//!
//! 1. a pass returns [`Drain`], so the sleep follows from what the pass
//!    actually learned instead of a second opinion; and
//! 2. every parking path is floored by [`MIN_WAIT`], so even a *future*
//!    unhandled skip reason degrades to a slow poll, never a hot loop.
//!
//! Anything deferred is excluded from the candidate query and the due-time
//! query alike (one shared map), so the two cannot disagree by construction.

use crate::listen::Wakeup;
use crate::runtime::{DefinitionRef, insert_engine_event, load_instance_nowait, persist_step};
use crate::{Engine, EngineError, MAX_RECORDED_TIMER_FAILURES};
use rbpmn_core::{Command, InstanceStatus, TimerId, step};
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SchedulerOptions {
    /// Fallback wake-up when no NOTIFY arrives and no timer is due sooner.
    pub poll_interval: Duration,
}

impl Default for SchedulerOptions {
    fn default() -> Self {
        SchedulerOptions {
            poll_interval: Duration::from_secs(30),
        }
    }
}

/// The floor under every sleep. Firing is unaffected (a pass that fires
/// loops immediately), so this only bounds how fast the loop may *retry*
/// when it found nothing to do — the structural guarantee that no skip
/// reason, present or future, can turn into a hot loop.
const MIN_WAIT: Duration = Duration::from_millis(50);

/// What one drain pass learned. The loop sleeps on this and nothing else.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Drain {
    /// Fired a timer; look for more immediately.
    Fired,
    /// Due work exists but was not actionable right now (deferred instance,
    /// peer replica holding it, caller holding the row). Retry when the
    /// earliest deferral expires — emphatically *without* asking the
    /// database when the next timer is due: it would answer "now" and spin.
    Deferred,
    /// Nothing actionable is due; sleep until the next timer is.
    Idle,
}

/// One firing attempt's outcome.
enum Attempt {
    Fired,
    /// Someone else holds this instance (advisory lock or row lock) — defer
    /// it briefly so the next pass sees *other* candidates instead of
    /// refilling the batch with this one.
    Busy,
    /// The timer or the instance resolved under us; nothing to defer.
    Resolved,
}

impl Engine {
    /// Runs forever (spawn it; abort to stop). Transient errors back off and
    /// continue — the loop must survive database restarts.
    pub async fn run_scheduler(&self, options: SchedulerOptions) {
        let mut wakeup = Wakeup::new("rbpmn_timer");
        loop {
            let wait = match self.drain_due_timers().await {
                Ok(Drain::Fired) => continue,
                Ok(drain) => {
                    if wakeup.ensure(self.pool()).await {
                        continue; // freshly listening: re-check the gap
                    }
                    let (_, until_eligible) = self.deferral_snapshot();
                    match drain {
                        // Deferred: the only thing worth waiting for is a
                        // deferral expiring (or a NOTIFY). Never ask
                        // next_due_in here — the due timer we just skipped
                        // is still due, and the answer would be zero.
                        Drain::Deferred => until_eligible.unwrap_or(MIN_WAIT),
                        Drain::Idle => {
                            let until_due = match self.next_due_in().await {
                                Ok(Some(d)) => d,
                                Ok(None) => options.poll_interval,
                                Err(e) => {
                                    tracing::warn!(error = %e, "next-due query failed");
                                    Duration::from_secs(1)
                                }
                            };
                            // A deferred instance's timer is invisible to
                            // next_due_in, so its expiry also bounds the nap.
                            until_due
                                .min(until_eligible.unwrap_or(options.poll_interval))
                                .min(options.poll_interval)
                        }
                        Drain::Fired => unreachable!("handled above"),
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "scheduler iteration failed; backing off");
                    Duration::from_secs(1)
                }
            };
            // The floor applies to EVERY parking path — see the module doc.
            wakeup.park(wait.max(MIN_WAIT)).await;
        }
    }

    /// Fire at most one due timer, reporting what the pass learned so the
    /// caller can sleep correctly (see the module doc's liveness
    /// invariant).
    ///
    /// A small *batch* of candidates is scanned, each behind a
    /// `pg_try_advisory_xact_lock` (try = never waits = never deadlocks) so
    /// replicas spread across instances instead of piling on the earliest
    /// timer. Anything skipped or failed is deferred, which both keeps the
    /// next pass's batch fresh (no head-of-line starvation) and keeps the
    /// sleep computation consistent with the claim rules.
    pub(crate) async fn drain_due_timers(&self) -> Result<Drain, EngineError> {
        let (deferred, _) = self.deferral_snapshot();
        let candidates = sqlx::query(
            "select t.instance_id, t.timer_no from rbpmn_timer t \
             join rbpmn_instance i on i.id = t.instance_id \
             where t.due_at <= now() and i.status = 'active' \
               and not (i.id = any($1)) \
             order by t.due_at limit 8",
        )
        .bind(&deferred)
        .fetch_all(self.pool())
        .await?;

        // Anything deferred *is* due work we are holding off on, so an
        // empty batch with a non-empty deferral set is Deferred, not Idle.
        let mut deferred_any = !deferred.is_empty();
        for candidate in candidates {
            let instance_id: Uuid = candidate.get("instance_id");
            let timer_no: i64 = candidate.get("timer_no");
            match self.try_fire(instance_id, timer_no).await {
                Ok(Attempt::Fired) => {
                    self.clear_deferral(instance_id);
                    return Ok(Drain::Fired);
                }
                Ok(Attempt::Busy) => {
                    self.defer_transient(instance_id);
                    deferred_any = true;
                }
                Ok(Attempt::Resolved) => continue,
                Err(e) => {
                    // Defer instead of returning Err: returning would retry
                    // the same head-of-line candidate forever while every
                    // other timer starves.
                    let failures = self.defer_failed(instance_id);
                    tracing::warn!(
                        instance = %instance_id, failures, error = %e,
                        "firing timer failed; deferring the instance"
                    );
                    // Never a silent in-memory loop: the first few failures
                    // are recorded in the instance's event history, where
                    // inspection can see them. Beyond that the backoff keeps
                    // growing but the event stream is not flooded by one
                    // stuck instance.
                    if failures <= MAX_RECORDED_TIMER_FAILURES
                        && let Err(record_err) =
                            self.record_timer_failure(instance_id, failures, &e).await
                    {
                        tracing::warn!(
                            instance = %instance_id, error = %record_err,
                            "could not record the timer failure event"
                        );
                    }
                    deferred_any = true;
                }
            }
        }
        Ok(if deferred_any {
            Drain::Deferred
        } else {
            Drain::Idle
        })
    }

    /// Fire at most one due timer; `true` when one fired. Thin wrapper over
    /// [`Engine::drain_due_timers`] for callers that only need the boolean
    /// (tests, one-shot drains).
    pub async fn fire_due_timer(&self) -> Result<bool, EngineError> {
        Ok(self.drain_due_timers().await? == Drain::Fired)
    }

    /// One firing attempt inside one transaction.
    async fn try_fire(&self, instance_id: Uuid, timer_no: i64) -> Result<Attempt, EngineError> {
        let mut tx = self.pool().begin().await?;
        // Advisory-first is deadlock-free: try-lock never waits, and every
        // other path takes only the instance row lock (a one-lock cycle
        // cannot exist).
        let claimed: bool =
            sqlx::query_scalar("select pg_try_advisory_xact_lock(hashtextextended($1, 8601))")
                .bind(instance_id.to_string())
                .fetch_one(&mut *tx)
                .await?;
        if !claimed {
            return Ok(Attempt::Busy); // a peer replica has this instance
        }
        // NOWAIT: a caller transaction (an *_in_tx embedder) may hold this
        // instance's row lock for a while — the sequential drain loop must
        // move on to other instances' timers, not park behind it.
        let Some((definition, proc, mut state)) =
            load_instance_nowait(self, &mut tx, instance_id, true).await?
        else {
            return Ok(Attempt::Busy); // a caller holds the instance row
        };
        if state.status != InstanceStatus::Active {
            return Ok(Attempt::Resolved); // resolved between pick and lock
        }
        // Re-check under the instance lock: a concurrent step may have
        // fired or cancelled it; due_at cannot move (timers never reschedule).
        let still_armed = sqlx::query(
            "select 1 from rbpmn_timer where instance_id = $1 and timer_no = $2 \
             and due_at <= now()",
        )
        .bind(instance_id)
        .bind(timer_no)
        .fetch_optional(&mut *tx)
        .await?;
        if still_armed.is_none() {
            return Ok(Attempt::Resolved); // fired or cancelled under us
        }

        let events = step(
            &proc,
            &mut state,
            Command::FireTimer {
                id: TimerId(timer_no as u64),
            },
        )?;
        persist_step(&mut tx, &proc, &definition, instance_id, &state, &events).await?;
        tx.commit().await?;
        Ok(Attempt::Fired)
    }

    /// A `timer-fire-failed` event row for a firing that keeps erroring —
    /// the persisted, inspectable trace of what would otherwise be an
    /// invisible in-process backoff loop.
    async fn record_timer_failure(
        &self,
        instance_id: Uuid,
        failures: u32,
        error: &EngineError,
    ) -> Result<(), EngineError> {
        let Some(row) =
            sqlx::query("select definition_id, definition_key from rbpmn_instance where id = $1")
                .bind(instance_id)
                .fetch_optional(self.pool())
                .await?
        else {
            return Ok(()); // instance gone; nothing to attach the event to
        };
        let definition = DefinitionRef::new(row.get("definition_id"), row.get("definition_key"));
        let mut conn = self.pool().acquire().await?;
        insert_engine_event(
            &mut conn,
            &definition,
            instance_id,
            "timer-fire-failed",
            None, // instance-level, not attributable to one element
            serde_json::json!({
                "kind": "timer-fire-failed",
                "error": error.to_string(),
                "failures": failures,
                "suppressedAfter": MAX_RECORDED_TIMER_FAILURES,
            }),
        )
        .await
    }

    /// Time until the earliest live timer is due, from database time (cheap
    /// on the `due_at` index) — `None` when nothing is armed. Frozen
    /// instances' timers don't count: they cannot fire until repaired, and
    /// counting them would make every scheduler spin on an overdue timer it
    /// is not allowed to touch. (`min()` over zero rows is NULL, and
    /// Postgres's `GREATEST` *ignores* NULLs rather than propagating them —
    /// the clamp must happen here, not in SQL.)
    pub async fn next_due_in(&self) -> Result<Option<Duration>, EngineError> {
        let (deferred, _) = self.deferral_snapshot();
        let secs: Option<f64> = sqlx::query_scalar(
            "select extract(epoch from (min(t.due_at) - now()))::float8 \
             from rbpmn_timer t join rbpmn_instance i on i.id = t.instance_id \
             where i.status = 'active' and not (i.id = any($1))",
        )
        .bind(&deferred)
        .fetch_one(self.pool())
        .await?;
        Ok(secs.map(|s| Duration::from_secs_f64(s.max(0.0))))
    }
}
