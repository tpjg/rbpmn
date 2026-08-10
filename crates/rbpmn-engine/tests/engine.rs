//! Integration tests against real PostgreSQL (design brief, testing
//! strategy #4). Each test creates a throwaway database, migrates it, and
//! drops it on success. Requires a reachable Postgres; override the admin
//! URL with RBPMN_TEST_ADMIN_URL (default: postgres://$USER@localhost:5432/postgres).

use rbpmn_core::Bindings;
use rbpmn_engine::{Completion, DeployError, Engine, FailOutcome};
use sqlx::{PgPool, Row};
use std::fs;
use std::path::Path;

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rbpmn-model/tests/fixtures")
        .join(name);
    fs::read_to_string(path).unwrap()
}

struct TestDb {
    pool: PgPool,
    admin_url: String,
    name: String,
}

impl TestDb {
    async fn create() -> Self {
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

    async fn drop(self) {
        self.pool.close().await;
        let admin = PgPool::connect(&self.admin_url).await.unwrap();
        sqlx::query(&format!("drop database {} (force)", self.name))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}

async fn engine(db: &TestDb) -> Engine {
    let engine = Engine::builder(db.pool.clone()).build();
    engine.migrate().await.unwrap();
    engine
}

#[tokio::test]
async fn deploy_is_idempotent_by_content() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = fixture("accept/01-minimal.bpmn");

    let first = engine.deploy(&xml, &Bindings::default()).await.unwrap();
    assert_eq!((first.version, first.reused), (1, false));

    let again = engine.deploy(&xml, &Bindings::default()).await.unwrap();
    assert_eq!((again.version, again.reused), (1, true));
    assert_eq!(again.definition_id, first.definition_id);

    // Changed content (even a comment) is a new version.
    let changed = xml.replace("<bpmn:process", "<!-- v2 --><bpmn:process");
    let second = engine.deploy(&changed, &Bindings::default()).await.unwrap();
    assert_eq!((second.version, second.reused), (2, false));
    db.drop().await;
}

#[tokio::test]
async fn environment_grows_and_gates_deploys() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = fixture("accept/16-foreign-binding-warn.bpmn");
    let bindings = Bindings::new().topic("st", "payments");

    // The environment does not cover 'payments' yet: rejected, loudly.
    match engine.deploy(&xml, &bindings).await {
        Err(DeployError::Rejected(diags)) => {
            assert!(diags.iter().any(|d| d.rule == "unresolved-topic"));
        }
        other => panic!("expected unresolved-topic rejection, got {other:?}"),
    }

    // Grow the environment after build — idempotently — and retry.
    engine.declare_topic("payments");
    engine.declare_topic("payments");
    let deployed = engine.deploy(&xml, &bindings).await.unwrap();
    assert!(!deployed.reused);
    // The vendor-attribute warn came through as a warning, not a rejection.
    assert!(
        deployed
            .warnings
            .iter()
            .any(|d| d.rule == "no-foreign-implementation")
    );
    db.drop().await;
}

#[tokio::test]
async fn full_flow_matches_the_core_golden_trace() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/03-parallel-gateway.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();

    let started = engine
        .start("p", Some("order-42"), serde_json::json!({}))
        .await
        .unwrap();
    let open = open_items(&db.pool, started.id).await;
    assert_eq!(open.len(), 2);

    for (id, _) in &open {
        let done = engine
            .complete_work_item(*id, serde_json::json!({}))
            .await
            .unwrap();
        assert!(matches!(done, Completion::Advanced(_)));
    }

    let status: String = sqlx::query("select status from instance where id = $1")
        .bind(started.id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get("status");
    assert_eq!(status, "completed");

    // The event rows, replayed through the core's Display, are the same
    // golden trace the scenario corpus asserts.
    let trace = event_trace(&db.pool, started.id).await;
    let expected: Vec<String> = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../rbpmn-core/tests/scenarios/03-parallel.json"),
        )
        .unwrap(),
    )
    .unwrap()["expect"]["trace"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(trace, expected);

    let tokens: i64 = sqlx::query("select count(*) from token where instance_id = $1")
        .bind(started.id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get(0);
    assert_eq!(tokens, 0);
    db.drop().await;
}

#[tokio::test]
async fn join_fires_exactly_once_under_concurrent_completion() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/03-parallel-gateway.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();

    for _ in 0..20 {
        let started = engine
            .start("p", None, serde_json::json!({}))
            .await
            .unwrap();
        let open = open_items(&db.pool, started.id).await;
        let (a, b) = (open[0].0, open[1].0);

        let (ra, rb) = tokio::join!(
            engine.complete_work_item(a, serde_json::json!({})),
            engine.complete_work_item(b, serde_json::json!({})),
        );
        assert!(matches!(ra.unwrap(), Completion::Advanced(_)));
        assert!(matches!(rb.unwrap(), Completion::Advanced(_)));

        let join_fired: i64 = sqlx::query(
            "select count(*) from event where instance_id = $1 \
             and kind = 'element-started' and element_id = 'pj'",
        )
        .bind(started.id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get(0);
        assert_eq!(join_fired, 1, "parallel join must fire exactly once");

        let status: String = sqlx::query("select status from instance where id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .get("status");
        assert_eq!(status, "completed");
    }
    db.drop().await;
}

#[tokio::test]
async fn double_completion_is_idempotent_including_concurrently() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();

    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    let id = open_items(&db.pool, started.id).await[0].0;

    let (r1, r2) = tokio::join!(
        engine.complete_work_item(id, serde_json::json!({})),
        engine.complete_work_item(id, serde_json::json!({})),
    );
    let outcomes = [r1.unwrap(), r2.unwrap()];
    let advanced = outcomes
        .iter()
        .filter(|c| matches!(c, Completion::Advanced(_)))
        .count();
    let closed = outcomes
        .iter()
        .filter(|c| matches!(c, Completion::AlreadyClosed { .. }))
        .count();
    assert_eq!((advanced, closed), (1, 1), "exactly one completion wins");

    // A later repeat is the same distinct no-op.
    let again = engine
        .complete_work_item(id, serde_json::json!({}))
        .await
        .unwrap();
    assert!(matches!(again, Completion::AlreadyClosed { .. }));
    db.drop().await;
}

#[tokio::test]
async fn failed_retries_raise_an_incident() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("payments");
    engine
        .deploy(
            &fixture("accept/16-foreign-binding-warn.bpmn"),
            &Bindings::new().topic("st", "payments"),
        )
        .await
        .unwrap();

    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    let id = open_items(&db.pool, started.id).await[0].0;

    assert_eq!(
        engine.fail_work_item(id).await.unwrap(),
        FailOutcome::Retrying { retries_left: 2 }
    );
    assert_eq!(
        engine.fail_work_item(id).await.unwrap(),
        FailOutcome::Retrying { retries_left: 1 }
    );
    assert_eq!(
        engine.fail_work_item(id).await.unwrap(),
        FailOutcome::IncidentRaised
    );

    let status: String = sqlx::query("select status from instance where id = $1")
        .bind(started.id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get("status");
    assert_eq!(status, "failed");

    // Completing on a failed instance is refused loudly.
    assert!(
        engine
            .complete_work_item(id, serde_json::json!({}))
            .await
            .is_err()
    );
    db.drop().await;
}

#[tokio::test]
async fn startup_revalidation_detects_environment_drift() {
    let db = TestDb::create().await;
    let engine_a = engine(&db).await;
    engine_a.declare_topic("payments");
    engine_a
        .deploy(
            &fixture("accept/16-foreign-binding-warn.bpmn"),
            &Bindings::new().topic("st", "payments"),
        )
        .await
        .unwrap();
    assert!(
        engine_a
            .check_active_definitions()
            .await
            .unwrap()
            .is_empty()
    );

    // A restart rebuilds the environment from code/config; this one forgot
    // the declaration. Startup re-validation flags it before it can hide.
    let engine_b = Engine::builder(db.pool.clone()).build();
    let diags = engine_b.check_active_definitions().await.unwrap();
    assert!(
        diags.iter().any(|d| d.rule == "unresolved-topic"),
        "{diags:?}"
    );

    engine_b.declare_topic("payments");
    assert!(
        engine_b
            .check_active_definitions()
            .await
            .unwrap()
            .is_empty()
    );
    db.drop().await;
}

async fn open_items(pool: &PgPool, instance: uuid::Uuid) -> Vec<(uuid::Uuid, String)> {
    sqlx::query(
        "select id, element_id from work_item \
         where instance_id = $1 and state = 'available' order by item_no",
    )
    .bind(instance)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("id"), r.get("element_id")))
    .collect()
}

async fn event_trace(pool: &PgPool, instance: uuid::Uuid) -> Vec<String> {
    sqlx::query("select payload from event where instance_id = $1 order by id")
        .bind(instance)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| {
            let event: rbpmn_core::Event =
                serde_json::from_value(r.get::<serde_json::Value, _>("payload")).unwrap();
            event.to_string()
        })
        .collect()
}
