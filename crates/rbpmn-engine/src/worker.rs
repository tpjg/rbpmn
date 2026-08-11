//! The push-mode worker loop: claims service work items for topics with
//! registered handlers (`FOR UPDATE SKIP LOCKED` — competing consumers
//! across any number of processes), invokes the handler outside any
//! transaction, and completes/fails the item through the same transactional
//! step path as every other caller.
//!
//! Leases, not long locks: a claim sets `lock_until = now() + lease`, and
//! the worker **renews its own lease** (every lease/3) while the handler is
//! in flight, so a long-running handler is never claimed concurrently while
//! a crashed worker's items still return within one TTL — no reaper needed.
//! Claims only touch items of *active* instances (an incident freezes its
//! whole instance) whose retry backoff has elapsed. `LISTEN rbpmn_work`
//! wakes the loop early; the poll interval is the safety net.

use crate::runtime::FailOptions;
use crate::{Engine, EngineError, WorkItem};
use sqlx::Row;
use sqlx::postgres::PgListener;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// Lease holder identity, recorded in `lock_owner`.
    pub owner: String,
    /// Base lease TTL; renewed automatically while a handler runs.
    pub lease: Duration,
    /// Fallback wake-up when no NOTIFY arrives.
    pub poll_interval: Duration,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        WorkerOptions {
            owner: format!("worker-{}", uuid::Uuid::new_v4().simple()),
            lease: Duration::from_secs(600),
            poll_interval: Duration::from_secs(30),
        }
    }
}

impl Engine {
    /// Runs forever (spawn it; abort to stop). Transient errors back off and
    /// continue — the loop must survive database restarts.
    pub async fn run_worker(&self, options: WorkerOptions) {
        let mut listener = None;
        loop {
            match self.work_once(&options).await {
                Ok(true) => continue,
                Ok(false) => {
                    if listener.is_none() {
                        listener = PgListener::connect_with(self.pool()).await.ok();
                        if let Some(l) = listener.as_mut()
                            && l.listen("rbpmn_work").await.is_err()
                        {
                            listener = None;
                        }
                    }
                    match listener.as_mut() {
                        Some(l) => {
                            if tokio::time::timeout(options.poll_interval, l.recv())
                                .await
                                .is_ok_and(|r| r.is_err())
                            {
                                listener = None; // connection lost; rebuild next round
                            }
                        }
                        None => tokio::time::sleep(options.poll_interval).await,
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "worker iteration failed; backing off");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    /// Claim and execute at most one work item. Returns whether one was
    /// processed (the caller drains until false).
    pub async fn work_once(&self, options: &WorkerOptions) -> Result<bool, EngineError> {
        let topics = self.handled_topics();
        if topics.is_empty() {
            return Ok(false);
        }

        // Single-statement claim: atomic without an explicit transaction.
        // Filters: handled topics, availability (or expired lease), retry
        // backoff elapsed, and — crucially — the instance still active, so
        // an incident-frozen instance's siblings are never re-executed.
        let Some(row) = sqlx::query(
            "update rbpmn_work_item set state = 'locked', lock_owner = $2, \
             lock_until = now() + make_interval(secs => $3) \
             where id = (select w.id from rbpmn_work_item w \
                join rbpmn_instance i on i.id = w.instance_id \
                where w.kind = 'service' and w.topic = any($1) \
                  and (w.state = 'available' or (w.state = 'locked' and w.lock_until < now())) \
                  and (w.retry_at is null or w.retry_at <= now()) \
                  and i.status = 'active' \
                order by w.created_at, w.item_no limit 1 for update of w skip locked) \
             returning id, instance_id, definition_key, element_id, topic",
        )
        .bind(&topics)
        .bind(&options.owner)
        .bind(options.lease.as_secs_f64())
        .fetch_optional(self.pool())
        .await?
        else {
            return Ok(false);
        };

        let item = WorkItem {
            id: row.get("id"),
            instance_id: row.get("instance_id"),
            definition_key: row.get("definition_key"),
            element_id: row.get("element_id"),
            topic: row.get("topic"),
            variables: sqlx::query("select variables from rbpmn_instance where id = $1")
                .bind(row.get::<uuid::Uuid, _>("instance_id"))
                .fetch_one(self.pool())
                .await?
                .get("variables"),
        };

        let Some(handler) = self.handler_for(&item.topic) else {
            // The environment changed underneath us: release the claim.
            self.release_claim(item.id, &options.owner).await?;
            return Ok(false);
        };

        // Handler runs outside any transaction (at-least-once delivery),
        // with the lease renewed underneath it while it works.
        let work_item_id = item.id;
        let handler_future = handler.execute(item);
        tokio::pin!(handler_future);
        let renew_every = (options.lease / 3).max(Duration::from_millis(200));
        let result = loop {
            tokio::select! {
                result = &mut handler_future => break result,
                _ = tokio::time::sleep(renew_every) => {
                    match self.renew_lease(work_item_id, &options.owner, options.lease).await {
                        Ok(true) => {}
                        Ok(false) => {
                            tracing::warn!(item = %work_item_id, "lease lost during handler execution");
                            // Keep going: completion stays exactly-once — if a
                            // competing claim finished first we get AlreadyClosed.
                        }
                        Err(e) => {
                            // A transient DB blip is precisely what renewal
                            // exists to ride out — cancelling the in-flight
                            // handler over it would re-run its side effects
                            // later. If the DB is really gone, completion
                            // fails loudly right after.
                            tracing::warn!(item = %work_item_id, error = %e, "lease renewal failed; continuing");
                        }
                    }
                }
            }
        };

        match result {
            Ok(patch) => match self.complete_work_item(work_item_id, patch).await {
                Ok(_) => {} // Advanced or the idempotent AlreadyClosed
                Err(EngineError::InvalidVariables(message)) => {
                    // The handler already ran; its patch is unstorable.
                    // Treat as a handler failure so it retries into an
                    // incident instead of poisoning the item forever.
                    let _ = self
                        .fail_work_item(
                            work_item_id,
                            &FailOptions {
                                detail: Some(format!(
                                    "handler returned invalid variables: {message}"
                                )),
                                owner: Some(options.owner.clone()),
                                ..FailOptions::default()
                            },
                        )
                        .await?;
                }
                Err(e) => {
                    // Keep the claim: releasing would make the drain loop
                    // re-claim instantly and re-run the handler with zero
                    // backoff — re-firing its side effects on every lap. The
                    // held lease *is* the backoff: nothing re-runs before
                    // the lease TTL, which is the at-least-once contract.
                    tracing::warn!(item = %work_item_id, error = %e, "completion failed; keeping the lease");
                    return Err(e);
                }
            },
            Err(failure) => {
                let outcome = self
                    .fail_work_item(
                        work_item_id,
                        &FailOptions {
                            error_code: failure.code,
                            detail: Some(failure.message),
                            owner: Some(options.owner.clone()),
                        },
                    )
                    .await;
                if let Err(e) = outcome {
                    // Same reasoning as the completion arm.
                    tracing::warn!(item = %work_item_id, error = %e, "recording the failure failed; keeping the lease");
                    return Err(e);
                }
            }
        }
        Ok(true)
    }

    /// Heartbeat: extends the lease only while this owner still holds it.
    /// Returns false when the lease was lost (expired and reclaimed).
    async fn renew_lease(
        &self,
        work_item: uuid::Uuid,
        owner: &str,
        lease: Duration,
    ) -> Result<bool, EngineError> {
        let result = sqlx::query(
            "update rbpmn_work_item set lock_until = now() + make_interval(secs => $3) \
             where id = $1 and lock_owner = $2 and state = 'locked' and lock_until > now()",
        )
        .bind(work_item)
        .bind(owner)
        .bind(lease.as_secs_f64())
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn release_claim(&self, work_item: uuid::Uuid, owner: &str) -> Result<(), EngineError> {
        sqlx::query(
            "update rbpmn_work_item set state = 'available', lock_owner = null, \
             lock_until = null where id = $1 and lock_owner = $2 and state = 'locked'",
        )
        .bind(work_item)
        .bind(owner)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}
