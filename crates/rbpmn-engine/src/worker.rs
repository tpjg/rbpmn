//! The push-mode worker loop: claims service work items for topics with
//! registered handlers (`FOR UPDATE SKIP LOCKED` — competing consumers
//! across any number of processes), invokes the handler outside any
//! transaction, and completes/fails the item through the same transactional
//! step path as every other caller.
//!
//! Leases, not long locks: a claim sets `lock_until = now() + lease`; the
//! availability predicate treats an expired lock as available again, so a
//! crashed worker's items return without a reaper. `LISTEN rbpmn_work` wakes
//! the loop early; the poll interval is the safety net, not the mechanism.

use crate::{Engine, EngineError, WorkItem};
use sqlx::Row;
use sqlx::postgres::PgListener;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// Lease holder identity, recorded in `lock_owner`.
    pub owner: String,
    /// Base lease TTL; short by design (holders renew while working).
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
                        listener = PgListener::connect_with(self.pool())
                            .await
                            .ok()
                            .filter(|_| true);
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
                Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
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
        let Some(row) = sqlx::query(
            "update work_item set state = 'locked', lock_owner = $2, \
             lock_until = now() + make_interval(secs => $3) \
             where id = (select id from work_item \
                where kind = 'service' and topic = any($1) \
                  and (state = 'available' or (state = 'locked' and lock_until < now())) \
                order by created_at, item_no limit 1 for update skip locked) \
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
            variables: sqlx::query("select variables from instance where id = $1")
                .bind(row.get::<uuid::Uuid, _>("instance_id"))
                .fetch_one(self.pool())
                .await?
                .get("variables"),
        };

        let Some(handler) = self.handler_for(&item.topic) else {
            // The environment changed underneath us: release the claim.
            sqlx::query(
                "update work_item set state = 'available', lock_owner = null, \
                 lock_until = null where id = $1 and lock_owner = $2",
            )
            .bind(item.id)
            .bind(&options.owner)
            .execute(self.pool())
            .await?;
            return Ok(false);
        };

        // Handler runs outside any transaction; delivery is at-least-once.
        let work_item_id = item.id;
        match handler.execute(item).await {
            Ok(patch) => {
                // AlreadyClosed is fine: someone else (or a previous
                // delivery) won — exactly-once state transition holds.
                let _ = self.complete_work_item(work_item_id, patch).await?;
            }
            Err(failure) => {
                let _ = self
                    .fail_work_item(work_item_id, failure.code.as_deref())
                    .await?;
            }
        }
        Ok(true)
    }
}
