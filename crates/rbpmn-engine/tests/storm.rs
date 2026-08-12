//! The storm, replay verification and the fsck (docs/stress-testing.md §4).
//!
//! These hunt the two third outcomes that live in the Postgres layer and
//! nowhere else:
//!
//!   outcome 3 — runs, but *differently* in Postgres than in the pure core.
//!               The architecture's central bet is that "the Postgres layer
//!               is a projection of this core"; until now exactly one fixture
//!               tested it. Replay verification tests it for every instance
//!               that ever ran.
//!   outcome 4 — correct alone, wrong under concurrency.
//!
//! The naive storm ("many workers, did it explode?") proves nothing. What
//! makes this one evidence is that `rbpmn_event` holds the complete ordered
//! history of every instance and is rich enough to reconstruct the *commands*
//! that produced it. So the storm's output becomes a corpus of executions to
//! re-derive against the core, offline, after the fact.

use rbpmn_core::{Command, Event, ExecutableProcess, InstanceState, InstanceStatus};
use rbpmn_engine::testing::TestDb;
use rbpmn_engine::{Engine, EventCursor};
use sqlx::{PgPool, Row};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use uuid::Uuid;

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rbpmn-model/tests/fixtures")
        .join(name);
    fs::read_to_string(path).unwrap()
}

// ---------------------------------------------------------------------- fsck

/// Relational invariants over the whole database, runnable at any moment
/// against a live system — the operator-facing form of the invariant set in
/// docs/stress-testing.md §1. Each entry is a query returning offending rows;
/// empty means clean.
///
/// Deliberately SQL rather than "rehydrate and check": this is what someone
/// debugging a production database would actually run, and it does not depend
/// on the loader being correct — which is part of what is under test.
const FSCK: &[(&str, &str)] = &[
    (
        "completed/terminated instances still hold tokens",
        "select i.id::text from rbpmn_instance i join rbpmn_token t on t.instance_id = i.id \
         where i.status in ('completed', 'terminated')",
    ),
    (
        "completed/terminated instances still hold open work items",
        "select w.id::text from rbpmn_work_item w join rbpmn_instance i on i.id = w.instance_id \
         where i.status in ('completed', 'terminated') and w.state in ('available', 'locked')",
    ),
    (
        "completed/terminated instances still hold timers",
        "select t.instance_id::text from rbpmn_timer t join rbpmn_instance i on i.id = t.instance_id \
         where i.status in ('completed', 'terminated')",
    ),
    (
        "completed/terminated instances still hold subscriptions",
        "select s.instance_id::text from rbpmn_subscription s \
         join rbpmn_instance i on i.id = s.instance_id \
         where i.status in ('completed', 'terminated')",
    ),
    (
        "a token waits on a work item that is not open",
        "select t.instance_id::text from rbpmn_token t \
         left join rbpmn_work_item w \
           on w.instance_id = t.instance_id and w.item_no = t.work_item_no \
         where t.wait_kind = 'work_item' \
           and (w.item_no is null or w.state not in ('available', 'locked') \
                or w.token_no is distinct from t.token_no)",
    ),
    (
        "an open work item has no token waiting on it",
        "select w.id::text from rbpmn_work_item w \
         join rbpmn_instance i on i.id = w.instance_id \
         left join rbpmn_token t \
           on t.instance_id = w.instance_id and t.work_item_no = w.item_no \
              and t.wait_kind = 'work_item' \
         where w.state in ('available', 'locked') and i.status = 'active' and t.token_no is null",
    ),
    (
        "a timer is armed on a token that does not exist",
        "select t.instance_id::text from rbpmn_timer t \
         left join rbpmn_token k on k.instance_id = t.instance_id and k.token_no = t.token_no \
         where k.token_no is null",
    ),
    (
        "a subscription is armed on a token that does not exist",
        "select s.instance_id::text from rbpmn_subscription s \
         left join rbpmn_token k on k.instance_id = s.instance_id and k.token_no = s.token_no \
         where k.token_no is null",
    ),
    (
        "two tokens parked at one join via the same flow",
        "select instance_id::text from rbpmn_token where wait_kind = 'join' \
         group by instance_id, element_id, arrived_via having count(*) > 1",
    ),
    (
        "a failed instance is not frozen at exactly one incident token",
        "select i.id::text from rbpmn_instance i where i.status = 'failed' \
           and (select count(*) from rbpmn_token t \
                where t.instance_id = i.id and t.wait_kind = 'incident') <> 1",
    ),
    (
        "a work item is locked without a live lease or an owner",
        "select id::text from rbpmn_work_item \
         where state = 'locked' and (lock_owner is null or lock_until is null)",
    ),
];

async fn fsck(pool: &PgPool) -> Vec<String> {
    let mut found = Vec::new();
    for (name, sql) in FSCK {
        let rows = sqlx::query(sql)
            .fetch_all(pool)
            .await
            .unwrap_or_else(|e| panic!("fsck query '{name}' failed: {e}"));
        if !rows.is_empty() {
            let ids: Vec<String> = rows.iter().take(3).map(|r| r.get::<String, _>(0)).collect();
            found.push(format!("{name}: {} row(s), e.g. {ids:?}", rows.len()));
        }
    }
    found
}

// -------------------------------------------------------- replay verification

/// Compile the definition an instance is pinned to, exactly as the engine
/// does (same XML, same bindings manifest, key as the process id).
async fn pinned_process(pool: &PgPool, instance: Uuid) -> ExecutableProcess {
    let row = sqlx::query(
        "select d.bpmn_xml, d.bindings, d.key from rbpmn_definition d \
         join rbpmn_instance i on i.definition_id = d.id where i.id = $1",
    )
    .bind(instance)
    .fetch_one(pool)
    .await
    .unwrap();
    let xml: String = row.get("bpmn_xml");
    let key: String = row.get("key");
    let bindings = serde_json::from_value(row.get::<serde_json::Value, _>("bindings")).unwrap();
    let defs = rbpmn_model::parse(&xml).unwrap();
    ExecutableProcess::compile(&defs, &key, &bindings).unwrap()
}

/// One instance's persisted history, in semantic order.
///
/// Ordering by `id` is exactly the phase-5 guarantee: an instance's steps
/// serialize on its row lock, so ids are allocated in emission order. (Stream
/// order is `(txid, id)` and is a different question — see `events.rs`.)
///
/// Engine-level events (`work-item-retrying`, `timer-fire-failed`) are not
/// `Event` variants, so failing to deserialize *is* the projection onto
/// core-visible kinds. No hand-maintained list to drift.
async fn core_events(pool: &PgPool, instance: Uuid) -> Vec<Event> {
    sqlx::query("select payload from rbpmn_event where instance_id = $1 order by id")
        .bind(instance)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|r| serde_json::from_value::<Event>(r.get("payload")).ok())
        .collect()
}

/// Reconstruct the command sequence from the history. Most events are
/// consequences; only these four are stimuli the outside world supplied. A
/// `variables-patched` immediately following its trigger carries that
/// command's merge patch — `step` emits them adjacently.
fn commands_from(events: &[Event]) -> Vec<Command> {
    let patch_after = |i: usize| -> serde_json::Value {
        match events.get(i + 1) {
            Some(Event::VariablesPatched { patch }) => patch.clone(),
            _ => serde_json::json!({}),
        }
    };
    let mut commands = Vec::new();
    for (i, event) in events.iter().enumerate() {
        match event {
            Event::WorkItemCompleted { id, .. } => commands.push(Command::CompleteWorkItem {
                id: *id,
                patch: patch_after(i),
            }),
            Event::WorkItemFailed { id, code, .. } => commands.push(Command::RaiseError {
                id: *id,
                code: code.clone(),
            }),
            Event::TimerFired { id, .. } => commands.push(Command::FireTimer { id: *id }),
            Event::MessageReceived { id, .. } => commands.push(Command::DeliverMessage {
                id: *id,
                patch: patch_after(i),
            }),
            _ => {}
        }
    }
    commands
}

/// **Replay verification.** Re-derive the instance's history by feeding its
/// reconstructed stimuli to the pure core, and assert the core produces
/// exactly the trace the database recorded. This is the systematic form of
/// "the Postgres layer is a projection of this core".
async fn replay_verify(
    pool: &PgPool,
    instance: Uuid,
    initial: &serde_json::Value,
) -> Result<usize, String> {
    let recorded = core_events(pool, instance).await;
    if recorded.is_empty() {
        return Err("instance has no core events at all".into());
    }
    let proc = pinned_process(pool, instance).await;

    let mut state = InstanceState::new();
    let mut replayed: Vec<Event> = step_or(
        &proc,
        &mut state,
        Command::Start {
            variables: initial.clone(),
        },
    )?;
    for command in commands_from(&recorded) {
        replayed.extend(step_or(&proc, &mut state, command)?);
    }

    let want: Vec<String> = recorded.iter().map(|e| e.to_string()).collect();
    let got: Vec<String> = replayed.iter().map(|e| e.to_string()).collect();
    if want != got {
        let at = want
            .iter()
            .zip(got.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(want.len().min(got.len()));
        return Err(format!(
            "trace diverges at {at}\n  database: {:?}\n  core:     {:?}",
            &want[at.saturating_sub(2)..want.len().min(at + 3)],
            &got[at.saturating_sub(2)..got.len().min(at + 3)],
        ));
    }

    // The final state must agree too, not just the path to it.
    let db_status: String = sqlx::query("select status from rbpmn_instance where id = $1")
        .bind(instance)
        .fetch_one(pool)
        .await
        .unwrap()
        .get("status");
    let replayed_status = match state.status {
        InstanceStatus::Active | InstanceStatus::Created => "active",
        InstanceStatus::Completed => "completed",
        InstanceStatus::Terminated => "terminated",
        InstanceStatus::Failed => "failed",
    };
    if db_status != replayed_status {
        return Err(format!(
            "status differs: database {db_status}, core {replayed_status}"
        ));
    }
    Ok(recorded.len())
}

fn step_or(
    proc: &ExecutableProcess,
    state: &mut InstanceState,
    command: Command,
) -> Result<Vec<Event>, String> {
    rbpmn_core::step(proc, state, command.clone())
        .map_err(|e| format!("replaying {command:?}: {e}"))
}

// --------------------------------------------------------------------- setup

async fn engine_on(pool: PgPool) -> Engine {
    Engine::builder(pool).retry_backoff(Duration::ZERO).build()
}

/// A boundary timer that actually fires during the test, so timer claims race
/// completions on the same instance — the interleaving `spec/LockOrder.tla`
/// says is the interesting one.
fn racing_timer_xml() -> String {
    fixture("accept/09-timer-boundary.bpmn").replace("PT1H", "PT0S")
}

// ------------------------------------------------------- outcome 3, in quiet

/// Replay verification on a calm workload: no concurrency, so a failure here
/// is a projection bug and nothing else. Covers the paths that write history
/// in different ways — parallel joins, boundary timers, correlation, retries
/// into an incident.
#[tokio::test]
async fn the_projection_replays_exactly_as_the_core() {
    let db = TestDb::create().await;
    let engine = engine_on(db.pool.clone()).await;
    engine.migrate().await.unwrap();

    // Distinct process ids, or the second deploy is merely a new *version* of
    // the first key and `start` silently runs the wrong model.
    engine
        .deploy(
            &with_process_id(&fixture("accept/03-parallel-gateway.bpmn"), "par"),
            &Default::default(),
        )
        .await
        .unwrap();
    engine
        .deploy(
            &with_process_id(&fixture("accept/17-message-catch.bpmn"), "msg"),
            &rbpmn_core::Bindings::new().correlation("c", "order.id"),
        )
        .await
        .unwrap();

    let mut started: Vec<(Uuid, serde_json::Value)> = Vec::new();

    // Parallel joins, completed in both orders.
    for flip in [false, true] {
        let vars = serde_json::json!({ "run": flip });
        let id = engine.start("par", None, vars.clone()).await.unwrap().id;
        started.push((id, vars));
        let mut open = open_items(&db.pool, id).await;
        assert_eq!(open.len(), 2, "the parallel model must offer two tasks");
        if flip {
            open.reverse();
        }
        for (item, _) in open {
            engine
                .complete_work_item(item, serde_json::json!({}))
                .await
                .unwrap();
        }
    }

    // Correlation, including a patch delivered with the message.
    let vars = serde_json::json!({ "order": { "id": "o-1" } });
    let id = engine.start("msg", None, vars.clone()).await.unwrap().id;
    started.push((id, vars));
    engine
        .correlate(
            "WarehouseAck",
            "o-1",
            serde_json::json!({ "shipped": true }),
        )
        .await
        .unwrap();

    // An instance left mid-flight: replay must reproduce a partial history too.
    let vars = serde_json::json!({ "order": { "id": "o-open" } });
    let id = engine.start("msg", None, vars.clone()).await.unwrap().id;
    started.push((id, vars));

    let mut events = 0;
    for (instance, initial) in &started {
        events += replay_verify(&db.pool, *instance, initial)
            .await
            .unwrap_or_else(|e| panic!("instance {instance}: {e}"));
    }
    // Non-vacuity: replay must have re-derived real histories, not empty ones.
    assert!(
        events >= started.len() * 5,
        "suspiciously few events replayed: {events} across {} instances",
        started.len()
    );
    assert!(
        fsck(&db.pool).await.is_empty(),
        "fsck: {:?}",
        fsck(&db.pool).await
    );
    println!("replayed {} instances, {events} events", started.len());
    db.drop().await;
}

async fn open_items(pool: &PgPool, instance: Uuid) -> Vec<(Uuid, String)> {
    sqlx::query(
        "select id, element_id from rbpmn_work_item \
         where instance_id = $1 and state in ('available','locked') order by item_no",
    )
    .bind(instance)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("id"), r.get("element_id")))
    .collect()
}

// ------------------------------------------------------ outcome 4, the storm

/// Every fixture declares `id="p"`, so deploying several under one key would
/// make *versions* rather than distinct definitions. Rename the process.
fn with_process_id(xml: &str, id: &str) -> String {
    xml.replace("id=\"p\"", &format!("id=\"{id}\""))
        .replace("bpmnElement=\"p\"", &format!("bpmnElement=\"{id}\""))
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query(sql).fetch_one(pool).await.unwrap().get(0)
}

/// Postgres counts deadlocks per database, so this observes them even though
/// the worker and scheduler loops handle their own errors. The design brief
/// claims the shipped lock order has none; `spec/LockOrder.tla` proves it of
/// the protocol, and this checks the implementation agrees.
async fn deadlocks(pool: &PgPool) -> i64 {
    count(
        pool,
        "select deadlocks from pg_stat_database where datname = current_database()",
    )
    .await
}

#[tokio::test]
async fn a_storm_holds_every_global_invariant() {
    let db = TestDb::create().await;
    let setup = engine_on(db.pool.clone()).await;
    setup.migrate().await.unwrap();

    setup
        .deploy(
            &with_process_id(&fixture("accept/03-parallel-gateway.bpmn"), "par"),
            &Default::default(),
        )
        .await
        .unwrap();
    setup
        .deploy(
            &with_process_id(&racing_timer_xml(), "timed"),
            &Default::default(),
        )
        .await
        .unwrap();
    setup
        .deploy(
            &with_process_id(&fixture("accept/17-message-catch.bpmn"), "msg"),
            &rbpmn_core::Bindings::new().correlation("c", "order.id"),
        )
        .await
        .unwrap();

    // Crank with RBPMN_STORM_ROUNDS when hunting; 20 keeps the suite quick.
    let rounds: u32 = std::env::var("RBPMN_STORM_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    let deadlocks_before = deadlocks(&db.pool).await;

    // Three engines on genuinely separate connection pools — active-active by
    // construction, no singleton anywhere.
    let mut nodes = Vec::new();
    for _ in 0..3 {
        let pool = PgPool::connect(&db.url()).await.unwrap();
        nodes.push(Arc::new(engine_on(pool).await));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut actors = Vec::new();

    // Schedulers on every node: timers fire under competing consumers.
    for node in &nodes {
        let (node, stop) = (node.clone(), stop.clone());
        actors.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let _ = node.fire_due_timer().await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }));
    }

    // Pull-mode consumers competing for the same user-task topics.
    let completed = Arc::new(AtomicUsize::new(0));
    for w in 0..6 {
        let (node, stop, completed) = (
            nodes[w % nodes.len()].clone(),
            stop.clone(),
            completed.clone(),
        );
        actors.push(tokio::spawn(async move {
            let options = rbpmn_engine::GetTaskOptions::new(format!("worker-{w}"));
            while !stop.load(Ordering::Relaxed) {
                let mut idle = true;
                for topic in ["ta", "tb", "ut", "t_esc", "c"] {
                    if let Ok(Some(task)) = node.get_task(topic, &options).await {
                        idle = false;
                        // Completion may lose a race with a boundary timer;
                        // that is a legal outcome, not a failure.
                        if node
                            .complete_task(task.id, &options.owner, serde_json::json!({}))
                            .await
                            .is_ok()
                        {
                            completed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                if idle {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }));
    }

    // A tailer reading the stream *while* the storm runs — the safe horizon
    // under many overlapping commits, not a crafted two-transaction case.
    let seen: Arc<std::sync::Mutex<Vec<(i64, i64)>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let (node, stop, seen) = (nodes[0].clone(), stop.clone(), seen.clone());
        actors.push(tokio::spawn(async move {
            let mut cursor = EventCursor::default();
            let mut quiet_after_stop = 0;
            loop {
                let batch = node.read_events(cursor, 200).await.unwrap_or_default();
                for record in &batch {
                    seen.lock().unwrap().push((record.txid, record.id));
                    cursor = record.cursor();
                }
                if batch.is_empty() {
                    // Keep draining past `stop`: the horizon only releases a
                    // transaction's events once every older one has finished,
                    // so the tail arrives slightly after the writers stop.
                    quiet_after_stop += i32::from(stop.load(Ordering::Relaxed));
                    if quiet_after_stop > 150 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                } else {
                    quiet_after_stop = 0;
                }
            }
        }));
    }

    // The workload: instances of all three definitions, started concurrently
    // from every node, with correlations chasing the message ones.
    let mut instances: Vec<(Uuid, serde_json::Value)> = Vec::new();
    for round in 0..rounds {
        let node = &nodes[round as usize % nodes.len()];
        let empty = serde_json::json!({});
        for key in ["par", "timed"] {
            let id = node.start(key, None, empty.clone()).await.unwrap().id;
            instances.push((id, empty.clone()));
        }
        let vars = serde_json::json!({ "order": { "id": format!("o-{round}") } });
        let id = node.start("msg", None, vars.clone()).await.unwrap().id;
        instances.push((id, vars));
    }
    // Deliver every message; each must land on exactly one subscription.
    let mut delivered = 0;
    for round in 0..rounds {
        for attempt in 0..40 {
            match nodes[round as usize % nodes.len()]
                .correlate("WarehouseAck", &format!("o-{round}"), serde_json::json!({}))
                .await
            {
                Ok(_) => {
                    delivered += 1;
                    break;
                }
                // The subscription may not be armed yet.
                Err(_) if attempt < 39 => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(e) => panic!("correlate o-{round}: {e}"),
            }
        }
    }
    assert_eq!(
        delivered, rounds,
        "every message must be delivered exactly once"
    );

    // Let the storm drain.
    for _ in 0..200 {
        let active = count(
            &db.pool,
            "select count(*) from rbpmn_instance where status = 'active'",
        )
        .await;
        if active == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop.store(true, Ordering::Relaxed);
    for actor in actors {
        let _ = tokio::time::timeout(Duration::from_secs(5), actor).await;
    }

    // ---------------------------------------------------------- assertions

    let violations = fsck(&db.pool).await;
    assert!(violations.is_empty(), "fsck found: {violations:?}");

    assert_eq!(
        deadlocks(&db.pool).await,
        deadlocks_before,
        "PostgreSQL recorded a deadlock — the single lock order was violated"
    );

    // Exactly-once, counted from the log: no work item ever completed twice,
    // and no timer ever fired twice.
    let double_completions = count(
        &db.pool,
        "select count(*) from (select instance_id, payload->>'id' as item \
         from rbpmn_event where kind = 'work-item-completed' \
         group by 1, 2 having count(*) > 1) x",
    )
    .await;
    assert_eq!(
        double_completions, 0,
        "a work item completed more than once"
    );

    let double_fires = count(
        &db.pool,
        "select count(*) from (select instance_id, payload->>'id' as timer \
         from rbpmn_event where kind = 'timer-fired' \
         group by 1, 2 having count(*) > 1) x",
    )
    .await;
    assert_eq!(double_fires, 0, "a timer fired more than once");

    let double_deliveries = count(
        &db.pool,
        "select count(*) from (select instance_id, payload->>'id' as sub \
         from rbpmn_event where kind = 'message-received' \
         group by 1, 2 having count(*) > 1) x",
    )
    .await;
    assert_eq!(
        double_deliveries, 0,
        "a message was delivered more than once"
    );

    // Non-vacuity: the storm must actually have raced. The boundary-timer
    // model is built so completion and timeout compete on the same token, and
    // BOTH outcomes must occur — otherwise this is a sequential test wearing
    // a storm's clothes and would keep passing after the race stopped racing.
    let fired = count(
        &db.pool,
        "select count(*) from rbpmn_event where kind = 'timer-fired'",
    )
    .await;
    let disarmed = count(
        &db.pool,
        "select count(*) from rbpmn_event where kind = 'timer-cancelled'",
    )
    .await;
    assert!(
        fired > 0 && disarmed > 0,
        "the boundary-timer race never went both ways ({fired} fired, \
         {disarmed} cancelled) — the storm is not exercising the interleaving \
         spec/LockOrder.tla is about"
    );
    let stuck = count(
        &db.pool,
        "select count(*) from rbpmn_instance where status = 'active'",
    )
    .await;
    assert_eq!(
        stuck, 0,
        "instances never drained; the storm proved nothing"
    );

    // Every instance's whole history re-derives from the core.
    let mut events = 0;
    for (instance, initial) in &instances {
        events += replay_verify(&db.pool, *instance, initial)
            .await
            .unwrap_or_else(|e| panic!("instance {instance}: {e}"));
    }

    // The tailer saw a prefix of the stream, in order, with no duplicates and
    // nothing skipped.
    let tailed = seen.lock().unwrap().clone();
    let mut sorted = tailed.clone();
    sorted.sort();
    assert_eq!(
        tailed, sorted,
        "the event stream arrived out of (txid, id) order"
    );
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        tailed.len(),
        "the event stream repeated an event"
    );
    let total = count(&db.pool, "select count(*) from rbpmn_event").await;
    assert_eq!(
        tailed.len() as i64,
        total,
        "the tailing cursor did not deliver every event exactly once \
         (saw {} of {total}); the safe horizon is cluster-wide, so a \
         long-running transaction elsewhere can delay — but never drop — a tail",
        tailed.len()
    );

    let kinds =
        sqlx::query("select kind, count(*) as n from rbpmn_event group by kind order by n desc")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    let statuses = sqlx::query(
        "select status, count(*) as n from rbpmn_instance group by status order by n desc",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    println!(
        "storm: {} instances, {events} events replayed, {} tasks completed, \
         {} events tailed of {total}",
        instances.len(),
        completed.load(Ordering::Relaxed),
        tailed.len()
    );
    println!(
        "  statuses: {:?}",
        statuses
            .iter()
            .map(|r| (r.get::<String, _>("status"), r.get::<i64, _>("n")))
            .collect::<Vec<_>>()
    );
    println!(
        "  events:   {:?}",
        kinds
            .iter()
            .map(|r| (r.get::<String, _>("kind"), r.get::<i64, _>("n")))
            .collect::<Vec<_>>()
    );
    db.drop().await;
}
