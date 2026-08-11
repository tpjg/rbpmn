//! The one LISTEN/reconnect scaffold, shared by the worker ("rbpmn_work")
//! and the scheduler ("rbpmn_timer"): establish lazily, rebuild on
//! connection loss, and surface the just-(re)established case so the caller
//! can re-check for work — a NOTIFY sent before LISTEN is lost forever.

use sqlx::PgPool;
use sqlx::postgres::PgListener;
use std::time::Duration;

pub(crate) struct Wakeup {
    channel: &'static str,
    listener: Option<PgListener>,
}

impl Wakeup {
    pub(crate) fn new(channel: &'static str) -> Self {
        Wakeup {
            channel,
            listener: None,
        }
    }

    /// Establish the LISTEN if it is not up. Returns true when it was just
    /// (re)established — the caller MUST re-check for work before parking,
    /// because anything notified in the un-listened gap was missed.
    pub(crate) async fn ensure(&mut self, pool: &PgPool) -> bool {
        if self.listener.is_some() {
            return false;
        }
        self.listener = PgListener::connect_with(pool).await.ok();
        if let Some(l) = self.listener.as_mut()
            && l.listen(self.channel).await.is_ok()
        {
            return true;
        }
        self.listener = None;
        false
    }

    /// Park up to `wait` for a NOTIFY (or just sleep when no listener could
    /// be established); a lost connection drops the listener so the next
    /// `ensure` rebuilds it.
    pub(crate) async fn park(&mut self, wait: Duration) {
        match self.listener.as_mut() {
            Some(l) => {
                if tokio::time::timeout(wait, l.recv())
                    .await
                    .is_ok_and(|r| r.is_err())
                {
                    self.listener = None;
                }
            }
            None => tokio::time::sleep(wait).await,
        }
    }
}
