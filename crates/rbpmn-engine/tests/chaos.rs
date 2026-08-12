//! Chaos (docs/stress-testing.md §5) — the crash tests the design brief has
//! listed as testing-strategy #5 since phase 2 with no implementation.
//!
//! The transactional design's central promise is "kill it anywhere, converge
//! on restart": the engine advances tokens inside a transaction, wait states
//! are the transaction boundaries, so a process that dies mid-step leaves
//! nothing half-done. Nothing tested that.
//!
//! What makes a chaos round evidence rather than a survival anecdote is the
//! assertion at the end, and it is the same one `storm.rs` uses: the fsck must
//! be clean, every instance must drain, and every instance's whole history
//! must still re-derive through the pure core. Convergence is not "it kept
//! running" — it is "the history is exactly what an uninterrupted run would
//! have produced".
//!
//! Faults injected: backends terminated mid-transaction, nodes dropped and
//! rebuilt while work is in flight, handlers that fail and panic, and leases
//! short enough that reclaim races run continuously.

mod harness;

use harness::*;
use rbpmn_engine::testing::TestDb;
use rbpmn_engine::{Engine, HandlerFailure, ServiceTaskHandler, WorkItem, WorkerOptions};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use uuid::Uuid;

/// Node pools are tagged so chaos can terminate *their* backends and leave the
/// test's own control connection alone.
const NODE_TAG: &str = "rbpmn_chaos_node";

async fn node_pool(url: &str) -> PgPool {
    let options = PgConnectOptions::from_str(url)
        .unwrap()
        .application_name(NODE_TAG);
    PgPoolOptions::new()
        // Small pools so a terminate actually disrupts in-flight work rather
        // than being absorbed by an idle spare.
        .max_connections(3)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
        .unwrap()
}

/// A service-task handler that misbehaves in a different way each call:
/// succeeds, fails (walking the retry budget), or panics outright. The worker
/// loop is expected to contain all three.
struct ChaoticHandler {
    calls: AtomicUsize,
}

impl ServiceTaskHandler for ChaoticHandler {
    fn execute(
        &self,
        _item: WorkItem,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, HandlerFailure>> + Send + '_>> {
        let n = self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            match n % 4 {
                0 | 1 => Ok(serde_json::json!({ "handled": n })),
                2 => Err(HandlerFailure {
                    code: None,
                    message: "chaos: deliberate handler failure".into(),
                }),
                _ => panic!(
                    "chaos: DELIBERATE handler panic — expected; this test asserts \
                     the worker loop contains it"
                ),
            }
        })
    }
}

/// Terminate every node backend that is currently inside a transaction —
/// the sharpest version of "kill it anywhere", since those are exactly the
/// connections holding row locks and half-applied steps.
async fn terminate_node_backends(control: &PgPool) -> i64 {
    let killed: i64 = sqlx::query(
        "select count(*) from ( \
           select pg_terminate_backend(pid) from pg_stat_activity \
           where datname = current_database() and application_name = $1 \
             and pid <> pg_backend_pid()) x",
    )
    .bind(NODE_TAG)
    .fetch_one(control)
    .await
    .map(|r| r.get(0))
    .unwrap_or(0);
    killed
}

#[tokio::test]
async fn the_engine_converges_through_chaos() {
    let db = TestDb::create().await;
    let setup = engine_on(db.pool.clone()).await;
    setup.migrate().await.unwrap();
    // `unresolved-topic` is checked against the *deploying* engine's
    // environment, so the topic must be known here too, not only on the nodes.
    setup.register_handler(
        "chaos",
        Arc::new(ChaoticHandler {
            calls: AtomicUsize::new(0),
        }),
    );

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
            &with_process_id(&fixture("accept/16-foreign-binding-warn.bpmn"), "svc"),
            &rbpmn_core::Bindings::new().topic("st", "chaos"),
        )
        .await
        .unwrap();

    let deadlocks_before = deadlocks(&db.pool).await;
    let rounds: u32 = std::env::var("RBPMN_CHAOS_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);

    // Nodes are rebuilt during the run, so they live behind a swappable slot.
    let url = db.url();
    type Node = (Arc<Engine>, PgPool);
    let mut slots: Vec<Arc<tokio::sync::RwLock<Node>>> = Vec::new();
    for _ in 0..3 {
        let pool = node_pool(&url).await;
        let engine = Arc::new(engine_on(pool.clone()).await);
        engine.register_handler(
            "chaos",
            Arc::new(ChaoticHandler {
                calls: AtomicUsize::new(0),
            }),
        );
        slots.push(Arc::new(tokio::sync::RwLock::new((engine, pool))));
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut actors = Vec::new();

    // Push-mode workers (service tasks, chaotic handler) and schedulers.
    for slot in &slots {
        let (slot, stop) = (slot.clone(), stop.clone());
        actors.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let engine = slot.read().await.0.clone();
                // A short lease is the whole recovery mechanism for a node
                // that dies holding one: there is no reaper, the item simply
                // becomes claimable again once `lock_until` passes. At the
                // 600s default a killed node's work would be stranded for ten
                // minutes — correct, and far longer than this test waits.
                let _ = engine
                    .work_once(&WorkerOptions {
                        lease: Duration::from_millis(250),
                        poll_interval: Duration::from_millis(10),
                        ..WorkerOptions::default()
                    })
                    .await;
                let _ = engine.fire_due_timer().await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }));
    }

    // Pull-mode consumers on a deliberately tiny lease: expiry and reclaim
    // race continuously, so completions from stale owners must be refused.
    let completed = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));
    for w in 0..6 {
        let (slot, stop) = (slots[w % slots.len()].clone(), stop.clone());
        let (completed, refused) = (completed.clone(), refused.clone());
        actors.push(tokio::spawn(async move {
            let mut options = rbpmn_engine::GetTaskOptions::new(format!("chaos-worker-{w}"));
            // The floor the API allows (10ms), so leases lapse *during* the
            // pretend work below and peers reclaim underneath us. Completion
            // authority must follow the current lease, not the claim.
            options.ttl = Duration::from_millis(10);
            while !stop.load(Ordering::Relaxed) {
                let engine = slot.read().await.0.clone();
                let mut idle = true;
                for topic in ["ta", "tb", "ut", "t_esc"] {
                    if let Ok(Some(task)) = engine.get_task(topic, &options).await {
                        idle = false;
                        // Outlive the lease deliberately.
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        match engine
                            .complete_task(task.id, &options.owner, serde_json::json!({}))
                            .await
                        {
                            Ok(_) => completed.fetch_add(1, Ordering::Relaxed),
                            Err(_) => refused.fetch_add(1, Ordering::Relaxed),
                        };
                    }
                }
                if idle {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }));
    }

    // The chaos loop: terminate backends, and periodically rebuild a node
    // outright (pool and all) as a process restart.
    let killed = Arc::new(AtomicUsize::new(0));
    let restarts = Arc::new(AtomicUsize::new(0));
    {
        let (slots, stop) = (slots.clone(), stop.clone());
        let (killed, restarts) = (killed.clone(), restarts.clone());
        let (control, url) = (db.pool.clone(), url.clone());
        actors.push(tokio::spawn(async move {
            let mut round = 0usize;
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(120)).await;
                let n = terminate_node_backends(&control).await;
                killed.fetch_add(n as usize, Ordering::Relaxed);
                if round.is_multiple_of(2) {
                    // Full node restart: drop the engine and its pool, build
                    // a new one. Anything it was doing is simply gone.
                    let slot = &slots[round % slots.len()];
                    let pool = node_pool(&url).await;
                    let fresh = Arc::new(engine_on(pool.clone()).await);
                    fresh.register_handler(
                        "chaos",
                        Arc::new(ChaoticHandler {
                            calls: AtomicUsize::new(0),
                        }),
                    );
                    let (_old_engine, old_pool) =
                        std::mem::replace(&mut *slot.write().await, (fresh, pool));
                    old_pool.close().await;
                    restarts.fetch_add(1, Ordering::Relaxed);
                }
                round += 1;
            }
        }));
    }

    // The workload, started through the control pool so a terminated node
    // cannot lose a start we are counting on.
    let mut instances: Vec<(Uuid, serde_json::Value)> = Vec::new();
    for _ in 0..rounds {
        for key in ["par", "timed", "svc"] {
            let vars = serde_json::json!({});
            let id = setup.start(key, None, vars.clone()).await.unwrap().id;
            instances.push((id, vars));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Chaos stops; the system gets a quiet window to converge on its own.
    // This is the actual claim under test — not that it survived, but that it
    // finishes the work afterwards without help.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let killed_total = killed.load(Ordering::Relaxed);
    let restarts_total = restarts.load(Ordering::Relaxed);
    actors.remove(actors.len() - 1).abort(); // the chaos loop only

    let mut drained = false;
    for _ in 0..300 {
        let active = count(
            &db.pool,
            "select count(*) from rbpmn_instance where status = 'active'",
        )
        .await;
        if active == 0 {
            drained = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop.store(true, Ordering::Relaxed);
    for actor in actors {
        let _ = tokio::time::timeout(Duration::from_secs(5), actor).await;
    }

    // ---------------------------------------------------------- convergence

    let stuck: Vec<(String, String)> = sqlx::query(
        "select i.id::text as id, i.definition_key as k from rbpmn_instance i \
         where i.status = 'active' limit 5",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("id"), r.get("k")))
    .collect();
    assert!(
        drained,
        "instances never converged after chaos stopped: {stuck:?}"
    );

    let violations = fsck(&db.pool).await;
    assert!(violations.is_empty(), "fsck after chaos: {violations:?}");

    assert_eq!(
        deadlocks(&db.pool).await,
        deadlocks_before,
        "PostgreSQL recorded a deadlock during chaos"
    );

    // Exactly-once survived the crashes: a step that died mid-transaction
    // rolled back whole, so nothing was applied twice.
    for (kind, what) in [
        (
            "work-item-completed",
            "a work item completed more than once",
        ),
        ("timer-fired", "a timer fired more than once"),
    ] {
        let doubles = count(
            &db.pool,
            &format!(
                "select count(*) from (select instance_id, payload->>'id' as x \
                 from rbpmn_event where kind = '{kind}' group by 1, 2 having count(*) > 1) y"
            ),
        )
        .await;
        assert_eq!(doubles, 0, "{what}");
    }

    // The real convergence assertion: every history still re-derives.
    let mut events = 0;
    for (instance, initial) in &instances {
        events += replay_verify(&db.pool, *instance, initial)
            .await
            .unwrap_or_else(|e| panic!("instance {instance} did not replay after chaos: {e}"));
    }

    // Non-vacuity: chaos must actually have happened, and the lease must
    // actually have been contested. Without these the test would keep
    // passing after the faults stopped being injected.
    assert!(
        killed_total > 0,
        "no backend was ever terminated — chaos injected nothing"
    );
    assert!(
        restarts_total > 0,
        "no node was ever restarted — chaos injected nothing"
    );
    // Completion authority follows the current lease, exercised for real:
    // consumers deliberately outlive their 10ms lease, peers reclaim
    // underneath them, and the stale completions must be refused. This is the
    // property `spec/Lease.tla` proves of the protocol, checked here against
    // the database.
    assert!(
        refused.load(Ordering::Relaxed) > 0,
        "no completion was ever refused — the lease was never actually \
         contested, so completion authority is untested here"
    );

    let statuses: Vec<(String, i64)> = sqlx::query(
        "select status, count(*) as n from rbpmn_instance group by status order by n desc",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("status"), r.get("n")))
    .collect();
    println!(
        "chaos: {} instances, {events} events replayed, {killed_total} backends killed, \
         {restarts_total} node restarts, {} completions, {} refused by lease",
        instances.len(),
        completed.load(Ordering::Relaxed),
        refused.load(Ordering::Relaxed),
    );
    println!("  statuses: {statuses:?}");
    db.drop().await;
}
