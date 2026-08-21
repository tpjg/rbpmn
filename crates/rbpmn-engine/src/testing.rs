//! Test harness (feature `test-util`): throwaway databases against a local
//! Postgres, shared by the engine's own integration tests and the server's.

use sqlx::PgPool;

/// The claim path's own SQL, exposed so a test can differential the published
/// `rbpmn_v_work_item.claimable` column against the very string `get_task`
/// claims by, rather than against a re-typed copy of it. Aliases `w` (work
/// item) and `i` (instance), as everywhere else.
///
/// Behind `test-util` deliberately: applications must read claimability from
/// the view, not paste the predicate into their own queries — a pasted copy
/// is exactly the drift the view exists to prevent.
pub const CLAIMABLE_SQL: &str = crate::CLAIMABLE;

/// The live-lease half, for the same reason. See [`CLAIMABLE_SQL`].
pub const IN_PROGRESS_SQL: &str = crate::IN_PROGRESS;

pub struct TestDb {
    pub pool: PgPool,
    admin_url: String,
    name: String,
}

impl TestDb {
    /// Creates `rbpmn_test_<uuid>` on the admin server and connects to it.
    /// Admin URL default: `postgres://$USER@localhost:5432/postgres`,
    /// override with `RBPMN_TEST_ADMIN_URL`.
    pub async fn create() -> Self {
        let user = std::env::var("USER").unwrap_or_else(|_| "postgres".into());
        let admin_url = std::env::var("RBPMN_TEST_ADMIN_URL")
            .unwrap_or_else(|_| format!("postgres://{user}@localhost:5432/postgres"));
        let admin = PgPool::connect(&admin_url).await.expect(
            "integration tests need a local Postgres \
             (set RBPMN_TEST_ADMIN_URL to override the default)",
        );
        let name = format!("rbpmn_test_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("create database {name}"))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;

        let base = admin_url.rsplit_once('/').unwrap().0;
        let pool = PgPool::connect(&format!("{base}/{name}")).await.unwrap();
        TestDb {
            pool,
            admin_url,
            name,
        }
    }

    /// Connection URL of the throwaway database — for tests that need their
    /// own pools rather than a clone of this one (the storm runs several
    /// engines as genuinely separate nodes).
    pub fn url(&self) -> String {
        // Parsed, not split: string surgery on the last '/' drops any query
        // string, so an admin URL carrying `?sslmode=require` would hand the
        // storm's node pools different connection options than `self.pool`.
        use sqlx::ConnectOptions;
        use std::str::FromStr;
        sqlx::postgres::PgConnectOptions::from_str(&self.admin_url)
            .expect("admin URL parses")
            .database(&self.name)
            .to_url_lossy()
            .to_string()
    }

    /// Drops the throwaway database (call at the end of a passing test; a
    /// panicked test leaves its database behind for inspection).
    pub async fn drop(self) {
        self.pool.close().await;
        let admin = PgPool::connect(&self.admin_url).await.unwrap();
        sqlx::query(&format!("drop database {} (force)", self.name))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}
