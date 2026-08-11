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

use crate::runtime::{load_instance, persist_step};
use crate::{Engine, EngineError};
use rbpmn_core::{Command, InstanceStatus, TimerId, step};
use sqlx::Row;
use sqlx::postgres::PgListener;
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

impl Engine {
    /// Runs forever (spawn it; abort to stop). Transient errors back off and
    /// continue — the loop must survive database restarts.
    pub async fn run_scheduler(&self, options: SchedulerOptions) {
        let mut listener = None;
        loop {
            match self.fire_due_timer().await {
                Ok(true) => continue,
                Ok(false) => {
                    if listener.is_none() {
                        listener = PgListener::connect_with(self.pool()).await.ok();
                        if let Some(l) = listener.as_mut()
                            && l.listen("rbpmn_timer").await.is_err()
                        {
                            listener = None;
                        }
                    }
                    let mut wait = match self.next_due_in().await {
                        Ok(Some(until_due)) => until_due.min(options.poll_interval),
                        Ok(None) => options.poll_interval,
                        Err(_) => Duration::from_secs(1),
                    };
                    // A backed-off instance's timer is invisible above; wake
                    // when its backoff expires (floored so an imminent
                    // expiry cannot degenerate into a hot loop).
                    if let (_, Some(until_retry)) = self.timer_backoff_snapshot() {
                        wait = wait.min(until_retry.max(Duration::from_millis(250)));
                    }
                    match listener.as_mut() {
                        Some(l) => {
                            if tokio::time::timeout(wait, l.recv())
                                .await
                                .is_ok_and(|r| r.is_err())
                            {
                                listener = None; // connection lost; rebuild next round
                            }
                        }
                        None => tokio::time::sleep(wait).await,
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "scheduler iteration failed; backing off");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    /// Fire at most one due timer. Returns whether an attempt was made (the
    /// caller drains until false); a candidate lost to a concurrent step
    /// still counts as an attempt, so the drain re-scans.
    ///
    /// Liveness by construction: a small *batch* of candidates is scanned,
    /// each behind a `pg_try_advisory_xact_lock` (try = never waits = never
    /// deadlocks) so replicas spread across instances instead of piling on
    /// the earliest timer — and an instance whose load or step keeps
    /// erroring goes on a short in-process backoff, so one poisoned
    /// instance can never head-of-line-block every timer engine-wide.
    pub async fn fire_due_timer(&self) -> Result<bool, EngineError> {
        // Backed-off instances are excluded in the query itself: they must
        // neither fill the candidate batch (head-of-line starvation) nor
        // drive the sleep computation (busy-spin).
        let (backed_off, _) = self.timer_backoff_snapshot();
        let candidates = sqlx::query(
            "select t.instance_id, t.timer_no from rbpmn_timer t \
             join rbpmn_instance i on i.id = t.instance_id \
             where t.due_at <= now() and i.status = 'active' \
               and not (i.id = any($1)) \
             order by t.due_at limit 8",
        )
        .bind(&backed_off)
        .fetch_all(self.pool())
        .await?;

        for candidate in candidates {
            let instance_id: Uuid = candidate.get("instance_id");
            let timer_no: i64 = candidate.get("timer_no");
            match self.try_fire(instance_id, timer_no).await {
                Ok(true) => return Ok(true),
                Ok(false) => continue, // lock busy or timer resolved meanwhile
                Err(e) => {
                    // Log and back this instance off instead of returning:
                    // returning Err would retry the same head-of-line
                    // candidate forever while every other timer starves.
                    tracing::warn!(
                        instance = %instance_id, error = %e,
                        "firing timer failed; backing the instance off"
                    );
                    self.set_timer_error_backoff(instance_id);
                    continue;
                }
            }
        }
        Ok(false)
    }

    /// One firing attempt inside one transaction. `Ok(false)` means someone
    /// else holds the instance's advisory lock or the timer resolved
    /// between candidate pick and instance lock — both mean "move on".
    async fn try_fire(&self, instance_id: Uuid, timer_no: i64) -> Result<bool, EngineError> {
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
            return Ok(false);
        }
        let (definition, proc, mut state) = load_instance(self, &mut tx, instance_id).await?;
        if state.status != InstanceStatus::Active {
            return Ok(false); // resolved between candidate pick and lock
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
            return Ok(false);
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
        Ok(true)
    }

    /// Time until the earliest live timer is due, from database time (cheap
    /// on the `due_at` index) — `None` when nothing is armed. Frozen
    /// instances' timers don't count: they cannot fire until repaired, and
    /// counting them would make every scheduler spin on an overdue timer it
    /// is not allowed to touch. (`min()` over zero rows is NULL, and
    /// Postgres's `GREATEST` *ignores* NULLs rather than propagating them —
    /// the clamp must happen here, not in SQL.)
    pub async fn next_due_in(&self) -> Result<Option<Duration>, EngineError> {
        let (backed_off, _) = self.timer_backoff_snapshot();
        let secs: Option<f64> = sqlx::query_scalar(
            "select extract(epoch from (min(t.due_at) - now()))::float8 \
             from rbpmn_timer t join rbpmn_instance i on i.id = t.instance_id \
             where i.status = 'active' and not (i.id = any($1))",
        )
        .bind(&backed_off)
        .fetch_one(self.pool())
        .await?;
        Ok(secs.map(|s| Duration::from_secs_f64(s.max(0.0))))
    }
}
