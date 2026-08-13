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

use rbpmn_engine::{GetTaskOptions, LockExtension, TaskFilter, TaskOrder};

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
        LockExtension::Lost
    );

    engine
        .complete_task(task.id, "w1", serde_json::json!({}))
        .await
        .unwrap();
    // Closed task: the heartbeat reports loss too.
    assert_eq!(
        engine
            .extend_lock(task.id, "w1", Duration::from_secs(600))
            .await
            .unwrap(),
        LockExtension::Lost
    );
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

    let mut all = Vec::new();
    for _ in 0..100 {
        all = engine
            .read_events(rbpmn_engine::EventCursor::default(), 200)
            .await
            .unwrap();
        if all.iter().any(|e| e.kind == "instance-completed") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mine: Vec<_> = all.iter().filter(|e| e.instance_id == started.id).collect();
    assert!(!mine.is_empty());

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
