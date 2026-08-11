//! Integration tests against real PostgreSQL (design brief, testing
//! strategy #4). Each test creates a throwaway database, migrates it, and
//! drops it on success (see rbpmn_engine::testing).

use rbpmn_core::Bindings;
use rbpmn_engine::testing::TestDb;
use rbpmn_engine::{
    Completion, DeployError, Engine, FailOptions, FailOutcome, HandlerFailure, HttpPostHandler,
    ServiceTaskHandler, WorkItem, WorkerOptions,
};
use sqlx::{PgPool, Row};
use std::fs;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rbpmn-model/tests/fixtures")
        .join(name);
    fs::read_to_string(path).unwrap()
}

async fn engine(db: &TestDb) -> Engine {
    // Zero retry backoff: tests drive failures deliberately and fast.
    let engine = Engine::builder(db.pool.clone())
        .retry_backoff(Duration::ZERO)
        .build();
    engine.migrate().await.unwrap();
    engine
}

fn fail_code(code: &str) -> FailOptions {
    FailOptions {
        error_code: Some(code.to_string()),
        ..FailOptions::default()
    }
}

/// Closure-backed handler for tests.
struct FnHandler<F>(F);

impl<F> ServiceTaskHandler for FnHandler<F>
where
    F: Fn(WorkItem) -> Result<serde_json::Value, HandlerFailure> + Send + Sync,
{
    fn execute(
        &self,
        item: WorkItem,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, HandlerFailure>> + Send + '_>> {
        let result = (self.0)(item);
        Box::pin(async move { result })
    }
}

fn worker_options() -> WorkerOptions {
    WorkerOptions {
        poll_interval: Duration::from_millis(200),
        ..WorkerOptions::default()
    }
}

async fn wait_for_status(pool: &PgPool, instance: uuid::Uuid, wanted: &str) {
    for _ in 0..100 {
        let status: String = sqlx::query("select status from rbpmn_instance where id = $1")
            .bind(instance)
            .fetch_one(pool)
            .await
            .unwrap()
            .get("status");
        if status == wanted {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("instance never reached status '{wanted}'");
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

    match engine.deploy(&xml, &bindings).await {
        Err(DeployError::Rejected(diags)) => {
            assert!(diags.iter().any(|d| d.rule == "unresolved-topic"));
        }
        other => panic!("expected unresolved-topic rejection, got {other:?}"),
    }

    engine.declare_topic("payments").await.unwrap();
    engine.declare_topic("payments").await.unwrap();
    let deployed = engine.deploy(&xml, &bindings).await.unwrap();
    assert!(!deployed.reused);
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

    wait_for_status(&db.pool, started.id, "completed").await;

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
            "select count(*) from rbpmn_event where instance_id = $1 \
             and kind = 'element-started' and element_id = 'pj'",
        )
        .bind(started.id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get(0);
        assert_eq!(join_fired, 1, "parallel join must fire exactly once");
        wait_for_status(&db.pool, started.id, "completed").await;
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
    db.drop().await;
}

#[tokio::test]
async fn library_transaction_shares_business_writes() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    sqlx::query("create table business (order_id int)")
        .execute(&db.pool)
        .await
        .unwrap();

    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    let item = open_items(&db.pool, started.id).await[0].0;

    // Rolled back together: neither the business write nor the completion.
    {
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query("insert into business values (42)")
            .execute(&mut *tx)
            .await
            .unwrap();
        let done = engine
            .complete_work_item_in_tx(&mut tx, item, serde_json::json!({}))
            .await
            .unwrap();
        assert!(matches!(done, Completion::Advanced(_)));
        drop(tx); // rollback
    }
    let rows: i64 = sqlx::query("select count(*) from business")
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 0);
    assert_eq!(
        open_items(&db.pool, started.id).await.len(),
        1,
        "completion rolled back"
    );

    // Committed together: both.
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("insert into business values (42)")
        .execute(&mut *tx)
        .await
        .unwrap();
    engine
        .complete_work_item_in_tx(&mut tx, item, serde_json::json!({}))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let rows: i64 = sqlx::query("select count(*) from business")
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get(0);
    assert_eq!(rows, 1);
    wait_for_status(&db.pool, started.id, "completed").await;
    db.drop().await;
}

#[tokio::test]
async fn failed_retries_raise_an_incident_without_a_boundary() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("payments").await.unwrap();
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

    let opts = FailOptions {
        detail: Some("card processor unreachable".to_string()),
        ..FailOptions::default()
    };
    assert_eq!(
        engine.fail_work_item(id, &opts).await.unwrap(),
        FailOutcome::Retrying { retries_left: 2 }
    );
    assert_eq!(
        engine.fail_work_item(id, &opts).await.unwrap(),
        FailOutcome::Retrying { retries_left: 1 }
    );
    assert_eq!(
        engine.fail_work_item(id, &opts).await.unwrap(),
        FailOutcome::IncidentRaised
    );

    wait_for_status(&db.pool, started.id, "failed").await;
    // The failure reason is recorded — incidents are diagnosable.
    let inspection = engine.inspect_instance(started.id).await.unwrap();
    assert_eq!(
        inspection.work_items[0].last_failure.as_deref(),
        Some("card processor unreachable")
    );
    // The failed item itself answers with the idempotent no-op (the
    // closed-item check precedes the incident gate); IncidentOpen is
    // reserved for *open* items of a frozen instance.
    match engine
        .complete_work_item(id, serde_json::json!({}))
        .await
        .unwrap()
    {
        Completion::AlreadyClosed { state } => assert_eq!(state, "failed"),
        other => panic!("expected AlreadyClosed, got {other:?}"),
    }
    db.drop().await;
}

#[tokio::test]
async fn exhausted_retries_take_a_matching_error_boundary() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("st").await.unwrap();
    engine
        .deploy(
            &fixture("accept/10-error-boundary.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();

    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    let id = open_items(&db.pool, started.id).await[0].0;

    engine
        .fail_work_item(id, &fail_code("PAYMENT_FAILED"))
        .await
        .unwrap();
    engine
        .fail_work_item(id, &fail_code("PAYMENT_FAILED"))
        .await
        .unwrap();
    match engine
        .fail_work_item(id, &fail_code("PAYMENT_FAILED"))
        .await
        .unwrap()
    {
        FailOutcome::ErrorCaught(events) => {
            assert!(events.iter().any(|e| e.to_string() == "element-started be"));
        }
        other => panic!("expected ErrorCaught, got {other:?}"),
    }

    // A retried fail of the now-closed item is the idempotent no-op, not 404.
    match engine
        .fail_work_item(id, &fail_code("PAYMENT_FAILED"))
        .await
        .unwrap()
    {
        FailOutcome::AlreadyClosed { state } => assert_eq!(state, "failed"),
        other => panic!("expected AlreadyClosed, got {other:?}"),
    }

    let open = open_items(&db.pool, started.id).await;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].1, "t_fix");
    engine
        .complete_work_item(open[0].0, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    db.drop().await;
}

/// The incident-livelock fix: siblings of a frozen instance are never
/// claimed, and a retried completion still converges on AlreadyClosed.
#[tokio::test]
async fn incident_freezes_siblings_without_livelock() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:process id="p2" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:parallelGateway id="ps"/>
    <bpmn:serviceTask id="st_a" name="A"/>
    <bpmn:serviceTask id="st_b" name="B"/>
    <bpmn:parallelGateway id="pj"/>
    <bpmn:endEvent id="end"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ps"/>
    <bpmn:sequenceFlow id="f2" sourceRef="ps" targetRef="st_a"/>
    <bpmn:sequenceFlow id="f3" sourceRef="ps" targetRef="st_b"/>
    <bpmn:sequenceFlow id="f4" sourceRef="st_a" targetRef="pj"/>
    <bpmn:sequenceFlow id="f5" sourceRef="st_b" targetRef="pj"/>
    <bpmn:sequenceFlow id="f6" sourceRef="pj" targetRef="end"/>
  </bpmn:process>
</bpmn:definitions>"#;
    engine.declare_topic("st_a").await.unwrap();
    let invocations = Arc::new(AtomicUsize::new(0));
    let counter = invocations.clone();
    engine.register_handler(
        "st_b",
        Arc::new(FnHandler(move |_item| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({}))
        })),
    );
    engine.deploy(xml, &Bindings::default()).await.unwrap();

    let started = engine
        .start("p2", None, serde_json::json!({}))
        .await
        .unwrap();
    let a = open_items(&db.pool, started.id)
        .await
        .into_iter()
        .find(|(_, el)| el == "st_a")
        .unwrap()
        .0;
    // Freeze the instance before any worker runs.
    for _ in 0..3 {
        engine
            .fail_work_item(a, &FailOptions::default())
            .await
            .unwrap();
    }
    wait_for_status(&db.pool, started.id, "failed").await;

    let worker = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run_worker(worker_options()).await }
    });
    tokio::time::sleep(Duration::from_millis(800)).await;
    worker.abort();

    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "sibling of a frozen instance must never be executed"
    );
    db.drop().await;
}

/// Idempotency survives a sibling incident: a retried, already-committed
/// completion answers AlreadyClosed, not 409.
#[tokio::test]
async fn completion_retry_converges_after_sibling_incident() {
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
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    let open = open_items(&db.pool, started.id).await;
    let (ta, tb) = (open[0].0, open[1].0);

    engine
        .complete_work_item(ta, serde_json::json!({}))
        .await
        .unwrap();
    for _ in 0..3 {
        engine
            .fail_work_item(tb, &FailOptions::default())
            .await
            .unwrap();
    }
    wait_for_status(&db.pool, started.id, "failed").await;

    match engine
        .complete_work_item(ta, serde_json::json!({}))
        .await
        .unwrap()
    {
        Completion::AlreadyClosed { state } => assert_eq!(state, "completed"),
        other => panic!("expected AlreadyClosed, got {other:?}"),
    }
    db.drop().await;
}

#[tokio::test]
async fn failing_a_live_lease_requires_its_owner() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("payments").await.unwrap();
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

    sqlx::query(
        "update rbpmn_work_item set state = 'locked', lock_owner = 'w1', \
         lock_until = now() + interval '10 minutes' where id = $1",
    )
    .bind(id)
    .execute(&db.pool)
    .await
    .unwrap();

    // Ownerless (HTTP-style) fail is refused; the owner's goes through.
    assert!(matches!(
        engine.fail_work_item(id, &FailOptions::default()).await,
        Err(rbpmn_engine::EngineError::ItemLeased(_))
    ));
    let owned = FailOptions {
        owner: Some("w1".to_string()),
        ..FailOptions::default()
    };
    assert_eq!(
        engine.fail_work_item(id, &owned).await.unwrap(),
        FailOutcome::Retrying { retries_left: 2 }
    );
    db.drop().await;
}

#[tokio::test]
async fn nul_bytes_in_variables_are_rejected_loudly() {
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

    let err = engine
        .complete_work_item(id, serde_json::json!({ "note": "a\u{0}b" }))
        .await;
    assert!(matches!(
        err,
        Err(rbpmn_engine::EngineError::InvalidVariables(_))
    ));
    // Not poisoned: a clean retry completes.
    engine
        .complete_work_item(id, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    db.drop().await;
}

#[tokio::test]
async fn worker_executes_registered_handlers() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.register_handler(
        "payments",
        Arc::new(FnHandler(|_item| {
            Ok(serde_json::json!({ "charged": true }))
        })),
    );
    engine
        .deploy(
            &fixture("accept/16-foreign-binding-warn.bpmn"),
            &Bindings::new().topic("st", "payments"),
        )
        .await
        .unwrap();

    let worker = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run_worker(worker_options()).await }
    });

    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;

    let variables: serde_json::Value =
        sqlx::query("select variables from rbpmn_instance where id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .get("variables");
    assert_eq!(variables, serde_json::json!({ "charged": true }));
    worker.abort();
    db.drop().await;
}

#[tokio::test]
async fn worker_failures_walk_retries_into_the_boundary() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.register_handler(
        "st",
        Arc::new(FnHandler(|_item| {
            Err(HandlerFailure {
                code: Some("PAYMENT_FAILED".into()),
                message: "card declined".into(),
            })
        })),
    );
    engine
        .deploy(
            &fixture("accept/10-error-boundary.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();

    let worker = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run_worker(worker_options()).await }
    });

    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    for _ in 0..100 {
        let open = open_items(&db.pool, started.id).await;
        if open.len() == 1 && open[0].1 == "t_fix" {
            worker.abort();
            // The failure reason was recorded along the way.
            let inspection = engine.inspect_instance(started.id).await.unwrap();
            let st = inspection
                .work_items
                .iter()
                .find(|w| w.element_id == "st")
                .unwrap();
            assert_eq!(st.last_failure.as_deref(), Some("card declined"));
            engine
                .complete_work_item(open[0].0, serde_json::json!({}))
                .await
                .unwrap();
            wait_for_status(&db.pool, started.id, "completed").await;
            db.drop().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("boundary path never reached");
}

/// A slow handler outlives its lease only because the worker renews it:
/// exactly one invocation despite lease << handler duration.
#[tokio::test]
async fn leases_renew_while_the_handler_runs() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;

    struct SlowHandler(Arc<AtomicUsize>);
    impl ServiceTaskHandler for SlowHandler {
        fn execute(
            &self,
            _item: WorkItem,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, HandlerFailure>> + Send + '_>>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(2500)).await;
                Ok(serde_json::json!({ "slow": true }))
            })
        }
    }
    let invocations = Arc::new(AtomicUsize::new(0));
    engine.register_handler("payments", Arc::new(SlowHandler(invocations.clone())));
    engine
        .deploy(
            &fixture("accept/16-foreign-binding-warn.bpmn"),
            &Bindings::new().topic("st", "payments"),
        )
        .await
        .unwrap();

    // Two competing workers with a lease far shorter than the handler.
    let opts = |name: &str| WorkerOptions {
        owner: name.to_string(),
        lease: Duration::from_millis(900),
        poll_interval: Duration::from_millis(150),
    };
    let w1 = tokio::spawn({
        let engine = engine.clone();
        let o = opts("w1");
        async move { engine.run_worker(o).await }
    });
    let w2 = tokio::spawn({
        let engine = engine.clone();
        let o = opts("w2");
        async move { engine.run_worker(o).await }
    });

    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    w1.abort();
    w2.abort();

    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "renewed lease must prevent concurrent re-execution"
    );
    db.drop().await;
}

#[tokio::test]
async fn expired_leases_are_reclaimed() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.register_handler(
        "payments",
        Arc::new(FnHandler(|_item| Ok(serde_json::json!({})))),
    );
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

    sqlx::query(
        "update rbpmn_work_item set state = 'locked', lock_owner = 'dead-worker', \
         lock_until = now() - interval '5 seconds' where instance_id = $1",
    )
    .bind(started.id)
    .execute(&db.pool)
    .await
    .unwrap();

    let worker = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run_worker(worker_options()).await }
    });
    wait_for_status(&db.pool, started.id, "completed").await;
    worker.abort();
    db.drop().await;
}

#[tokio::test]
async fn http_post_handler_end_to_end() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/work",
        axum::routing::post(|body: axum::Json<serde_json::Value>| async move {
            assert_eq!(body["topic"], "payments");
            axum::Json(serde_json::json!({ "paid": true }))
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    engine.register_handler(
        "payments",
        Arc::new(HttpPostHandler::new(format!("http://{addr}/work"))),
    );
    engine
        .deploy(
            &fixture("accept/16-foreign-binding-warn.bpmn"),
            &Bindings::new().topic("st", "payments"),
        )
        .await
        .unwrap();

    let worker = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run_worker(worker_options()).await }
    });
    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;

    let variables: serde_json::Value =
        sqlx::query("select variables from rbpmn_instance where id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .get("variables");
    assert_eq!(variables, serde_json::json!({ "paid": true }));
    worker.abort();
    db.drop().await;
}

/// A 2xx with a non-object body must not wipe the variables: it is a
/// handler failure that retries into an incident, with the reason recorded.
#[tokio::test]
async fn non_object_handler_body_fails_instead_of_wiping_variables() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/work",
        axum::routing::post(|| async { axum::Json(serde_json::json!("ok")) }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    engine.register_handler(
        "payments",
        Arc::new(HttpPostHandler::new(format!("http://{addr}/work"))),
    );
    engine
        .deploy(
            &fixture("accept/16-foreign-binding-warn.bpmn"),
            &Bindings::new().topic("st", "payments"),
        )
        .await
        .unwrap();

    let worker = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run_worker(worker_options()).await }
    });
    let started = engine
        .start("p", None, serde_json::json!({ "precious": 1 }))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "failed").await;
    worker.abort();

    let inspection = engine.inspect_instance(started.id).await.unwrap();
    assert_eq!(inspection.variables, serde_json::json!({ "precious": 1 }));
    assert!(
        inspection.work_items[0]
            .last_failure
            .as_deref()
            .unwrap()
            .contains("JSON object"),
    );
    db.drop().await;
}

#[tokio::test]
async fn api_declared_topics_survive_restart() {
    let db = TestDb::create().await;
    let engine_a = engine(&db).await;
    engine_a.declare_topic("payments").await.unwrap();
    engine_a
        .deploy(
            &fixture("accept/16-foreign-binding-warn.bpmn"),
            &Bindings::new().topic("st", "payments"),
        )
        .await
        .unwrap();

    // "Restart": a fresh engine with no config declarations at all.
    let engine_b = Engine::builder(db.pool.clone()).build();
    engine_b.sync_environment().await.unwrap();
    assert!(
        engine_b
            .check_active_definitions()
            .await
            .unwrap()
            .is_empty(),
        "persisted declaration must survive the restart"
    );
    db.drop().await;
}

#[tokio::test]
async fn startup_revalidation_detects_handler_drift() {
    let db = TestDb::create().await;
    let engine_a = engine(&db).await;
    // Covered by a *handler* (code, not persistable) — removing it from the
    // config is exactly the drift the startup check must catch.
    engine_a.register_handler(
        "payments",
        Arc::new(FnHandler(|_item| Ok(serde_json::json!({})))),
    );
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

    let engine_b = Engine::builder(db.pool.clone()).build();
    engine_b.sync_environment().await.unwrap();
    let diags = engine_b.check_active_definitions().await.unwrap();
    assert!(
        diags.iter().any(|d| d.rule == "unresolved-topic"),
        "{diags:?}"
    );

    engine_b.declare_topic("payments").await.unwrap();
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
        "select id, element_id from rbpmn_work_item \
         where instance_id = $1 and state in ('available', 'locked') order by item_no",
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
    sqlx::query("select payload from rbpmn_event where instance_id = $1 order by id")
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
