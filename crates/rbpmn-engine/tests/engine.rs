//! Integration tests against real PostgreSQL (design brief, testing
//! strategy #4). Each test creates a throwaway database, migrates it, and
//! drops it on success (see rbpmn_engine::testing).

mod harness;

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

    // A completed instance holds zero token rows — the quiescent snapshot
    // of an empty core state.
    let tokens: i64 = sqlx::query("select count(*) from rbpmn_token where instance_id = $1")
        .bind(started.id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get(0);
    assert_eq!(tokens, 0);

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
            .complete_work_item_in_tx(&mut tx, item, None, serde_json::json!({}))
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
        .complete_work_item_in_tx(&mut tx, item, None, serde_json::json!({}))
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

/// The variable document is the application's, and the engine promises only
/// to carry it. A number wider than an `f64` must therefore come back exactly
/// as it went in — through `start`, through a merge patch, and through the
/// `jsonb` column in between.
///
/// This is what `serde_json`'s `arbitrary_precision` feature buys, and the
/// only thing that pins it: without it `serde_json::Number` is an `f64`, so a
/// 30-digit order id silently became `1.2345678901234568e+29` on the way in —
/// before PostgreSQL, whose `jsonb` numbers are arbitrary-precision `numeric`,
/// ever saw it. Removing the feature turns this test red rather than turning
/// somebody's invoice wrong (docs/dmn.md, "The `arbitrary_precision` spike").
#[tokio::test]
async fn numbers_wider_than_f64_survive_the_variable_document() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();

    // Written as text, so the assertion is about the values themselves and
    // not about whatever `json!` would have parsed them into.
    let initial: serde_json::Value = serde_json::from_str(
        r#"{ "rate": 0.3333333333333333333333333333333333,
              "orderId": 123456789012345678901234567890,
              "price": 1.50 }"#,
    )
    .unwrap();
    let started = engine.start("p", None, initial).await.unwrap();

    let id = open_items(&db.pool, started.id).await[0].0;
    let patch: serde_json::Value =
        serde_json::from_str(r#"{ "total": 9999999999999999999999999999999999 }"#).unwrap();
    engine.complete_work_item(id, patch).await.unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;

    let vars = engine.inspect_instance(started.id).await.unwrap().variables;
    for (field, expected) in [
        ("rate", "0.3333333333333333333333333333333333"),
        ("orderId", "123456789012345678901234567890"),
        // Trailing zeros are the application's business too: a price is 1.50.
        ("price", "1.50"),
        ("total", "9999999999999999999999999999999999"),
    ] {
        assert_eq!(
            vars[field].to_string(),
            expected,
            "{field} did not survive the round trip"
        );
    }
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
    //
    // This asserts the *happy* path of renewal: one invocation, because the
    // heartbeat keeps winning. It therefore assumes the renewal UPDATE is not
    // starved for longer than the 900ms lease — true in a quiet run, and it
    // can flake under heavy parallel load. Losing that race is legal (delivery
    // is at-least-once); what must hold regardless is exactly-once state
    // transition, which `a_starved_renewal_reruns_the_handler_but_applies_once`
    // pins deterministically.
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

// ---------------------------------------------------------------------------
// Phase 3: timers & messages
// ---------------------------------------------------------------------------

/// start -> timer catch (`spec`) -> end.
fn timer_catch_xml(spec: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                  id="defs" targetNamespace="urn:test">
  <bpmn:process id="pt" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:intermediateCatchEvent id="c">
      <bpmn:timerEventDefinition>{spec}</bpmn:timerEventDefinition>
    </bpmn:intermediateCatchEvent>
    <bpmn:endEvent id="end"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="c"/>
    <bpmn:sequenceFlow id="f2" sourceRef="c" targetRef="end"/>
  </bpmn:process>
</bpmn:definitions>"#
    )
}

async fn timer_rows(pool: &PgPool, instance: uuid::Uuid) -> i64 {
    sqlx::query("select count(*) from rbpmn_timer where instance_id = $1")
        .bind(instance)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<i64, _>(0)
}

async fn subscription_rows(pool: &PgPool, instance: uuid::Uuid) -> i64 {
    sqlx::query("select count(*) from rbpmn_subscription where instance_id = $1")
        .bind(instance)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<i64, _>(0)
}

async fn event_count(pool: &PgPool, instance: uuid::Uuid, kind: &str) -> i64 {
    sqlx::query("select count(*) from rbpmn_event where instance_id = $1 and kind = $2")
        .bind(instance)
        .bind(kind)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<i64, _>(0)
}

#[tokio::test]
async fn timer_fires_from_database_time() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT0S</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(timer_rows(&db.pool, started.id).await, 1);

    assert!(engine.fire_due_timer().await.unwrap());
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_eq!(timer_rows(&db.pool, started.id).await, 0);
    assert_eq!(event_count(&db.pool, started.id, "timer-fired").await, 1);
    // Nothing left to fire.
    assert!(!engine.fire_due_timer().await.unwrap());
    db.drop().await;
}

/// A deadline read from the variable document: standard BPMN (`timeDuration`
/// is typed `tExpression`), resolved at arm time, and stored as the resolved
/// literal so the projection's SQL cast only ever sees a validated value.
#[tokio::test]
async fn timer_deadline_comes_from_the_variable_document() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml(
                r#"<bpmn:timeDuration xsi:type="bpmn:tFormalExpression">sla.wait</bpmn:timeDuration>"#,
            ),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("pt", None, serde_json::json!({ "sla": { "wait": "PT0S" } }))
        .await
        .unwrap();

    // The row carries what was resolved, not the expression: rehydration and
    // the scheduler never see a name they would have to look up again.
    let spec: String =
        sqlx::query_scalar("select due_spec from rbpmn_timer where instance_id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(spec, "PT0S");

    assert!(engine.fire_due_timer().await.unwrap());
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_eq!(event_count(&db.pool, started.id, "timer-fired").await, 1);
    db.drop().await;
}

/// The failure mode the feature exists to get right. An unresolvable deadline
/// must not reach the projection's `$1::interval` cast: that would abort the
/// step transaction, stranding the token at its *previous* wait state with a
/// worker retrying into the same failure forever. Nor may it be swallowed,
/// which parks a token no timer will ever wake. It freezes at an incident
/// carrying the reason — the same way a correlation key that cannot resolve
/// does.
#[tokio::test]
async fn an_unresolvable_deadline_freezes_at_an_incident() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml(
                r#"<bpmn:timeDuration xsi:type="bpmn:tFormalExpression">sla.wait</bpmn:timeDuration>"#,
            ),
            &Bindings::default(),
        )
        .await
        .unwrap();

    for (variables, expected) in [
        (serde_json::json!({}), "missing"),
        (serde_json::json!({ "sla": { "wait": 5 } }), "a number"),
        (
            serde_json::json!({ "sla": { "wait": "5 minutes" } }),
            "not valid",
        ),
    ] {
        let started = engine.start("pt", None, variables).await.unwrap();
        wait_for_status(&db.pool, started.id, "failed").await;
        // No timer row: nothing to fire, and nothing for the scheduler to
        // trip over later.
        assert_eq!(timer_rows(&db.pool, started.id).await, 0);
        assert!(!engine.fire_due_timer().await.unwrap());

        // Frozen where it failed, and inspectable — the repair API's single
        // resume point. The reason must be reachable *through inspection*:
        // it is deliberately absent from the Display format (that is the
        // stable golden trace), so if it were not carried separately an
        // operator would see only that the arm failed, never why.
        let view = engine.inspect_instance(started.id).await.unwrap();
        let failure = view
            .events
            .iter()
            .find(|e| e.kind == "timer-resolve-failed")
            .expect("the incident records why it could not resolve");
        let reason = failure.detail.clone().unwrap_or_default();
        assert!(
            reason.contains("sla.wait") && reason.contains(expected),
            "unhelpful incident reason: {reason}"
        );
        assert_eq!(view.status, "failed");
        assert_eq!(view.tokens.len(), 1);
        assert_eq!(view.tokens[0].element_id, "c");
        assert_eq!(view.tokens[0].wait_kind, "incident");
    }
    db.drop().await;
}

/// Freezing mid-entry must leave nothing half-open. The failing arm happens
/// *after* the host's work item exists and, for a subprocess, after its scope
/// is allocated — so the freeze has to close both. Latent while claimability
/// requires `status = 'active'`, but a repair API clearing the incident would
/// otherwise hand a worker an item whose token is parked at an incident, and
/// completing it would advance straight past that incident.
#[tokio::test]
async fn a_freeze_mid_entry_leaves_nothing_open() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:process id="pb" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:subProcess id="sub">
      <bpmn:startEvent id="s2"/>
      <bpmn:userTask id="inner"/>
      <bpmn:endEvent id="e2"/>
      <bpmn:sequenceFlow id="g1" sourceRef="s2" targetRef="inner"/>
      <bpmn:sequenceFlow id="g2" sourceRef="inner" targetRef="e2"/>
    </bpmn:subProcess>
    <bpmn:boundaryEvent id="bt" attachedToRef="sub">
      <bpmn:timerEventDefinition>
        <bpmn:timeDuration>sla.wait</bpmn:timeDuration>
      </bpmn:timerEventDefinition>
    </bpmn:boundaryEvent>
    <bpmn:endEvent id="end"/>
    <bpmn:endEvent id="timeout"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="sub"/>
    <bpmn:sequenceFlow id="f2" sourceRef="sub" targetRef="end"/>
    <bpmn:sequenceFlow id="f3" sourceRef="bt" targetRef="timeout"/>
  </bpmn:process>
</bpmn:definitions>"#;
    engine.deploy(xml, &Bindings::default()).await.unwrap();
    let started = engine
        .start("pb", None, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "failed").await;

    // The subprocess body never started, so its scope must not survive: a
    // scope row with no members whose owner is an incident is exactly what a
    // resume would trip on.
    let scopes: i64 = sqlx::query_scalar("select count(*) from rbpmn_scope where instance_id = $1")
        .bind(started.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(scopes, 0, "the unopened scope outlived its owner");
    // And no work item is left claimable on a failed instance.
    let open: i64 = sqlx::query_scalar(
        "select count(*) from rbpmn_work_item \
         where instance_id = $1 and state in ('available', 'locked')",
    )
    .bind(started.id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(open, 0);
    db.drop().await;
}

/// Parse order is the signal, not the `xsi:type` marker. bpmn-moddle stamps
/// `xsi:type="bpmn:tFormalExpression"` on every expression object, so every
/// bpmn-js modeler writes it on ordinary literals — an earlier version keyed
/// off it and turned `P5D` typed into a properties panel into a variable
/// named `P5D`.
#[tokio::test]
async fn a_literal_is_a_literal_however_it_is_marked() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    // Marked, but a valid duration: still a literal, and no warning.
    let deployed = engine
        .deploy(
            &timer_catch_xml(
                r#"<bpmn:timeDuration xsi:type="bpmn:tFormalExpression">PT0S</bpmn:timeDuration>"#,
            ),
            &Bindings::default(),
        )
        .await
        .unwrap();
    assert!(
        !deployed
            .warnings
            .iter()
            .any(|d| d.rule == "timer-expression")
    );
    let started = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();
    assert!(engine.fire_due_timer().await.unwrap());
    wait_for_status(&db.pool, started.id, "completed").await;

    // Unmarked, and not a duration: read as a variable, with the warning.
    let deployed = engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>sla.wait</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let warning = deployed
        .warnings
        .iter()
        .find(|d| d.rule == "timer-expression")
        .expect("expected a timer-expression warning");
    // The warning carries the ISO-8601 complaint that made it fall through,
    // which is what keeps a mistyped duration legible rather than silent.
    assert!(
        warning.message.contains("sla.wait") && warning.message.contains("not a literal"),
        "unhelpful warning: {}",
        warning.message
    );
    db.drop().await;
}

#[tokio::test]
async fn sleeping_timer_is_a_passive_row() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>P1D</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();

    // Not due: nothing fires, the row just sits there with a far due_at.
    assert!(!engine.fire_due_timer().await.unwrap());
    let hours: f64 = sqlx::query(
        "select (extract(epoch from (due_at - now()))/3600)::float8 from rbpmn_timer \
         where instance_id = $1",
    )
    .bind(started.id)
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .get::<f64, _>(0);
    assert!((23.9..24.1).contains(&hours), "due in {hours} hours");
    db.drop().await;
}

#[tokio::test]
async fn date_timer_arms_at_the_absolute_instant() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDate>2100-01-01T00:00:00Z</bpmn:timeDate>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();

    assert!(!engine.fire_due_timer().await.unwrap());
    let due: String = sqlx::query(
        "select to_char(due_at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
         from rbpmn_timer where instance_id = $1",
    )
    .bind(started.id)
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .get::<String, _>(0);
    assert_eq!(due, "2100-01-01T00:00:00Z");
    db.drop().await;
}

/// The scheduler sleeps until min(due_at), not the poll interval: a 1-second
/// timer completes long before the 30-second fallback poll would tick.
#[tokio::test]
async fn scheduler_wakes_for_the_earliest_timer() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT1S</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let scheduler = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .run_scheduler(rbpmn_engine::SchedulerOptions::default())
                .await
        }
    });
    let started = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    scheduler.abort();
    db.drop().await;
}

#[tokio::test]
async fn competing_schedulers_fire_each_timer_exactly_once() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT0S</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let mut instances = Vec::new();
    for _ in 0..5 {
        instances.push(
            engine
                .start("pt", None, serde_json::json!({}))
                .await
                .unwrap()
                .id,
        );
    }
    let schedulers: Vec<_> = (0..2)
        .map(|_| {
            tokio::spawn({
                let engine = engine.clone();
                async move {
                    engine
                        .run_scheduler(rbpmn_engine::SchedulerOptions {
                            poll_interval: Duration::from_millis(100),
                        })
                        .await
                }
            })
        })
        .collect();
    for id in &instances {
        wait_for_status(&db.pool, *id, "completed").await;
    }
    for s in schedulers {
        s.abort();
    }
    for id in &instances {
        assert_eq!(
            event_count(&db.pool, *id, "timer-fired").await,
            1,
            "a timer must fire exactly once under competing schedulers"
        );
    }
    db.drop().await;
}

/// start -> user task ut (boundary timer bt `spec` -> e_esc) -> end.
fn boundary_timer_xml(spec: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:process id="pb" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:userTask id="ut" name="Approve"/>
    <bpmn:boundaryEvent id="bt" attachedToRef="ut">
      <bpmn:timerEventDefinition>{spec}</bpmn:timerEventDefinition>
    </bpmn:boundaryEvent>
    <bpmn:endEvent id="end"/>
    <bpmn:endEvent id="e_esc"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ut"/>
    <bpmn:sequenceFlow id="f2" sourceRef="ut" targetRef="end"/>
    <bpmn:sequenceFlow id="f3" sourceRef="bt" targetRef="e_esc"/>
  </bpmn:process>
</bpmn:definitions>"#
    )
}

#[tokio::test]
async fn boundary_timer_disarms_when_the_task_completes() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &boundary_timer_xml("<bpmn:timeDuration>PT1H</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("pb", None, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(timer_rows(&db.pool, started.id).await, 1);

    let (item, _) = open_items(&db.pool, started.id).await[0].clone();
    engine
        .complete_work_item(item, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_eq!(timer_rows(&db.pool, started.id).await, 0);
    assert_eq!(
        event_count(&db.pool, started.id, "timer-cancelled").await,
        1
    );
    db.drop().await;
}

#[tokio::test]
async fn boundary_timer_interrupts_the_task() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &boundary_timer_xml("<bpmn:timeDuration>PT0S</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("pb", None, serde_json::json!({}))
        .await
        .unwrap();

    assert!(engine.fire_due_timer().await.unwrap());
    wait_for_status(&db.pool, started.id, "completed").await;
    // The task never completed: its item was cancelled by the interruption.
    let state: String = sqlx::query(
        "select state from rbpmn_work_item where instance_id = $1 and element_id = 'ut'",
    )
    .bind(started.id)
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .get("state");
    assert_eq!(state, "cancelled");
    assert_eq!(event_count(&db.pool, started.id, "timer-fired").await, 1);
    db.drop().await;
}

#[tokio::test]
async fn correlate_delivers_exactly_once() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bindings = Bindings::new().correlation("c", "order.id");
    engine
        .deploy(&fixture("accept/17-message-catch.bpmn"), &bindings)
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({"order": {"id": "o-1"}}))
        .await
        .unwrap();

    // Wrong key: nowhere to go, said loudly.
    let miss = engine
        .correlate("WarehouseAck", "o-2", serde_json::json!({}))
        .await;
    assert!(matches!(
        miss,
        Err(rbpmn_engine::EngineError::NoSubscription { .. })
    ));

    let correlation = engine
        .correlate("WarehouseAck", "o-1", serde_json::json!({"shipped": true}))
        .await
        .unwrap();
    assert_eq!(correlation.instance_id, started.id);
    wait_for_status(&db.pool, started.id, "completed").await;
    let variables: serde_json::Value =
        sqlx::query("select variables from rbpmn_instance where id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .get("variables");
    assert_eq!(variables["shipped"], serde_json::json!(true));
    assert_eq!(subscription_rows(&db.pool, started.id).await, 0);

    // The subscription is consumed: a repeat has nowhere to go.
    let repeat = engine
        .correlate("WarehouseAck", "o-1", serde_json::json!({}))
        .await;
    assert!(matches!(
        repeat,
        Err(rbpmn_engine::EngineError::NoSubscription { .. })
    ));
    db.drop().await;
}

#[tokio::test]
async fn ambiguous_correlation_is_refused() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bindings = Bindings::new().correlation("c", "order.id");
    engine
        .deploy(&fixture("accept/17-message-catch.bpmn"), &bindings)
        .await
        .unwrap();
    let vars = serde_json::json!({"order": {"id": "dup"}});
    engine.start("p", None, vars.clone()).await.unwrap();
    engine.start("p", None, vars).await.unwrap();

    let result = engine
        .correlate("WarehouseAck", "dup", serde_json::json!({}))
        .await;
    assert!(matches!(
        result,
        Err(rbpmn_engine::EngineError::AmbiguousCorrelation { .. })
    ));
    db.drop().await;
}

/// The caller-transaction property extends to message delivery: a rollback
/// takes the business write and the delivery back together.
#[tokio::test]
async fn correlate_in_tx_shares_business_writes() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    sqlx::query("create table shipments (order_id text primary key)")
        .execute(&db.pool)
        .await
        .unwrap();
    let bindings = Bindings::new().correlation("c", "order.id");
    engine
        .deploy(&fixture("accept/17-message-catch.bpmn"), &bindings)
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({"order": {"id": "o-9"}}))
        .await
        .unwrap();

    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("insert into shipments (order_id) values ('o-9')")
        .execute(&mut *tx)
        .await
        .unwrap();
    engine
        .correlate_in_tx(&mut tx, "WarehouseAck", "o-9", serde_json::json!({}))
        .await
        .unwrap();
    tx.rollback().await.unwrap();

    // Both gone: the delivery never happened, the subscription is still open.
    assert_eq!(subscription_rows(&db.pool, started.id).await, 1);
    let shipments: i64 = sqlx::query("select count(*) from shipments")
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get(0);
    assert_eq!(shipments, 0);

    // And the retry converges.
    engine
        .correlate("WarehouseAck", "o-9", serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    db.drop().await;
}

#[tokio::test]
async fn missing_correlation_key_freezes_loudly() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bindings = Bindings::new().correlation("c", "order.id");
    engine
        .deploy(&fixture("accept/17-message-catch.bpmn"), &bindings)
        .await
        .unwrap();
    // No order.id in the variables: the subscription could never match.
    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "failed").await;
    assert_eq!(
        event_count(&db.pool, started.id, "correlation-failed").await,
        1
    );
    assert_eq!(subscription_rows(&db.pool, started.id).await, 0);
    db.drop().await;
}

/// Terminate tears everything down in one transaction — including armed
/// timers (fixture-12 shape with a timer in the surviving branch).
#[tokio::test]
async fn terminate_clears_armed_timers() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:process id="ptr" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:parallelGateway id="ps"/>
    <bpmn:intermediateCatchEvent id="tc">
      <bpmn:timerEventDefinition><bpmn:timeDuration>P1D</bpmn:timeDuration></bpmn:timerEventDefinition>
    </bpmn:intermediateCatchEvent>
    <bpmn:userTask id="tb" name="Check"/>
    <bpmn:exclusiveGateway id="xs" default="f_go"/>
    <bpmn:endEvent id="e_term"><bpmn:terminateEventDefinition/></bpmn:endEvent>
    <bpmn:parallelGateway id="pj"/>
    <bpmn:endEvent id="end"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ps"/>
    <bpmn:sequenceFlow id="f2" sourceRef="ps" targetRef="tc"/>
    <bpmn:sequenceFlow id="f3" sourceRef="tc" targetRef="pj"/>
    <bpmn:sequenceFlow id="f4" sourceRef="ps" targetRef="tb"/>
    <bpmn:sequenceFlow id="f5" sourceRef="tb" targetRef="xs"/>
    <bpmn:sequenceFlow id="f_cancel" sourceRef="xs" targetRef="e_term">
      <bpmn:conditionExpression>cancelled = true</bpmn:conditionExpression>
    </bpmn:sequenceFlow>
    <bpmn:sequenceFlow id="f_go" sourceRef="xs" targetRef="pj"/>
    <bpmn:sequenceFlow id="f6" sourceRef="pj" targetRef="end"/>
  </bpmn:process>
</bpmn:definitions>"#;
    engine.deploy(xml, &Bindings::default()).await.unwrap();
    let started = engine
        .start("ptr", None, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(timer_rows(&db.pool, started.id).await, 1);

    let (item, _) = open_items(&db.pool, started.id).await[0].clone();
    engine
        .complete_work_item(item, serde_json::json!({"cancelled": true}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "terminated").await;
    assert_eq!(timer_rows(&db.pool, started.id).await, 0);
    assert_eq!(
        event_count(&db.pool, started.id, "timer-cancelled").await,
        1
    );
    db.drop().await;
}

#[tokio::test]
async fn event_gateway_race_message_wins() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bindings = Bindings::new()
        .correlation("c_paid", "order.id")
        .correlation("c_cancel", "order.id");
    engine
        .deploy(&fixture("accept/11-event-based-gateway.bpmn"), &bindings)
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({"order": {"id": "o-e"}}))
        .await
        .unwrap();
    assert_eq!(timer_rows(&db.pool, started.id).await, 1);
    assert_eq!(subscription_rows(&db.pool, started.id).await, 2);

    engine
        .correlate("PaymentReceived", "o-e", serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    // The losers are withdrawn with the race.
    assert_eq!(timer_rows(&db.pool, started.id).await, 0);
    assert_eq!(subscription_rows(&db.pool, started.id).await, 0);
    db.drop().await;
}

#[tokio::test]
async fn event_gateway_race_timer_wins() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:message id="m" name="Answer"/>
  <bpmn:process id="pe" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:eventBasedGateway id="ebg"/>
    <bpmn:intermediateCatchEvent id="c_t">
      <bpmn:timerEventDefinition><bpmn:timeDuration>PT0S</bpmn:timeDuration></bpmn:timerEventDefinition>
    </bpmn:intermediateCatchEvent>
    <bpmn:intermediateCatchEvent id="c_m">
      <bpmn:messageEventDefinition messageRef="m"/>
    </bpmn:intermediateCatchEvent>
    <bpmn:endEvent id="e_t"/>
    <bpmn:endEvent id="e_m"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ebg"/>
    <bpmn:sequenceFlow id="f2" sourceRef="ebg" targetRef="c_t"/>
    <bpmn:sequenceFlow id="f3" sourceRef="ebg" targetRef="c_m"/>
    <bpmn:sequenceFlow id="f4" sourceRef="c_t" targetRef="e_t"/>
    <bpmn:sequenceFlow id="f5" sourceRef="c_m" targetRef="e_m"/>
  </bpmn:process>
</bpmn:definitions>"#;
    let bindings = Bindings::new().correlation("c_m", "k");
    engine.deploy(xml, &bindings).await.unwrap();
    let started = engine
        .start("pe", None, serde_json::json!({"k": "x"}))
        .await
        .unwrap();

    assert!(engine.fire_due_timer().await.unwrap());
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_eq!(subscription_rows(&db.pool, started.id).await, 0);
    // The losing subscription is gone: its message has nowhere to go now.
    let late = engine.correlate("Answer", "x", serde_json::json!({})).await;
    assert!(matches!(
        late,
        Err(rbpmn_engine::EngineError::NoSubscription { .. })
    ));
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Post-phase-3 review round: scheduler liveness, worker loops, boundary guards
// ---------------------------------------------------------------------------

/// The busy-spin regression (Postgres's GREATEST ignores NULLs): with no
/// live timers the scheduler must have nothing to sleep on — never a
/// zero-duration "next due". Frozen instances' timers must not count either:
/// the scheduler is not allowed to touch them, so sleeping on them spins.
#[tokio::test]
async fn idle_scheduler_has_nothing_due() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;

    // Empty database: nothing armed, nothing due.
    assert_eq!(engine.next_due_in().await.unwrap(), None);

    // A far-future timer: due roughly a day out, never clamped to zero.
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>P1D</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();
    let due_in = engine.next_due_in().await.unwrap().unwrap();
    assert!(due_in > Duration::from_secs(23 * 3600), "{due_in:?}");

    // Freeze the instance while its timer is armed (simulate an incident):
    // the overdue timer of a frozen instance must not drive the sleep.
    sqlx::query("update rbpmn_instance set status = 'failed'")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("update rbpmn_timer set due_at = now() - interval '1 hour'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(engine.next_due_in().await.unwrap(), None);
    assert!(!engine.fire_due_timer().await.unwrap());
    db.drop().await;
}

/// Head-of-line regression: one instance whose definition no longer loads
/// must not starve every other timer engine-wide.
#[tokio::test]
async fn poisoned_instance_cannot_starve_other_timers() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    // Two definitions; the poisoned one holds the *earliest* due timer.
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT0S</bpmn:timeDuration>").replace("pt", "pa"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT0S</bpmn:timeDuration>").replace("pt", "pb"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let poisoned = engine
        .start("pa", None, serde_json::json!({}))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await; // strictly earlier due_at
    let healthy = engine
        .start("pb", None, serde_json::json!({}))
        .await
        .unwrap();
    // Poison the instance's own rows (a ghost token): rehydration fails on
    // every load, and no compile cache can paper over corrupted state.
    sqlx::query("update rbpmn_token set element_id = 'ghost' where instance_id = $1")
        .bind(poisoned.id)
        .execute(&db.pool)
        .await
        .unwrap();

    // First pass: the poisoned candidate errors, goes on backoff, and the
    // healthy instance fires in the same call.
    assert!(engine.fire_due_timer().await.unwrap());
    wait_for_status(&db.pool, healthy.id, "completed").await;

    // Only the poisoned one remains, and it is backed off: no livelock —
    // and nothing to sleep on either (the backed-off instance's overdue
    // timer must not drive the scheduler's wait to zero: the busy-spin).
    assert!(!engine.fire_due_timer().await.unwrap());
    assert_eq!(engine.next_due_in().await.unwrap(), None);
    let status: String = sqlx::query("select status from rbpmn_instance where id = $1")
        .bind(poisoned.id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get("status");
    assert_eq!(status, "active");
    db.drop().await;
}

/// A persistent completion error must not re-run the handler in a hot loop:
/// the worker keeps its lease (the lease TTL *is* the backoff).
#[tokio::test]
async fn completion_error_keeps_the_lease_and_never_reruns_the_handler() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let invocations = Arc::new(AtomicUsize::new(0));
    let counter = invocations.clone();
    engine.register_handler(
        "st",
        Arc::new(FnHandler(move |_item| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({}))
        })),
    );
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:process id="pc" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:serviceTask id="st"/>
    <bpmn:endEvent id="end"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="st"/>
    <bpmn:sequenceFlow id="f2" sourceRef="st" targetRef="end"/>
  </bpmn:process>
</bpmn:definitions>"#;
    engine.deploy(xml, &Bindings::default()).await.unwrap();
    let started = engine
        .start("pc", None, serde_json::json!({}))
        .await
        .unwrap();
    // Poison the instance rows *after* start (a ghost token): the claim
    // still works, the handler runs, and then completion's rehydrate fails
    // persistently — no compile cache can paper over corrupted state.
    sqlx::query("update rbpmn_token set element_id = 'ghost' where instance_id = $1")
        .bind(started.id)
        .execute(&db.pool)
        .await
        .unwrap();

    let worker = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run_worker(worker_options()).await }
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    worker.abort();

    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "the handler must not re-run while the completion error persists"
    );
    let state: String = sqlx::query("select state from rbpmn_work_item where instance_id = $1")
        .bind(started.id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get("state");
    assert_eq!(state, "locked", "the lease is the backoff");
    db.drop().await;
}

/// A NUL byte in a handler's failure message is scrubbed, not allowed to
/// abort the fail transaction (which would loop the failure forever).
#[tokio::test]
async fn nul_in_failure_detail_is_scrubbed_not_wedged() {
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
    let (item, _) = open_items(&db.pool, started.id).await[0].clone();

    let outcome = engine
        .fail_work_item(
            item,
            &FailOptions {
                detail: Some("binary\u{0}garbage".to_string()),
                ..FailOptions::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(outcome, FailOutcome::Retrying { .. }));
    let last: String =
        sqlx::query("select last_failure from rbpmn_work_item where instance_id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .get("last_failure");
    assert_eq!(last, "binary\u{fffd}garbage");
    db.drop().await;
}

/// A non-object RFC 7386 patch would replace the whole variables document —
/// rejected at every boundary, including initial variables.
#[tokio::test]
async fn non_object_patches_are_rejected_everywhere() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bindings = Bindings::new().correlation("c", "order.id");
    engine
        .deploy(&fixture("accept/17-message-catch.bpmn"), &bindings)
        .await
        .unwrap();

    let bad_start = engine.start("p", None, serde_json::json!([1, 2])).await;
    assert!(matches!(
        bad_start,
        Err(rbpmn_engine::EngineError::InvalidVariables(_))
    ));

    let started = engine
        .start("p", None, serde_json::json!({"order": {"id": "o-1"}}))
        .await
        .unwrap();
    let bad_correlate = engine
        .correlate("WarehouseAck", "o-1", serde_json::json!(5))
        .await;
    assert!(matches!(
        bad_correlate,
        Err(rbpmn_engine::EngineError::InvalidVariables(_))
    ));
    // The subscription is untouched by the refused delivery, and a proper
    // object still goes through.
    assert_eq!(subscription_rows(&db.pool, started.id).await, 1);
    engine
        .correlate("WarehouseAck", "o-1", serde_json::json!({"ok": true}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    db.drop().await;
}

/// A frozen instance's leftover subscription must not block delivery to an
/// active instance sharing the correlation key — and must answer "no
/// subscription", not ambiguity, when it is the only holder.
#[tokio::test]
async fn frozen_instances_do_not_block_correlation() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:message id="m" name="Go"/>
  <bpmn:process id="pf" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:parallelGateway id="ps"/>
    <bpmn:intermediateCatchEvent id="c">
      <bpmn:messageEventDefinition messageRef="m"/>
    </bpmn:intermediateCatchEvent>
    <bpmn:serviceTask id="st"/>
    <bpmn:parallelGateway id="pj"/>
    <bpmn:endEvent id="end"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ps"/>
    <bpmn:sequenceFlow id="f2" sourceRef="ps" targetRef="c"/>
    <bpmn:sequenceFlow id="f3" sourceRef="ps" targetRef="st"/>
    <bpmn:sequenceFlow id="f4" sourceRef="c" targetRef="pj"/>
    <bpmn:sequenceFlow id="f5" sourceRef="st" targetRef="pj"/>
    <bpmn:sequenceFlow id="f6" sourceRef="pj" targetRef="end"/>
  </bpmn:process>
</bpmn:definitions>"#;
    engine.declare_topic("st").await.unwrap();
    let bindings = Bindings::new().correlation("c", "k");
    engine.deploy(xml, &bindings).await.unwrap();
    let vars = serde_json::json!({"k": "shared"});
    let frozen = engine.start("pf", None, vars.clone()).await.unwrap();
    let active = engine.start("pf", None, vars).await.unwrap();

    // Freeze the first instance via an unmatched error on its service task;
    // its subscription row stays (frozen for repair).
    let item = open_items(&db.pool, frozen.id)
        .await
        .into_iter()
        .find(|(_, el)| el == "st")
        .unwrap()
        .0;
    for _ in 0..3 {
        engine
            .fail_work_item(item, &FailOptions::default())
            .await
            .unwrap();
    }
    wait_for_status(&db.pool, frozen.id, "failed").await;
    assert_eq!(subscription_rows(&db.pool, frozen.id).await, 1);

    // Delivery reaches the active instance — no ambiguity from the corpse.
    let correlation = engine
        .correlate("Go", "shared", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(correlation.instance_id, active.id);

    // With only the frozen holder left: loud no-destination, not ambiguity
    // and not IncidentOpen.
    let miss = engine
        .correlate("Go", "shared", serde_json::json!({}))
        .await;
    assert!(matches!(
        miss,
        Err(rbpmn_engine::EngineError::NoSubscription { .. })
    ));
    db.drop().await;
}

/// The 1 MiB response cap is enforced while streaming: an oversized handler
/// response becomes a recorded failure, not an unbounded buffer.
#[tokio::test]
async fn oversized_handler_response_is_refused() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().route(
        "/work",
        axum::routing::post(|| async {
            // > 1 MiB of valid JSON.
            let huge = "x".repeat(2 * 1024 * 1024);
            axum::Json(serde_json::json!({ "blob": huge }))
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
    wait_for_status(&db.pool, started.id, "failed").await;
    worker.abort();

    let last: String =
        sqlx::query("select last_failure from rbpmn_work_item where instance_id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .get("last_failure");
    assert!(last.contains("too large"), "{last}");
    db.drop().await;
}

/// NUL bytes in text parameters are 400-class rejections, never a
/// transaction-poisoning database error.
#[tokio::test]
async fn nul_in_text_parameters_is_rejected_loudly() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bindings = Bindings::new().correlation("c", "order.id");
    engine
        .deploy(&fixture("accept/17-message-catch.bpmn"), &bindings)
        .await
        .unwrap();

    let bad_key = engine
        .correlate("WarehouseAck", "a\u{0}b", serde_json::json!({}))
        .await;
    assert!(matches!(
        bad_key,
        Err(rbpmn_engine::EngineError::InvalidVariables(_))
    ));
    let bad_name = engine
        .correlate("Ware\u{0}houseAck", "o-1", serde_json::json!({}))
        .await;
    assert!(matches!(
        bad_name,
        Err(rbpmn_engine::EngineError::InvalidVariables(_))
    ));
    let bad_bk = engine
        .start(
            "p",
            Some("bk\u{0}"),
            serde_json::json!({"order": {"id": "x"}}),
        )
        .await;
    assert!(matches!(
        bad_bk,
        Err(rbpmn_engine::EngineError::InvalidVariables(_))
    ));
    db.drop().await;
}

/// The inspection view exposes armed timers and open subscriptions — the
/// token overlay's data for phase-3 wait states.
#[tokio::test]
async fn inspection_shows_timers_and_subscriptions() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bindings = Bindings::new()
        .correlation("c_paid", "order.id")
        .correlation("c_cancel", "order.id");
    engine
        .deploy(&fixture("accept/11-event-based-gateway.bpmn"), &bindings)
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({"order": {"id": "o-i"}}))
        .await
        .unwrap();

    let view = engine.inspect_instance(started.id).await.unwrap();
    assert_eq!(view.timers.len(), 1);
    assert_eq!(view.timers[0].element_id, "c_late");
    assert_eq!(view.timers[0].due_spec, "P3D");
    // RFC 3339 UTC, e.g. "2026-08-14T09:00:00Z".
    assert!(
        view.timers[0].due_at.ends_with('Z'),
        "{}",
        view.timers[0].due_at
    );
    let subs: Vec<(String, String, String)> = view
        .subscriptions
        .iter()
        .map(|s| {
            (
                s.element_id.clone(),
                s.message_name.clone(),
                s.correlation_key.clone(),
            )
        })
        .collect();
    assert_eq!(
        subs,
        vec![
            ("c_paid".into(), "PaymentReceived".into(), "o-i".into()),
            ("c_cancel".into(), "OrderCancelled".into(), "o-i".into()),
        ]
    );
    db.drop().await;
}

/// The deployed wiring travels with the inspection. The runtime rows only
/// ever reveal a topic for work items that were actually instantiated, and
/// the manifest is deliberately absent from the XML — so without this the
/// wiring of everything the token has not reached yet is unrecoverable from
/// the view.
#[tokio::test]
async fn inspection_carries_the_bindings_manifest() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("payments").await.unwrap();
    let bindings = Bindings::new()
        .topic("st", "payments")
        .topic("ut", "review-queue")
        .correlation("rt", "order.id");
    engine
        .deploy(&fixture("accept/07-task-kinds.bpmn"), &bindings)
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({"order": {"id": "o-1"}}))
        .await
        .unwrap();

    let view = engine.inspect_instance(started.id).await.unwrap();
    assert_eq!(view.bindings, bindings);

    // Only the service task has been reached, so the runtime rows account for
    // exactly one element ...
    let instantiated: Vec<&str> = view
        .work_items
        .iter()
        .map(|w| w.element_id.as_str())
        .collect();
    assert_eq!(instantiated, vec!["st"]);
    // ... while the still-unreached user task's queue and the receive task's
    // correlation path exist nowhere else the view can see.
    assert_eq!(
        view.bindings.topics.get("ut").map(String::as_str),
        Some("review-queue")
    );
    assert_eq!(
        view.bindings.correlations.get("rt").map(String::as_str),
        Some("order.id")
    );
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Phase 4: the pull-mode task API
// ---------------------------------------------------------------------------

use rbpmn_engine::{GetTaskOptions, LockExtension, Released, TaskFilter, TaskOrder};

async fn three_review_instances(engine: &Engine) -> Vec<uuid::Uuid> {
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    let mut ids = Vec::new();
    for n in 0..3 {
        ids.push(
            engine
                .start("p", None, serde_json::json!({ "n": n }))
                .await
                .unwrap()
                .id,
        );
    }
    ids
}

#[tokio::test]
async fn tasks_are_fifo_by_default_lifo_on_request() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let ids = three_review_instances(&engine).await;

    let first = engine
        .get_task("review", &GetTaskOptions::new("w1"))
        .await
        .unwrap()
        .expect("a task");
    assert_eq!(first.instance_id, ids[0], "FIFO: oldest first");
    assert_eq!(first.element_id, "review");
    assert_eq!(first.variables, serde_json::json!({ "n": 0 }));

    let mut lifo = GetTaskOptions::new("w2");
    lifo.order = TaskOrder::Lifo;
    let last = engine
        .get_task("review", &lifo)
        .await
        .unwrap()
        .expect("a task");
    assert_eq!(last.instance_id, ids[2], "LIFO: freshest first");
    db.drop().await;
}

#[tokio::test]
async fn a_claimed_task_names_the_pinned_definition_not_the_latest() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = fixture("accept/01-minimal.bpmn");

    let v1 = engine.deploy(&xml, &Bindings::default()).await.unwrap();
    assert_eq!(v1.version, 1);
    // The instance pins v1 and never migrates.
    let instance = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    let changed = xml.replace("<bpmn:process", "<!-- v2 --><bpmn:process");
    let v2 = engine.deploy(&changed, &Bindings::default()).await.unwrap();
    assert_eq!(v2.version, 2, "max(version) for key 'p' is now 2");
    assert_ne!(v2.definition_id, v1.definition_id);

    let task = engine
        .get_task("review", &GetTaskOptions::new("w1"))
        .await
        .unwrap()
        .expect("a task");
    assert_eq!(task.instance_id, instance.id);
    assert_eq!(task.definition_key, "p");
    assert_eq!(
        (task.definition_id, task.definition_version),
        (v1.definition_id, 1),
        "the claim reports the pinned definition, not the latest version"
    );
    db.drop().await;
}

#[tokio::test]
async fn releasing_a_task_returns_it_to_the_queue_at_once() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let ids = three_review_instances(&engine).await;

    // A long lease: without the release, nothing else could have this task
    // for ten minutes.
    let task = engine
        .get_task("review", &GetTaskOptions::new("alice"))
        .await
        .unwrap()
        .expect("a task");
    assert_eq!(task.instance_id, ids[0]);

    // Held: a peer claiming FIFO gets the *next* instance, not this one.
    let peer = engine
        .get_task("review", &GetTaskOptions::new("bob"))
        .await
        .unwrap()
        .expect("a task");
    assert_eq!(peer.instance_id, ids[1], "alice's task is not on offer");

    assert_eq!(
        engine
            .release_task(task.id, "alice", task.lease_no)
            .await
            .unwrap(),
        Released::Released
    );
    // Immediately claimable again — by someone else, at the front of the
    // queue, with the lease gone rather than waited out.
    let reclaimed = engine
        .get_task("review", &GetTaskOptions::new("carol"))
        .await
        .unwrap()
        .expect("a task");
    assert_eq!(reclaimed.id, task.id, "FIFO: the released task is oldest");
    assert_eq!(reclaimed.instance_id, ids[0]);

    // The lease is carol's now: alice's stale release cannot take it from
    // her — on either half of the guard — and it stays claimed.
    assert_ne!(reclaimed.lease_no, task.lease_no, "a claim mints an epoch");
    assert_eq!(
        engine
            .release_task(task.id, "alice", task.lease_no)
            .await
            .unwrap(),
        Released::Lost {
            state: "locked".into()
        }
    );
    let (state, owner): (String, Option<String>) =
        sqlx::query_as("select state, lock_owner from rbpmn_work_item where id = $1")
            .bind(task.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        (state.as_str(), owner.as_deref()),
        ("locked", Some("carol"))
    );

    // A stranger cannot hand back what bob is holding, even holding bob's
    // epoch — the owner check, from the other side (spec/Lease.tla,
    // LiveLeaseEndsOnlyByItsHolderOrTheProcess: the clock, the holder or the
    // process end a live lease, and no other worker).
    assert_eq!(
        engine
            .release_task(peer.id, "not-bob", peer.lease_no)
            .await
            .unwrap(),
        Released::Lost {
            state: "locked".into()
        }
    );
    // Bob's own release lands, and a task nobody holds is the quiet no-op.
    assert_eq!(
        engine
            .release_task(peer.id, "bob", peer.lease_no)
            .await
            .unwrap(),
        Released::Released
    );
    assert_eq!(
        engine
            .release_task(peer.id, "bob", peer.lease_no)
            .await
            .unwrap(),
        Released::Lost {
            state: "available".into()
        },
        "a second release has nothing left to hand back — and says what the \
         item is now, which is back on the queue"
    );
    db.drop().await;
}

#[tokio::test]
async fn leases_expire_and_tasks_return() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    let mut short = GetTaskOptions::new("w1");
    short.ttl = Duration::from_millis(300);
    let task = engine.get_task("review", &short).await.unwrap().unwrap();

    // Live lease: nothing for a second consumer.
    assert!(
        engine
            .get_task("review", &GetTaskOptions::new("w2"))
            .await
            .unwrap()
            .is_none()
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    // Expired: the same task is claimable again — no reaper involved.
    let reclaimed = engine
        .get_task("review", &GetTaskOptions::new("w2"))
        .await
        .unwrap()
        .expect("expired lease is claimable");
    assert_eq!(reclaimed.id, task.id);
    db.drop().await;
}

/// The bug the lease epoch exists for, driven exactly as it would happen:
/// a release whose response was lost, retried after the same owner has
/// claimed the same task again. Owner alone cannot tell those two claims
/// apart, and FIFO makes the second one *likely* rather than exotic — the
/// item just released is the oldest, so it is what the next claim returns.
#[tokio::test]
async fn a_replayed_release_cannot_free_the_claim_that_replaced_it() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    let first = engine
        .get_task("review", &GetTaskOptions::new("alice"))
        .await
        .unwrap()
        .expect("a task");
    // Committed, but the response never reached the client.
    assert_eq!(
        engine
            .release_task(first.id, "alice", first.lease_no)
            .await
            .unwrap(),
        Released::Released
    );

    // Alice's next claim hands back the very task she released.
    let again = engine
        .get_task("review", &GetTaskOptions::new("alice"))
        .await
        .unwrap()
        .expect("a task");
    assert_eq!(again.id, first.id);
    assert_eq!(again.lease_no, first.lease_no + 1, "a claim mints an epoch");

    // The retry lands. It names a spent epoch, so it does nothing — and
    // says so, rather than reporting a success that undid a live claim.
    assert_eq!(
        engine
            .release_task(first.id, "alice", first.lease_no)
            .await
            .unwrap(),
        Released::Lost {
            state: "locked".into()
        }
    );
    let (state, owner): (String, Option<String>) =
        sqlx::query_as("select state, lock_owner from rbpmn_work_item where id = $1")
            .bind(first.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        (state.as_str(), owner.as_deref()),
        ("locked", Some("alice")),
        "the live claim survives its own predecessor's retry"
    );
    // Nobody else can take it while alice holds it.
    assert!(
        engine
            .get_task("review", &GetTaskOptions::new("bob"))
            .await
            .unwrap()
            .is_none(),
        "a freed task here would be the double delivery the lease prevents"
    );

    // The current epoch still releases, so nothing was wedged by refusing.
    assert_eq!(
        engine
            .release_task(again.id, "alice", again.lease_no)
            .await
            .unwrap(),
        Released::Released
    );
    db.drop().await;
}

/// The one place `release_task` deliberately parts company with
/// `extend_lock`: its guard carries no liveness clause. An expired lease
/// nobody reclaimed still names its owner, and releasing it tidies the stale
/// `lock_owner` off a row that was already claimable. Harmonising the two
/// statements would look like a cleanup and would silently delete this.
#[tokio::test]
async fn an_expired_lease_is_still_its_owners_to_release() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    let mut short = GetTaskOptions::new("w1");
    short.ttl = Duration::from_millis(300);
    let task = engine.get_task("review", &short).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The heartbeat is gone — extend_lock requires `lock_until > now()`.
    assert_eq!(
        engine
            .extend_lock(task.id, "w1", Duration::from_secs(60))
            .await
            .unwrap(),
        // Expired, not withdrawn: the row is still `locked` with a stale
        // owner, which is exactly the "reassigned" story `state` exists to
        // tell apart from `cancelled`.
        LockExtension::Lost {
            state: "locked".into()
        }
    );
    // The release is not: same owner, still locked, so the row is ours to
    // hand back — and it comes back clean rather than lapsed.
    assert_eq!(
        engine
            .release_task(task.id, "w1", task.lease_no)
            .await
            .unwrap(),
        Released::Released
    );
    let (state, owner, deadline_cleared): (String, Option<String>, bool) = sqlx::query_as(
        "select state, lock_owner, lock_until is null from rbpmn_work_item where id = $1",
    )
    .bind(task.id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        (state.as_str(), owner.as_deref(), deadline_cleared),
        ("available", None, true),
        "no lapsed lease left behind"
    );

    // Once a peer has taken it, the same expired claim is no longer ours.
    let reclaimed = engine
        .get_task("review", &GetTaskOptions::new("w2"))
        .await
        .unwrap()
        .expect("released task is claimable");
    assert_eq!(reclaimed.id, task.id);
    assert_eq!(
        engine
            .release_task(task.id, "w1", task.lease_no)
            .await
            .unwrap(),
        Released::Lost {
            state: "locked".into()
        },
        "ownership is the guard, and it moved"
    );
    db.drop().await;
}

#[tokio::test]
async fn extend_lock_heartbeats_and_reports_loss() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    let task = engine
        .get_task("review", &GetTaskOptions::new("w1"))
        .await
        .unwrap()
        .unwrap();

    let extended = engine
        .extend_lock(task.id, "w1", Duration::from_secs(600))
        .await
        .unwrap();
    assert!(matches!(extended, LockExtension::Extended { .. }));

    // Wrong owner: typed loss, not silence.
    assert_eq!(
        engine
            .extend_lock(task.id, "somebody-else", Duration::from_secs(600))
            .await
            .unwrap(),
        LockExtension::Lost {
            state: "locked".into()
        }
    );

    engine
        .complete_task(task.id, "w1", serde_json::json!({}))
        .await
        .unwrap();
    // Closed task: the heartbeat reports loss too, and names the state so
    // the frontend can say "already done" rather than "reassigned".
    assert_eq!(
        engine
            .extend_lock(task.id, "w1", Duration::from_secs(600))
            .await
            .unwrap(),
        LockExtension::Lost {
            state: "completed".into()
        }
    );
    // An id that names no task at all is a 404's worth of error, not a
    // loss — the same answer complete_task and fail_task give.
    assert!(matches!(
        engine
            .extend_lock(uuid::Uuid::new_v4(), "w1", Duration::from_secs(600))
            .await,
        Err(rbpmn_engine::EngineError::UnknownWorkItem(_))
    ));
    assert!(matches!(
        engine.release_task(uuid::Uuid::new_v4(), "w1", 1).await,
        Err(rbpmn_engine::EngineError::UnknownWorkItem(_))
    ));
    db.drop().await;
}

#[tokio::test]
async fn completing_a_task_requires_its_live_owner() {
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
    let task = engine
        .get_task("review", &GetTaskOptions::new("w1"))
        .await
        .unwrap()
        .unwrap();

    let stolen = engine
        .complete_task(task.id, "w2", serde_json::json!({}))
        .await;
    assert!(matches!(
        stolen,
        Err(rbpmn_engine::EngineError::ItemLeased(_))
    ));

    let done = engine
        .complete_task(task.id, "w1", serde_json::json!({ "ok": true }))
        .await
        .unwrap();
    assert!(matches!(done, Completion::Advanced(_)));
    wait_for_status(&db.pool, started.id, "completed").await;

    // Retried completion converges on the idempotent no-op, same contract
    // as the push path.
    let again = engine
        .complete_task(task.id, "w1", serde_json::json!({}))
        .await
        .unwrap();
    assert!(matches!(again, Completion::AlreadyClosed { .. }));
    db.drop().await;
}

#[tokio::test]
async fn filters_match_live_instance_variables() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    let north = engine
        .start("p", None, serde_json::json!({ "region": "north" }))
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({ "region": "south" }))
        .await
        .unwrap();

    assert_eq!(engine.count_tasks("review", None).await.unwrap(), 2);
    let filter = TaskFilter::new("p").field("region", "north");
    assert_eq!(
        engine.count_tasks("review", Some(&filter)).await.unwrap(),
        1
    );

    let mut options = GetTaskOptions::new("w1");
    options.filter = Some(filter);
    let task = engine.get_task("review", &options).await.unwrap().unwrap();
    assert_eq!(task.instance_id, north.id);
    // The other region's task is invisible through this filter.
    assert!(engine.get_task("review", &options).await.unwrap().is_none());

    // Injection-shaped field names are rejected loudly, never embedded.
    let evil = TaskFilter::new("p").field("x') or ('1'='1", "y");
    let mut options = GetTaskOptions::new("w1");
    options.filter = Some(evil);
    assert!(matches!(
        engine.get_task("review", &options).await,
        Err(rbpmn_engine::EngineError::InvalidVariables(_))
    ));
    db.drop().await;
}

/// The design-mandated index test: a declared field's filter/count queries
/// actually use the generated partial index (asserted via
/// pg_stat_user_indexes against the *real* query, so an expression-shape
/// drift in the filter compiler cannot silently pass), while an undeclared
/// field stays correct without one.
#[tokio::test]
async fn declared_indexes_serve_the_filter_queries() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    for i in 0..300 {
        engine
            .start(
                "p",
                None,
                serde_json::json!({ "region": format!("r{i}"), "shade": format!("s{i}") }),
            )
            .await
            .unwrap();
    }
    engine.declare_index("p", "region").await.unwrap();
    engine.declare_index("p", "region").await.unwrap(); // idempotent
    sqlx::query("analyze rbpmn_instance")
        .execute(&db.pool)
        .await
        .unwrap();

    // Undeclared field: correct via sequential scan.
    let undeclared = TaskFilter::new("p").field("shade", "s7");
    assert_eq!(
        engine
            .count_tasks("review", Some(&undeclared))
            .await
            .unwrap(),
        1
    );

    // Declared field: correct AND index-served.
    let declared = TaskFilter::new("p").field("region", "r250");
    assert_eq!(
        engine.count_tasks("review", Some(&declared)).await.unwrap(),
        1
    );
    let mut options = GetTaskOptions::new("w1");
    options.filter = Some(declared);
    assert!(engine.get_task("review", &options).await.unwrap().is_some());

    let mut scans = 0i64;
    for _ in 0..20 {
        scans = sqlx::query_scalar::<_, Option<i64>>(
            "select idx_scan::bigint from pg_stat_user_indexes where indexrelname = $1",
        )
        .bind(rbpmn_engine::declared_index_name("p", "region"))
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .unwrap_or(0);
        if scans > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await; // stats lag
    }
    assert!(
        scans > 0,
        "the declared index was never used by the filter queries"
    );
    db.drop().await;
}

#[tokio::test]
async fn manifest_index_declarations_apply_at_deploy() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;

    // A bad field name refuses the whole deploy before anything persists.
    let bad = engine
        .deploy(
            &fixture("accept/01-minimal.bpmn"),
            &Bindings::new().index("no'good"),
        )
        .await;
    assert!(matches!(
        bad,
        Err(rbpmn_engine::DeployError::InvalidManifest(_))
    ));
    let defs: i64 = sqlx::query("select count(*) from rbpmn_definition")
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get(0);
    assert_eq!(defs, 0, "a rejected manifest must not deploy");

    engine
        .deploy(
            &fixture("accept/01-minimal.bpmn"),
            &Bindings::new().index("region"),
        )
        .await
        .unwrap();
    let exists: bool =
        sqlx::query_scalar("select exists (select 1 from pg_class where relname = $1)")
            .bind(rbpmn_engine::declared_index_name("p", "region"))
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        exists,
        "the manifest's declared index must exist after deploy"
    );

    // The collision pair: ("a.b","c") and ("a","b_c") flatten identically
    // but must get distinct indexes.
    assert_ne!(
        rbpmn_engine::declared_index_name("a.b", "c"),
        rbpmn_engine::declared_index_name("a", "b_c")
    );
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Environment: undeclaring topics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn undeclare_topic_is_protected_by_active_definitions() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("payments").await.unwrap();
    engine.declare_topic("unused").await.unwrap();
    engine
        .deploy(
            &fixture("accept/16-foreign-binding-warn.bpmn"),
            &Bindings::new().topic("st", "payments"),
        )
        .await
        .unwrap();

    // Needed by the latest version of 'p': refused, with the culprit named.
    let refused = engine.undeclare_topic("payments").await;
    match refused {
        Err(rbpmn_engine::EngineError::TopicInUse { definitions, .. }) => {
            assert!(
                definitions.iter().any(|d| d.starts_with("p v1")),
                "{definitions:?}"
            );
        }
        other => panic!("expected TopicInUse, got {other:?}"),
    }
    // Still declared: a redeploy binding it keeps working.
    engine
        .deploy(
            &fixture("accept/16-foreign-binding-warn.bpmn"),
            &Bindings::new().topic("st", "payments"),
        )
        .await
        .unwrap();

    // Unused: undeclares (idempotently), and a deploy binding it now fails.
    engine.undeclare_topic("unused").await.unwrap();
    engine.undeclare_topic("unused").await.unwrap(); // absent = no-op
    let gone = engine
        .deploy(
            &fixture("accept/16-foreign-binding-warn.bpmn"),
            &Bindings::new().topic("st", "unused"),
        )
        .await;
    match gone {
        Err(DeployError::Rejected(diags)) => {
            assert!(diags.iter().any(|d| d.rule == "unresolved-topic"));
        }
        other => panic!("expected unresolved-topic rejection, got {other:?}"),
    }
    db.drop().await;
}

/// A superseded definition version with active instances still protects its
/// topics — instances pin the version that needs them.
#[tokio::test]
async fn undeclare_protects_versions_with_active_instances() {
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

    // Supersede with a version bound to a different topic.
    engine.declare_topic("payments-v2").await.unwrap();
    let v2 = fixture("accept/16-foreign-binding-warn.bpmn")
        .replace("<bpmn:process", "<!-- v2 --><bpmn:process");
    engine
        .deploy(&v2, &Bindings::new().topic("st", "payments-v2"))
        .await
        .unwrap();

    // The running v1 instance still needs 'payments': refused.
    assert!(matches!(
        engine.undeclare_topic("payments").await,
        Err(rbpmn_engine::EngineError::TopicInUse { .. })
    ));

    // Finish the instance; with only quiescent history left, undeclare works.
    let (item, _) = open_items(&db.pool, started.id).await[0].clone();
    engine
        .complete_work_item(item, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    engine.undeclare_topic("payments").await.unwrap();
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Phase 5: the event-stream tailing contract
// ---------------------------------------------------------------------------

/// The safe horizon under out-of-order commits — including the txid
/// inversion: a business transaction takes its xid at its FIRST write, so
/// an `*_in_tx` caller can hold an OLD txid while inserting LATE, high-id
/// events. An id-only cursor provably loses events here; the (txid, id)
/// cursor must not.
#[tokio::test]
async fn event_stream_never_misses_out_of_order_commits() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    sqlx::query("create table audit_marks (n int)")
        .execute(&db.pool)
        .await
        .unwrap();
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    assert!(
        engine
            .read_events(rbpmn_engine::EventCursor::default(), 100)
            .await
            .unwrap()
            .is_empty()
    );

    // The inversion: tx H writes a business row FIRST (assigning it an old
    // xid), stays open...
    let mut held = db.pool.begin().await.unwrap();
    sqlx::query("insert into audit_marks values (1)")
        .execute(&mut *held)
        .await
        .unwrap();

    // ...while a younger transaction (higher xid) inserts LOWER event ids
    // and commits immediately.
    let young = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    // Now H (old xid) inserts HIGHER event ids and stays open.
    let old_tx = engine
        .start_in_tx(&mut held, "p", None, serde_json::json!({}))
        .await
        .unwrap();

    // Nothing may be released: H (the oldest in-progress tx) pins the
    // horizon below BOTH transactions' rows. An id-only horizon would have
    // released young's rows here — and later H's lower-txid rows would sort
    // before a cursor that already passed them.
    assert!(
        engine
            .read_events(rbpmn_engine::EventCursor::default(), 100)
            .await
            .unwrap()
            .is_empty(),
        "events past an in-flight transaction must not be released"
    );

    // Commit H; poll until the horizon releases both (it is CLUSTER-wide,
    // so concurrent tests' transactions can briefly hold it back).
    held.commit().await.unwrap();
    let mut all = Vec::new();
    for _ in 0..100 {
        all = engine
            .read_events(rbpmn_engine::EventCursor::default(), 100)
            .await
            .unwrap();
        let complete = all.iter().any(|e| e.instance_id == young.id)
            && all.iter().any(|e| e.instance_id == old_tx.id);
        if complete {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !all.is_empty(),
        "horizon never released the committed events"
    );

    // Stream order is (txid, id): H's events (older txid, HIGHER ids) come
    // first — the id-only ordering would have lost them.
    let cursors: Vec<(i64, i64)> = all.iter().map(|e| (e.txid, e.id)).collect();
    let mut sorted = cursors.clone();
    sorted.sort();
    assert_eq!(cursors, sorted, "stream is in (txid, id) order");
    assert_eq!(
        all.first().unwrap().instance_id,
        old_tx.id,
        "the old-txid transaction's events sort first despite higher ids"
    );
    // Per instance, the stream preserves semantic (id) order.
    for inst in [young.id, old_tx.id] {
        let ids: Vec<i64> = all
            .iter()
            .filter(|e| e.instance_id == inst)
            .map(|e| e.id)
            .collect();
        let mut s = ids.clone();
        s.sort();
        assert_eq!(ids, s, "per-instance id order broken for {inst}");
    }

    // Cursor walk: paging with limit 3 yields exactly the same stream,
    // no misses, no duplicates.
    let mut walked = Vec::new();
    let mut cursor = rbpmn_engine::EventCursor::default();
    loop {
        let page = engine.read_events(cursor, 3).await.unwrap();
        if page.is_empty() {
            break;
        }
        cursor = page.last().unwrap().cursor();
        walked.extend(page.into_iter().map(|e| (e.txid, e.id)));
    }
    assert_eq!(walked, cursors, "paged walk must equal the full stream");
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Post-phase-5 review round
// ---------------------------------------------------------------------------

/// A panicking handler must not kill the worker loop: the panic becomes a
/// recorded failure and the loop keeps serving other work. Before this, the
/// spawned task died silently while the process kept answering HTTP.
#[tokio::test]
async fn panicking_handler_is_contained_as_a_failure() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:process id="pp" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:serviceTask id="boom"/>
    <bpmn:endEvent id="end"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="boom"/>
    <bpmn:sequenceFlow id="f2" sourceRef="boom" targetRef="end"/>
  </bpmn:process>
</bpmn:definitions>"#;
    engine.register_handler(
        "boom",
        Arc::new(FnHandler(
            |_item| -> Result<serde_json::Value, HandlerFailure> {
                panic!("handler exploded");
            },
        )),
    );
    engine.deploy(xml, &Bindings::default()).await.unwrap();

    let worker = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run_worker(worker_options()).await }
    });
    let started = engine
        .start("pp", None, serde_json::json!({}))
        .await
        .unwrap();

    // Retries are exhausted by repeated panics and the instance freezes —
    // the loop survived every one of them.
    wait_for_status(&db.pool, started.id, "failed").await;
    let last: String =
        sqlx::query("select last_failure from rbpmn_work_item where instance_id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap()
            .get("last_failure");
    assert!(last.contains("panicked"), "{last}");
    assert!(last.contains("handler exploded"), "{last}");

    // And the same worker still processes a fresh instance of other work.
    let done = Arc::new(AtomicUsize::new(0));
    let counter = done.clone();
    engine.register_handler(
        "boom",
        Arc::new(FnHandler(move |_item| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({}))
        })),
    );
    let second = engine
        .start("pp", None, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, second.id, "completed").await;
    assert_eq!(done.load(Ordering::SeqCst), 1);
    worker.abort();
    db.drop().await;
}

/// A repeatedly-failing timer records a `timer-fire-failed` event: the
/// in-process backoff must never be the only trace ("do NOT silently
/// swallow").
#[tokio::test]
async fn timer_firing_failures_are_recorded_not_swallowed() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT0S</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();
    // Poison rehydration so every firing attempt errors.
    sqlx::query("update rbpmn_token set element_id = 'ghost' where instance_id = $1")
        .bind(started.id)
        .execute(&db.pool)
        .await
        .unwrap();

    assert!(!engine.fire_due_timer().await.unwrap());
    assert_eq!(
        event_count(&db.pool, started.id, "timer-fire-failed").await,
        1,
        "the backoff must leave a persisted trace"
    );
    db.drop().await;
}

/// The scheduler must not park its whole drain behind an `*_in_tx` caller
/// holding an instance row lock: NOWAIT lets it fire other instances' due
/// timers immediately.
#[tokio::test]
async fn scheduler_skips_instances_locked_by_a_caller() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT0S</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let blocked = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await; // strictly later due_at
    let free = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();

    // Hold the first instance's row lock, as a business transaction would.
    let mut held = db.pool.begin().await.unwrap();
    sqlx::query("select id from rbpmn_instance where id = $1 for update")
        .bind(blocked.id)
        .fetch_one(&mut *held)
        .await
        .unwrap();

    // The blocked instance owns the EARLIEST due timer, so a blocking load
    // would stall here; NOWAIT moves on and fires the free one.
    let fired = tokio::time::timeout(Duration::from_secs(5), engine.fire_due_timer())
        .await
        .expect("scheduler must not block on a caller-held lock")
        .unwrap();
    assert!(fired);
    wait_for_status(&db.pool, free.id, "completed").await;

    // Once the caller commits, the skipped timer fires — after its short
    // transient deferral expires (the skip is remembered on purpose, so the
    // next pass looks at other instances first).
    held.commit().await.unwrap();
    let mut fired = false;
    for _ in 0..50 {
        if engine.fire_due_timer().await.unwrap() {
            fired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        fired,
        "the deferred timer never fired after the lock released"
    );
    wait_for_status(&db.pool, blocked.id, "completed").await;
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Second post-phase-5 review round
// ---------------------------------------------------------------------------

async fn transaction_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "select xact_commit + xact_rollback from pg_stat_database \
         where datname = current_database()",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The liveness invariant, measured rather than argued: while a caller holds
/// an instance's row lock, the scheduler must NOT hot-spin. Every previous
/// incarnation of this bug produced thousands of transactions per second;
/// the drain now reports "deferred" and the loop sleeps on that.
#[tokio::test]
async fn scheduler_does_not_spin_while_a_caller_holds_a_lock() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT0S</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let held_instance = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();

    // A business transaction holds the row of the ONLY instance with a due
    // timer — the exact shape that used to spin.
    let mut held = db.pool.begin().await.unwrap();
    sqlx::query("select id from rbpmn_instance where id = $1 for update")
        .bind(held_instance.id)
        .fetch_one(&mut *held)
        .await
        .unwrap();

    let before = transaction_count(&db.pool).await;
    let scheduler = tokio::spawn({
        let engine = engine.clone();
        async move {
            engine
                .run_scheduler(rbpmn_engine::SchedulerOptions::default())
                .await
        }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    scheduler.abort();
    let spent = transaction_count(&db.pool).await - before;
    assert!(
        spent < 100,
        "scheduler burned {spent} transactions in 1s against a locked instance — spinning"
    );

    // And it is not merely asleep: releasing the lock lets the timer fire.
    held.commit().await.unwrap();
    for _ in 0..50 {
        if engine.fire_due_timer().await.unwrap() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    wait_for_status(&db.pool, held_instance.id, "completed").await;
    db.drop().await;
}

/// Per-instance semantic order is `id`, and the stream's (txid, id) order is
/// a DIFFERENT key: an `*_in_tx` caller that took its xid before locking the
/// instance delivers its later events earlier in the stream. Both halves of
/// that contract are asserted here — on a single instance, which the
/// cross-instance test could not see.
#[tokio::test]
async fn per_instance_order_is_id_not_stream_position() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    sqlx::query("create table audit_marks (n int)")
        .execute(&db.pool)
        .await
        .unwrap();
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
    let items = open_items(&db.pool, started.id).await;
    let ta = items.iter().find(|(_, el)| el == "ta").unwrap().0;
    let tb = items.iter().find(|(_, el)| el == "tb").unwrap().0;

    // H takes its xid FIRST, before touching the instance at all.
    let mut held = db.pool.begin().await.unwrap();
    sqlx::query("insert into audit_marks values (1)")
        .execute(&mut *held)
        .await
        .unwrap();

    // A younger autocommit transaction steps the instance first (higher
    // txid, lower ids)...
    engine
        .complete_work_item(ta, serde_json::json!({}))
        .await
        .unwrap();
    // ...then H steps it (lower txid, higher ids) and commits.
    engine
        .complete_work_item_in_tx(&mut held, tb, None, serde_json::json!({}))
        .await
        .unwrap();
    held.commit().await.unwrap();

    // Wait for every event this test asserts on, not merely the first to
    // surface — which is the trap this very test documents, sprung on the
    // test itself. H holds the *older* txid, so its events (the join and
    // `instance-completed`) cross the safe horizon before ta's younger
    // autocommit events do. Breaking the loop on `instance-completed` could
    // therefore leave ta's completion still above the horizon: `mine` is
    // non-empty, the emptiness check passes, and the lookup below fails with
    // "no work-item-completed ta". Rare when the machine is quiet, reliable
    // under a loaded cluster, because the horizon is cluster-wide and any
    // concurrent transaction holds it back.
    let mut all = Vec::new();
    for _ in 0..100 {
        all = engine
            .read_events(rbpmn_engine::EventCursor::default(), 200)
            .await
            .unwrap();
        let mine: Vec<_> = all.iter().filter(|e| e.instance_id == started.id).collect();
        let complete = mine.iter().any(|e| e.kind == "instance-completed")
            && ["ta", "tb"].iter().all(|element| {
                mine.iter().any(|e| {
                    e.kind == "work-item-completed" && e.element_id.as_deref() == Some(*element)
                })
            });
        if complete {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mine: Vec<_> = all.iter().filter(|e| e.instance_id == started.id).collect();
    assert!(
        mine.len() >= 3,
        "horizon never released this instance's events: {mine:?}"
    );

    // Guarantee: ids ascend in semantic order — ta's completion precedes
    // tb's, which precedes the join and the instance completing.
    let id_of = |kind: &str, element: Option<&str>| -> i64 {
        mine.iter()
            .find(|e| e.kind == kind && e.element_id.as_deref() == element)
            .unwrap_or_else(|| panic!("no {kind} {element:?} in {mine:?}"))
            .id
    };
    assert!(id_of("work-item-completed", Some("ta")) < id_of("work-item-completed", Some("tb")));
    assert!(id_of("work-item-completed", Some("tb")) < id_of("instance-completed", None));

    // Caveat: stream position is NOT that order — H's later events carry the
    // lower txid and therefore arrive first.
    let stream_pos = |kind: &str, element: Option<&str>| -> usize {
        mine.iter()
            .position(|e| e.kind == kind && e.element_id.as_deref() == element)
            .unwrap()
    };
    assert!(
        stream_pos("work-item-completed", Some("tb"))
            < stream_pos("work-item-completed", Some("ta")),
        "the documented caveat did not reproduce; the test no longer proves it"
    );
    db.drop().await;
}

/// Aborting a worker cancels its in-flight handler: `tokio::spawn` detaches
/// by default, which would leave the handler running (and re-firing side
/// effects) with nobody left to record its result.
#[tokio::test]
async fn aborting_a_worker_cancels_the_running_handler() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;

    struct SlowHandler {
        started: Arc<AtomicUsize>,
        finished: Arc<AtomicUsize>,
    }
    impl ServiceTaskHandler for SlowHandler {
        fn execute(
            &self,
            _item: WorkItem,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, HandlerFailure>> + Send + '_>>
        {
            self.started.fetch_add(1, Ordering::SeqCst);
            let finished = self.finished.clone();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                finished.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({}))
            })
        }
    }
    let started_count = Arc::new(AtomicUsize::new(0));
    let finished_count = Arc::new(AtomicUsize::new(0));
    engine.register_handler(
        "payments",
        Arc::new(SlowHandler {
            started: started_count.clone(),
            finished: finished_count.clone(),
        }),
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
    engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    // Let it claim and enter the handler, then pull the rug.
    for _ in 0..50 {
        if started_count.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(started_count.load(Ordering::SeqCst), 1, "handler never ran");
    worker.abort();

    // Well past the handler's own duration: it must never have finished.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        finished_count.load(Ordering::SeqCst),
        0,
        "the handler kept running after its worker was aborted"
    );
    db.drop().await;
}

/// Repeated timer failures escalate their backoff and stop being persisted
/// after a handful — one stuck instance must not flood the event stream
/// forever.
#[tokio::test]
async fn repeated_timer_failures_are_bounded_in_the_stream() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT0S</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();
    sqlx::query("update rbpmn_token set element_id = 'ghost' where instance_id = $1")
        .bind(started.id)
        .execute(&db.pool)
        .await
        .unwrap();

    // Drive many failure cycles, clearing the deferral each time so the
    // instance is re-attempted immediately (the escalating backoff would
    // otherwise make this test minutes long).
    for _ in 0..12 {
        assert!(!engine.fire_due_timer().await.unwrap());
        engine.expire_deferral_for_test(started.id);
    }
    let recorded = event_count(&db.pool, started.id, "timer-fire-failed").await;
    assert!(
        (1..=5).contains(&recorded),
        "expected the first few failures only, got {recorded}"
    );
    db.drop().await;
}

#[tokio::test]
async fn event_cursor_inputs_are_validated() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bad_cursor = rbpmn_engine::EventCursor { txid: -1, id: 0 };
    assert!(matches!(
        engine.read_events(bad_cursor, 10).await,
        Err(rbpmn_engine::EngineError::InvalidVariables(_))
    ));
    assert!(matches!(
        engine
            .read_events(rbpmn_engine::EventCursor::default(), 0)
            .await,
        Err(rbpmn_engine::EngineError::InvalidVariables(_))
    ));
    db.drop().await;
}

/// The at-least-once contract, demonstrated rather than asserted.
///
/// A lease is renewed by an UPDATE on the work-item row, so anything holding
/// that row longer than the remaining lease starves the heartbeat: the lease
/// lapses and a peer re-runs the handler. That is *allowed* — delivery is
/// at-least-once and handlers must be idempotent. What must not happen is a
/// second **state transition**.
///
/// Blocking the row on purpose makes the race deterministic, which is also the
/// mechanism behind occasional flakes in
/// `leases_renew_while_the_handler_runs`: that test asserts renewal keeps the
/// handler to one invocation, which holds only while the renewal is not
/// starved. Under heavy load it can be, and the engine is still correct.
#[tokio::test]
async fn a_starved_renewal_reruns_the_handler_but_applies_once() {
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

    let opts = |name: &str| WorkerOptions {
        owner: name.to_string(),
        lease: Duration::from_millis(900),
        poll_interval: Duration::from_millis(150),
    };
    let w1 = tokio::spawn({
        let (engine, o) = (engine.clone(), opts("w1"));
        async move { engine.run_worker(o).await }
    });
    let w2 = tokio::spawn({
        let (engine, o) = (engine.clone(), opts("w2"));
        async move { engine.run_worker(o).await }
    });

    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    // Wait for the claim, then hold the row past the lease so every renewal
    // attempt in that window queues behind this transaction.
    let mut item = None;
    for _ in 0..200 {
        let row = sqlx::query("select id from rbpmn_work_item where state = 'locked' limit 1")
            .fetch_optional(&db.pool)
            .await
            .unwrap();
        if let Some(row) = row {
            item = Some(row.get::<uuid::Uuid, _>("id"));
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let item = item.expect("the work item was never claimed");

    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("select 1 from rbpmn_work_item where id = $1 for update")
        .bind(item)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1400)).await;
    tx.rollback().await.unwrap();

    wait_for_status(&db.pool, started.id, "completed").await;
    w1.abort();
    w2.abort();

    let invoked = invocations.load(Ordering::SeqCst);
    assert!(
        invoked >= 2,
        "the lease was never actually lost — this test is not exercising \
         re-delivery (handler ran {invoked} time(s))"
    );
    assert_eq!(
        event_count(&db.pool, started.id, "work-item-completed").await,
        1,
        "the handler ran {invoked} times; exactly-once state transition means \
         the engine must still have applied exactly one completion"
    );
    assert_eq!(
        event_count(&db.pool, started.id, "variables-patched").await,
        1,
        "the merge patch was applied more than once"
    );
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Phase 6: embedded subprocesses
// ---------------------------------------------------------------------------

async fn scope_rows(pool: &PgPool, instance: uuid::Uuid) -> Vec<(i64, i64, String)> {
    sqlx::query(
        "select scope_no, parent_scope_no, element_id from rbpmn_scope \
         where instance_id = $1 order by scope_no",
    )
    .bind(instance)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| {
        (
            r.get::<i64, _>("scope_no"),
            r.get::<i64, _>("parent_scope_no"),
            r.get::<String, _>("element_id"),
        )
    })
    .collect()
}

/// Nested scopes survive the round-trip through Postgres: every step
/// rehydrates the scope tree from rows, so a two-level subprocess completes
/// across three separate transactions.
#[tokio::test]
async fn nested_subprocess_scopes_rehydrate() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/21-nested-subprocess.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    // Two open scopes: outer (root's child) and inner (outer's child).
    let scopes = scope_rows(&db.pool, started.id).await;
    assert_eq!(scopes.len(), 2, "{scopes:?}");
    assert_eq!(scopes[0], (1, 0, "outer".to_string()));
    assert_eq!(scopes[1], (2, 1, "inner".to_string()));
    // The waiting work item lives in the inner scope.
    let token_scope: i64 = sqlx::query(
        "select t.scope_no from rbpmn_token t where t.instance_id = $1 and t.element_id = 'count'",
    )
    .bind(started.id)
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .get("scope_no");
    assert_eq!(token_scope, 2);

    // Completing the inner task closes the inner scope only.
    let (count_item, _) = open_items(&db.pool, started.id)
        .await
        .into_iter()
        .find(|(_, el)| el == "count")
        .unwrap();
    engine
        .complete_work_item(count_item, serde_json::json!({}))
        .await
        .unwrap();
    let scopes = scope_rows(&db.pool, started.id).await;
    assert_eq!(scopes, vec![(1, 0, "outer".to_string())]);

    // Completing the outer task closes the outer scope and the instance.
    let (ship_item, _) = open_items(&db.pool, started.id)
        .await
        .into_iter()
        .find(|(_, el)| el == "ship")
        .unwrap();
    engine
        .complete_work_item(ship_item, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    assert!(scope_rows(&db.pool, started.id).await.is_empty());
    let tokens: i64 = sqlx::query("select count(*) from rbpmn_token where instance_id = $1")
        .bind(started.id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get(0);
    assert_eq!(tokens, 0);
    db.drop().await;
}

/// An interrupting boundary tears the whole scope down in one transaction:
/// the subprocess's work item is cancelled, its scope row is gone, and the
/// timer that fired is consumed — all committed together.
#[tokio::test]
async fn boundary_timer_tears_down_a_subprocess_scope() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    // 13-subprocess has a P2D boundary timer on the subprocess; shorten it
    // so the scheduler can fire it here.
    let xml = fixture("accept/13-subprocess.bpmn").replace("P2D", "PT0S");
    engine.deploy(&xml, &Bindings::default()).await.unwrap();
    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(scope_rows(&db.pool, started.id).await.len(), 1);
    assert_eq!(
        open_items(&db.pool, started.id).await[0].1,
        "ti",
        "the inner task is waiting"
    );

    assert!(engine.fire_due_timer().await.unwrap());

    // Scope gone, inner work item cancelled, escalation task waiting.
    assert!(scope_rows(&db.pool, started.id).await.is_empty());
    let inner_state: String = sqlx::query(
        "select state from rbpmn_work_item where instance_id = $1 and element_id = 'ti'",
    )
    .bind(started.id)
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .get("state");
    assert_eq!(inner_state, "cancelled");
    assert_eq!(timer_rows(&db.pool, started.id).await, 0);
    let open = open_items(&db.pool, started.id).await;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].1, "t_late");
    db.drop().await;
}

/// The scoped error handler across a real transaction boundary: a service
/// task failing deep inside a subprocess is caught by the boundary on the
/// subprocess, which tears the scope down and takes the repair path.
#[tokio::test]
async fn subprocess_error_boundary_catches_from_within() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("reserve").await.unwrap();
    engine
        .deploy(
            &fixture("accept/20-subprocess-error-boundary.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    let (item, _) = open_items(&db.pool, started.id)
        .await
        .into_iter()
        .find(|(_, el)| el == "reserve")
        .unwrap();

    // Exhaust the retry budget with the matching error code.
    let mut outcome = None;
    for _ in 0..3 {
        outcome = Some(
            engine
                .fail_work_item(item, &fail_code("OUT_OF_STOCK"))
                .await
                .unwrap(),
        );
    }
    assert!(matches!(outcome, Some(FailOutcome::ErrorCaught(_))));

    // The scope is gone and the repair task is open at the root.
    assert!(scope_rows(&db.pool, started.id).await.is_empty());
    let open = open_items(&db.pool, started.id).await;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].1, "backorder");
    let status: String = sqlx::query("select status from rbpmn_instance where id = $1")
        .bind(started.id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get("status");
    assert_eq!(
        status, "active",
        "a caught error must not freeze the instance"
    );
    db.drop().await;
}

// ---------------------------------------------------------------- decisions
//
// A deployment carries its DMN artifacts (docs/dmn.md, D4), so an instance
// pins the decisions that were in force exactly as it pins its process. These
// tests run without the `dmn` feature too — and *that* is the interesting
// half: an engine that cannot validate decisions must refuse a bundle
// carrying them rather than accept it silently.

const DECISION_DMN: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<definitions xmlns="https://www.omg.org/spec/DMN/20191111/MODEL/"
             namespace="https://rbpmn.example/pricing" name="pricing" id="_pricing">
  <inputData name="Amount" id="amount"><variable name="Amount" typeRef="number"/></inputData>
  <decision name="Discount" id="discount">
    <variable name="Discount" typeRef="number"/>
    <informationRequirement><requiredInput href="#amount"/></informationRequirement>
    <literalExpression><text>Amount * 0.1</text></literalExpression>
  </decision>
</definitions>"##;

#[cfg(feature = "dmn")]
async fn decisions_of(pool: &PgPool, definition_id: uuid::Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "select dmn_xml from rbpmn_definition_decision where definition_id = $1 order by ordinal",
    )
    .bind(definition_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The artifacts persist with the definition, in order, and are part of its
/// content: changing a decision allocates a new version exactly as changing
/// the diagram does. Without that, a rule change would silently apply to
/// instances that were validated against the old one.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn a_deployment_carries_and_versions_its_decisions() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bundle =
        rbpmn_engine::Bundle::new(fixture("accept/01-minimal.bpmn")).decision(DECISION_DMN);

    let first = engine.deploy_bundle(&bundle).await.unwrap();
    assert_eq!(decisions_of(&db.pool, first.definition_id).await.len(), 1);

    // Byte-identical bundle: same version, no new row.
    let again = engine.deploy_bundle(&bundle).await.unwrap();
    assert!(again.reused);
    assert_eq!(again.version, first.version);

    // A changed decision is changed content.
    let edited = rbpmn_engine::Bundle::new(fixture("accept/01-minimal.bpmn"))
        .decision(DECISION_DMN.replace("Amount * 0.1", "Amount * 0.2"));
    let third = engine.deploy_bundle(&edited).await.unwrap();
    assert_eq!(third.version, first.version + 1);
    assert!(decisions_of(&db.pool, third.definition_id).await[0].contains("0.2"));

    // ...and the old version keeps the decisions it was validated with.
    assert!(decisions_of(&db.pool, first.definition_id).await[0].contains("0.1"));
    db.drop().await;
}

/// The rules that only exist because the artifacts travel inside the bundle:
/// binding a decision needs no environment, so deploy can answer completely.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn decision_bindings_are_resolved_against_the_bundle() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bpmn = fixture("accept/01-minimal.bpmn");

    let refused = |e: rbpmn_engine::DeployError, rule: &str| match e {
        rbpmn_engine::DeployError::Rejected(d) => assert!(
            d.iter().any(|d| d.rule == rule),
            "expected {rule}, got {d:?}"
        ),
        other => panic!("expected a rejection, got {other:?}"),
    };

    // Nothing bundled: the binding cannot resolve.
    let err = engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(&bpmn).bindings(Bindings::new().decision(
                "st",
                "Discount",
                "order.discount",
            )),
        )
        .await
        .unwrap_err();
    refused(err, "unresolved-decision");

    // Bundled, and it resolves.
    engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(&bpmn)
                .bindings(Bindings::new().decision("st", "Discount", "order.discount"))
                .decision(DECISION_DMN),
        )
        .await
        .unwrap();

    // A result path that is not a FEEL qualified name.
    let err = engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(&bpmn)
                .bindings(Bindings::new().decision("st", "Discount", "not a path!"))
                .decision(DECISION_DMN),
        )
        .await
        .unwrap_err();
    refused(err, "decision-has-binding");

    // Two artifacts defining the same name: refused, never picked.
    let other = DECISION_DMN
        .replace("rbpmn.example/pricing", "rbpmn.example/other")
        .replace(r#"name="pricing""#, r#"name="other""#);
    let err = engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(&bpmn)
                .bindings(Bindings::new().decision("st", "Discount", "order.discount"))
                .decision(DECISION_DMN)
                .decision(other),
        )
        .await
        .unwrap_err();
    refused(err, "unresolved-decision");
    db.drop().await;
}

/// A decision that reads the clock is refused at deploy, with the element
/// that owns it — the same verdict the editor reaches offline.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn a_nondeterministic_decision_never_deploys() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let stamped = DECISION_DMN.replace("Amount * 0.1", "now()");
    let err = engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(fixture("accept/01-minimal.bpmn")).decision(stamped),
        )
        .await
        .unwrap_err();
    match err {
        rbpmn_engine::DeployError::Rejected(d) => assert!(
            d.iter()
                .any(|d| d.rule == "feel-deterministic" && d.element == "discount"),
            "{d:?}"
        ),
        other => panic!("expected a rejection, got {other:?}"),
    }
    db.drop().await;
}

/// The half that must hold in *every* build: an engine without DMN support
/// refuses a bundle carrying decisions. Accepting it would deploy a
/// definition nobody validated.
#[cfg(not(feature = "dmn"))]
#[tokio::test]
async fn a_build_without_dmn_refuses_a_bundle_carrying_decisions() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let err = engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(fixture("accept/01-minimal.bpmn")).decision(DECISION_DMN),
        )
        .await
        .unwrap_err();
    match err {
        rbpmn_engine::DeployError::Rejected(d) => {
            assert!(d.iter().any(|d| d.rule == "dmn-validates"), "{d:?}")
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    // ...while a bundle with no decisions is the ordinary case.
    engine
        .deploy_bundle(&rbpmn_engine::Bundle::new(fixture(
            "accept/01-minimal.bpmn",
        )))
        .await
        .unwrap();
    db.drop().await;
}

/// Decisions cannot outlive the definition that carries them — enforced by
/// the database, not by this code remembering to delete them.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn deleting_a_definition_takes_its_decisions_with_it() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let deployed = engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(fixture("accept/01-minimal.bpmn")).decision(DECISION_DMN),
        )
        .await
        .unwrap();
    assert_eq!(
        decisions_of(&db.pool, deployed.definition_id).await.len(),
        1
    );

    engine
        .delete_definition("p", deployed.version)
        .await
        .unwrap();
    let orphans: i64 = sqlx::query_scalar("select count(*) from rbpmn_definition_decision")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(orphans, 0, "a decision outlived its definition");
    db.drop().await;
}

/// Startup re-validation covers decisions too. Definitions persist, but the
/// code that validates them does not — a binary rebuilt without DMN support
/// must say so rather than run a definition it can no longer check.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn startup_revalidation_covers_decisions() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(fixture("accept/01-minimal.bpmn")).decision(DECISION_DMN),
        )
        .await
        .unwrap();
    assert!(
        engine.check_active_definitions().await.unwrap().is_empty(),
        "a good deployment must re-validate clean"
    );

    // Corrupt the stored artifact behind the engine's back: this is what a
    // dsntk upgrade that stopped accepting it would look like.
    sqlx::query("update rbpmn_definition_decision set dmn_xml = $1")
        .bind("<not-dmn")
        .execute(&db.pool)
        .await
        .unwrap();
    let drift = engine.check_active_definitions().await.unwrap();
    assert!(
        drift.iter().any(|d| d.rule == "dmn-validates"),
        "drift must be loud: {drift:?}"
    );
    db.drop().await;
}

/// A decision actually running: the token reaches the business-rule task, the
/// projection evaluates inside the same transaction, the answer lands at the
/// bound path, and the instance completes — one call, no polling, no worker.
///
/// This is the shape D3 chose: the pure core parks and says what it needs,
/// evaluation happens where dsntk is allowed, and the answer re-enters as
/// command data.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn a_business_rule_task_evaluates_within_the_step() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(fixture("accept/25-business-rule-task.bpmn"))
                .bindings(Bindings::new().decision("decide", "Discount", "order.discount"))
                .decision(DECISION_DMN),
        )
        .await
        .unwrap();

    let started = engine
        .start("p", None, serde_json::json!({ "Amount": 250 }))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;

    let vars: serde_json::Value =
        sqlx::query_scalar("select variables from rbpmn_instance where id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    // Exactly 25, not 25.0: FEEL decimals are exact and the document
    // now stores what it was given (the arbitrary_precision spike).
    assert_eq!(vars["order"]["discount"].to_string(), "25");
    // The input is untouched: a decision reads the document, it does not own it.
    assert_eq!(vars["Amount"], serde_json::json!(250));

    let trace: Vec<String> =
        sqlx::query_scalar("select kind from rbpmn_event where instance_id = $1 order by id")
            .bind(started.id)
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert!(trace.iter().any(|k| k == "decision-requested"), "{trace:?}");
    assert!(trace.iter().any(|k| k == "decision-evaluated"), "{trace:?}");
    db.drop().await;
}

/// A decision whose answer the variable document cannot hold freezes the
/// instance at the element rather than dropping the answer — the uniform
/// incident shape, so inspection shows *where*.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn an_unrepresentable_answer_freezes_at_the_element() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    // A decision returning a range: a perfectly good FEEL value with no JSON
    // representation. Typed `Any`, because a `number`-typed decision would
    // have its range coerced to null by DMN's own type checking — which is a
    // different outcome (an answer) and takes a different path.
    let ranged = DECISION_DMN.replace("Amount * 0.1", "[1..10]").replace(
        r#"<variable name="Discount" typeRef="number"/>"#,
        r#"<variable name="Discount" typeRef="Any"/>"#,
    );
    engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(fixture("accept/25-business-rule-task.bpmn"))
                .bindings(Bindings::new().decision("decide", "Discount", "order.discount"))
                .decision(ranged),
        )
        .await
        .unwrap();

    let started = engine
        .start("p", None, serde_json::json!({ "Amount": 250 }))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "failed").await;

    let (element, wait): (String, String) =
        sqlx::query_as("select element_id, wait_kind from rbpmn_token where instance_id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!((element.as_str(), wait.as_str()), ("decide", "incident"));
    db.drop().await;
}

/// A null answer is an answer (docs/dmn.md, "What P1 measured"): dsntk cannot
/// tell a legal "no rule matched" from a broken evaluation, so the token
/// continues with a null result and the *model* decides what that means.
/// Freezing here would turn every incomplete decision table into an incident.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn a_null_answer_continues_rather_than_freezing() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    // An incomplete table: nothing matches a negative amount.
    let table = DECISION_DMN.replace(
        "<literalExpression><text>Amount * 0.1</text></literalExpression>",
        r#"<decisionTable hitPolicy="UNIQUE">
             <input><inputExpression typeRef="number"><text>Amount</text></inputExpression></input>
             <output typeRef="number"/>
             <rule><inputEntry><text>&gt; 0</text></inputEntry><outputEntry><text>1</text></outputEntry></rule>
           </decisionTable>"#,
    );
    engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(fixture("accept/25-business-rule-task.bpmn"))
                .bindings(Bindings::new().decision("decide", "Discount", "order.discount"))
                .decision(table),
        )
        .await
        .unwrap();

    let started = engine
        .start("p", None, serde_json::json!({ "Amount": -1 }))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    let vars: serde_json::Value =
        sqlx::query_scalar("select variables from rbpmn_instance where id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    // `vars["order"]["discount"]` is `Null` for a *missing* key too, so
    // indexing cannot tell "the decision answered null" from "the answer went
    // nowhere". Ask whether the key exists. It did not, before: the answer
    // was applied as an RFC 7386 merge patch, where null means delete.
    let order = vars["order"].as_object().expect("the bound path exists");
    assert!(
        order.contains_key("discount"),
        "a null answer must be stored, not delete the path: {vars}"
    );
    assert!(order["discount"].is_null());
    db.drop().await;
}

/// A business-rule task with no manifest binding cannot deploy. There is no
/// default: guessing a decision by element id would invoke business logic
/// nobody chose.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn a_business_rule_task_needs_a_binding() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let err = engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(fixture("accept/25-business-rule-task.bpmn"))
                .decision(DECISION_DMN),
        )
        .await
        .unwrap_err();
    match err {
        rbpmn_engine::DeployError::Rejected(d) => assert!(
            d.iter()
                .any(|d| d.rule == "decision-has-binding" && d.element == "decide"),
            "{d:?}"
        ),
        other => panic!("expected a rejection, got {other:?}"),
    }
    db.drop().await;
}

/// A timer that resumes a token straight into a business-rule task.
///
/// The scheduler is a *separate* step path from `start` and `complete`, and it
/// was the one that still called the bare `step` — parking a token on a
/// decision nobody answered, persisting a `wait_kind` no loader understands,
/// and wedging the instance permanently and silently. Every decision test
/// before this one reached the task through `start`, which is exactly why
/// none of them saw it.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn a_timer_can_resume_into_a_decision() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy_bundle(
            &rbpmn_engine::Bundle::new(fixture("accept/26-timer-then-decision.bpmn"))
                .bindings(Bindings::new().decision("decide", "Discount", "order.discount"))
                .decision(DECISION_DMN),
        )
        .await
        .unwrap();

    let started = engine
        .start("p", None, serde_json::json!({ "Amount": 250 }))
        .await
        .unwrap();
    // PT0S: due immediately.
    while engine.fire_due_timer().await.unwrap() {}
    wait_for_status(&db.pool, started.id, "completed").await;

    let vars: serde_json::Value =
        sqlx::query_scalar("select variables from rbpmn_instance where id = $1")
            .bind(started.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(vars["order"]["discount"].to_string(), "25");

    // ...and the instance is still readable, which is the half that failed:
    // a persisted decision wait made every later load error out.
    engine.inspect_instance(started.id).await.unwrap();
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Message boundary events, slice 1 (docs/design/boundary-messages.md)
// ---------------------------------------------------------------------------

/// Bindings for fixture 29: the boundary's *own* element id carries the
/// correlation, exactly as a catch's does. Nothing in the XML.
fn contest_bindings() -> Bindings {
    Bindings::new().correlation("paid_during_contest", "ticket.reference")
}

/// The golden trace a scenario pins, read from the core's corpus so the
/// projection is held to the same history the pure core produces — not to a
/// second copy of it maintained here.
fn golden_trace(scenario: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rbpmn-core/tests/scenarios")
        .join(scenario);
    let json: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    json["expect"]["trace"]
        .as_array()
        .expect("scenario has expect.trace")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

async fn assert_fsck_clean(pool: &PgPool) {
    let violations = harness::fsck(pool).await;
    assert!(violations.is_empty(), "fsck: {violations:?}");
}

async fn item_state(pool: &PgPool, instance: uuid::Uuid, element: &str) -> String {
    sqlx::query_scalar(
        "select state from rbpmn_work_item where instance_id = $1 and element_id = $2",
    )
    .bind(instance)
    .bind(element)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn variables_of(pool: &PgPool, instance: uuid::Uuid) -> serde_json::Value {
    sqlx::query_scalar("select variables from rbpmn_instance where id = $1")
        .bind(instance)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The motivating case, end to end against the database: a payment arrives
/// while a clerk is holding the contest task under a live lease. The process
/// withdraws the item — a lease is a row value that protects a holder from
/// *other workers*, never from the process (`spec/Lease.tla`, `Cancel`) —
/// and every verb the clerk has left answers typed, about a `cancelled`
/// item, with no 5xx and no patch applied.
#[tokio::test]
async fn message_boundary_interrupts_a_leased_user_task() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/29-message-boundary.bpmn"),
            &contest_bindings(),
        )
        .await
        .unwrap();
    let started = engine
        .start(
            "ticket",
            None,
            serde_json::json!({ "ticket": { "reference": "T-2026-0042" } }),
        )
        .await
        .unwrap();

    let task = engine
        .get_task("handle_contest", &GetTaskOptions::new("clerk"))
        .await
        .unwrap()
        .expect("the clerk's task");
    assert_eq!(task.element_id, "handle_contest");
    assert_eq!(subscription_rows(&db.pool, started.id).await, 1);

    let correlation = engine
        .correlate(
            "PAID",
            "T-2026-0042",
            serde_json::json!({ "payment": { "amount": 60 } }),
        )
        .await
        .unwrap();
    assert_eq!(correlation.instance_id, started.id);
    wait_for_status(&db.pool, started.id, "completed").await;

    // The clerk's completion, arriving a moment late. The idempotent no-op,
    // naming the state — and the patch it carried is nowhere.
    let refused = engine
        .complete_task(
            task.id,
            "clerk",
            serde_json::json!({ "contest": { "upheld": true } }),
        )
        .await
        .unwrap();
    assert!(
        matches!(&refused, Completion::AlreadyClosed { state } if state == "cancelled"),
        "{refused:?}"
    );
    let variables = variables_of(&db.pool, started.id).await;
    assert_eq!(variables["payment"]["amount"], 60);
    assert!(
        variables.get("contest").is_none(),
        "the refused completion's patch must not have landed: {variables}"
    );

    // The other two verbs, same story. Note the lease columns still name the
    // clerk: cancellation writes the state column and nothing else, so the
    // state is the only thing that could tell "withdrawn" from "reassigned".
    assert_eq!(
        engine
            .extend_lock(task.id, "clerk", Duration::from_secs(600))
            .await
            .unwrap(),
        LockExtension::Lost {
            state: "cancelled".into()
        }
    );
    assert_eq!(
        engine
            .release_task(task.id, "clerk", task.lease_no)
            .await
            .unwrap(),
        Released::Lost {
            state: "cancelled".into()
        }
    );
    assert!(matches!(
        engine
            .fail_task(task.id, "clerk", Some("NOPE".into()), None)
            .await
            .unwrap(),
        FailOutcome::AlreadyClosed { state } if state == "cancelled"
    ));

    assert_eq!(
        item_state(&db.pool, started.id, "handle_contest").await,
        "cancelled"
    );
    assert_eq!(subscription_rows(&db.pool, started.id).await, 0);
    assert_eq!(
        event_trace(&db.pool, started.id).await,
        golden_trace("29-message-boundary-delivered.json")
    );
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// The other order, and the loud answer it owes: the clerk decides first, the
/// completion withdraws the boundary's arm in its own transaction, and the
/// payment then has nowhere to go — 404, never a silent drop and never a
/// delivery to a decided contest.
#[tokio::test]
async fn completion_wins_then_the_message_is_404() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/29-message-boundary.bpmn"),
            &contest_bindings(),
        )
        .await
        .unwrap();
    let started = engine
        .start(
            "ticket",
            None,
            serde_json::json!({ "ticket": { "reference": "T-2026-0042" } }),
        )
        .await
        .unwrap();
    let task = engine
        .get_task("handle_contest", &GetTaskOptions::new("clerk"))
        .await
        .unwrap()
        .expect("the clerk's task");

    let done = engine
        .complete_task(
            task.id,
            "clerk",
            serde_json::json!({ "contest": { "upheld": true } }),
        )
        .await
        .unwrap();
    assert!(matches!(done, Completion::Advanced(_)));
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_eq!(subscription_rows(&db.pool, started.id).await, 0);

    let late = engine
        .correlate("PAID", "T-2026-0042", serde_json::json!({}))
        .await;
    assert!(
        matches!(late, Err(rbpmn_engine::EngineError::NoSubscription { .. })),
        "{late:?}"
    );
    assert_eq!(
        event_trace(&db.pool, started.id).await,
        golden_trace("29-message-boundary-completed.json")
    );
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// `spec/BoundaryExit.tla` against the database: completion and delivery
/// launched concurrently on one token, on two separate connections, round
/// after round. Exactly one wins; the loser is refused typed and *before*
/// mutating anything, so the instance ends with exactly one of the two end
/// events in its history.
///
/// Non-vacuity is the point of the round count: both orders must actually
/// occur, or this is a sequential test wearing a race's clothes.
#[tokio::test]
async fn correlate_and_complete_race_exactly_one_wins() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    // A genuinely separate pool for the correlator: two connections racing on
    // the instance row, not two futures sharing one.
    let correlator = Engine::builder(PgPool::connect(&db.url()).await.unwrap())
        .retry_backoff(Duration::ZERO)
        .build();
    engine
        .deploy(
            &fixture("accept/29-message-boundary.bpmn"),
            &contest_bindings(),
        )
        .await
        .unwrap();

    const ROUNDS: u32 = 24;
    let (mut paid, mut decided) = (0, 0);
    for round in 0..ROUNDS {
        let key = format!("T-{round}");
        let started = engine
            .start(
                "ticket",
                None,
                serde_json::json!({ "ticket": { "reference": key } }),
            )
            .await
            .unwrap();
        let task = engine
            .get_task("handle_contest", &GetTaskOptions::new("clerk"))
            .await
            .unwrap()
            .expect("the clerk's task");

        // A sub-millisecond bias, alternating sides: both calls are still in
        // flight together, but the interleaving varies instead of settling
        // into whichever order this machine happens to schedule.
        let lead = Duration::from_micros(u64::from(round % 5) * 150);
        let (early, late) = if round.is_multiple_of(2) {
            (Duration::ZERO, lead)
        } else {
            (lead, Duration::ZERO)
        };
        let completing = async {
            tokio::time::sleep(early).await;
            engine
                .complete_task(
                    task.id,
                    "clerk",
                    serde_json::json!({ "contest": { "upheld": true } }),
                )
                .await
        };
        let correlating = async {
            tokio::time::sleep(late).await;
            correlator
                .correlate(
                    "PAID",
                    &key,
                    serde_json::json!({ "payment": { "amount": 60 } }),
                )
                .await
        };
        let (completion, delivery) = tokio::join!(completing, correlating);

        let completed = matches!(completion, Ok(Completion::Advanced(_)));
        let delivered = delivery.is_ok();
        assert!(
            completed ^ delivered,
            "round {round}: exactly one exit, got completion={completion:?} \
             delivery={delivery:?}"
        );
        if completed {
            decided += 1;
            // The typed refusal, before any mutation. Two shapes, both loud
            // and both 4xx: the delivery either resolved nothing at all
            // (the instance was already inactive when it looked) or its
            // re-check under the instance lock found the winner had closed
            // the instance underneath it.
            assert!(
                matches!(
                    delivery,
                    Err(rbpmn_engine::EngineError::NoSubscription { .. })
                        | Err(rbpmn_engine::EngineError::InstanceNotActive(..))
                ),
                "round {round}: {delivery:?}"
            );
        } else {
            paid += 1;
            assert!(
                matches!(
                    &completion,
                    Ok(Completion::AlreadyClosed { state }) if state == "cancelled"
                ),
                "round {round}: {completion:?}"
            );
        }

        wait_for_status(&db.pool, started.id, "completed").await;
        let trace = event_trace(&db.pool, started.id).await;
        let ends: Vec<&String> = trace
            .iter()
            .filter(|e| e.starts_with("element-completed end_"))
            .collect();
        assert_eq!(ends.len(), 1, "round {round}: {trace:?}");
        assert_eq!(
            ends[0].as_str(),
            if completed {
                "element-completed end_decided"
            } else {
                "element-completed end_paid"
            },
            "round {round}"
        );
    }
    assert!(
        paid > 0 && decided > 0,
        "the correlate-vs-complete race never went both ways ({paid} paid, \
         {decided} decided) — this round proved nothing about the interleaving \
         spec/BoundaryExit.tla is about"
    );
    assert_fsck_clean(&db.pool).await;
    println!("boundary race: {paid} paid, {decided} decided over {ROUNDS} rounds");
    db.drop().await;
}

/// Two subscriptions on one token, told apart by element — the loader fix
/// (docs/design/boundary-messages.md, finding 2) against the database.
/// Whichever message arrives, the other arm is withdrawn with it.
#[tokio::test]
async fn message_boundary_on_a_receive_task_rehydrates_the_right_subscription() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bindings = Bindings::new()
        .correlation("await_payment", "order.id")
        .correlation("cancelled_meanwhile", "order.id");
    engine
        .deploy(
            &fixture("accept/30-receive-task-message-boundary.bpmn"),
            &bindings,
        )
        .await
        .unwrap();

    // One instance at a time, both on o-77: two live instances would share
    // the key and `correlate` would (correctly) refuse the ambiguity.
    let host = engine
        .start("p", None, serde_json::json!({ "order": { "id": "o-77" } }))
        .await
        .unwrap();
    assert_eq!(
        subscription_rows(&db.pool, host.id).await,
        2,
        "the host's arm and the boundary's, on one token"
    );
    engine
        .correlate(
            "PAID",
            "o-77",
            serde_json::json!({ "payment": { "amount": 60 } }),
        )
        .await
        .unwrap();
    wait_for_status(&db.pool, host.id, "completed").await;
    assert_eq!(
        event_trace(&db.pool, host.id).await,
        golden_trace("30-receive-host-delivered.json")
    );

    let boundary = engine
        .start("p", None, serde_json::json!({ "order": { "id": "o-77" } }))
        .await
        .unwrap();
    engine
        .correlate(
            "CANCELLED",
            "o-77",
            serde_json::json!({ "cancellation": { "by": "buyer" } }),
        )
        .await
        .unwrap();
    wait_for_status(&db.pool, boundary.id, "completed").await;
    assert_eq!(
        event_trace(&db.pool, boundary.id).await,
        golden_trace("30-receive-boundary-delivered.json")
    );

    // ...and the same again with the two rows renumbered the other way
    // round. Arming allocates the host's subscription before its boundary's,
    // so resolving a `message` token by `token_no` alone happens to pick the
    // host today — right by arm order rather than by intent, which is
    // exactly the bug. The permuted row set is still fsck-clean (each token
    // has exactly one subscription at its own element), so it is a state the
    // invariants permit, and the loader must not lean on anything they do
    // not promise. Resolve by token alone here and the host's own message
    // takes the *boundary* arm: the receive task never completes.
    let permuted = engine
        .start("p", None, serde_json::json!({ "order": { "id": "o-78" } }))
        .await
        .unwrap();
    let numbers: Vec<(i64, String)> = sqlx::query_as(
        "select subscription_no, element_id from rbpmn_subscription \
         where instance_id = $1 order by subscription_no",
    )
    .bind(permuted.id)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(numbers[0].1, "await_payment", "the host arms first");
    let (low, high) = (numbers[0].0, numbers[1].0);
    let spare = high + 1;
    for (from, to) in [(low, spare), (high, low), (spare, high)] {
        sqlx::query(
            "update rbpmn_subscription set subscription_no = $3 \
             where instance_id = $1 and subscription_no = $2",
        )
        .bind(permuted.id)
        .bind(from)
        .bind(to)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    assert_fsck_clean(&db.pool).await;
    engine
        .correlate(
            "PAID",
            "o-78",
            serde_json::json!({ "payment": { "amount": 60 } }),
        )
        .await
        .unwrap();
    wait_for_status(&db.pool, permuted.id, "completed").await;
    // The same golden trace as the un-permuted host delivery, key aside.
    // Resolve by token alone and this diverges by one event — the host is
    // *entered* rather than completed, because the delivery took the
    // boundary arm and `interrupt_to_boundary` walked out of the receive
    // task as though it were a boundary event.
    let expected: Vec<String> = golden_trace("30-receive-host-delivered.json")
        .into_iter()
        .map(|e| e.replace("o-77", "o-78"))
        .collect();
    assert_eq!(event_trace(&db.pool, permuted.id).await, expected);
    assert_eq!(subscription_rows(&db.pool, permuted.id).await, 0);
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// The message that tears a whole scope down — the mirror of
/// `boundary_timer_tears_down_a_subprocess_scope`, on the other arm kind.
#[tokio::test]
async fn message_boundary_tears_down_a_subprocess_scope() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/31-subprocess-message-boundary.bpmn"),
            &Bindings::new().correlation("cancelled_during_work", "order.id"),
        )
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({ "order": { "id": "o-31" } }))
        .await
        .unwrap();
    assert_eq!(scope_rows(&db.pool, started.id).await.len(), 1);
    assert_eq!(open_items(&db.pool, started.id).await[0].1, "pick");

    engine
        .correlate(
            "CANCELLED",
            "o-31",
            serde_json::json!({ "cancellation": { "by": "buyer" } }),
        )
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;

    // The scope is gone, the work open inside it was cancelled with it, and
    // the boundary's own arm went with the token it was armed on.
    assert!(scope_rows(&db.pool, started.id).await.is_empty());
    assert_eq!(item_state(&db.pool, started.id, "pick").await, "cancelled");
    assert_eq!(subscription_rows(&db.pool, started.id).await, 0);
    assert_eq!(
        event_trace(&db.pool, started.id).await,
        golden_trace("31-subprocess-message-boundary.json")
    );
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// Competing consumers of one message: both resolve the same row without a
/// lock, the first to take the instance row delivers, the second's re-check
/// answers it typed. Unchanged by boundaries — a boundary's subscription is
/// a row like any other — and asserted here because slice 1 makes a human
/// with a payment button one of the competing consumers.
#[tokio::test]
async fn competing_correlators_deliver_once() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let rival = Engine::builder(PgPool::connect(&db.url()).await.unwrap())
        .retry_backoff(Duration::ZERO)
        .build();
    engine
        .deploy(
            &fixture("accept/29-message-boundary.bpmn"),
            &contest_bindings(),
        )
        .await
        .unwrap();
    let started = engine
        .start(
            "ticket",
            None,
            serde_json::json!({ "ticket": { "reference": "T-both" } }),
        )
        .await
        .unwrap();

    let (first, second) = tokio::join!(
        engine.correlate(
            "PAID",
            "T-both",
            serde_json::json!({ "payment": { "by": "a" } })
        ),
        rival.correlate(
            "PAID",
            "T-both",
            serde_json::json!({ "payment": { "by": "b" } })
        ),
    );
    assert!(
        first.is_ok() ^ second.is_ok(),
        "exactly one delivery: {first:?} / {second:?}"
    );
    let loser = if first.is_ok() { second } else { first };
    assert!(
        matches!(
            loser,
            Err(rbpmn_engine::EngineError::NoSubscription { .. })
                | Err(rbpmn_engine::EngineError::InstanceNotActive(..))
        ),
        "{loser:?}"
    );
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_eq!(
        event_count(&db.pool, started.id, "message-received").await,
        1
    );
    assert_eq!(subscription_rows(&db.pool, started.id).await, 0);
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// Both interrupting kinds on one host: whichever fires withdraws the other
/// in the same transaction, and the row it left behind is gone with it.
#[tokio::test]
async fn message_and_timer_boundaries_either_wins() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bindings = Bindings::new().correlation("paid", "ticket.reference");
    engine
        .deploy(
            &fixture("accept/32-message-and-timer-boundaries.bpmn"),
            &bindings,
        )
        .await
        .unwrap();
    // The same model with a timer that is due at once, under its own process
    // id: a second deploy of key `p` would be a new *version*, not a second
    // definition.
    let due_now = harness::with_process_id(
        &fixture("accept/32-message-and-timer-boundaries.bpmn").replace("P2D", "PT0S"),
        "p32now",
    );
    engine.deploy(&due_now, &bindings).await.unwrap();

    // The message wins: the timer row goes with the token it was armed on.
    let paid = engine
        .start(
            "p",
            None,
            serde_json::json!({ "ticket": { "reference": "T-32" } }),
        )
        .await
        .unwrap();
    assert_eq!(timer_rows(&db.pool, paid.id).await, 1);
    engine
        .correlate("PAID", "T-32", serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, paid.id, "completed").await;
    assert_eq!(timer_rows(&db.pool, paid.id).await, 0);
    assert_eq!(
        item_state(&db.pool, paid.id, "handle_contest").await,
        "cancelled"
    );
    assert_eq!(
        event_trace(&db.pool, paid.id).await,
        golden_trace("32-message-wins.json")
    );

    // The timer wins: the subscription goes the same way. Started after the
    // first instance finished, so the two do not share the key `T-32`.
    let overdue = engine
        .start(
            "p32now",
            None,
            serde_json::json!({ "ticket": { "reference": "T-32" } }),
        )
        .await
        .unwrap();
    assert_eq!(subscription_rows(&db.pool, overdue.id).await, 1);
    assert!(engine.fire_due_timer().await.unwrap());
    wait_for_status(&db.pool, overdue.id, "completed").await;
    assert_eq!(subscription_rows(&db.pool, overdue.id).await, 0);
    assert_eq!(
        item_state(&db.pool, overdue.id, "handle_contest").await,
        "cancelled"
    );
    // The golden trace, with the one literal this deployment changed.
    let expected: Vec<String> = golden_trace("32-timer-wins.json")
        .into_iter()
        .map(|e| e.replace("timer-armed overdue P2D", "timer-armed overdue PT0S"))
        .collect();
    assert_eq!(event_trace(&db.pool, overdue.id).await, expected);
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// The snapshot hazard, made deterministic — and the reason `extend_lock`
/// reads the state in a **second** statement.
///
/// The payment's transaction has already written `cancelled` on the work
/// item and is holding the row lock. The clerk's heartbeat arrives on
/// another connection: its `UPDATE` finds the row in its own snapshot,
/// blocks on the lock, and only after the payment commits re-evaluates its
/// predicate against the new version (EvalPlanQual) and matches nothing.
/// Everything else in that statement — a sub-select in a CTE, say — would
/// still be reading the pre-payment snapshot and would answer `locked`:
/// "your task was reassigned" for a ticket that was paid, which is exactly
/// the answer `state` exists to prevent. A separate statement takes a fresh
/// snapshot and cannot be stale.
#[tokio::test]
async fn a_heartbeat_blocked_by_a_cancellation_reports_the_new_state() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    // The heartbeat runs on its own pool: it must be a different backend
    // from the one holding the payment's transaction open.
    let clerk = Engine::builder(PgPool::connect(&db.url()).await.unwrap())
        .retry_backoff(Duration::ZERO)
        .build();
    engine
        .deploy(
            &fixture("accept/29-message-boundary.bpmn"),
            &contest_bindings(),
        )
        .await
        .unwrap();
    let started = engine
        .start(
            "ticket",
            None,
            serde_json::json!({ "ticket": { "reference": "T-snap" } }),
        )
        .await
        .unwrap();
    let task = engine
        .get_task("handle_contest", &GetTaskOptions::new("clerk"))
        .await
        .unwrap()
        .expect("the clerk's task");

    // The payment, uncommitted: the work item row is written and locked.
    let mut tx = db.pool.begin().await.unwrap();
    engine
        .correlate_in_tx(&mut tx, "PAID", "T-snap", serde_json::json!({}))
        .await
        .unwrap();

    let heartbeat = tokio::spawn(async move {
        clerk
            .extend_lock(task.id, "clerk", Duration::from_secs(600))
            .await
    });
    // Wait for the heartbeat to be *actually* parked on the row lock rather
    // than trusting a sleep — the hazard only exists while its statement
    // snapshot predates the commit below.
    let mut blocked = false;
    for _ in 0..200 {
        let waiting: i64 = sqlx::query_scalar(
            "select count(*) from pg_stat_activity where datname = current_database() \
             and wait_event_type = 'Lock' and query like '%update rbpmn_work_item%'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        if waiting > 0 {
            blocked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        blocked,
        "the heartbeat never blocked on the payment's row lock"
    );
    tx.commit().await.unwrap();

    assert_eq!(
        heartbeat.await.unwrap().unwrap(),
        LockExtension::Lost {
            state: "cancelled".into()
        }
    );
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Non-interrupting boundary events, slice 2
// (docs/design/boundary-messages.md §3.5 and §5)
//
// The claim these hold the projection to is a negative one: a side token is an
// *ordinary* token and needed no engine code. So every assertion below is
// really "the thing that would have needed special-casing did not happen" —
// the host's lease survived, the re-arm is a new row, the sibling lives in the
// host token's scope, a teardown reaps it like anything else, and the instance
// stays alive until the last token is consumed whichever one that is.
// ---------------------------------------------------------------------------

/// Fixture 33's wiring: the boundary carries the correlation, exactly as a
/// catch does, and the side path's service task takes the default topic. As
/// ever, none of it is in the XML.
fn casefile_bindings() -> Bindings {
    Bindings::new().correlation("note_received", "case.id")
}

/// A migrated engine with fixture 33 deployed and its side path's topic
/// declared — `file_note` is a service task, so the environment must cover it
/// before `unresolved-topic` will let the deploy through.
async fn casefile_engine(db: &TestDb) -> Engine {
    let engine = engine(db).await;
    engine.declare_topic("file_note").await.unwrap();
    engine
        .deploy(
            &fixture("accept/33-non-interrupting-message-boundary.bpmn"),
            &casefile_bindings(),
        )
        .await
        .unwrap();
    engine
}

async fn status_of(pool: &PgPool, instance: uuid::Uuid) -> String {
    sqlx::query_scalar("select status from rbpmn_instance where id = $1")
        .bind(instance)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The armed subscriptions of an instance, in allocation order:
/// `(subscription_no, element_id, correlation_key)`.
async fn subscriptions_of(pool: &PgPool, instance: uuid::Uuid) -> Vec<(i64, String, String)> {
    sqlx::query(
        "select subscription_no, element_id, correlation_key from rbpmn_subscription \
         where instance_id = $1 order by subscription_no",
    )
    .bind(instance)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| {
        (
            r.get::<i64, _>("subscription_no"),
            r.get::<String, _>("element_id"),
            r.get::<String, _>("correlation_key"),
        )
    })
    .collect()
}

/// Every live token's scope, by element. Enough for the fixtures here, where
/// no element holds two tokens at once.
async fn token_scopes(
    pool: &PgPool,
    instance: uuid::Uuid,
) -> std::collections::BTreeMap<String, i64> {
    sqlx::query("select element_id, scope_no from rbpmn_token where instance_id = $1")
        .bind(instance)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("element_id"),
                r.get::<i64, _>("scope_no"),
            )
        })
        .collect()
}

/// Backdate one armed timer so the scheduler will pick *it* next.
///
/// The alternative — rewriting the duration literal to `PT0S` before deploy,
/// as `boundary_timer_xml` and `racing_timer_xml` do — cannot order two
/// timers on one instance against each other, and it changes the history
/// (`timer-armed` prints the spec the model carries), which would put the
/// golden trace out of reach. This changes neither.
async fn make_due(pool: &PgPool, instance: uuid::Uuid, element: &str) {
    let rows = sqlx::query(
        "update rbpmn_timer set due_at = now() - interval '1 second' \
         where instance_id = $1 and element_id = $2",
    )
    .bind(instance)
    .bind(element)
    .execute(pool)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(rows, 1, "no timer armed at '{element}' to make due");
}

/// The whole of slice 2 in one instance: a note arrives while the reviewer is
/// holding the task under a live lease, and *nothing* of the reviewer's is
/// touched. The lease heartbeats, the item stays `locked`, the boundary is
/// re-armed in the same transaction that consumed it — a new row, so a second
/// note has somewhere to land — and each delivery leaves a sibling token
/// behind that keeps the instance alive after the review is decided.
///
/// The contrast with `message_boundary_interrupts_a_leased_user_task` is the
/// point: same verbs, same lease, opposite answers, and the only difference in
/// the model is `cancelActivity="false"`.
#[tokio::test]
async fn a_non_interrupting_message_leaves_the_lease_alive() {
    let db = TestDb::create().await;
    let engine = casefile_engine(&db).await;
    let started = engine
        .start(
            "casefile",
            None,
            serde_json::json!({ "case": { "id": "c-33" } }),
        )
        .await
        .unwrap();

    let task = engine
        .get_task("review", &GetTaskOptions::new("reviewer"))
        .await
        .unwrap()
        .expect("the reviewer's task");
    let armed = subscriptions_of(&db.pool, started.id).await;
    assert_eq!(armed.len(), 1, "{armed:?}");
    assert_eq!(
        (armed[0].1.as_str(), armed[0].2.as_str()),
        ("note_received", "c-33")
    );

    // The note. Empty patches throughout: the golden trace this run is held
    // to records none, and `step` emits one `variables-patched` per patch.
    engine
        .correlate("NOTE", "c-33", serde_json::json!({}))
        .await
        .unwrap();

    // The reviewer notices nothing. A heartbeat still extends — the verb that
    // answered `Lost { state: "cancelled" }` for the interrupting boundary —
    // and the item is still `locked` in the reviewer's name.
    assert!(
        matches!(
            engine
                .extend_lock(task.id, "reviewer", Duration::from_secs(600))
                .await
                .unwrap(),
            LockExtension::Extended { .. }
        ),
        "the host's lease must survive a non-interrupting delivery"
    );
    assert_eq!(item_state(&db.pool, started.id, "review").await, "locked");

    // Exactly one subscription at the boundary between deliveries, and a
    // *different* one: the delivery consumed the arm and the re-arm opened a
    // new row in the same transaction, so the host is never observably
    // without its boundary.
    let rearmed = subscriptions_of(&db.pool, started.id).await;
    assert_eq!(rearmed.len(), 1, "{rearmed:?}");
    assert_eq!(
        (rearmed[0].1.as_str(), rearmed[0].2.as_str()),
        ("note_received", "c-33")
    );
    assert!(
        rearmed[0].0 > armed[0].0,
        "the re-arm must be a new subscription, not the consumed one \
         ({armed:?} -> {rearmed:?})"
    );

    // The side token's work item is an ordinary one: claimable on its own
    // topic, leasable, handed back like any other.
    let side = engine
        .get_task("file_note", &GetTaskOptions::new("filer"))
        .await
        .unwrap()
        .expect("the side path's service task");
    assert_eq!(
        (side.element_id.as_str(), side.kind.as_str()),
        ("file_note", "service")
    );
    assert_eq!(
        engine
            .release_task(side.id, "filer", side.lease_no)
            .await
            .unwrap(),
        Released::Released
    );

    // A second note, on the re-armed subscription. Without the re-arm this is
    // a 404 and the rest of this test never happens.
    engine
        .correlate("NOTE", "c-33", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(subscription_rows(&db.pool, started.id).await, 1);

    // The review is decided. Its arm goes with it — but the two notes it let
    // through are tokens of their own, and the instance is not finished.
    let done = engine
        .complete_task(task.id, "reviewer", serde_json::json!({}))
        .await
        .unwrap();
    assert!(matches!(done, Completion::Advanced(_)), "{done:?}");
    assert_eq!(subscription_rows(&db.pool, started.id).await, 0);
    assert_eq!(
        status_of(&db.pool, started.id).await,
        "active",
        "the instance must outlive its host: two side tokens are still open"
    );

    let open = open_items(&db.pool, started.id).await;
    assert_eq!(
        open.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>(),
        ["file_note", "file_note"],
        "one side token per delivery"
    );
    engine
        .complete_work_item(open[0].0, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        status_of(&db.pool, started.id).await,
        "active",
        "one side token still to be consumed"
    );
    engine
        .complete_work_item(open[1].0, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;

    assert_eq!(
        variables_of(&db.pool, started.id).await,
        serde_json::json!({ "case": { "id": "c-33" } })
    );
    assert_eq!(
        event_trace(&db.pool, started.id).await,
        golden_trace("33-non-interrupting-delivered-twice-then-host-completes.json")
    );
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// The other order. Host completion withdraws the arm exactly as an
/// interrupting one's would — non-interrupting says what a delivery *does*,
/// never how long the boundary lives — so a note arriving afterwards is the
/// same loud 404, and no side path ever ran.
#[tokio::test]
async fn host_completion_withdraws_a_non_interrupting_arm() {
    let db = TestDb::create().await;
    let engine = casefile_engine(&db).await;
    let started = engine
        .start(
            "casefile",
            None,
            serde_json::json!({ "case": { "id": "c-33" } }),
        )
        .await
        .unwrap();
    let task = engine
        .get_task("review", &GetTaskOptions::new("reviewer"))
        .await
        .unwrap()
        .expect("the reviewer's task");

    let done = engine
        .complete_task(task.id, "reviewer", serde_json::json!({}))
        .await
        .unwrap();
    assert!(matches!(done, Completion::Advanced(_)), "{done:?}");
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_eq!(
        event_count(&db.pool, started.id, "subscription-cancelled").await,
        1
    );
    assert_eq!(subscription_rows(&db.pool, started.id).await, 0);

    let late = engine
        .correlate("NOTE", "c-33", serde_json::json!({}))
        .await;
    assert!(
        matches!(late, Err(rbpmn_engine::EngineError::NoSubscription { .. })),
        "{late:?}"
    );
    // The side path never existed: no token took it, so no item was created.
    let items: i64 = sqlx::query_scalar(
        "select count(*) from rbpmn_work_item where instance_id = $1 and element_id = 'file_note'",
    )
    .bind(started.id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(items, 0);
    assert_eq!(
        event_trace(&db.pool, started.id).await,
        golden_trace("33-host-completes-first.json")
    );
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// The reminder shape, through the scheduler: a timer fires beside an open
/// approval instead of taking it away. Both directions in one test, because
/// the interesting pair is "fired, host untouched" against "host first, arm
/// withdrawn" — the same two the golden traces pin.
#[tokio::test]
async fn a_non_interrupting_timer_spawns_a_reminder() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    // Due at once so the scheduler can fire it here — the same rewrite
    // `boundary_timer_xml` and `racing_timer_xml` use, and the one literal the
    // golden trace has to be mapped through below.
    let xml = fixture("accept/34-non-interrupting-timer-boundary.bpmn").replace("PT1H", "PT0S");
    engine.deploy(&xml, &Bindings::default()).await.unwrap();
    let ping = |trace: Vec<String>| -> Vec<String> {
        trace
            .into_iter()
            .map(|e| e.replace("timer-armed bt PT1H", "timer-armed bt PT0S"))
            .collect()
    };

    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(timer_rows(&db.pool, started.id).await, 1);
    assert!(engine.fire_due_timer().await.unwrap());

    // The reminder ran *beside* the approval. The clerk's item was never
    // touched: still available, still claimable, still the same item.
    let open = open_items(&db.pool, started.id).await;
    assert_eq!(
        open.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>(),
        ["ut", "t_ping"],
        "the host's item and the side token's, together"
    );
    assert_eq!(item_state(&db.pool, started.id, "ut").await, "available");
    let approval = engine
        .get_task("ut", &GetTaskOptions::new("clerk"))
        .await
        .unwrap()
        .expect("the approval is still claimable");
    assert_eq!(
        (approval.id, approval.element_id.as_str()),
        (open[0].0, "ut")
    );

    // The host completes; the reminder keeps the instance alive on its own.
    assert!(matches!(
        engine
            .complete_task(approval.id, "clerk", serde_json::json!({}))
            .await
            .unwrap(),
        Completion::Advanced(_)
    ));
    assert_eq!(status_of(&db.pool, started.id).await, "active");
    engine
        .complete_work_item(open[1].0, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_eq!(
        event_trace(&db.pool, started.id).await,
        ping(golden_trace("34-reminder-fires-then-approved.json"))
    );

    // The mirror: the clerk is quicker than the deadline. The arm goes with
    // the host, and there is nothing left for the scheduler to find — a
    // reminder for a decision already made would be the bug.
    let quick = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(timer_rows(&db.pool, quick.id).await, 1);
    let (item, _) = open_items(&db.pool, quick.id).await[0].clone();
    engine
        .complete_work_item(item, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, quick.id, "completed").await;
    assert_eq!(timer_rows(&db.pool, quick.id).await, 0);
    assert_eq!(event_count(&db.pool, quick.id, "timer-cancelled").await, 1);
    assert!(
        !engine.fire_due_timer().await.unwrap(),
        "the withdrawn arm must leave the scheduler nothing to fire"
    );
    assert_eq!(
        event_trace(&db.pool, quick.id).await,
        ping(golden_trace("34-host-completes-before-reminder.json"))
    );
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// Scheduler liveness, and the guard against a re-arm sneaking into the
/// single-shot path. A `timeDuration` boundary fires once — that is what the
/// spec it carries says — so after the fire the whole database has nothing
/// armed and `next_due_in` must be `None`.
///
/// A boundary that quietly re-armed itself would not fail any trace assertion
/// above: it would show up here, as a scheduler that never sleeps again. The
/// repeating form (`timeCycle`) is the one that re-arms, and it is a separate
/// path — this is the guard that it stayed separate (the cycle tests at the
/// end of this file check the other side of the same line).
#[tokio::test]
async fn a_single_shot_side_timer_leaves_the_scheduler_idle() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = fixture("accept/34-non-interrupting-timer-boundary.bpmn").replace("PT1H", "PT0S");
    engine.deploy(&xml, &Bindings::default()).await.unwrap();
    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    assert!(
        engine.next_due_in().await.unwrap().is_some(),
        "the boundary is armed and overdue"
    );

    assert!(engine.fire_due_timer().await.unwrap());
    assert_eq!(
        engine.next_due_in().await.unwrap(),
        None,
        "the single-shot boundary re-armed itself — the scheduler will now spin"
    );
    assert!(!engine.fire_due_timer().await.unwrap());
    assert_eq!(timer_rows(&db.pool, started.id).await, 0);
    assert_eq!(event_count(&db.pool, started.id, "timer-armed").await, 1);
    assert_eq!(event_count(&db.pool, started.id, "timer-fired").await, 1);

    // ...and the host is still open with the reminder beside it, so this is
    // an idle scheduler over live work, not over a finished instance.
    for (item, _) in open_items(&db.pool, started.id).await {
        engine
            .complete_work_item(item, serde_json::json!({}))
            .await
            .unwrap();
    }
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_eq!(engine.next_due_in().await.unwrap(), None);
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// A non-interrupting boundary on a *subprocess*: the escalation runs beside
/// the work, in the parent scope, because that is where the boundary's
/// outgoing flow lives. The projection is what makes this checkable — the
/// side token's `scope_no` is a column — and it is the one place where
/// "the host token's scope" and "the host's own scope" are different answers.
#[tokio::test]
async fn a_side_token_on_a_subprocess_host_runs_in_the_parent_scope() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("warn_customer").await.unwrap();
    let xml = fixture("accept/35-non-interrupting-on-subprocess.bpmn").replace("PT4H", "PT0S");
    engine.deploy(&xml, &Bindings::default()).await.unwrap();
    let started = engine
        .start("shipment", None, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        scope_rows(&db.pool, started.id).await,
        vec![(1, 0, "sp".to_string())]
    );

    assert!(engine.fire_due_timer().await.unwrap());

    // The three tokens, and the whole claim of §3.5 in one assertion: the
    // sibling is in the *parent* scope beside the parked subprocess token,
    // while the work inside the subprocess keeps its child scope.
    let scopes = token_scopes(&db.pool, started.id).await;
    assert_eq!(scopes.get("warn_customer"), Some(&0), "{scopes:?}");
    assert_eq!(scopes.get("sp"), Some(&0), "the parked host token");
    assert_eq!(scopes.get("pack"), Some(&1), "{scopes:?}");

    // The subprocess finishes on its own — the boundary took nothing from it
    // — and the escalation then keeps the instance alive after its host's
    // scope has closed.
    let (pack, _) = open_items(&db.pool, started.id)
        .await
        .into_iter()
        .find(|(_, e)| e == "pack")
        .unwrap();
    engine
        .complete_work_item(pack, serde_json::json!({}))
        .await
        .unwrap();
    assert!(
        scope_rows(&db.pool, started.id).await.is_empty(),
        "the subprocess completed with the side token outside it"
    );
    assert_eq!(status_of(&db.pool, started.id).await, "active");

    let open = open_items(&db.pool, started.id).await;
    assert_eq!(
        open.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>(),
        ["warn_customer"]
    );
    engine
        .complete_work_item(open[0].0, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    let expected: Vec<String> = golden_trace("35-side-token-in-parent-scope.json")
        .into_iter()
        .map(|e| {
            e.replace(
                "timer-armed taking_long PT4H",
                "timer-armed taking_long PT0S",
            )
        })
        .collect();
    assert_eq!(event_trace(&db.pool, started.id).await, expected);
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// Teardown reaps side tokens, and nothing special-cases them. A side path
/// runs *inside* a subprocess; the deadline on that subprocess then tears the
/// whole scope down, and the sibling goes with everything else in it — its
/// work item cancelled, its token gone, no scope row left behind.
///
/// Both timers are driven by backdating their rows rather than by rewriting
/// literals: two arms on one instance have to fire in a defined order, and
/// this keeps the history identical to the golden one.
#[tokio::test]
async fn teardown_reaps_side_tokens() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/36-side-token-reaped.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("claim", None, serde_json::json!({}))
        .await
        .unwrap();

    make_due(&db.pool, started.id, "nudge").await;
    assert!(engine.fire_due_timer().await.unwrap());
    let open = open_items(&db.pool, started.id).await;
    assert_eq!(
        open.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>(),
        ["assess", "chase"]
    );
    // The sibling is inside the subprocess' scope, beside its host: the
    // boundary is on `assess`, whose token lives there.
    let scopes = token_scopes(&db.pool, started.id).await;
    assert_eq!(scopes.get("chase"), Some(&1), "{scopes:?}");
    assert_eq!(scopes.get("assess"), Some(&1), "{scopes:?}");

    make_due(&db.pool, started.id, "deadline").await;
    assert!(engine.fire_due_timer().await.unwrap());
    wait_for_status(&db.pool, started.id, "completed").await;

    // Nothing of the side path survived, and nothing of it was treated
    // differently from the host's own work.
    assert!(scope_rows(&db.pool, started.id).await.is_empty());
    assert!(open_items(&db.pool, started.id).await.is_empty());
    assert_eq!(item_state(&db.pool, started.id, "chase").await, "cancelled");
    assert_eq!(
        item_state(&db.pool, started.id, "assess").await,
        "cancelled"
    );
    assert_eq!(timer_rows(&db.pool, started.id).await, 0);
    assert!(token_scopes(&db.pool, started.id).await.is_empty());
    assert_eq!(
        event_trace(&db.pool, started.id).await,
        golden_trace("36-teardown-reaps-a-side-token.json")
    );
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

/// The re-arm is an **arm**, and arms evaluate at arm time: the new
/// subscription's key is read from the document the delivery just patched,
/// not from the one the first arm saw. Nothing else in the corpus shows this
/// — every other slice-2 delivery carries an empty patch — and it is visible
/// only here, as the `correlation_key` column of a row the core asked for.
///
/// The trace slice at the end pins §3.5's emission order: delivery, its
/// patch, **then** the re-arm, and only then the sibling's first move. A live
/// host is never observably without its boundary.
#[tokio::test]
async fn a_re_arm_reads_its_key_from_the_patched_document() {
    let db = TestDb::create().await;
    let engine = casefile_engine(&db).await;
    let started = engine
        .start(
            "casefile",
            None,
            serde_json::json!({ "case": { "id": "c-old" } }),
        )
        .await
        .unwrap();
    engine
        .correlate(
            "NOTE",
            "c-old",
            serde_json::json!({ "case": { "id": "c-new" } }),
        )
        .await
        .unwrap();

    let rearmed = subscriptions_of(&db.pool, started.id).await;
    assert_eq!(rearmed.len(), 1, "{rearmed:?}");
    assert_eq!(
        rearmed[0].2, "c-new",
        "the re-arm must evaluate its key at arm time, against the patched document"
    );
    let trace = event_trace(&db.pool, started.id).await;
    let at = trace
        .iter()
        .position(|e| e == "message-received note_received NOTE")
        .expect("the delivery");
    assert_eq!(
        &trace[at..at + 4],
        [
            "message-received note_received NOTE",
            "variables-patched",
            "message-subscribed note_received NOTE c-new",
            "element-started note_received",
        ]
    );

    // So the old key is nobody's any more, and the new one delivers.
    let stale = engine
        .correlate("NOTE", "c-old", serde_json::json!({}))
        .await;
    assert!(
        matches!(stale, Err(rbpmn_engine::EngineError::NoSubscription { .. })),
        "{stale:?}"
    );
    engine
        .correlate("NOTE", "c-new", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        event_count(&db.pool, started.id, "message-received").await,
        2
    );

    let task = engine
        .get_task("review", &GetTaskOptions::new("reviewer"))
        .await
        .unwrap()
        .expect("the reviewer's task");
    engine
        .complete_task(task.id, "reviewer", serde_json::json!({}))
        .await
        .unwrap();
    for (item, _) in open_items(&db.pool, started.id).await {
        engine
            .complete_work_item(item, serde_json::json!({}))
            .await
            .unwrap();
    }
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_eq!(
        variables_of(&db.pool, started.id).await,
        serde_json::json!({ "case": { "id": "c-new" } })
    );
    assert_fsck_clean(&db.pool).await;
    db.drop().await;
}

// ----------------------------------------------------------- cycles (slice 3)

const WEEK: f64 = 604_800.0;

/// Every armed timer of an instance: (timer_no, due in epoch seconds,
/// remaining) — the three things a cycle's row adds up to.
async fn timer_dues(pool: &PgPool, instance: uuid::Uuid) -> Vec<(i64, f64, Option<i32>)> {
    sqlx::query(
        "select timer_no, extract(epoch from due_at)::float8 as due, remaining \
         from rbpmn_timer where instance_id = $1 order by timer_no",
    )
    .bind(instance)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("timer_no"), r.get("due"), r.get("remaining")))
    .collect()
}

async fn db_epoch(pool: &PgPool, expr: &str) -> f64 {
    sqlx::query_scalar(&format!("select extract(epoch from {expr})::float8"))
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Push every armed occurrence of an instance `ago` into the past. The golden
/// traces are untouched by this — only the instant moves, and the instant is
/// the projection's.
async fn backdate_timers_by(pool: &PgPool, instance: uuid::Uuid, ago: &str) {
    sqlx::query("update rbpmn_timer set due_at = now() - $2::interval where instance_id = $1")
        .bind(instance)
        .bind(ago)
        .execute(pool)
        .await
        .unwrap();
}

/// A scheduler that is an hour late: the armed occurrence is overdue by the
/// time anyone looks, but by less than one period.
async fn backdate_timers(pool: &PgPool, instance: uuid::Uuid) {
    backdate_timers_by(pool, instance, "1 hour").await;
}

async fn late_fee_engine(db: &TestDb) -> (Engine, uuid::Uuid) {
    let engine = engine(db).await;
    engine.declare_topic("add_late_fee").await.unwrap();
    engine
        .deploy(
            &fixture("accept/40-late-fee-cycle.bpmn"),
            &Bindings::new().correlation("await_payment", "ticket.reference"),
        )
        .await
        .unwrap();
    let started = engine
        .start(
            "ticket",
            None,
            serde_json::json!({ "ticket": { "reference": "T-40" } }),
        )
        .await
        .unwrap();
    (engine, started.id)
}

/// The schedule is *previous due + period*, never *now + period*: a
/// scheduler that ran an hour late must not push every later occurrence an
/// hour later too. `continues` in the re-arm's payload is how the projection
/// knew which due to step from.
#[tokio::test]
async fn a_cycle_rearms_from_its_previous_due() {
    let db = TestDb::create().await;
    let (engine, instance) = late_fee_engine(&db).await;

    let now = db_epoch(&db.pool, "clock_timestamp()").await;
    let armed = timer_dues(&db.pool, instance).await;
    assert_eq!(armed.len(), 1, "one occurrence at a time");
    let (first_no, first_due, remaining) = armed[0];
    assert!(remaining.is_none(), "R/… is unbounded");
    assert!(
        (first_due - (now + WEEK)).abs() < 5.0,
        "the first due is a week from the arm, off by {}s",
        first_due - now - WEEK
    );

    backdate_timers(&db.pool, instance).await;
    let (_, overdue, _) = timer_dues(&db.pool, instance).await[0];
    assert!(engine.fire_due_timer().await.unwrap());

    let next = timer_dues(&db.pool, instance).await;
    assert_eq!(next.len(), 1, "the fired row is gone and the next is in");
    assert_eq!(next[0].0, first_no + 1);
    assert!(
        (next[0].1 - (overdue + WEEK)).abs() < 0.001,
        "the next due steps from the overdue one, not from now: got {}, wanted {}",
        next[0].1,
        overdue + WEEK
    );
    assert!(next[0].2.is_none());

    // The side token is real work beside the untouched host (a receive task:
    // no item of its own, its subscription still there).
    let open = open_items(&db.pool, instance).await;
    assert_eq!(
        open.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>(),
        ["add_late_fee"]
    );
    assert_eq!(subscription_rows(&db.pool, instance).await, 1);
    let continues: Option<i64> = sqlx::query_scalar(
        "select (payload->>'continues')::bigint from rbpmn_event \
         where instance_id = $1 and kind = 'timer-armed' order by id desc limit 1",
    )
    .bind(instance)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        continues,
        Some(first_no),
        "the re-arm names the occurrence it continues"
    );
    assert!(harness::fsck(&db.pool).await.is_empty());
    db.drop().await;
}

/// **Downtime is not a backlog.** A re-arm lands on the grid of the previous
/// due at the first occurrence *at or after* now, so an engine that was down
/// across three periods re-arms *once*, at the next occurrence. Stepping
/// blindly to `previous due + period` would leave the re-arm already past,
/// and nothing would slow the replay down: `drain_due_timers` picks it on the
/// very next pass and `Drain::Fired` never sleeps, so a day of downtime on an
/// `R/PT15M` boundary would spawn 96 side tokens back to back.
///
/// The occurrences the outage missed are skipped, never replayed — and
/// because a bounded `R<n>` counts *fires*, skipping them costs it nothing.
#[tokio::test]
async fn a_re_arm_skips_occurrences_missed_while_down() {
    let db = TestDb::create().await;
    let (engine, instance) = late_fee_engine(&db).await;

    // Down for three periods and a bit, on `R/P7D`.
    backdate_timers_by(&db.pool, instance, "22 days").await;
    let (_, overdue, _) = timer_dues(&db.pool, instance).await[0];
    assert!(engine.fire_due_timer().await.unwrap());

    let next = timer_dues(&db.pool, instance).await;
    assert_eq!(
        next.len(),
        1,
        "one occurrence armed, not one per missed period"
    );
    let (_, due, remaining) = next[0];
    assert!(remaining.is_none(), "R/… is unbounded");
    let now = db_epoch(&db.pool, "clock_timestamp()").await;
    assert!(
        due > now && due <= now + WEEK,
        "the re-arm is the next occurrence, not a past one: due {due}, now {now}"
    );
    let periods = (due - overdue) / WEEK;
    assert!(
        (periods - periods.round()).abs() < 1e-6,
        "still on the previous due's grid, not on now's: {periods} periods on"
    );
    assert_eq!(
        periods.round(),
        4.0,
        "the first whole period at or after now"
    );

    // The burst, in the one place it would show: a second pass finds nothing
    // due, and exactly one side token was spawned.
    assert!(
        !engine.fire_due_timer().await.unwrap(),
        "the scheduler has nothing left to fire — no catch-up burst"
    );
    let open = open_items(&db.pool, instance).await;
    assert_eq!(
        open.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>(),
        ["add_late_fee"],
        "one fire, one late fee — not one per missed week"
    );
    assert_eq!(event_count(&db.pool, instance, "timer-fired").await, 1);

    // A bounded cycle spends its count on fires. The same outage over `R2`
    // leaves one fire left, not none.
    engine.declare_topic("nudge").await.unwrap();
    engine
        .deploy(
            &fixture("accept/41-anchored-cycle.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let bounded = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(timer_dues(&db.pool, bounded.id).await[0].2, Some(2));
    backdate_timers_by(&db.pool, bounded.id, "22 days").await;
    assert!(engine.fire_due_timer().await.unwrap());
    let after = timer_dues(&db.pool, bounded.id).await;
    assert_eq!(after.len(), 1, "R2 re-armed once");
    assert_eq!(
        after[0].2,
        Some(1),
        "the fire spent one; the three occurrences the outage skipped spent nothing"
    );
    assert!(
        !engine.fire_due_timer().await.unwrap(),
        "and no burst on the bounded cycle either"
    );

    assert!(harness::fsck(&db.pool).await.is_empty());
    db.drop().await;
}

/// The host ending is what ends a cycle — and only the cycle: side work
/// already spawned runs to its end and keeps the instance alive until then.
#[tokio::test]
async fn host_completion_cancels_the_cycle_but_not_the_side_work() {
    let db = TestDb::create().await;
    let (engine, instance) = late_fee_engine(&db).await;
    backdate_timers(&db.pool, instance).await;
    assert!(engine.fire_due_timer().await.unwrap());
    assert_eq!(timer_rows(&db.pool, instance).await, 1, "re-armed");

    engine
        .correlate("PAID", "T-40", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        timer_rows(&db.pool, instance).await,
        0,
        "the arm went with the host"
    );
    assert_eq!(event_count(&db.pool, instance, "timer-cancelled").await, 1);
    assert_eq!(status_of(&db.pool, instance).await, "active");
    assert!(!engine.fire_due_timer().await.unwrap());

    let (fee, _) = open_items(&db.pool, instance).await[0].clone();
    engine
        .complete_work_item(fee, serde_json::json!({ "fees": 1 }))
        .await
        .unwrap();
    wait_for_status(&db.pool, instance, "completed").await;
    assert!(harness::fsck(&db.pool).await.is_empty());
    db.drop().await;
}

/// The anchor fixes the *phase*, not a set of instants: the first due is the
/// first occurrence at or after the arm, aligned to the anchor, and nothing
/// in the past is replayed. Checked for an anchor long past and one that may
/// still be ahead, with one assertion that is true either way.
#[tokio::test]
async fn an_anchored_cycle_starts_at_its_phase() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("nudge").await.unwrap();
    let fixture_xml = fixture("accept/41-anchored-cycle.bpmn");
    let past = fixture_xml
        .replace(
            "R2/2026-08-31T00:00:00+02:00/P7D",
            "R2/2020-01-06T00:00:00Z/P7D",
        )
        .replace("<bpmn:process id=\"p\"", "<bpmn:process id=\"past\"");
    engine
        .deploy(&fixture_xml, &Bindings::default())
        .await
        .unwrap();
    engine.deploy(&past, &Bindings::default()).await.unwrap();

    for (key, anchor) in [
        ("p", "'2026-08-31T00:00:00+02:00'::timestamptz"),
        ("past", "'2020-01-06T00:00:00Z'::timestamptz"),
    ] {
        let started = engine
            .start(key, None, serde_json::json!({}))
            .await
            .unwrap();
        let now = db_epoch(&db.pool, "clock_timestamp()").await;
        let anchor = db_epoch(&db.pool, anchor).await;
        let armed = timer_dues(&db.pool, started.id).await;
        assert_eq!(armed.len(), 1, "{key}: no catch-up burst, one occurrence");
        let (_, due, remaining) = armed[0];
        assert_eq!(remaining, Some(2), "{key}: R2 starts with two fires left");
        let floor = now.max(anchor);
        assert!(
            due >= floor - 1.0 && due < floor + WEEK,
            "{key}: the first occurrence at or after the arm, got {due} (now {now}, anchor {anchor})"
        );
        let phase = ((due - anchor) / WEEK).fract().abs();
        assert!(
            phase < 1e-6 || (1.0 - phase) < 1e-6,
            "{key}: aligned to the anchor's phase, off by {phase} weeks"
        );
        assert!(harness::fsck(&db.pool).await.is_empty());
    }
    db.drop().await;
}

/// `R2`: two fires and then nothing — no third row, and a scheduler that
/// knows it (`next_due_in` is None while the host is still open, so it is
/// idleness over live work, not an empty database).
#[tokio::test]
async fn a_bounded_cycle_exhausts() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("nudge").await.unwrap();
    engine
        .deploy(
            &fixture("accept/41-anchored-cycle.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    backdate_timers(&db.pool, started.id).await;
    let (_, first, _) = timer_dues(&db.pool, started.id).await[0];
    assert!(engine.fire_due_timer().await.unwrap());
    let second = timer_dues(&db.pool, started.id).await;
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].2, Some(1), "the last fire left");
    assert!((second[0].1 - (first + WEEK)).abs() < 0.001);

    backdate_timers(&db.pool, started.id).await;
    assert!(engine.fire_due_timer().await.unwrap());
    assert_eq!(
        timer_rows(&db.pool, started.id).await,
        0,
        "no third occurrence"
    );
    assert_eq!(event_count(&db.pool, started.id, "timer-armed").await, 2);
    assert_eq!(event_count(&db.pool, started.id, "timer-fired").await, 2);
    assert!(engine.next_due_in().await.unwrap().is_none());
    assert!(!engine.fire_due_timer().await.unwrap());

    let open = open_items(&db.pool, started.id).await;
    assert_eq!(
        open.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>(),
        ["await_signature", "nudge", "nudge"],
        "the host untouched, one side token per fire"
    );
    assert_eq!(status_of(&db.pool, started.id).await, "active");
    assert!(harness::fsck(&db.pool).await.is_empty());
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Scoped variable indexes: `definition` (per definition, today's behaviour)
// and `shared` (one index per field, across definitions).
// ---------------------------------------------------------------------------

/// EXPLAIN of a *parameterised* lookup. PREPARE and EXPLAIN must see the same
/// session, so both run on one pinned connection. Parameterised deliberately:
/// the shape an application actually issues is `definition_key = any($n)`, and
/// a literal array would be a different question.
async fn explain_prepared(pool: &PgPool, name: &str, prepare: &str, execute: &str) -> String {
    let mut conn = pool.acquire().await.unwrap();
    // Pooled connections come back with their prepared statements intact, so
    // a second call in one test collides on the name. Drop just ours —
    // `deallocate all` would take sqlx's own statement cache with it and
    // break every later query on this connection.
    let _ = sqlx::query(&format!("deallocate {name}"))
        .execute(&mut *conn)
        .await;
    sqlx::query(prepare).execute(&mut *conn).await.unwrap();
    let rows = sqlx::query(&format!("explain (costs off) {execute}"))
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    rows.iter()
        .map(|r| r.get::<String, _>(0))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The plan lines that mention `needle`, so an assertion can distinguish
/// "appears as an Index Cond" from "appears as a Filter".
fn plan_lines<'a>(plan: &'a str, needle: &str) -> Vec<&'a str> {
    plan.lines().filter(|l| l.contains(needle)).collect()
}

/// Both tables the plan assertions are about.
///
/// `rbpmn_work_item` was missing here, and that was not cosmetic: without
/// statistics the planner works from hardcoded defaults, so every plan
/// asserted below was a plan no real installation gets. It is the same
/// finding `just bench` records for the claim path, one altitude down.
async fn analyze(pool: &PgPool) {
    for table in ["rbpmn_instance", "rbpmn_work_item"] {
        sqlx::query(&format!("analyze {table}"))
            .execute(pool)
            .await
            .unwrap();
    }
}

/// Bulk instance rows for the planner's benefit.
///
/// Deliberately inserted as SQL rather than driven through `start`: a plan
/// test needs table statistics, not engine history, and the difference
/// between 600 rows and 40 000 is the difference between a hash join being
/// genuinely cheaper and the index being the only sane choice. Driving 40 000
/// instances through the step function would take minutes and prove the same
/// thing about the planner.
///
/// `half` of them carry `order_no`; the rest carry none at all — those are
/// exactly the rows a shared index's `IS NOT NULL` predicate keeps out.
async fn bulk_instances(pool: &PgPool, key: &str, prefix: &str, n: i32) {
    sqlx::query(
        "insert into rbpmn_instance \
           (definition_id, definition_key, definition_version, status, variables) \
         select d.id, d.key, d.version, 'active', \
                case when g % 2 = 0 \
                     then jsonb_build_object('order_no', $2 || g) \
                     else jsonb_build_object('channel', 'web') end \
           from rbpmn_definition d, generate_series(1, $3) g \
          where d.key = $1",
    )
    .bind(key)
    .bind(prefix)
    .bind(n)
    .execute(pool)
    .await
    .unwrap();
}

/// Two definitions hoisting the same business identifier, each with ~n
/// instances carrying it, plus a third definition that never carries it (the
/// rows a shared index's `IS NOT NULL` predicate keeps out).
async fn two_definitions_with(engine: &Engine, bindings: &Bindings, n: usize) {
    engine.declare_topic("warn_customer").await.unwrap();
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), bindings)
        .await
        .unwrap();
    engine
        .deploy(
            &fixture("accept/35-non-interrupting-on-subprocess.bpmn"),
            bindings,
        )
        .await
        .unwrap();
    for i in 0..n {
        engine
            .start(
                "p",
                None,
                serde_json::json!({ "order_no": format!("A-{i}") }),
            )
            .await
            .unwrap();
        engine
            .start(
                "shipment",
                None,
                serde_json::json!({ "order_no": format!("B-{i}") }),
            )
            .await
            .unwrap();
    }
}

/// Through the **published view**, which is how an application reaches this.
/// If the view ever stopped being a plain inlinable projection — a
/// `security_barrier`, a WHERE, a volatile function — the outside predicate
/// could no longer be pushed below it (`jsonb ->>` is not leakproof) and this
/// plan would collapse to a full scan.
const XDEF_PREPARE: &str = "prepare xdef(text, text[]) as \
     select id, definition_key from rbpmn_v_instance \
      where variables->>'order_no' = $1 and definition_key = any($2)";
const XDEF_EXECUTE: &str = "execute xdef('X-42', array['p','shipment'])";

/// The headline: a lookup across definitions cannot use the per-definition
/// indexes — Postgres can prove a partial predicate only from an equality
/// against a constant, and `definition_key = any($1)` is not one — but does
/// use the shared index, with the key set demoted to a filter.
#[tokio::test]
async fn shared_index_serves_the_cross_definition_lookup() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    two_definitions_with(&engine, &Bindings::new().index("order_no"), 5).await;
    bulk_instances(&db.pool, "p", "X-", 20_000).await;
    bulk_instances(&db.pool, "shipment", "Y-", 20_000).await;
    analyze(&db.pool).await;

    // Before: only the two definition-scoped indexes exist.
    let before = explain_prepared(&db.pool, "xdef", XDEF_PREPARE, XDEF_EXECUTE).await;
    for key in ["p", "shipment"] {
        assert!(
            !before.contains(&rbpmn_engine::declared_index_name(key, "order_no")),
            "a definition-scoped index cannot serve the cross-definition \
             lookup, but the plan used one:\n{before}"
        );
    }

    engine.declare_shared_index("order_no").await.unwrap();
    engine.declare_shared_index("order_no").await.unwrap(); // idempotent
    analyze(&db.pool).await;

    let after = explain_prepared(&db.pool, "xdef", XDEF_PREPARE, XDEF_EXECUTE).await;
    let shared = rbpmn_engine::shared_index_name("order_no");
    assert!(
        after.contains(&shared),
        "the shared index must serve the cross-definition lookup:\n{after}"
    );
    // The roles must invert: the field becomes the index qual, the key set a
    // filter applied afterwards. A `definition_key` index condition would mean
    // the planner led with the key set again.
    assert!(
        plan_lines(&after, "definition_key")
            .iter()
            .all(|l| !l.contains("Index Cond")),
        "definition_key must be a filter, not an index condition:\n{after}"
    );
    assert!(
        plan_lines(&after, "Index Cond")
            .iter()
            .any(|l| l.contains("order_no")),
        "the hoisted field must be the index condition:\n{after}"
    );
    db.drop().await;
}

/// The definition-scoped path is untouched by the existence of a shared
/// index: `TaskFilter`'s literal key still plans onto its own partial index,
/// which serves it strictly better (measured: a single index scan, versus a
/// BitmapAnd against the definition-key index when only the shared one is
/// available).
#[tokio::test]
async fn the_definition_scoped_path_is_unchanged_by_a_shared_index() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    two_definitions_with(&engine, &Bindings::new().index("order_no"), 5).await;
    bulk_instances(&db.pool, "p", "X-", 20_000).await;
    bulk_instances(&db.pool, "shipment", "Y-", 20_000).await;
    engine.declare_shared_index("order_no").await.unwrap();
    analyze(&db.pool).await;

    let plan = explain_prepared(
        &db.pool,
        "scoped",
        "prepare scoped(text) as select id from rbpmn_v_instance \
          where definition_key = 'p' and variables->>'order_no' = $1",
        "execute scoped('X-42')",
    )
    .await;
    assert!(
        plan.contains(&rbpmn_engine::declared_index_name("p", "order_no")),
        "the definition-scoped filter must keep using its own index:\n{plan}"
    );
    db.drop().await;
}

/// N definitions declaring the same shared field converge on ONE index —
/// by construction, because the name is derived from the field alone and
/// `IF NOT EXISTS` does the rest. No reference counting anywhere.
#[tokio::test]
async fn shared_declarations_converge_on_one_index() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    two_definitions_with(&engine, &Bindings::new().shared_index("order_no"), 2).await;

    let shared: i64 =
        sqlx::query_scalar("select count(*) from pg_class where relname like 'rbpmn\\_vixs\\_%'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(shared, 1, "two definitions, one shared index");
    let exists: bool =
        sqlx::query_scalar("select exists (select 1 from pg_class where relname = $1)")
            .bind(rbpmn_engine::shared_index_name("order_no"))
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(exists);
    // The shared DDL, pinned the same way: no definition key anywhere, and
    // the `is not null` predicate that keeps out every definition which never
    // carries the field.
    let ddl: String = sqlx::query_scalar("select indexdef from pg_indexes where indexname = $1")
        .bind(rbpmn_engine::shared_index_name("order_no"))
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        ddl,
        format!(
            "CREATE INDEX {} ON public.rbpmn_instance \
             USING btree (((variables ->> 'order_no'::text))) \
             WHERE ((variables ->> 'order_no'::text) IS NOT NULL)",
            rbpmn_engine::shared_index_name("order_no")
        )
    );
    // And no definition-scoped index was created alongside it.
    let scoped: i64 =
        sqlx::query_scalar("select count(*) from pg_class where relname like 'rbpmn\\_vix\\_%'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(scoped, 0);
    db.drop().await;
}

/// The two namespaces cannot collide, including for a definition whose key is
/// literally `shared`.
#[test]
fn index_names_are_domain_separated() {
    assert_ne!(
        rbpmn_engine::shared_index_name("f"),
        rbpmn_engine::declared_index_name("shared", "f")
    );
    assert!(rbpmn_engine::shared_index_name("f").starts_with("rbpmn_vixs_"));
    assert!(rbpmn_engine::declared_index_name("k", "f").starts_with("rbpmn_vix_"));
    // Postgres identifiers cap at 63 bytes, and the hash always survives.
    let long = rbpmn_engine::shared_index_name(&"f".repeat(200));
    assert!(long.len() <= 63, "{}", long.len());
}

/// Back-compat: the string form is definition-scoped and produces exactly the
/// index it always has; and spelling the default the long way is the *same
/// wiring*, so it hashes the same and does not allocate a version.
#[tokio::test]
async fn the_string_manifest_form_stays_definition_scoped() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;

    let from_json: Bindings = serde_json::from_str(r#"{"indexes":["channel"]}"#).unwrap();
    let first = engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &from_json)
        .await
        .unwrap();
    assert_eq!(first.version, 1);
    let exists: bool =
        sqlx::query_scalar("select exists (select 1 from pg_class where relname = $1)")
            .bind(rbpmn_engine::declared_index_name("p", "channel"))
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(exists, "the string form must build the index it always has");

    // Byte-for-byte against the DDL that shipped before scopes existed —
    // taken from a database built by the old code. This is not decoration:
    // the index name is derived from (key, field) only, so a drifted
    // predicate or expression would be silently kept by `IF NOT EXISTS` on
    // every deployment that already has one, and the drift would never
    // surface.
    let ddl: String = sqlx::query_scalar("select indexdef from pg_indexes where indexname = $1")
        .bind(rbpmn_engine::declared_index_name("p", "channel"))
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        ddl,
        "CREATE INDEX rbpmn_vix_p_channel_d70abfc6 ON public.rbpmn_instance \
         USING btree (((variables ->> 'channel'::text))) \
         WHERE (definition_key = 'p'::text)"
    );

    let spelled_out: Bindings =
        serde_json::from_str(r#"{"indexes":[{"field":"channel","scope":"definition"}]}"#).unwrap();
    let again = engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &spelled_out)
        .await
        .unwrap();
    assert!(
        again.reused && again.version == 1,
        "the same wiring spelled differently must not allocate a version"
    );
    db.drop().await;
}

/// One manifest saying both things about one field says nothing rbpmn can act
/// on, so it is refused before anything persists.
#[tokio::test]
async fn contradictory_scopes_in_one_manifest_are_refused() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let contradictory: Bindings =
        serde_json::from_str(r#"{"indexes":["order_no",{"field":"order_no","scope":"shared"}]}"#)
            .unwrap();
    let refused = engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &contradictory)
        .await;
    match refused {
        Err(DeployError::InvalidManifest(m)) => {
            assert!(m.contains("order_no") && m.contains("shared"), "{m}")
        }
        other => panic!("expected InvalidManifest, got {other:?}"),
    }
    let defs: i64 = sqlx::query_scalar("select count(*) from rbpmn_definition")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(defs, 0, "a rejected manifest must not deploy");
    db.drop().await;
}

/// An interrupted `CREATE INDEX CONCURRENTLY` leaves an *invalid* index that
/// `IF NOT EXISTS` would accept forever. Both scopes take the same recovery
/// path: drop the corpse, say so, and rebuild on the next call.
#[tokio::test]
async fn an_invalid_shared_index_is_dropped_and_reported() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine.declare_shared_index("order_no").await.unwrap();
    let name = rbpmn_engine::shared_index_name("order_no");

    // Exactly what an interrupted build leaves behind.
    sqlx::query("update pg_index set indisvalid = false where indexrelid = $1::regclass")
        .bind(&name)
        .execute(&db.pool)
        .await
        .unwrap();

    match engine.declare_shared_index("order_no").await {
        Err(rbpmn_engine::EngineError::InvalidVariables(m)) => {
            assert!(m.contains(&name) && m.contains("invalid"), "{m}")
        }
        other => panic!("expected the loud invalid-index error, got {other:?}"),
    }
    let gone: bool =
        sqlx::query_scalar("select not exists (select 1 from pg_class where relname = $1)")
            .bind(&name)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(gone, "the corpse must be dropped, not kept");

    engine.declare_shared_index("order_no").await.unwrap();
    let valid: bool = sqlx::query_scalar(
        "select i.indisvalid from pg_class c join pg_index i on i.indexrelid = c.oid \
         where c.relname = $1",
    )
    .bind(&name)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(valid, "the next call must rebuild it");
    db.drop().await;
}

/// Two deploys racing on the same shared field. The advisory lock makes this
/// deterministic instead of resting on an unstated Postgres property: both
/// succeed, and exactly one valid index exists afterwards.
#[tokio::test]
async fn concurrent_deploys_of_one_shared_field() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("warn_customer").await.unwrap();

    // Genuinely separate nodes: separate pools over one database.
    let a = Engine::builder(sqlx::PgPool::connect(&db.url()).await.unwrap()).build();
    let b = Engine::builder(sqlx::PgPool::connect(&db.url()).await.unwrap()).build();
    let bindings = Bindings::new().shared_index("order_no");

    let minimal = fixture("accept/01-minimal.bpmn");
    let shipment = fixture("accept/35-non-interrupting-on-subprocess.bpmn");
    let (ra, rb) = tokio::join!(
        a.deploy(&minimal, &bindings),
        b.deploy(&shipment, &bindings),
    );
    ra.unwrap();
    rb.unwrap();

    let indexes: Vec<(String, bool)> = sqlx::query_as(
        "select c.relname, i.indisvalid from pg_class c \
         join pg_index i on i.indexrelid = c.oid \
         where c.relname like 'rbpmn\\_vixs\\_%'",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(indexes.len(), 1, "one shared field, one index: {indexes:?}");
    assert!(indexes[0].1, "and it must be valid: {indexes:?}");
    db.drop().await;
}

/// The lifecycle answer, made visible. Nothing drops a declared index — not
/// `delete_definition`, not retention, not dropping the field from the
/// manifest — so the audit is how an operator finds what is left over, and
/// how a shared index still needed by a *second* definition is shown to be
/// safe when the first goes away.
#[tokio::test]
async fn declared_indexes_reports_orphans_and_shared_survivors() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("warn_customer").await.unwrap();
    let bindings = Bindings::new().index("channel").shared_index("order_no");
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &bindings)
        .await
        .unwrap();
    engine
        .deploy(
            &fixture("accept/35-non-interrupting-on-subprocess.bpmn"),
            &bindings,
        )
        .await
        .unwrap();

    let shared_name = rbpmn_engine::shared_index_name("order_no");
    let audit = engine.declared_indexes().await.unwrap();
    let shared = audit.iter().find(|i| i.name == shared_name).unwrap();
    assert_eq!(shared.declared_by.len(), 2, "{shared:?}");
    assert!(shared.present && shared.valid);
    assert_eq!(shared.scope, Some(rbpmn_engine::IndexScope::Shared));

    // Remove one of the two definitions entirely. Nothing is dropped.
    engine.delete_definition("p", 1).await.unwrap();
    let audit = engine.declared_indexes().await.unwrap();

    let shared = audit.iter().find(|i| i.name == shared_name).unwrap();
    assert_eq!(
        shared.declared_by,
        vec!["shipment v1".to_string()],
        "the surviving definition still declares it — it must not read as an orphan"
    );
    assert!(shared.present && shared.valid);

    // The departed definition's own index is still there, and now orphaned:
    // the field name is gone with the manifest, because the name is one-way.
    let orphan = audit
        .iter()
        .find(|i| i.name == rbpmn_engine::declared_index_name("p", "channel"))
        .expect("the orphan is still in the catalogue");
    assert!(orphan.declared_by.is_empty(), "{orphan:?}");
    assert!(orphan.present, "nothing drops a declared index");
    assert_eq!(orphan.field, None);
    db.drop().await;
}

/// The view is public API, so its shape is asserted the way rule ids and the
/// `Event` display format are: columns may be added, never removed or
/// repurposed.
#[tokio::test]
async fn the_published_view_has_the_documented_shape() {
    let db = TestDb::create().await;
    let _engine = engine(&db).await;

    let columns: Vec<(String, String)> = sqlx::query_as(
        "select column_name::text, data_type::text from information_schema.columns \
         where table_name = 'rbpmn_v_instance' order by ordinal_position",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("id".into(), "uuid".into()),
            ("definition_key".into(), "text".into()),
            ("definition_version".into(), "integer".into()),
            ("business_key".into(), "text".into()),
            ("status".into(), "text".into()),
            ("variables".into(), "jsonb".into()),
            ("created_at".into(), "timestamp with time zone".into()),
            ("completed_at".into(), "timestamp with time zone".into()),
        ],
        "rbpmn_v_instance is public API — adding a column is fine, changing \
         or removing one is a breaking change"
    );

    // A barrier view would refuse to push `variables->>'f' = $1` below itself,
    // because `jsonb ->>` is not leakproof — and every declared variable index
    // would then sit unused beneath a full scan.
    let barrier: Option<Vec<String>> =
        sqlx::query_scalar("select reloptions from pg_class where relname = 'rbpmn_v_instance'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        barrier.is_none(),
        "the view must carry no reloptions at all, and above all not \
         security_barrier: {barrier:?}"
    );
    db.drop().await;
}

/// What an application actually does: its own table, its own tenancy filter,
/// its own ordering, joined against rbpmn's published view on the identifier
/// it hoisted into `variables`. This is the join no data-returning API can do
/// as well, and the reason the view exists — so it must plan onto the shared
/// index, not scan every instance in the system.
#[tokio::test]
async fn an_application_joins_its_own_table_against_the_published_view() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    two_definitions_with(&engine, &Bindings::new().shared_index("order_no"), 5).await;
    bulk_instances(&db.pool, "p", "X-", 20_000).await;
    bulk_instances(&db.pool, "shipment", "Y-", 20_000).await;

    // The application's own rows, keyed by the same business identifier.
    // Three of them belong to the tenant asking — the shape that makes a
    // nested loop into the shared index the right plan.
    sqlx::query("create table app_order (order_id text primary key, tenant text not null)")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "insert into app_order (order_id, tenant) \
         select 'X-' || g, case when g <= 6 then 'acme' else 'other' end \
         from generate_series(2, 20000, 2) g",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("analyze app_order")
        .execute(&db.pool)
        .await
        .unwrap();
    analyze(&db.pool).await;

    let plan = explain_prepared(
        &db.pool,
        "tenant_inbox",
        "prepare tenant_inbox(text) as \
           select i.id, i.definition_key, i.status, o.order_id \
             from app_order o \
             join rbpmn_v_instance i on i.variables->>'order_no' = o.order_id \
            where o.tenant = $1 \
            order by i.created_at",
        "execute tenant_inbox('acme')",
    )
    .await;

    let shared = rbpmn_engine::shared_index_name("order_no");
    assert!(
        plan.contains(&shared),
        "an application's join must reach the shared index:\n{plan}"
    );
    assert!(
        !plan.contains("Subquery Scan"),
        "the view must be inlined, not materialised as a subquery:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan on rbpmn_instance"),
        "the join must not fall back to scanning every instance:\n{plan}"
    );

    // And it returns what the application asked for: its three acme orders,
    // resolved across two different definitions without naming either.
    let rows: Vec<(uuid::Uuid, String, String, String)> = sqlx::query_as(
        "select i.id, i.definition_key, i.status, o.order_id \
           from app_order o \
           join rbpmn_v_instance i on i.variables->>'order_no' = o.order_id \
          where o.tenant = $1 order by o.order_id",
    )
    .bind("acme")
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows.iter().all(|r| r.1 == "p"));
    assert_eq!(
        rows.iter().map(|r| r.3.as_str()).collect::<Vec<_>>(),
        vec!["X-2", "X-4", "X-6"]
    );
    db.drop().await;
}

/// The no-SQL entry point: index-backed by construction, bounded, and loud
/// when the index it promises is not there.
#[tokio::test]
async fn find_by_shared_index_resolves_across_definitions() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    two_definitions_with(&engine, &Bindings::new().shared_index("order_no"), 50).await;

    let found = engine
        .find_by_shared_index("order_no", "A-7", 10)
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].definition_key, "p");

    // The same identifier carried by two definitions resolves to both — the
    // whole point of not knowing which workflow holds it.
    engine
        .start(
            "shipment",
            Some("bk-7"),
            serde_json::json!({ "order_no": "A-7" }),
        )
        .await
        .unwrap();
    let found = engine
        .find_by_shared_index("order_no", "A-7", 10)
        .await
        .unwrap();
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(
        found
            .iter()
            .map(|m| m.definition_key.as_str())
            .collect::<Vec<_>>(),
        vec!["p", "shipment"],
        "oldest first, deterministically"
    );
    assert_eq!(found[1].business_key.as_deref(), Some("bk-7"));

    // Bounded, and the bound is enforced at both ends.
    assert_eq!(
        engine
            .find_by_shared_index("order_no", "A-7", 1)
            .await
            .unwrap()
            .len(),
        1
    );
    for bad in [0, rbpmn_engine::MAX_FIND_LIMIT + 1] {
        assert!(matches!(
            engine.find_by_shared_index("order_no", "A-7", bad).await,
            Err(rbpmn_engine::EngineError::InvalidVariables(_))
        ));
    }
    db.drop().await;
}

/// Refused, not silently sequential-scanned: the call's whole contract is
/// that it is index-backed, and "correct but catastrophically slow" is the
/// "seems to run" failure this project rejects everywhere else.
#[tokio::test]
async fn find_by_shared_index_refuses_an_undeclared_field() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    // Declared, but definition-scoped — which is not the index this needs.
    two_definitions_with(&engine, &Bindings::new().index("order_no"), 2).await;

    match engine.find_by_shared_index("order_no", "A-1", 10).await {
        Err(rbpmn_engine::EngineError::UndeclaredSharedIndex { field, index }) => {
            assert_eq!(field, "order_no");
            assert_eq!(index, rbpmn_engine::shared_index_name("order_no"));
        }
        other => panic!("expected UndeclaredSharedIndex, got {other:?}"),
    }

    // Injection-shaped field names never reach SQL, index or no index.
    assert!(matches!(
        engine.find_by_shared_index("x') or ('1'='1", "v", 10).await,
        Err(rbpmn_engine::EngineError::InvalidVariables(_))
    ));
    db.drop().await;
}

/// A hazard that **predates** the shared scope: two definitions deploying at
/// once each build a `CREATE INDEX CONCURRENTLY` on `rbpmn_instance`, and two
/// concurrent builds on one table deadlock — whether or not they name the same
/// index. Two different fields, therefore, and no shared scope in sight.
#[tokio::test]
async fn concurrent_deploys_of_different_indexes_do_not_deadlock() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("warn_customer").await.unwrap();

    let a = Engine::builder(sqlx::PgPool::connect(&db.url()).await.unwrap()).build();
    let b = Engine::builder(sqlx::PgPool::connect(&db.url()).await.unwrap()).build();
    let minimal = fixture("accept/01-minimal.bpmn");
    let shipment = fixture("accept/35-non-interrupting-on-subprocess.bpmn");

    let channel = Bindings::new().index("channel");
    let region = Bindings::new().index("region");
    let (ra, rb) = tokio::join!(a.deploy(&minimal, &channel), b.deploy(&shipment, &region),);
    ra.unwrap();
    rb.unwrap();

    for (key, field) in [("p", "channel"), ("shipment", "region")] {
        let exists: bool =
            sqlx::query_scalar("select exists (select 1 from pg_class where relname = $1)")
                .bind(rbpmn_engine::declared_index_name(key, field))
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(exists, "{key}/{field} was not built");
    }
    db.drop().await;
}

/// The recovery stampede the shared scope makes routine: one corpse, several
/// declarers finding it at once. Whatever order they arrive in, nobody
/// deadlocks and the next declaration leaves exactly one valid index.
#[tokio::test]
async fn a_recovery_stampede_leaves_one_valid_index() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine.declare_shared_index("order_no").await.unwrap();
    let name = rbpmn_engine::shared_index_name("order_no");
    sqlx::query("update pg_index set indisvalid = false where indexrelid = $1::regclass")
        .bind(&name)
        .execute(&db.pool)
        .await
        .unwrap();

    let a = Engine::builder(sqlx::PgPool::connect(&db.url()).await.unwrap()).build();
    let b = Engine::builder(sqlx::PgPool::connect(&db.url()).await.unwrap()).build();
    let (ra, rb) = tokio::join!(
        a.declare_shared_index("order_no"),
        b.declare_shared_index("order_no"),
    );
    // Each declarer either recovered the corpse (loudly) or arrived after the
    // recovery and rebuilt. Neither is allowed to be a deadlock or a panic.
    for r in [&ra, &rb] {
        if let Err(e) = r {
            assert!(
                matches!(e, rbpmn_engine::EngineError::InvalidVariables(m) if m.contains(&name)),
                "unexpected error: {e:?}"
            );
        }
    }
    engine.declare_shared_index("order_no").await.unwrap();
    let valid: Vec<bool> = sqlx::query_scalar(
        "select i.indisvalid from pg_class c join pg_index i on i.indexrelid = c.oid \
         where c.relname = $1",
    )
    .bind(&name)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(valid, vec![true], "exactly one valid index at the end");
    db.drop().await;
}

// ---------------------------------------------------------------------------
// The published work-item view: rbpmn_v_work_item, and the claimability it
// computes so applications do not have to.
// ---------------------------------------------------------------------------

/// `(waiting, in_progress)` straight from the view, the way a dashboard asks.
async fn view_depth(pool: &PgPool, key: &str, topic: &str) -> (i64, i64) {
    let row = sqlx::query(&format!(
        "select count(*) filter (where claimable) as waiting, \
                count(*) filter (where in_progress) as in_progress \
           from {} where definition_key = $1 and topic = $2",
        rbpmn_engine::WORK_ITEM_VIEW
    ))
    .bind(key)
    .bind(topic)
    .fetch_one(pool)
    .await
    .unwrap();
    (row.get("waiting"), row.get("in_progress"))
}

/// **The property that matters.** For a topic with N claimable items and no
/// competing worker, exactly N consecutive `get_task` calls succeed and the
/// N+1st returns None — and the view's count agrees at every single step.
///
/// A dashboard whose depths disagree with what the engine actually hands out
/// is worse than no dashboard, so this is the test the whole surface exists
/// to pass. It walks the count down one claim at a time rather than checking
/// only the endpoints: an off-by-one, or a claimable rule that disagreed
/// about lapsed leases, would sit in the middle of the walk.
#[tokio::test]
async fn the_views_depth_agrees_with_what_get_task_hands_out() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();

    const N: i64 = 12;
    for _ in 0..N {
        engine
            .start("p", None, serde_json::json!({}))
            .await
            .unwrap();
    }
    assert_eq!(view_depth(&db.pool, "p", "review").await, (N, 0));

    for taken in 0..N {
        assert_eq!(
            view_depth(&db.pool, "p", "review").await,
            (N - taken, taken),
            "the view disagreed after {taken} claim(s)"
        );
        let claimed = engine
            .get_task("review", &GetTaskOptions::new("w1"))
            .await
            .unwrap();
        assert!(
            claimed.is_some(),
            "claim {} of {N} should have succeeded while the view said {} waiting",
            taken + 1,
            N - taken
        );
    }

    assert_eq!(view_depth(&db.pool, "p", "review").await, (0, N));
    assert!(
        engine
            .get_task("review", &GetTaskOptions::new("w1"))
            .await
            .unwrap()
            .is_none(),
        "the N+1st claim must find nothing"
    );
    // And the typed call reports the same thing it does.
    let depths = engine.queue_depths(&["p".to_string()]).await.unwrap();
    assert_eq!(
        depths,
        vec![rbpmn_engine::QueueDepth {
            definition_key: "p".into(),
            topic: "review".into(),
            waiting: 0,
            in_progress: N as u64,
        }]
    );
    db.drop().await;
}

/// The view's `claimable` column against the very string the claim path
/// claims by — a *behavioural* differential, so it survives either side being
/// rewritten rather than merely re-typed. The corpus below deliberately
/// contains every edge the rule has: available, a live lease, a lapsed lease,
/// backoff not yet due, backoff come due, completed, cancelled, failed, and a
/// frozen instance holding an otherwise-perfect item.
#[tokio::test]
async fn the_view_and_the_claim_predicate_cannot_drift() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    for _ in 0..9 {
        engine
            .start("p", None, serde_json::json!({}))
            .await
            .unwrap();
    }
    // Paint every state directly: this is a predicate test, and driving each
    // edge through the engine would prove less about more.
    let ids: Vec<uuid::Uuid> =
        sqlx::query_scalar("select id from rbpmn_work_item order by created_at, item_no")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    let paint = |sql: &'static str, id: uuid::Uuid| {
        let pool = db.pool.clone();
        async move {
            sqlx::query(sql).bind(id).execute(&pool).await.unwrap();
        }
    };
    paint(
        "update rbpmn_work_item set state='locked', lock_until=now()+interval '10 min' where id=$1",
        ids[1],
    )
    .await;
    paint(
        "update rbpmn_work_item set state='locked', lock_until=now()-interval '10 min' where id=$1",
        ids[2],
    )
    .await;
    paint(
        "update rbpmn_work_item set retry_at=now()+interval '1 hour' where id=$1",
        ids[3],
    )
    .await;
    paint(
        "update rbpmn_work_item set retry_at=now()-interval '1 hour' where id=$1",
        ids[4],
    )
    .await;
    for (i, state) in [(5, "completed"), (6, "cancelled"), (7, "failed")] {
        sqlx::query(&format!(
            "update rbpmn_work_item set state='{state}' where id=$1"
        ))
        .bind(ids[i])
        .execute(&db.pool)
        .await
        .unwrap();
    }
    // A perfectly claimable item whose instance has frozen on an incident.
    sqlx::query(
        "update rbpmn_instance set status='failed' where id = \
         (select instance_id from rbpmn_work_item where id=$1)",
    )
    .bind(ids[8])
    .execute(&db.pool)
    .await
    .unwrap();
    // And the pathological row the totality argument is about: locked with no
    // lock_until at all, which no code path writes but nothing forbids.
    sqlx::query("update rbpmn_work_item set state='locked', lock_until=null where id=$1")
        .bind(ids[0])
        .execute(&db.pool)
        .await
        .unwrap();

    let disagreements: i64 = sqlx::query_scalar(&format!(
        "select count(*) from {view} v \
           join rbpmn_work_item w on w.id = v.id \
           join rbpmn_instance i on i.id = w.instance_id \
          where v.claimable is distinct from ({claimable}) \
             or v.in_progress is distinct from ({in_progress})",
        view = rbpmn_engine::WORK_ITEM_VIEW,
        claimable = rbpmn_engine::testing::CLAIMABLE_SQL,
        in_progress = rbpmn_engine::testing::IN_PROGRESS_SQL,
    ))
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        disagreements, 0,
        "the view's claimability disagreed with the predicate get_task uses"
    );

    // `is distinct from` above would pass if BOTH sides were NULL, so the
    // totality the read model promises is asserted separately: a boolean
    // column an application can trust to split the world in two.
    let nulls: i64 = sqlx::query_scalar(&format!(
        "select count(*) from {} where claimable is null or in_progress is null",
        rbpmn_engine::WORK_ITEM_VIEW
    ))
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(nulls, 0, "claimable/in_progress must never be null");

    // Disjoint by construction: a live lease is not claimable, a lapsed one
    // is not in progress.
    let both: i64 = sqlx::query_scalar(&format!(
        "select count(*) from {} where claimable and in_progress",
        rbpmn_engine::WORK_ITEM_VIEW
    ))
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(both, 0);
    db.drop().await;
}

/// Public API, asserted the way `rbpmn_v_instance`'s shape is: columns may be
/// added, never removed or repurposed.
#[tokio::test]
async fn the_published_work_item_view_has_the_documented_shape() {
    let db = TestDb::create().await;
    let _engine = engine(&db).await;

    let columns: Vec<(String, String)> = sqlx::query_as(
        "select column_name::text, data_type::text from information_schema.columns \
         where table_name = 'rbpmn_v_work_item' order by ordinal_position",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("id".into(), "uuid".into()),
            ("instance_id".into(), "uuid".into()),
            ("item_no".into(), "bigint".into()),
            ("definition_key".into(), "text".into()),
            ("definition_version".into(), "integer".into()),
            ("element_id".into(), "text".into()),
            ("topic".into(), "text".into()),
            ("kind".into(), "text".into()),
            ("state".into(), "text".into()),
            ("claimable".into(), "boolean".into()),
            ("in_progress".into(), "boolean".into()),
            ("lock_owner".into(), "text".into()),
            ("lock_until".into(), "timestamp with time zone".into()),
            ("retry_at".into(), "timestamp with time zone".into()),
            ("retries".into(), "integer".into()),
            ("failures".into(), "integer".into()),
            ("last_failure".into(), "text".into()),
            ("created_at".into(), "timestamp with time zone".into()),
        ],
        "rbpmn_v_work_item is public API"
    );

    // Not security_barrier — a barrier view stops an outside predicate being
    // pushed below it, and the depth index would go unused beneath a scan.
    let barrier: Option<Vec<String>> =
        sqlx::query_scalar("select reloptions from pg_class where relname = 'rbpmn_v_work_item'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(barrier.is_none(), "must carry no reloptions: {barrier:?}");
    db.drop().await;
}

/// A lease is a loan, and the view has to say so: while it is live the item
/// is in progress and not waiting; the moment it lapses it is waiting again,
/// because that is exactly when `get_task` will hand it to someone else.
#[tokio::test]
async fn a_lapsed_lease_returns_to_the_queue_and_a_live_one_does_not() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(view_depth(&db.pool, "p", "review").await, (1, 0));

    // A live lease: held, not waiting.
    let mut options = GetTaskOptions::new("w1");
    options.ttl = Duration::from_millis(20);
    let task = engine.get_task("review", &options).await.unwrap().unwrap();
    assert_eq!(view_depth(&db.pool, "p", "review").await, (0, 1));

    // Let it lapse. Waiting again, and no longer in progress — and the engine
    // agrees, because it hands the very same item to the next caller.
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(view_depth(&db.pool, "p", "review").await, (1, 0));
    let again = engine
        .get_task("review", &GetTaskOptions::new("w2"))
        .await
        .unwrap()
        .expect("a lapsed lease must be claimable again");
    assert_eq!(again.id, task.id);
    db.drop().await;
}

/// Retry backoff is a promise not to try again yet. A dashboard that counted
/// a backed-off item as waiting would send someone to a queue the engine will
/// refuse to serve from.
#[tokio::test]
async fn an_item_in_retry_backoff_is_not_waiting_until_it_is_due() {
    let db = TestDb::create().await;
    // A real backoff, not the tests' usual zero.
    let engine = Engine::builder(db.pool.clone())
        .retry_backoff(Duration::from_secs(3600))
        .build();
    engine.migrate().await.unwrap();
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    let task = engine
        .get_task("review", &GetTaskOptions::new("w1"))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        engine
            .fail_task(task.id, "w1", None, Some("nope".into()))
            .await
            .unwrap(),
        FailOutcome::Retrying { .. }
    ));

    // Available again, but not due: neither waiting nor in progress.
    assert_eq!(view_depth(&db.pool, "p", "review").await, (0, 0));
    assert!(
        engine
            .get_task("review", &GetTaskOptions::new("w2"))
            .await
            .unwrap()
            .is_none(),
        "the engine must refuse it too, or the view is lying"
    );

    // Travel to when it comes due; both agree again.
    sqlx::query("update rbpmn_work_item set retry_at = now() - interval '1 second'")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(view_depth(&db.pool, "p", "review").await, (1, 0));
    assert!(
        engine
            .get_task("review", &GetTaskOptions::new("w2"))
            .await
            .unwrap()
            .is_some()
    );
    db.drop().await;
}

/// Closed is closed: a completed item and one the process withdrew both leave
/// every bucket for good. The cancellation here is real — a message boundary
/// takes the task out from under its holder — rather than a state painted on
/// by the test.
#[tokio::test]
async fn closed_items_leave_the_queue_for_good() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    let task = engine
        .get_task("review", &GetTaskOptions::new("w1"))
        .await
        .unwrap()
        .unwrap();
    engine
        .complete_task(task.id, "w1", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(view_depth(&db.pool, "p", "review").await, (0, 0));

    // The withdrawn one, through the boundary that withdraws it.
    engine
        .deploy(
            &fixture("accept/29-message-boundary.bpmn"),
            &contest_bindings(),
        )
        .await
        .unwrap();
    engine
        .start(
            "ticket",
            None,
            serde_json::json!({ "ticket": { "reference": "T-1" } }),
        )
        .await
        .unwrap();
    assert_eq!(
        view_depth(&db.pool, "ticket", "handle_contest").await,
        (1, 0)
    );
    let clerk = engine
        .get_task("handle_contest", &GetTaskOptions::new("clerk"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        view_depth(&db.pool, "ticket", "handle_contest").await,
        (0, 1)
    );

    engine
        .correlate(
            "PAID",
            "T-1",
            serde_json::json!({ "payment": { "amount": 1 } }),
        )
        .await
        .unwrap();
    let state: String = sqlx::query_scalar("select state from rbpmn_work_item where id = $1")
        .bind(clerk.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(state, "cancelled");
    assert_eq!(
        view_depth(&db.pool, "ticket", "handle_contest").await,
        (0, 0),
        "a withdrawn item is neither waiting nor in progress"
    );
    db.drop().await;
}

/// An instance frozen on an incident keeps its work items exactly where they
/// were — and none of them may be handed out. This is why `claimable` needs
/// the instance at all, and it is asserted on a *sibling*: two parallel user
/// tasks, one driven to an incident, the other still `available` and still
/// gone from the queue.
#[tokio::test]
async fn a_frozen_instance_holds_its_sibling_work_out_of_the_queue() {
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
    assert_eq!(view_depth(&db.pool, "p", "ta").await, (1, 0));
    assert_eq!(view_depth(&db.pool, "p", "tb").await, (1, 0));

    // Drive `ta` to an incident: three failures with the tests' zero backoff.
    let ta = engine
        .get_task("ta", &GetTaskOptions::new("w1"))
        .await
        .unwrap()
        .unwrap();
    for _ in 0..2 {
        assert!(matches!(
            engine
                .fail_task(ta.id, "w1", None, Some("boom".into()))
                .await
                .unwrap(),
            FailOutcome::Retrying { .. }
        ));
        engine
            .get_task("ta", &GetTaskOptions::new("w1"))
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(
        engine
            .fail_task(ta.id, "w1", None, Some("boom".into()))
            .await
            .unwrap(),
        FailOutcome::IncidentRaised
    );
    wait_for_status(&db.pool, started.id, "failed").await;

    // `tb` never failed and is still `available` — but the instance is frozen,
    // so it is not work anyone can take, and the view must not offer it.
    let tb_state: String = sqlx::query_scalar(
        "select state from rbpmn_work_item where instance_id = $1 and element_id = 'tb'",
    )
    .bind(started.id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        tb_state, "available",
        "the sibling is untouched by the incident"
    );
    assert_eq!(
        view_depth(&db.pool, "p", "tb").await,
        (0, 0),
        "a frozen instance's untouched sibling must still leave the queue"
    );
    assert!(
        engine
            .get_task("tb", &GetTaskOptions::new("w2"))
            .await
            .unwrap()
            .is_none(),
        "and the engine must refuse it, or the view is lying"
    );
    db.drop().await;
}

/// Bulk work items, for the planner's benefit — same reasoning as
/// `bulk_instances`: a plan test needs table statistics, not engine history.
/// `open_n` of every instance's items are left open and the rest completed,
/// which is the shape a real deployment has: closed work dominates the table
/// forever, and that is exactly what the partial index exists to skip.
async fn bulk_work_items(pool: &PgPool, key: &str, topics: &[&str], open_n: i32, closed_n: i32) {
    for (t, topic) in topics.iter().enumerate() {
        sqlx::query(
            "insert into rbpmn_work_item \
               (instance_id, item_no, definition_id, definition_key, definition_version, \
                token_no, kind, topic, element_id, state) \
             select i.id, $4 * 1000 + g, i.definition_id, i.definition_key, \
                    i.definition_version, 0, 'user', $2, $2, \
                    case when g <= $3 then 'available' else 'completed' end \
               from rbpmn_instance i, generate_series(1, $3 + $5) g \
              where i.definition_key = $1",
        )
        .bind(key)
        .bind(topic)
        .bind(open_n)
        .bind(t as i32 + 1)
        .bind(closed_n)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// The motivating query, planned THROUGH the view: "for every queue, how many
/// are waiting?" must be index-driven, not a scan of every work item ever
/// created. Issued through the view on purpose — a `security_barrier` or a
/// non-inlinable definition would silently defeat the index, and the shape
/// test alone would not notice.
#[tokio::test]
async fn the_grouped_depth_query_is_index_driven() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();
    bulk_instances(&db.pool, "p", "Z-", 2_000).await;
    // 2 open per topic against 18 closed: ~12k open in a ~120k-row table.
    bulk_work_items(&db.pool, "p", &["alpha", "beta", "gamma"], 2, 18).await;
    // A second definition, three times the size, because "filtered by a key
    // set" only means anything when the set is a *subset*. With one
    // definition in the table the predicate matches every row, the leading
    // index column filters nothing, and the planner is right to ignore
    // `rbpmn_work_item_depth` — which is what it did, and what this test used
    // to miss by asserting against a table with no statistics.
    engine.declare_topic("warn_customer").await.unwrap();
    engine
        .deploy(
            &fixture("accept/35-non-interrupting-on-subprocess.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    bulk_instances(&db.pool, "shipment", "Y-", 6_000).await;
    bulk_work_items(&db.pool, "shipment", &["delta", "epsilon", "zeta"], 2, 18).await;
    analyze(&db.pool).await;

    let plan = explain_prepared(
        &db.pool,
        "depths",
        &format!(
            "prepare depths as select definition_key, topic, count(*) \
               from {} where claimable group by 1, 2",
            rbpmn_engine::WORK_ITEM_VIEW
        ),
        "execute depths",
    )
    .await;

    // Unfiltered, the whole-installation question: index-driven over the open
    // set only. Note it is the *pre-existing* `rbpmn_work_item_fifo` that
    // serves this — same partial predicate, and with one key to group by the
    // new index offers nothing over it. Asserted as the property that matters
    // rather than by index name, because which partial index wins here is the
    // planner's business; that none of them is a full scan is ours.
    assert!(
        !plan.contains("Seq Scan on rbpmn_work_item"),
        "the depth query must not scan every work item in the system:\n{plan}"
    );
    assert!(
        plan.contains("state = ANY ('{available,locked}'::text[])"),
        "and must be driven by a partial index over the open states:\n{plan}"
    );
    assert!(
        !plan.contains("Subquery Scan"),
        "the view must be inlined, not materialised:\n{plan}"
    );

    // Filtered by a key set — the shape `queue_depths` issues, and the shape
    // a real dashboard issues, because a user works *some* queues. This is
    // where `rbpmn_work_item_depth` earns its keep: `definition_key` becomes
    // an index condition instead of a filter, and the instance join collapses
    // from hashing every instance to a nested loop on the primary key.
    let filtered = explain_prepared(
        &db.pool,
        "depths_f",
        &format!(
            "prepare depths_f(text[]) as select definition_key, topic, count(*) \
               from {} where claimable and definition_key = any($1) group by 1, 2",
            rbpmn_engine::WORK_ITEM_VIEW
        ),
        "execute depths_f(array['p'])",
    )
    .await;
    // True on every supported version, and the property that actually
    // protects the table: whatever index wins, none of them is a full scan.
    assert!(
        !filtered.contains("Seq Scan on rbpmn_work_item"),
        "the filtered depth query must not scan every work item:\n{filtered}"
    );

    // The rest is asserted from 18 up, which is what development and CI run
    // and therefore the only version this plan has been observed on. 13 is
    // the floor the schema needs, not a version anyone has promised optimal
    // plans on, so an older laptop warns rather than reddening a suite over a
    // planner's choice. Un-gate this the day the plan is checked there.
    if server_version_num(&db.pool).await >= 180_000 {
        assert!(
            filtered.contains("rbpmn_work_item_depth"),
            "the filtered depth query must reach the depth index:\n{filtered}"
        );
        assert!(
            filtered
                .lines()
                .any(|l| l.contains("Index Cond") && l.contains("definition_key")),
            "and definition_key must be an index condition, not a filter:\n{filtered}"
        );
        // How the instance join executes is deliberately NOT asserted, and
        // migration 0015's comment is wrong about it. It says the join
        // "collapses from hashing every instance to a nested loop on the
        // primary key"; that was measured against a table with no statistics,
        // and with them the planner hashes several thousand active instances
        // instead — correctly, because which join wins is a function of how
        // many are live. The correction lives here rather than there because
        // a migration's whole text is checksummed (`MigrationDrift`), so
        // editing a comment in a released one would refuse to boot every
        // database that already applied it.
    } else {
        warn_out_of_band(
            "the queue-depth index assertions were skipped: they have only \
             been observed on PostgreSQL 18, which is what development and CI \
             run. Nothing is known to be wrong here — nothing is known at all.",
        );
    }
    db.drop().await;
}

/// `server_version_num` — 180000 for 18.0. Two plan assertions have a
/// version-dependent answer and say so rather than asserting the newest
/// planner's behaviour everywhere.
async fn server_version_num(pool: &PgPool) -> i32 {
    sqlx::query_scalar("select current_setting('server_version_num')::int")
        .fetch_one(pool)
        .await
        .expect("server_version_num")
}

/// A warning a *passing* test can actually be heard making.
///
/// The harness captures `println!` and `eprintln!` and shows them only for
/// failures or under `--nocapture`, so a warning written that way is a
/// warning nobody reads. A direct write to the process's stderr is not
/// captured — verified, not assumed.
fn warn_out_of_band(message: &str) {
    use std::io::Write as _;
    let mut err = std::io::stderr();
    let _ = writeln!(err, "\nwarning: {message}\n");
    let _ = err.flush();
}

/// The typed call: the caller's key set is an argument bound into the query,
/// so it composes with everything else instead of filtering a result that was
/// already cut down. Busiest queue first, because that is the question.
#[tokio::test]
async fn queue_depths_composes_the_callers_key_set() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("warn_customer").await.unwrap();
    engine
        .deploy(
            &fixture("accept/03-parallel-gateway.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    engine
        .deploy(
            &fixture("accept/35-non-interrupting-on-subprocess.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    for _ in 0..5 {
        engine
            .start("p", None, serde_json::json!({}))
            .await
            .unwrap();
    }
    for _ in 0..2 {
        engine
            .start("shipment", None, serde_json::json!({}))
            .await
            .unwrap();
    }
    // One of p's `ta` items goes to a worker, so the two buckets differ.
    engine
        .get_task("ta", &GetTaskOptions::new("w1"))
        .await
        .unwrap()
        .unwrap();

    let mine = engine.queue_depths(&["p".to_string()]).await.unwrap();
    assert_eq!(
        mine,
        vec![
            rbpmn_engine::QueueDepth {
                definition_key: "p".into(),
                topic: "tb".into(),
                waiting: 5,
                in_progress: 0
            },
            rbpmn_engine::QueueDepth {
                definition_key: "p".into(),
                topic: "ta".into(),
                waiting: 4,
                in_progress: 1
            },
        ],
        "busiest first, and the leased one counted as in progress"
    );

    // The other definition's queues are not mine to see unless I ask.
    assert!(mine.iter().all(|d| d.definition_key == "p"));
    let both = engine
        .queue_depths(&["p".to_string(), "shipment".to_string()])
        .await
        .unwrap();
    assert!(both.iter().any(|d| d.definition_key == "shipment"));

    // An empty key set matches no keys: plain SQL set semantics, not a
    // special case that quietly means "everything".
    assert_eq!(engine.queue_depths(&[]).await.unwrap(), vec![]);
    db.drop().await;
}

/// The two published views compose on `instance_id`, which is the point of
/// publishing them: an application groups queue depth by its *own* dimension
/// — here a tenant hoisted into the variable document — in one statement.
#[tokio::test]
async fn the_two_views_compose_on_instance_id() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    for i in 0..7 {
        engine
            .start(
                "p",
                None,
                serde_json::json!({ "tenant": if i % 3 == 0 { "acme" } else { "globex" } }),
            )
            .await
            .unwrap();
    }

    let rows: Vec<(String, String, i64)> = sqlx::query_as(&format!(
        "select i.variables->>'tenant' as tenant, w.topic, count(*) as waiting \
           from {work} w join {inst} i on i.id = w.instance_id \
          where w.claimable \
          group by 1, 2 order by waiting desc",
        work = rbpmn_engine::WORK_ITEM_VIEW,
        inst = rbpmn_engine::INSTANCE_VIEW,
    ))
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("globex".to_string(), "review".to_string(), 4),
            ("acme".to_string(), "review".to_string(), 3),
        ]
    );
    db.drop().await;
}

// ---------------------------------------------------------------------------
// The published timer view: rbpmn_v_timer, the third wait state.
// ---------------------------------------------------------------------------

/// The soonest armed timer the view reports, restricted to live instances the
/// way the scheduler's own candidate query is. `order by due_at limit 1`,
/// never `min(due_at)` — see the migration comment.
async fn view_next_due(pool: &PgPool) -> Option<(uuid::Uuid, i64)> {
    sqlx::query(&format!(
        "select instance_id, timer_no from {} \
          where instance_status = 'active' order by due_at limit 1",
        rbpmn_engine::TIMER_VIEW
    ))
    .fetch_optional(pool)
    .await
    .unwrap()
    .map(|r| (r.get("instance_id"), r.get("timer_no")))
}

async fn timer_still_armed(pool: &PgPool, instance: uuid::Uuid, timer_no: i64) -> bool {
    sqlx::query_scalar::<_, i64>(&format!(
        "select count(*) from {} where instance_id = $1 and timer_no = $2",
        rbpmn_engine::TIMER_VIEW
    ))
    .bind(instance)
    .bind(timer_no)
    .fetch_one(pool)
    .await
    .unwrap()
        > 0
}

/// Public API, asserted the way the other two views' shapes are.
#[tokio::test]
async fn the_published_timer_view_has_the_documented_shape() {
    let db = TestDb::create().await;
    let _engine = engine(&db).await;

    let columns: Vec<(String, String)> = sqlx::query_as(
        "select column_name::text, data_type::text from information_schema.columns \
         where table_name = 'rbpmn_v_timer' order by ordinal_position",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("instance_id".into(), "uuid".into()),
            ("timer_no".into(), "bigint".into()),
            ("definition_key".into(), "text".into()),
            ("definition_version".into(), "integer".into()),
            ("element_id".into(), "text".into()),
            ("due_kind".into(), "text".into()),
            ("due_spec".into(), "text".into()),
            ("due_at".into(), "timestamp with time zone".into()),
            ("remaining".into(), "integer".into()),
            ("instance_status".into(), "text".into()),
            ("created_at".into(), "timestamp with time zone".into()),
        ],
        "rbpmn_v_timer is public API"
    );

    let barrier: Option<Vec<String>> =
        sqlx::query_scalar("select reloptions from pg_class where relname = 'rbpmn_v_timer'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(barrier.is_none(), "must carry no reloptions: {barrier:?}");
    db.drop().await;
}

/// **The property that matters**, and the timer analogue of the work-item
/// view's claimability differential: what the view calls the next due timer
/// must be the timer the scheduler actually fires next.
///
/// Walked one firing at a time rather than checked at the endpoints — a read
/// model that agreed only about the first and last pick would be no use — and
/// with **distinct** due instants on purpose: the scheduler orders by `due_at`
/// with no tie-break, so among equal instants "next" is genuinely
/// unspecified and asserting a particular row would be asserting an accident.
#[tokio::test]
async fn the_views_next_due_is_the_timer_the_scheduler_fires() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT0S</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();

    const N: i64 = 8;
    let mut started = Vec::new();
    for _ in 0..N {
        started.push(
            engine
                .start("pt", None, serde_json::json!({}))
                .await
                .unwrap()
                .id,
        );
    }
    // Distinct, all in the past, and deliberately not in creation order — so
    // agreeing means agreeing about `due_at`, not about insertion order.
    for (i, instance) in started.iter().enumerate() {
        let offset = ((i * 7) % 8) as i64 + 1;
        sqlx::query(&format!(
            "update rbpmn_timer set due_at = now() - interval '{offset} hours' \
             where instance_id = $1"
        ))
        .bind(instance)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    for fired in 0..N {
        let (instance, timer_no) = view_next_due(&db.pool)
            .await
            .unwrap_or_else(|| panic!("the view ran out of timers after {fired} firing(s)"));
        assert!(
            engine.fire_due_timer().await.unwrap(),
            "the scheduler found nothing while the view named one"
        );
        assert!(
            !timer_still_armed(&db.pool, instance, timer_no).await,
            "firing {} of {N}: the scheduler fired something other than the timer \
             the view named ({instance} #{timer_no} is still armed)",
            fired + 1
        );
    }

    assert_eq!(view_next_due(&db.pool).await, None, "the view is empty too");
    assert!(
        !engine.fire_due_timer().await.unwrap(),
        "and so is the scheduler"
    );
    db.drop().await;
}

/// A frozen instance's timer is armed and overdue and will never fire — the
/// distinction the health question turns on, and the reason `instance_status`
/// is a column rather than a second join.
#[tokio::test]
async fn a_frozen_instances_timer_is_armed_but_not_the_scheduler_being_behind() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT0S</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();
    sqlx::query("update rbpmn_timer set due_at = now() - interval '1 day'")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("update rbpmn_instance set status = 'failed' where id = $1")
        .bind(started.id)
        .execute(&db.pool)
        .await
        .unwrap();

    // Still armed, still overdue, and still visible — support needs to see it.
    let row: (String, bool) = sqlx::query_as(&format!(
        "select instance_status, due_at < now() from {} where instance_id = $1",
        rbpmn_engine::TIMER_VIEW
    ))
    .bind(started.id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row, ("failed".to_string(), true));

    // But it is not the scheduler being behind, and the view says which.
    assert_eq!(view_next_due(&db.pool).await, None);
    assert!(!engine.fire_due_timer().await.unwrap());
    db.drop().await;
}

/// A cycle is one row at a time, and the view must never show two "next"
/// occurrences for one arm — an application rendering a date would have to
/// guess which one it had. Firing replaces the row rather than adding to it.
#[tokio::test]
async fn a_cycle_shows_one_next_occurrence_not_a_series() {
    let db = TestDb::create().await;
    let (engine, instance) = late_fee_engine(&db).await;

    let armed: Vec<(i64, f64, Option<i32>, String, String)> = sqlx::query_as(&format!(
        "select timer_no, extract(epoch from due_at)::float8, remaining, due_kind, due_spec \
           from {} where instance_id = $1 and element_id = 'late_fee_due'",
        rbpmn_engine::TIMER_VIEW
    ))
    .bind(instance)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(armed.len(), 1, "one occurrence at a time: {armed:?}");
    let (first_no, _pre_backdate_due, remaining, kind, spec) = armed[0].clone();
    assert_eq!(kind, "cycle");
    // The period lives inside the spec: there is no period column, and this
    // is the reason `due_spec` is the load-bearing one.
    assert_eq!(spec, "R/P7D");
    assert_eq!(remaining, None, "R/… is unbounded");

    backdate_timers(&db.pool, instance).await;
    assert!(engine.fire_due_timer().await.unwrap());

    let after: Vec<(i64, f64)> = sqlx::query_as(&format!(
        "select timer_no, extract(epoch from due_at)::float8 from {} \
           where instance_id = $1 and element_id = 'late_fee_due'",
        rbpmn_engine::TIMER_VIEW
    ))
    .bind(instance)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(after.len(), 1, "still one occurrence, not two: {after:?}");
    assert_ne!(after[0].0, first_no, "a new row, not the old one re-dated");
    // Still renderable as "next": in the future. The grid arithmetic itself —
    // that it steps from the *previous due* and not from now — is
    // `a_cycle_rearms_from_its_previous_due`'s job, not this one's; here the
    // property is only that an application never has two rows to choose
    // between. (`_pre_backdate_due` is deliberately not compared against:
    // `backdate_timers` rewrites due_at absolutely, so the occurrence that
    // actually fired was an hour ago, not a week hence.)
    let now = db_epoch(&db.pool, "clock_timestamp()").await;
    assert!(
        after[0].1 > now,
        "the next occurrence must still be ahead ({} vs {now})",
        after[0].1
    );
    db.drop().await;
}

/// A timer disappears with its token when the wait ends another way — here a
/// boundary timer on a task that completed first. The view is a projection of
/// what is armed, so "nothing armed" has to mean nothing armed.
#[tokio::test]
async fn a_timer_leaves_the_view_when_its_wait_ends() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &boundary_timer_xml("<bpmn:timeDuration>PT1H</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("pb", None, serde_json::json!({}))
        .await
        .unwrap();

    let rows = |id: uuid::Uuid| {
        let pool = db.pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(&format!(
                "select count(*) from {} where instance_id = $1",
                rbpmn_engine::TIMER_VIEW
            ))
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(rows(started.id).await, 1, "armed while the task is open");

    let (item, _) = open_items(&db.pool, started.id).await[0].clone();
    engine
        .complete_work_item(item, serde_json::json!({}))
        .await
        .unwrap();
    wait_for_status(&db.pool, started.id, "completed").await;
    assert_eq!(rows(started.id).await, 0, "and gone with its token");
    db.drop().await;
}

/// Both questions the view exists for, planned THROUGH it. No new index was
/// added for either: the scheduler's own already serve them, and this is what
/// says so out loud rather than leaving it to be re-derived.
#[tokio::test]
async fn the_timer_queries_are_index_driven() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT1H</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    let started = engine
        .start("pt", None, serde_json::json!({}))
        .await
        .unwrap();
    // Enough rows that an index is the cheaper answer; inserted as SQL for the
    // same reason `bulk_work_items` is — a plan test needs statistics, not
    // engine history.
    bulk_instances(&db.pool, "pt", "T-", 20_000).await;
    sqlx::query(
        "insert into rbpmn_timer \
           (instance_id, timer_no, token_no, element_id, due_kind, due_spec, due_at) \
         select i.id, 1, 1, 'c', 'duration', 'PT1H', \
                now() + make_interval(secs => (i.definition_version * 37 + g) * 60) \
           from rbpmn_instance i, generate_series(1, 1) g \
          where i.definition_key = 'pt' and i.id <> $1",
    )
    .bind(started.id)
    .execute(&db.pool)
    .await
    .unwrap();
    analyze(&db.pool).await;
    sqlx::query("analyze rbpmn_timer")
        .execute(&db.pool)
        .await
        .unwrap();

    // "What is armed for this instance" -> the primary key's leading column.
    let per_instance = explain_prepared(
        &db.pool,
        "armed",
        &format!(
            "prepare armed(uuid) as select due_at, element_id, due_spec from {} \
               where instance_id = $1 order by due_at limit 1",
            rbpmn_engine::TIMER_VIEW
        ),
        &format!("execute armed('{}')", started.id),
    )
    .await;
    assert!(
        per_instance.contains("rbpmn_timer_pkey"),
        "the per-instance lookup must use the timer primary key:\n{per_instance}"
    );
    assert!(
        !per_instance.contains("Seq Scan on rbpmn_timer"),
        "and must not scan every armed timer:\n{per_instance}"
    );
    assert!(
        !per_instance.contains("Subquery Scan"),
        "the view must be inlined:\n{per_instance}"
    );

    // "Everything overdue right now" -> the scheduler's due index.
    let overdue = explain_prepared(
        &db.pool,
        "overdue",
        &format!(
            "prepare overdue as select count(*) from {} where due_at < now()",
            rbpmn_engine::TIMER_VIEW
        ),
        "execute overdue",
    )
    .await;
    assert!(
        overdue.contains("rbpmn_timer_due"),
        "the overdue sweep must use the scheduler's due index:\n{overdue}"
    );
    assert!(
        !overdue.contains("Seq Scan on rbpmn_timer"),
        "and must not scan every armed timer:\n{overdue}"
    );
    db.drop().await;
}

/// The three published views compose on `instance_id`: an application groups
/// deadlines by its own dimension — a tenant hoisted into the variable
/// document — in one statement, and can ask about queues in the same breath.
#[tokio::test]
async fn the_timer_view_composes_with_the_instance_view() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &timer_catch_xml("<bpmn:timeDuration>PT1H</bpmn:timeDuration>"),
            &Bindings::default(),
        )
        .await
        .unwrap();
    for i in 0..6 {
        engine
            .start(
                "pt",
                None,
                serde_json::json!({ "tenant": if i % 2 == 0 { "acme" } else { "globex" } }),
            )
            .await
            .unwrap();
    }

    let rows: Vec<(String, i64)> = sqlx::query_as(&format!(
        "select i.variables->>'tenant' as tenant, count(*) as deadlines \
           from {timer} t join {inst} i on i.id = t.instance_id \
          where t.due_at > now() group by 1 order by 1",
        timer = rbpmn_engine::TIMER_VIEW,
        inst = rbpmn_engine::INSTANCE_VIEW,
    ))
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![("acme".to_string(), 3), ("globex".to_string(), 3)]
    );
    db.drop().await;
}

// ---------------------------------------------------------------------------
// The published subscription view: rbpmn_v_subscription, the fourth wait state.
// ---------------------------------------------------------------------------

/// How many live subscriptions the view shows for one (message, key) — the
/// number `correlate`'s three-way answer turns on.
async fn view_live_matches(pool: &PgPool, message: &str, key: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "select count(*) from {} where message_name = $1 and correlation_key = $2 \
           and instance_status = 'active'",
        rbpmn_engine::SUBSCRIPTION_VIEW
    ))
    .bind(message)
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_published_subscription_view_has_the_documented_shape() {
    let db = TestDb::create().await;
    let _engine = engine(&db).await;

    let columns: Vec<(String, String)> = sqlx::query_as(
        "select column_name::text, data_type::text from information_schema.columns \
         where table_name = 'rbpmn_v_subscription' order by ordinal_position",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            ("instance_id".into(), "uuid".into()),
            ("subscription_no".into(), "bigint".into()),
            ("definition_key".into(), "text".into()),
            ("definition_version".into(), "integer".into()),
            ("element_id".into(), "text".into()),
            ("message_name".into(), "text".into()),
            ("correlation_key".into(), "text".into()),
            ("instance_status".into(), "text".into()),
            ("created_at".into(), "timestamp with time zone".into()),
        ],
        "rbpmn_v_subscription is public API"
    );

    let barrier: Option<Vec<String>> = sqlx::query_scalar(
        "select reloptions from pg_class where relname = 'rbpmn_v_subscription'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(barrier.is_none(), "must carry no reloptions: {barrier:?}");
    db.drop().await;
}

/// **The differential**: what the view shows live for a (message, key) must
/// predict which of `correlate`'s three answers you get — deliver on exactly
/// one, `NoSubscription` on none, `AmbiguousCorrelation` on two or more. A
/// support surface that disagreed with the verb it exists to explain would be
/// worse than reading the table directly.
#[tokio::test]
async fn the_view_predicts_correlates_three_way_answer() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/17-message-catch.bpmn"),
            &Bindings::new().correlation("c", "order.id"),
        )
        .await
        .unwrap();
    let start = |key: &str| {
        let engine = engine.clone();
        let key = key.to_string();
        async move {
            engine
                .start("p", None, serde_json::json!({ "order": { "id": key } }))
                .await
                .unwrap()
                .id
        }
    };

    // none armed
    assert_eq!(
        view_live_matches(&db.pool, "WarehouseAck", "absent").await,
        0
    );
    assert!(matches!(
        engine
            .correlate("WarehouseAck", "absent", serde_json::json!({}))
            .await,
        Err(rbpmn_engine::EngineError::NoSubscription { .. })
    ));

    // exactly one armed
    start("solo").await;
    assert_eq!(view_live_matches(&db.pool, "WarehouseAck", "solo").await, 1);
    assert!(
        engine
            .correlate("WarehouseAck", "solo", serde_json::json!({}))
            .await
            .is_ok()
    );
    // ...and consumed, so the view and the verb agree on the way back down too
    assert_eq!(view_live_matches(&db.pool, "WarehouseAck", "solo").await, 0);
    assert!(matches!(
        engine
            .correlate("WarehouseAck", "solo", serde_json::json!({}))
            .await,
        Err(rbpmn_engine::EngineError::NoSubscription { .. })
    ));

    // two armed
    start("dup").await;
    start("dup").await;
    assert_eq!(view_live_matches(&db.pool, "WarehouseAck", "dup").await, 2);
    assert!(matches!(
        engine
            .correlate("WarehouseAck", "dup", serde_json::json!({}))
            .await,
        Err(rbpmn_engine::EngineError::AmbiguousCorrelation { .. })
    ));
    db.drop().await;
}

/// A frozen instance keeps its subscriptions, and `correlate` ignores them —
/// so the view must show the row *and* say why it is not answering. This is
/// what `instance_status` is for: one column between "nothing is waiting" and
/// "the thing waiting is frozen", which are opposite support answers.
#[tokio::test]
async fn a_frozen_instances_subscription_is_visible_but_not_answering() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/17-message-catch.bpmn"),
            &Bindings::new().correlation("c", "order.id"),
        )
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({ "order": { "id": "o-9" } }))
        .await
        .unwrap();
    sqlx::query("update rbpmn_instance set status = 'failed' where id = $1")
        .bind(started.id)
        .execute(&db.pool)
        .await
        .unwrap();

    // Visible, with the reason attached.
    let row: (uuid::Uuid, String, String) = sqlx::query_as(&format!(
        "select instance_id, message_name, instance_status from {} \
           where correlation_key = $1",
        rbpmn_engine::SUBSCRIPTION_VIEW
    ))
    .bind("o-9")
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (started.id, "WarehouseAck".to_string(), "failed".to_string())
    );

    // And correlate does not see it, exactly as the view's live count says.
    assert_eq!(view_live_matches(&db.pool, "WarehouseAck", "o-9").await, 0);
    assert!(matches!(
        engine
            .correlate("WarehouseAck", "o-9", serde_json::json!({}))
            .await,
        Err(rbpmn_engine::EngineError::NoSubscription { .. })
    ));
    db.drop().await;
}

/// The 409 diagnostic: the documented ambiguity query must name exactly the
/// pairs `correlate` refuses — no more (a frozen duplicate is not a conflict)
/// and no fewer.
#[tokio::test]
async fn the_ambiguity_query_names_exactly_what_correlate_refuses() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/17-message-catch.bpmn"),
            &Bindings::new().correlation("c", "order.id"),
        )
        .await
        .unwrap();
    for key in ["clash", "clash", "lonely", "frozen-dup", "frozen-dup"] {
        engine
            .start("p", None, serde_json::json!({ "order": { "id": key } }))
            .await
            .unwrap();
    }
    // One of the frozen-dup pair freezes, which makes it no longer a conflict.
    sqlx::query(
        "update rbpmn_instance set status = 'failed' where id = \
         (select instance_id from rbpmn_subscription where correlation_key = 'frozen-dup' \
           order by created_at limit 1)",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let conflicts: Vec<(String, String, i64)> = sqlx::query_as(&format!(
        "select message_name, correlation_key, waiting from ({}) q order by correlation_key",
        rbpmn_engine::Engine::AMBIGUOUS_CORRELATIONS_SQL
    ))
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        conflicts,
        vec![("WarehouseAck".to_string(), "clash".to_string(), 2)],
        "only the live duplicate is a conflict"
    );

    // Which is exactly what the verb does.
    assert!(matches!(
        engine
            .correlate("WarehouseAck", "clash", serde_json::json!({}))
            .await,
        Err(rbpmn_engine::EngineError::AmbiguousCorrelation { .. })
    ));
    assert!(
        engine
            .correlate("WarehouseAck", "frozen-dup", serde_json::json!({}))
            .await
            .is_ok(),
        "one live half of a frozen pair still delivers"
    );
    assert!(
        engine
            .correlate("WarehouseAck", "lonely", serde_json::json!({}))
            .await
            .is_ok()
    );
    db.drop().await;
}

/// A subscription leaves the view with its token when the wait ends — here
/// because the message arrived.
#[tokio::test]
async fn a_subscription_leaves_the_view_when_its_wait_ends() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/17-message-catch.bpmn"),
            &Bindings::new().correlation("c", "order.id"),
        )
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({ "order": { "id": "o-1" } }))
        .await
        .unwrap();
    let rows = |id: uuid::Uuid| {
        let pool = db.pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(&format!(
                "select count(*) from {} where instance_id = $1",
                rbpmn_engine::SUBSCRIPTION_VIEW
            ))
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(rows(started.id).await, 1);
    engine
        .correlate("WarehouseAck", "o-1", serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(rows(started.id).await, 0, "gone with its token");
    db.drop().await;
}

/// The support question — "what is waiting on this order number?" — planned
/// THROUGH the view, and the reason `rbpmn_subscription_by_key` exists.
///
/// The correlate index is `(message_name, correlation_key)`, so a predicate on
/// the second column with nothing on the first has no leading equality to seek
/// on. Skip scan gives it one from PostgreSQL 18, by seeking once per distinct
/// message name — which is why this names `rbpmn_subscription_by_key`
/// specifically rather than settling for "an index was used". The looser
/// assertion would pass on a development 18 while the same query has no index
/// path at all on the 15 CI runs, and would still pass on 18 while costing one
/// seek per name in the deployment's model portfolio. Measured on 60 000
/// subscriptions: 24 buffers at 4 message names, 394 at 400, against 3 here.
#[tokio::test]
async fn the_business_identifier_lookup_is_index_driven() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/17-message-catch.bpmn"),
            &Bindings::new().correlation("c", "order.id"),
        )
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({ "order": { "id": "ORD-1" } }))
        .await
        .unwrap();
    // Statistics, not history — same reasoning as `bulk_work_items`.
    bulk_instances(&db.pool, "p", "S-", 20_000).await;
    sqlx::query(
        "insert into rbpmn_subscription \
           (instance_id, subscription_no, token_no, element_id, message_name, correlation_key) \
         select i.id, 1, 1, 'c', 'MSG-' || (i.definition_version * 7 % 400), \
                'ORD-' || i.id \
           from rbpmn_instance i where i.definition_key = 'p' \
             and not exists (select 1 from rbpmn_subscription s where s.instance_id = i.id)",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("analyze rbpmn_subscription")
        .execute(&db.pool)
        .await
        .unwrap();
    analyze(&db.pool).await;

    let plan = explain_prepared(
        &db.pool,
        "waiting",
        &format!(
            "prepare waiting(text) as {}",
            rbpmn_engine::Engine::WAITING_ON_KEY_SQL
        ),
        "execute waiting('ORD-1')",
    )
    .await;
    assert!(
        plan.contains("rbpmn_subscription_by_key"),
        "the business-identifier lookup must use its own index rather than \
         skip-scanning the correlate index once per message name:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan on rbpmn_subscription"),
        "and must not scan every armed subscription:\n{plan}"
    );
    assert!(
        !plan.contains("Subquery Scan"),
        "the view must be inlined:\n{plan}"
    );
    db.drop().await;
}

// ---------------------------------------------------------------------------
// The published definition views: what is deployed, and the artifacts it was
// deployed with.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_published_definition_views_have_the_documented_shape() {
    let db = TestDb::create().await;
    let _engine = engine(&db).await;

    let columns = |view: &'static str| {
        let pool = db.pool.clone();
        async move {
            sqlx::query_as::<_, (String, String)>(
                "select column_name::text, data_type::text from information_schema.columns \
                 where table_name = $1 order by ordinal_position",
            )
            .bind(view)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    assert_eq!(
        columns("rbpmn_v_definition").await,
        vec![
            ("id".into(), "uuid".into()),
            ("key".into(), "text".into()),
            ("version".into(), "integer".into()),
            ("content_hash".into(), "text".into()),
            ("deployed_at".into(), "timestamp with time zone".into()),
            ("bpmn_xml".into(), "text".into()),
            ("bindings".into(), "jsonb".into()),
            ("retired_instances".into(), "bigint".into()),
        ],
        "rbpmn_v_definition is public API"
    );
    assert_eq!(
        columns("rbpmn_v_definition_decision").await,
        vec![
            ("definition_id".into(), "uuid".into()),
            ("definition_key".into(), "text".into()),
            ("definition_version".into(), "integer".into()),
            ("ordinal".into(), "integer".into()),
            ("dmn_xml".into(), "text".into()),
        ],
        "rbpmn_v_definition_decision is public API"
    );

    for view in ["rbpmn_v_definition", "rbpmn_v_definition_decision"] {
        let barrier: Option<Vec<String>> =
            sqlx::query_scalar("select reloptions from pg_class where relname = $1")
                .bind(view)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(barrier.is_none(), "{view} must carry no reloptions");
    }
    db.drop().await;
}

/// The view hands back exactly what was deployed — same XML, same manifest,
/// same hash — so an application can reconcile "the model in git" against
/// "the model that is running" without trusting a copy.
#[tokio::test]
async fn the_definition_view_returns_what_was_deployed() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = fixture("accept/01-minimal.bpmn");
    let bindings = Bindings::new().index("channel");
    let deployed = engine.deploy(&xml, &bindings).await.unwrap();

    let row: (
        uuid::Uuid,
        String,
        i32,
        String,
        String,
        serde_json::Value,
        i64,
    ) = sqlx::query_as(&format!(
        "select id, key, version, content_hash, bpmn_xml, bindings, retired_instances \
               from {} where key = $1 and version = $2",
        rbpmn_engine::DEFINITION_VIEW
    ))
    .bind("p")
    .bind(1)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row.0, deployed.definition_id);
    assert_eq!((row.1.as_str(), row.2), ("p", 1));
    assert_eq!(row.4, xml, "the model itself, byte for byte");
    assert_eq!(row.5, serde_json::to_value(&bindings).unwrap());
    assert_eq!(row.6, 0);

    // The hash is the idempotency key deploy actually uses: redeploying the
    // same bundle must not add a row, and the view must not show two.
    let again = engine.deploy(&xml, &bindings).await.unwrap();
    assert!(again.reused);
    let versions: i64 = sqlx::query_scalar(&format!(
        "select count(*) from {} where key = 'p'",
        rbpmn_engine::DEFINITION_VIEW
    ))
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(versions, 1);
    assert_eq!(row.3.len(), 64, "a sha256 hex digest: {}", row.3);
    db.drop().await;
}

/// The DMN half: the artifacts come back in deployment order, and each
/// version keeps the ones it was validated with.
#[cfg(feature = "dmn")]
#[tokio::test]
async fn the_decision_view_returns_the_artifacts_in_order() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let bundle =
        rbpmn_engine::Bundle::new(fixture("accept/01-minimal.bpmn")).decision(DECISION_DMN);
    let first = engine.deploy_bundle(&bundle).await.unwrap();

    let artifacts: Vec<(String, i32, i32, String)> = sqlx::query_as(&format!(
        "select definition_key, definition_version, ordinal, dmn_xml from {} \
           where definition_id = $1 order by ordinal",
        rbpmn_engine::DEFINITION_DECISION_VIEW
    ))
    .bind(first.definition_id)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(
        (artifacts[0].0.as_str(), artifacts[0].1, artifacts[0].2),
        ("p", 1, 0)
    );
    assert_eq!(artifacts[0].3, DECISION_DMN);

    // A changed rule is changed content: a new version, and the old one keeps
    // the artifact it was validated against.
    let edited = rbpmn_engine::Bundle::new(fixture("accept/01-minimal.bpmn"))
        .decision(DECISION_DMN.replace("Amount * 0.1", "Amount * 0.2"));
    let second = engine.deploy_bundle(&edited).await.unwrap();
    assert_eq!(second.version, first.version + 1);

    let by_version: Vec<(i32, String)> = sqlx::query_as(&format!(
        "select definition_version, dmn_xml from {} where definition_key = 'p' \
          order by definition_version",
        rbpmn_engine::DEFINITION_DECISION_VIEW
    ))
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(by_version.len(), 2);
    assert!(by_version[0].1.contains("0.1"));
    assert!(by_version[1].1.contains("0.2"));

    // And they go with the definition when it goes.
    engine.delete_definition("p", 1).await.unwrap();
    let left: i64 = sqlx::query_scalar(&format!(
        "select count(*) from {} where definition_version = 1",
        rbpmn_engine::DEFINITION_DECISION_VIEW
    ))
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
    db.drop().await;
}

/// The deployment inventory — the question asked most often, and the shape
/// that answers it without dragging every model across the wire.
#[tokio::test]
async fn the_deployed_now_query_reports_the_latest_of_every_key() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine.declare_topic("warn_customer").await.unwrap();
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine
        .deploy(
            &fixture("accept/01-minimal.bpmn"),
            &Bindings::new().index("channel"),
        )
        .await
        .unwrap();
    engine
        .deploy(
            &fixture("accept/35-non-interrupting-on-subprocess.bpmn"),
            &Bindings::default(),
        )
        .await
        .unwrap();

    let now: Vec<(String, i32)> = sqlx::query_as(&format!(
        "select key, version from ({}) q order by key",
        rbpmn_engine::Engine::DEPLOYED_NOW_SQL
    ))
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        now,
        vec![("p".to_string(), 2), ("shipment".to_string(), 1)],
        "the latest of every key, and only the latest"
    );
    db.drop().await;
}

/// The whole surface joins up: an instance's definition is reachable through
/// the stable pair, which is what lets one statement answer "what is running,
/// on which version of which model".
#[tokio::test]
async fn an_instance_reaches_its_definition_through_the_stable_pair() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/01-minimal.bpmn"),
            &Bindings::new().index("channel"),
        )
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    let row: (uuid::Uuid, String, i32, String) = sqlx::query_as(&format!(
        "select i.id, d.key, d.version, d.content_hash \
           from {inst} i join {def} d \
             on d.key = i.definition_key and d.version = i.definition_version \
          where i.id = $1",
        inst = rbpmn_engine::INSTANCE_VIEW,
        def = rbpmn_engine::DEFINITION_VIEW,
    ))
    .bind(started.id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!((row.0, row.1.as_str(), row.2), (started.id, "p", 1));
    assert_eq!(row.3.len(), 64);
    db.drop().await;
}

// ---------------------------------------------------------------------------
// Task config: manifest wiring delivered on the work item
// ---------------------------------------------------------------------------

/// The pull claim carries what the manifest configured the element with, and
/// `None` — never an empty object — when it configured nothing.
#[tokio::test]
async fn config_reaches_a_pull_claim() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/01-minimal.bpmn"),
            &Bindings::new().config("review", serde_json::json!({ "form": "contest" })),
        )
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    let task = engine
        .get_task("review", &GetTaskOptions::new("w1"))
        .await
        .unwrap()
        .expect("a task");
    assert_eq!(task.config, Some(serde_json::json!({ "form": "contest" })));
    db.drop().await;
}

/// `None`, never an empty object: every element of every definition deployed
/// before config existed answers this way, and a handler must be able to tell
/// "nothing was configured" from "configured with nothing".
#[tokio::test]
async fn an_unconfigured_element_claims_without_config() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(&fixture("accept/01-minimal.bpmn"), &Bindings::default())
        .await
        .unwrap();
    engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    let task = engine
        .get_task("review", &GetTaskOptions::new("w1"))
        .await
        .unwrap()
        .expect("a task");
    assert_eq!(task.config, None);
    db.drop().await;
}

/// The assertion the whole feature exists for. Config is inside
/// `content_hash` and an instance is pinned to the version it started on, so
/// a later deploy that changes a template does not change what an in-flight
/// instance sends. A sidecar keyed by definition *key* would get this wrong.
#[tokio::test]
async fn an_instance_keeps_the_config_of_the_version_it_started_on() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = fixture("accept/01-minimal.bpmn");

    let v1 = engine
        .deploy(
            &xml,
            &Bindings::new().config("review", serde_json::json!({ "template": "warning_first" })),
        )
        .await
        .unwrap();
    let pinned = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    // Same diagram, different config: a new version, because the manifest is
    // hashed with the model.
    let v2 = engine
        .deploy(
            &xml,
            &Bindings::new().config(
                "review",
                serde_json::json!({ "template": "warning_second" }),
            ),
        )
        .await
        .unwrap();
    assert_eq!((v1.version, v2.version), (1, 2));
    assert!(!v2.reused, "a config change is a new definition version");

    let fresh = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    let first = engine
        .get_task("review", &GetTaskOptions::new("w1"))
        .await
        .unwrap()
        .expect("a task");
    assert_eq!(first.instance_id, pinned.id, "FIFO: the older instance");
    assert_eq!(first.definition_version, 1);
    assert_eq!(
        first.config,
        Some(serde_json::json!({ "template": "warning_first" }))
    );

    let second = engine
        .get_task("review", &GetTaskOptions::new("w2"))
        .await
        .unwrap()
        .expect("a task");
    assert_eq!(second.instance_id, fresh.id);
    assert_eq!(second.definition_version, 2);
    assert_eq!(
        second.config,
        Some(serde_json::json!({ "template": "warning_second" }))
    );
    db.drop().await;
}

/// The push handler gets the same value, and — new with it — the pinned
/// `(definition_id, definition_version)` it could never resolve against
/// before.
#[tokio::test]
async fn config_and_the_pinned_version_reach_a_push_handler() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let seen: Arc<std::sync::Mutex<Vec<WorkItem>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = seen.clone();
    engine.register_handler(
        "payments",
        Arc::new(FnHandler(move |item: WorkItem| {
            recorder.lock().unwrap().push(item);
            Ok(serde_json::json!({}))
        })),
    );
    let deployed = engine
        .deploy(
            &fixture("accept/16-foreign-binding-warn.bpmn"),
            &Bindings::new()
                .topic("st", "payments")
                .config("st", serde_json::json!({ "template": "warning_first" })),
        )
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    let worker = tokio::spawn({
        let engine = engine.clone();
        async move { engine.run_worker(worker_options()).await }
    });
    wait_for_status(&db.pool, started.id, "completed").await;
    worker.abort();

    let item = seen
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("the handler ran");
    assert_eq!(
        item.config,
        Some(serde_json::json!({ "template": "warning_first" }))
    );
    assert_eq!(item.definition_id, deployed.definition_id);
    assert_eq!(item.definition_version, 1);
    db.drop().await;
}

/// A config entry that binds nothing is refused at deploy, where a stale
/// `topics` key is not. The asymmetry is deliberate: a topic has a default
/// to fall back to and config has none.
#[tokio::test]
async fn deploy_refuses_config_that_binds_nothing() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    let xml = fixture("accept/01-minimal.bpmn");

    match engine
        .deploy(
            &xml,
            &Bindings::new().config("renamed_away", serde_json::json!({ "form": "contest" })),
        )
        .await
    {
        Err(DeployError::Rejected(diags)) => {
            assert!(
                diags.iter().any(|d| d.rule == "config-binds-task"),
                "{diags:?}"
            );
        }
        other => panic!("expected config-binds-task rejection, got {other:?}"),
    }

    engine
        .deploy(&xml, &Bindings::new().topic("renamed_away", "review"))
        .await
        .expect("a stale topic key stays lenient");
    db.drop().await;
}

/// Config is resolved after the claim statement, so it can fail with an item
/// already locked. When it does, the claim is handed back rather than left to
/// lapse — otherwise a definition whose manifest cannot be read would let a
/// retrying worker lock a whole queue one lease at a time.
///
/// Corrupting the row is the only way to reach it: a manifest is written by
/// `deploy` from a `Bindings`, and startup re-validation refuses to boot on
/// one that no longer deserializes. The second engine is what makes the read
/// happen at all — the first one cached the manifest when it compiled the
/// process to start the instance.
#[tokio::test]
async fn a_claim_is_handed_back_when_the_manifest_cannot_be_read() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/01-minimal.bpmn"),
            &Bindings::new().config("review", serde_json::json!({ "form": "contest" })),
        )
        .await
        .unwrap();
    let started = engine
        .start("p", None, serde_json::json!({}))
        .await
        .unwrap();

    sqlx::query("update rbpmn_definition set bindings = '\"corrupt\"'::jsonb")
        .execute(&db.pool)
        .await
        .unwrap();

    let cold = Engine::builder(db.pool.clone()).build();
    let outcome = cold.get_task("review", &GetTaskOptions::new("w1")).await;
    assert!(
        matches!(
            outcome,
            Err(rbpmn_engine::EngineError::CorruptManifest { .. })
        ),
        "{outcome:?}"
    );

    let row = sqlx::query(
        "select state, lock_owner, lease_no from rbpmn_work_item where instance_id = $1",
    )
    .bind(started.id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("state"), "available");
    assert_eq!(row.get::<Option<String>, _>("lock_owner"), None);
    // The lease epoch is spent all the same: the claim happened, and a client
    // holding that number must not be able to act on it.
    assert_eq!(row.get::<i64, _>("lease_no"), 1);
    db.drop().await;
}

/// The manifest is stored as jsonb, and PostgreSQL cannot represent a NUL in
/// a string. Config is the first manifest field carrying arbitrary
/// application text — topics resolve against a NUL-free declared set,
/// correlations are parsed FEEL names, index fields are identifier-validated
/// — so this boundary became reachable with it, and a well-formed request
/// must not come back as a raw database error.
#[tokio::test]
async fn a_nul_in_the_manifest_is_refused_at_the_boundary() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    match engine
        .deploy(
            &fixture("accept/01-minimal.bpmn"),
            &Bindings::new().config("review", serde_json::json!({ "note": "a\u{0}b" })),
        )
        .await
    {
        Err(DeployError::InvalidManifest(message)) => {
            assert!(message.contains("u0000"), "{message}");
        }
        other => panic!("expected InvalidManifest, got {other:?}"),
    }
    db.drop().await;
}

/// Startup re-validation reproduces the deploy gates against stored rows, and
/// config is one of them: an entry that no longer binds a task would deliver
/// nothing and say nothing, which is the failure `config-binds-task` exists
/// to prevent. Reached by editing the row, since deploy refuses to write one.
#[tokio::test]
async fn startup_revalidation_sees_a_config_key_that_stopped_binding() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    engine
        .deploy(
            &fixture("accept/01-minimal.bpmn"),
            &Bindings::new().config("review", serde_json::json!({ "form": "contest" })),
        )
        .await
        .unwrap();
    assert!(engine.check_active_definitions().await.unwrap().is_empty());

    sqlx::query(
        "update rbpmn_definition set bindings = \
         '{\"config\":{\"renamed_away\":{\"form\":\"contest\"}}}'::jsonb",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let diags = Engine::builder(db.pool.clone())
        .build()
        .check_active_definitions()
        .await
        .unwrap();
    assert!(
        diags.iter().any(|d| d.rule == "config-binds-task"),
        "{diags:?}"
    );
    db.drop().await;
}
