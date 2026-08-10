//! Test harness (feature `test-util`): throwaway databases against a local
//! Postgres, shared by the engine's own integration tests and the server's.

use sqlx::PgPool;

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
