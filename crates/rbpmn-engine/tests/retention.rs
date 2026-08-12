//! Phase 7: retention. The two tests that lead this file exist to kill the
//! two designs that were considered and rejected — a global watermark
//! (jammed forever by one stuck instance) and age-only deletion (which
//! silently breaks a tailing consumer). The third covers the archive seam:
//! no export, no deletion.

use rbpmn_core::Bindings;
use rbpmn_engine::testing::TestDb;
use rbpmn_engine::{
    ArchiveBatch, ArchiveError, Engine, EngineError, EventCursor, EventRecord, FailOptions,
    FailOutcome, RetentionArchive, RetentionOptions, RetentionPolicy,
};
mod harness;

use sqlx::PgPool;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rbpmn-model/tests/fixtures")
        .join(name);
    std::fs::read_to_string(path).unwrap()
}

async fn engine(db: &TestDb) -> Engine {
    let engine = Engine::builder(db.pool.clone())
        .retry_backoff(Duration::ZERO)
        .build();
    engine.migrate().await.unwrap();
    engine
}

/// Everything expires immediately; the tests control eligibility by
/// backdating `completed_at` instead, which keeps the production SQL
/// clock-side (database time is the only clock) while staying deterministic.
fn options(policy: RetentionPolicy) -> RetentionOptions {
    RetentionOptions::new(policy)
}

fn after(secs: u64) -> RetentionPolicy {
    RetentionPolicy::new(Some(Duration::from_secs(secs))).unwrap()
}

/// Move an instance's completion into the past — the deterministic stand-in
/// for waiting out a retention age.
async fn backdate(pool: &PgPool, id: Uuid, secs: i64) {
    sqlx::query(
        "update rbpmn_instance set completed_at = now() - make_interval(secs => $2::double precision) \
         where id = $1",
    )
    .bind(id)
    .bind(secs as f64)
    .execute(pool)
    .await
    .unwrap();
}

async fn count(pool: &PgPool, sql: &str, id: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn events_of(pool: &PgPool, id: Uuid) -> i64 {
    count(
        pool,
        "select count(*) from rbpmn_event where instance_id = $1",
        id,
    )
    .await
}

async fn work_items_of(pool: &PgPool, id: Uuid) -> i64 {
    count(
        pool,
        "select count(*) from rbpmn_work_item where instance_id = $1",
        id,
    )
    .await
}

/// A completed instance of `07-task-kinds`, driven all the way through so it
/// leaves behind the residue retention exists to reclaim: completed work
/// item rows, and a full event history.
async fn completed_instance(engine: &Engine, pool: &PgPool, order: &str) -> Uuid {
    let started = engine
        .start(
            "p",
            Some(order),
            serde_json::json!({ "order": { "id": order } }),
        )
        .await
        .unwrap();
    let id = started.id;
    for element in ["st", "ut"] {
        let item: Uuid = sqlx::query_scalar(
            "select id from rbpmn_work_item where instance_id = $1 and element_id = $2",
        )
        .bind(id)
        .bind(element)
        .fetch_one(pool)
        .await
        .unwrap();
        engine
            .complete_work_item(item, serde_json::json!({}))
            .await
            .unwrap();
    }
    engine
        .correlate("ShipmentConfirmed", order, serde_json::json!({}))
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("select status from rbpmn_instance where id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(status, "completed");
    id
}

/// A deliberately shorter record than `completed_instance`: `01-minimal` is
/// start -> one user task -> end, so it retires with a fraction of the
/// events. Deployed under its own key so both fixtures can coexist.
async fn deploy_short(engine: &Engine, key: &str) {
    engine
        .deploy(
            &fixture("accept/01-minimal.bpmn")
                .replace(r#"process id="p""#, &format!(r#"process id="{key}""#)),
            &Bindings::default(),
        )
        .await
        .unwrap();
}

async fn short_instance(engine: &Engine, pool: &PgPool, key: &str, business: &str) -> Uuid {
    let id = engine
        .start(key, Some(business), serde_json::json!({}))
        .await
        .unwrap()
        .id;
    let item: Uuid = sqlx::query_scalar("select id from rbpmn_work_item where instance_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    engine
        .complete_work_item(item, serde_json::json!({}))
        .await
        .unwrap();
    id
}

async fn deploy_task_kinds(engine: &Engine) {
    engine.declare_topic("st").await.unwrap();
    let bindings = Bindings::new().correlation("rt", "order.id");
    engine
        .deploy(&fixture("accept/07-task-kinds.bpmn"), &bindings)
        .await
        .unwrap();
}

/// An instance frozen at an incident: a work item failed past its budget
/// with no matching boundary.
async fn failed_instance(engine: &Engine, pool: &PgPool, order: &str) -> Uuid {
    let started = engine
        .start(
            "p",
            Some(order),
            serde_json::json!({ "order": { "id": order } }),
        )
        .await
        .unwrap();
    let item: Uuid = sqlx::query_scalar(
        "select id from rbpmn_work_item where instance_id = $1 and element_id = 'st'",
    )
    .bind(started.id)
    .fetch_one(pool)
    .await
    .unwrap();
    let mut outcome = engine
        .fail_work_item(item, &FailOptions::default())
        .await
        .unwrap();
    while matches!(outcome, FailOutcome::Retrying { .. }) {
        outcome = engine
            .fail_work_item(item, &FailOptions::default())
            .await
            .unwrap();
    }
    assert_eq!(outcome, FailOutcome::IncidentRaised);
    started.id
}

/// The stream's safe horizon is **cluster-wide** (xids are global), so a
/// transaction belonging to any concurrently running test can hold it back
/// for a moment. Late, never lost — so poll rather than assume.
async fn read_released(engine: &Engine, after: EventCursor, want: usize) -> Vec<EventRecord> {
    for _ in 0..100 {
        let page = engine.read_events(after, 1000).await.unwrap();
        if page.len() >= want {
            return page;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the horizon never released {want} events");
}

/// Records every batch it is handed; optionally refuses.
struct Recorder {
    seen: Mutex<Vec<ArchiveBatch>>,
    fail: bool,
}

impl Recorder {
    fn new(fail: bool) -> Arc<Self> {
        Arc::new(Recorder {
            seen: Mutex::new(Vec::new()),
            fail,
        })
    }

    fn records(&self) -> Vec<rbpmn_engine::InstanceRecord> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| b.instances.clone())
            .collect()
    }
}

impl RetentionArchive for Recorder {
    fn archive<'a>(
        &'a self,
        batch: &'a ArchiveBatch,
    ) -> Pin<Box<dyn Future<Output = Result<(), ArchiveError>> + Send + 'a>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(batch.clone());
            if self.fail {
                Err(ArchiveError::new("sink is down"))
            } else {
                Ok(())
            }
        })
    }
}

// ------------------------------------------------- the designs this rejects

/// The trap in the obvious design: a watermark at "the oldest event of any
/// non-terminal instance" is one number, trivially safe, and permanently
/// jammed by a single stuck instance. Eligibility is per instance precisely
/// so a wedged neighbour costs its own storage and nothing else.
#[tokio::test]
async fn a_stuck_instance_does_not_block_its_neighbours() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;

    // The stuck one is *older* than everything else, which is exactly the
    // position a watermark design would let it hold the floor from.
    let stuck = failed_instance(&engine, &db.pool, "o-stuck").await;
    sqlx::query("update rbpmn_instance set created_at = now() - interval '400 days' where id = $1")
        .bind(stuck)
        .execute(&db.pool)
        .await
        .unwrap();

    let done = completed_instance(&engine, &db.pool, "o-1").await;
    backdate(&db.pool, done, 3600).await;

    let report = engine
        .sweep_retention_once(&options(after(60)))
        .await
        .unwrap();
    assert_eq!(report.instances_deleted, 1);

    // The neighbour retired on schedule; the incident is untouched, in full.
    assert_eq!(events_of(&db.pool, done).await, 0);
    assert!(events_of(&db.pool, stuck).await > 0);
    assert_eq!(
        count(
            &db.pool,
            "select count(*) from rbpmn_instance where id = $1",
            stuck
        )
        .await,
        1
    );
    // And the frozen incident token is still there to be repaired from.
    assert!(
        count(
            &db.pool,
            "select count(*) from rbpmn_token where instance_id = $1",
            stuck
        )
        .await
            > 0
    );
    db.drop().await;
}

/// The cursor contract survives retention: above the floor the stream is
/// provably complete, below it the consumer is *told* rather than silently
/// skipped past the gap.
#[tokio::test]
async fn a_cursor_above_the_floor_sees_a_complete_stream() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;

    let old = completed_instance(&engine, &db.pool, "o-old").await;
    backdate(&db.pool, old, 3600).await;
    let drained = read_released(
        &engine,
        EventCursor::default(),
        events_of(&db.pool, old).await as usize,
    )
    .await;
    // A cursor that has kept up: parked at the end of everything so far.
    let caught_up = drained.last().unwrap().cursor();
    let lagging = drained.first().unwrap().cursor();

    let fresh = completed_instance(&engine, &db.pool, "o-fresh").await;
    let report = engine
        .sweep_retention_once(&options(after(60)))
        .await
        .unwrap();
    assert_eq!(report.instances_deleted, 1);
    assert!(report.floor > EventCursor::default());

    // The consumer that kept up loses nothing and sees the new instance
    // whole — retention deleted only what it had already read.
    let want = events_of(&db.pool, fresh).await as usize;
    let tail = read_released(&engine, caught_up, want).await;
    assert!(tail.iter().all(|e| e.instance_id == fresh));
    assert_eq!(tail.len(), want);

    // The consumer that lagged is told, with the floor to resume from.
    match engine.read_events(lagging, 1000).await {
        Err(EngineError::CursorTruncated { cursor, floor }) => {
            assert_eq!(cursor, lagging);
            assert_eq!(floor, report.floor);
        }
        other => panic!("expected CursorTruncated, got {other:?}"),
    }
    // Resuming from the floor is allowed: the gap is now acknowledged.
    engine.read_events(report.floor, 1000).await.unwrap();
    // And a new consumer starting at the beginning reads what is retained.
    let from_start = engine
        .read_events(EventCursor::default(), 1000)
        .await
        .unwrap();
    assert!(from_start.iter().all(|e| e.instance_id == fresh));
    db.drop().await;
}

/// No export, no deletion — the whole point of putting the sink between
/// planning and executing.
#[tokio::test]
async fn a_failing_archive_deletes_nothing() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;
    let sink = Recorder::new(true);
    engine.register_archive(sink.clone());

    let done = completed_instance(&engine, &db.pool, "o-1").await;
    backdate(&db.pool, done, 3600).await;
    let before = events_of(&db.pool, done).await;

    match engine.sweep_retention_once(&options(after(60))).await {
        Err(EngineError::ArchiveFailed(msg)) => assert!(msg.contains("sink is down")),
        other => panic!("expected ArchiveFailed, got {other:?}"),
    }

    assert_eq!(events_of(&db.pool, done).await, before);
    assert_eq!(
        engine.retention_floor().await.unwrap(),
        EventCursor::default()
    );
    // It did see the batch — the refusal is the sink's, not a planning miss.
    assert_eq!(sink.records().len(), 1);
    db.drop().await;
}

// ------------------------------------------------------------- the two knobs

/// A record retires whole: header, children and events, in one transaction,
/// after the sink has a complete copy. Inspection goes away with it, because
/// the record genuinely no longer exists — it is in the archive.
#[tokio::test]
async fn retirement_deletes_the_whole_record() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;
    let sink = Recorder::new(false);
    engine.register_archive(sink.clone());

    let done = completed_instance(&engine, &db.pool, "o-1").await;
    backdate(&db.pool, done, 3600).await;
    let history = events_of(&db.pool, done).await;

    let report = engine
        .sweep_retention_once(&options(after(60)))
        .await
        .unwrap();
    assert_eq!(report.instances_deleted, 1);
    assert_eq!(report.events_deleted, history as u64);

    assert!(matches!(
        engine.inspect_instance(done).await,
        Err(EngineError::UnknownInstance(_))
    ));

    // The sink got the complete record, in semantic order, before deletion.
    let records = sink.records();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.id, done);
    assert_eq!(record.definition_key, "p");
    assert_eq!(record.business_key.as_deref(), Some("o-1"));
    assert_eq!(record.status, "completed");
    assert_eq!(
        record.variables,
        serde_json::json!({ "order": { "id": "o-1" } })
    );
    assert_eq!(record.events.len() as i64, history);
    assert!(record.events.windows(2).all(|w| w[0].id < w[1].id));
    assert_eq!(record.events[0].kind, "instance-started");
    db.drop().await;
}

/// An event never outlives its instance row — enforced by the 0007 foreign
/// key rather than asserted, which is what lets `delete_definition` answer
/// "is anything still referencing this?" with an indexed lookup instead of a
/// scan of the largest table in the schema. The direct insert proves the
/// constraint is real and not merely a convention the sweeper happens to
/// honour.
#[tokio::test]
async fn no_event_ever_outlives_its_instance() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;

    let a = completed_instance(&engine, &db.pool, "o-a").await;
    let b = completed_instance(&engine, &db.pool, "o-b").await;
    backdate(&db.pool, a, 3600).await;
    backdate(&db.pool, b, 3600).await;

    engine
        .sweep_retention_once(&options(after(60)))
        .await
        .unwrap();

    let orphans: i64 = sqlx::query_scalar(
        "select count(*) from rbpmn_event e \
         where not exists (select 1 from rbpmn_instance i where i.id = e.instance_id)",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(orphans, 0);

    // The database refuses to create one, which is the point of the FK.
    let orphaned = sqlx::query(
        "insert into rbpmn_event (instance_id, definition_id, definition_key, kind, payload) \
         select gen_random_uuid(), d.id, d.key, 'invented', '{}'::jsonb \
         from rbpmn_definition d limit 1",
    )
    .execute(&db.pool)
    .await;
    assert!(orphaned.is_err(), "the FK let an orphan event in");
    db.drop().await;
}

// ------------------------------------------------------- what it cannot touch

#[tokio::test]
async fn active_and_failed_instances_are_never_swept() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;

    let active = engine
        .start(
            "p",
            Some("o-live"),
            serde_json::json!({"order": {"id": "o-live"}}),
        )
        .await
        .unwrap()
        .id;
    let failed = failed_instance(&engine, &db.pool, "o-bad").await;
    // Age is not the question: both are ancient.
    sqlx::query(
        "update rbpmn_instance set created_at = now() - interval '400 days', \
         completed_at = now() - interval '400 days' where id = any($1)",
    )
    .bind(vec![active, failed])
    .execute(&db.pool)
    .await
    .unwrap();

    let report = engine
        .sweep_retention_once(&options(after(0)))
        .await
        .unwrap();
    assert_eq!(report, Default::default());
    assert!(events_of(&db.pool, active).await > 0);
    assert!(events_of(&db.pool, failed).await > 0);
    assert!(work_items_of(&db.pool, active).await > 0);

    // The escape hatch is explicit, one instance at a time, and refuses an
    // instance that is still running.
    assert!(matches!(
        engine.delete_instance(active).await,
        Err(EngineError::InstanceStillActive(_))
    ));
    let sink = Recorder::new(false);
    engine.register_archive(sink.clone());
    let report = engine.delete_instance(failed).await.unwrap();
    assert_eq!(report.instances_deleted, 1);
    assert!(report.floor > EventCursor::default());
    // Even the escape hatch archives: it is not a way around the audit trail.
    assert_eq!(sink.records().len(), 1);
    assert_eq!(sink.records()[0].status, "failed");
    assert_eq!(events_of(&db.pool, failed).await, 0);
    db.drop().await;
}

/// Retention is opt-in twice over: no sweeper, nothing happens; and a
/// sweeper whose policy is `forever` deletes nothing either.
#[tokio::test]
async fn forever_is_a_valid_and_inert_policy() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;
    let done = completed_instance(&engine, &db.pool, "o-1").await;
    backdate(&db.pool, done, 400 * 24 * 3600).await;

    let report = engine
        .sweep_retention_once(&options(RetentionPolicy::forever()))
        .await
        .unwrap();
    assert_eq!(report, Default::default());
    assert!(events_of(&db.pool, done).await > 0);
    db.drop().await;
}

/// A per-key override beats the sweeper default, in both directions.
#[tokio::test]
async fn per_key_policy_overrides_the_default() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;
    let done = completed_instance(&engine, &db.pool, "o-1").await;
    backdate(&db.pool, done, 3600).await;

    // Default would delete it; the key's own policy says keep forever.
    engine
        .set_retention_policy("p", RetentionPolicy::forever())
        .await
        .unwrap();
    let report = engine
        .sweep_retention_once(&options(after(60)))
        .await
        .unwrap();
    assert_eq!(report, Default::default());
    assert!(events_of(&db.pool, done).await > 0);
    assert_eq!(
        engine.retention_policy("p").await.unwrap(),
        Some(RetentionPolicy::forever())
    );

    // Now the reverse: default keeps forever, the key's policy retires it.
    engine.set_retention_policy("p", after(60)).await.unwrap();
    let report = engine
        .sweep_retention_once(&options(RetentionPolicy::forever()))
        .await
        .unwrap();
    assert_eq!(report.instances_deleted, 1);
    assert_eq!(events_of(&db.pool, done).await, 0);
    db.drop().await;
}

/// "Forever" is spelled `None`, never as a very large duration: seconds are
/// stored as bigint, and `Duration::MAX.as_secs() as i64` is -1 — a cutoff in
/// the *future*, which would delete every terminal record on the next sweep.
#[tokio::test]
async fn an_out_of_range_retention_age_is_refused() {
    let err = RetentionPolicy::new(Some(Duration::MAX)).unwrap_err();
    assert!(matches!(err, EngineError::InvalidRetentionPolicy(_)));
    assert_eq!(RetentionPolicy::forever().retain(), None);
    assert_eq!(
        RetentionPolicy::new(Some(Duration::from_secs(60)))
            .unwrap()
            .retain(),
        Some(Duration::from_secs(60))
    );
}

/// A policy naming a definition that was never deployed is refused, not
/// stored: it would otherwise sit there looking like protection while the
/// sweeper's default deleted the very history it meant to keep.
#[tokio::test]
async fn a_policy_for_an_unknown_definition_is_refused() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;

    assert!(matches!(
        engine
            .set_retention_policy("odrers", RetentionPolicy::forever())
            .await,
        Err(EngineError::UnknownDefinition(_))
    ));
    assert_eq!(engine.retention_policy("odrers").await.unwrap(), None);
    engine
        .set_retention_policy("p", RetentionPolicy::forever())
        .await
        .unwrap();
    db.drop().await;
}

// ---------------------------------------------------------------- definitions

/// Definitions are bounded, so they are never swept — and once retention has
/// retired a record, the definition that explains its archived element ids is
/// pinned too. Without that counter the guard would be void exactly when it
/// matters: retention removes the instance rows the guard counts.
#[tokio::test]
async fn definitions_go_only_by_hand() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;
    let done = completed_instance(&engine, &db.pool, "o-1").await;
    backdate(&db.pool, done, 3600).await;

    // While the record exists, the definition that explains it is pinned.
    let listed = engine.prunable_definitions().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].instances, listed[0].retired_instances), (1, 0));
    assert!(listed[0].blocked_by.is_some());
    assert!(matches!(
        engine.delete_definition("p", 1).await,
        Err(EngineError::DefinitionInUse { .. })
    ));

    // The sweeper never takes the definition, however old.
    engine
        .sweep_retention_once(&options(after(60)))
        .await
        .unwrap();
    assert_eq!(engine.prunable_definitions().await.unwrap().len(), 1);

    // The record is gone, but its history is in someone's archive keyed by
    // this model's element ids — so the definition is still refused, now with
    // a different reason.
    let listed = engine.prunable_definitions().await.unwrap();
    assert_eq!((listed[0].instances, listed[0].retired_instances), (0, 1));
    let reason = listed[0].blocked_by.clone().unwrap();
    assert!(reason.contains("retired"), "unhelpful reason: {reason}");
    match engine.delete_definition("p", 1).await {
        Err(EngineError::DefinitionInUse { reason, .. }) => {
            assert!(reason.contains("retired"), "unhelpful reason: {reason}")
        }
        other => panic!("expected DefinitionInUse, got {other:?}"),
    }

    // A definition that never ran has nothing to explain, so it goes.
    deploy_short(&engine, "unused").await;
    engine
        .set_retention_policy("unused", after(60))
        .await
        .unwrap();
    engine.delete_definition("unused", 1).await.unwrap();
    // The last version took its now-dangling policy with it.
    assert_eq!(engine.retention_policy("unused").await.unwrap(), None);
    assert_eq!(engine.prunable_definitions().await.unwrap().len(), 1);

    assert!(matches!(
        engine.delete_definition("unused", 1).await,
        Err(EngineError::UnknownDefinitionVersion { .. })
    ));
    db.drop().await;
}

// --------------------------------------------------------------- concurrency

/// Two sweepers, one database: no error, no double deletion, and a floor
/// that only ever moves forward.
#[tokio::test]
async fn concurrent_sweeps_are_safe() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;

    for n in 0..6 {
        let id = completed_instance(&engine, &db.pool, &format!("o-{n}")).await;
        backdate(&db.pool, id, 3600).await;
    }
    let total: i64 = sqlx::query_scalar("select count(*) from rbpmn_event")
        .fetch_one(&db.pool)
        .await
        .unwrap();

    let mut opts = options(after(60));
    opts.max_instances = 2;
    let (a, b, c) = tokio::join!(
        {
            let e = engine.clone();
            let o = opts.clone();
            async move { sweep_until_idle(&e, &o).await }
        },
        {
            let e = engine.clone();
            let o = opts.clone();
            async move { sweep_until_idle(&e, &o).await }
        },
        {
            let e = engine.clone();
            let o = opts.clone();
            async move { sweep_until_idle(&e, &o).await }
        },
    );

    // Exactly the six records, deleted exactly once between them.
    assert_eq!(a.0 + b.0 + c.0, 6);
    assert_eq!(a.1 + b.1 + c.1, total as u64);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from rbpmn_event")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from rbpmn_instance")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        0
    );
    db.drop().await;
}

/// Sweep until a pass moves nothing; returns (instances, events) deleted.
async fn sweep_until_idle(engine: &Engine, options: &RetentionOptions) -> (u64, u64) {
    let (mut instances, mut events) = (0, 0);
    let mut floor = EventCursor::default();
    for _ in 0..50 {
        let report = engine.sweep_retention_once(options).await.unwrap();
        assert!(report.floor >= floor, "the floor moved backwards");
        floor = report.floor;
        if !report.moved() {
            // Another node may still be working; give it a turn.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let again = engine.sweep_retention_once(options).await.unwrap();
            instances += again.instances_deleted;
            events += again.events_deleted;
            if !again.moved() {
                break;
            }
            continue;
        }
        instances += report.instances_deleted;
        events += report.events_deleted;
    }
    (instances, events)
}

/// An instance being stepped right now is skipped, not fought over: the
/// sweep never blocks behind a caller's transaction and never errors.
#[tokio::test]
async fn a_locked_instance_is_left_for_the_next_pass() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;
    let a = completed_instance(&engine, &db.pool, "o-a").await;
    let b = completed_instance(&engine, &db.pool, "o-b").await;
    backdate(&db.pool, a, 3600).await;
    backdate(&db.pool, b, 3600).await;

    let mut holder = db.pool.begin().await.unwrap();
    sqlx::query("select id from rbpmn_instance where id = $1 for update")
        .bind(a)
        .fetch_one(&mut *holder)
        .await
        .unwrap();

    // b retires; a is skipped rather than waited on.
    let report = engine
        .sweep_retention_once(&options(after(60)))
        .await
        .unwrap();
    assert_eq!(report.instances_deleted, 1);
    assert!(events_of(&db.pool, a).await > 0);
    assert_eq!(events_of(&db.pool, b).await, 0);

    holder.rollback().await.unwrap();
    let report = engine
        .sweep_retention_once(&options(after(60)))
        .await
        .unwrap();
    assert_eq!(report.instances_deleted, 1);
    assert_eq!(events_of(&db.pool, a).await, 0);
    db.drop().await;
}

/// The batch is bounded by events as well as instances, and never splits a
/// record — an archived instance is complete or absent.
#[tokio::test]
async fn a_batch_never_splits_an_instance() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;
    let a = completed_instance(&engine, &db.pool, "o-a").await;
    let b = completed_instance(&engine, &db.pool, "o-b").await;
    backdate(&db.pool, a, 7200).await;
    backdate(&db.pool, b, 3600).await;
    let per_instance = events_of(&db.pool, a).await;

    let mut opts = options(after(60));
    // Room for exactly one record: the second does not get half-taken.
    opts.max_events = per_instance as u32;
    let sink = Recorder::new(false);
    engine.register_archive(sink.clone());

    let report = engine.sweep_retention_once(&opts).await.unwrap();
    assert_eq!(report.instances_deleted, 1);
    assert_eq!(report.events_deleted, per_instance as u64);
    assert_eq!(report.oversized_skipped, 0);
    let records = sink.records();
    assert_eq!(records.len(), 1);
    // Oldest first, and whole.
    assert_eq!(records[0].id, a);
    assert_eq!(records[0].events.len() as i64, per_instance);
    assert!(events_of(&db.pool, b).await > 0);
    db.drop().await;
}

/// A record too large for one batch is skipped — loudly, and *without being
/// loaded* — and its neighbours retire anyway. The earlier rule ("always take
/// the first candidate whole, so an oversized record cannot stall retention")
/// did the opposite: an oversized record is by definition the oldest
/// candidate, so every pass would load its whole history, die, and retry it
/// forever.
#[tokio::test]
async fn an_oversized_record_is_skipped_and_its_neighbours_still_retire() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;
    deploy_short(&engine, "tiny").await;

    let big = completed_instance(&engine, &db.pool, "o-big").await;
    let small = short_instance(&engine, &db.pool, "tiny", "o-small").await;
    let big_events = events_of(&db.pool, big).await;
    let small_events = events_of(&db.pool, small).await;
    assert!(small_events < big_events, "fixtures must differ in size");
    // The oversized one is also the oldest — the position that used to jam
    // the sweeper permanently.
    backdate(&db.pool, big, 7200).await;
    backdate(&db.pool, small, 3600).await;

    let mut opts = options(after(60));
    opts.max_events = (big_events - 1) as u32;
    let sink = Recorder::new(false);
    engine.register_archive(sink.clone());

    let report = engine.sweep_retention_once(&opts).await.unwrap();
    assert_eq!(report.oversized_skipped, 1);
    assert_eq!(report.instances_deleted, 1);
    assert_eq!(events_of(&db.pool, small).await, 0);
    // Untouched, and never materialised: the sink only ever saw the small one.
    assert_eq!(events_of(&db.pool, big).await, big_events);
    let records = sink.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, small);

    // Raising the ceiling retires it; nothing had to be repaired first.
    opts.max_events = big_events as u32;
    let report = engine.sweep_retention_once(&opts).await.unwrap();
    assert_eq!((report.instances_deleted, report.oversized_skipped), (1, 0));
    assert_eq!(events_of(&db.pool, big).await, 0);
    db.drop().await;
}

/// Without a sink there is nothing to archive, so event bodies are never
/// loaded — the plan still names exactly the same records.
#[tokio::test]
async fn no_sink_means_no_event_bodies_are_loaded() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;
    let done = completed_instance(&engine, &db.pool, "o-1").await;
    backdate(&db.pool, done, 3600).await;

    let batch = engine.plan_retention(&options(after(60))).await.unwrap();
    assert_eq!(batch.records().instances.len(), 1);
    assert_eq!(batch.records().instances[0].id, done);
    assert!(batch.records().instances[0].events.is_empty());

    // With one registered, the same plan carries the whole history.
    engine.register_archive(Recorder::new(false));
    let batch = engine.plan_retention(&options(after(60))).await.unwrap();
    assert_eq!(
        batch.records().instances[0].events.len() as i64,
        events_of(&db.pool, done).await
    );
    db.drop().await;
}

/// Retention against a live system: instances starting, stepping and
/// completing while a sweeper retires everything the moment it is eligible.
/// The operator-facing fsck is the assertion — the same invariant set the
/// storm and chaos runs use, now including retention's own three.
#[tokio::test]
async fn retention_is_safe_against_a_live_system() {
    let db = TestDb::create().await;
    let engine = engine(&db).await;
    deploy_task_kinds(&engine).await;

    // Nothing is spared by age: a record is eligible the moment it closes.
    let mut opts = options(after(0));
    opts.max_instances = 3;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sweeper = tokio::spawn({
        let engine = engine.clone();
        let stop = stop.clone();
        let opts = opts.clone();
        async move {
            let mut deleted = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                deleted += engine
                    .sweep_retention_once(&opts)
                    .await
                    .unwrap()
                    .instances_deleted;
                tokio::task::yield_now().await;
            }
            deleted
        }
    });

    // Three instances deliberately left mid-flight, parked on a work item.
    let mut live = Vec::new();
    for n in 0..3 {
        live.push(
            engine
                .start(
                    "p",
                    Some(&format!("live-{n}")),
                    serde_json::json!({ "order": { "id": format!("live-{n}") } }),
                )
                .await
                .unwrap()
                .id,
        );
    }

    // ...while twenty run to completion underneath the sweeper.
    for n in 0..20 {
        let order = format!("o-{n}");
        engine
            .start(
                "p",
                Some(&order),
                serde_json::json!({ "order": { "id": order } }),
            )
            .await
            .unwrap();
        for element in ["st", "ut"] {
            let item: Uuid = sqlx::query_scalar(
                "select w.id from rbpmn_work_item w join rbpmn_instance i on i.id = w.instance_id \
                 where i.business_key = $1 and w.element_id = $2",
            )
            .bind(&order)
            .bind(element)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            engine
                .complete_work_item(item, serde_json::json!({}))
                .await
                .unwrap();
        }
        engine
            .correlate("ShipmentConfirmed", &order, serde_json::json!({}))
            .await
            .unwrap();
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    sweeper.await.unwrap();
    while engine.sweep_retention_once(&opts).await.unwrap().moved() {}

    // Every completed record retired; the in-flight ones are untouched and
    // still steppable — retention took nothing it was not owed.
    assert_eq!(
        sqlx::query_scalar::<_, i64>("select count(*) from rbpmn_instance")
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        live.len() as i64
    );
    for id in &live {
        assert!(work_items_of(&db.pool, *id).await > 0);
        assert!(events_of(&db.pool, *id).await > 0);
    }
    let item: Uuid = sqlx::query_scalar(
        "select id from rbpmn_work_item where instance_id = $1 and element_id = 'st'",
    )
    .bind(live[0])
    .fetch_one(&db.pool)
    .await
    .unwrap();
    engine
        .complete_work_item(item, serde_json::json!({}))
        .await
        .unwrap();

    let problems = harness::fsck(&db.pool).await;
    assert!(problems.is_empty(), "fsck found: {problems:?}");
    db.drop().await;
}
