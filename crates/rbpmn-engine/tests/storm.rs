//! The storm and replay verification (docs/stress-testing.md §4).
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
//!
//! Chaos — killing connections and restarting nodes under the same load —
//! is `chaos.rs`, built on the same harness.

mod harness;

use harness::*;
use rbpmn_engine::EventCursor;
use rbpmn_engine::testing::TestDb;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use uuid::Uuid;

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

// ------------------------------------------------------ outcome 4, the storm

/// Every fixture declares `id="p"`, so deploying several under one key would
/// make *versions* rather than distinct definitions. Rename the process.
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
    // Two-level nesting: scopes open and close across separate transactions,
    // so the scope projection is exercised under the same concurrency as
    // everything else rather than only in quiet integration tests.
    setup
        .deploy(
            &with_process_id(&fixture("accept/21-nested-subprocess.bpmn"), "nested"),
            &Default::default(),
        )
        .await
        .unwrap();
    // The message-boundary race: a payment arriving while a consumer is
    // claiming and completing the contest task. Deployed under its own
    // process id already (`ticket`), so unlike the others it needs no rename.
    setup
        .deploy(
            &fixture("accept/29-message-boundary.bpmn"),
            &rbpmn_core::Bindings::new().correlation("paid_during_contest", "ticket.reference"),
        )
        .await
        .unwrap();
    // The *non-interrupting* boundary (slice 2), whose race is a different
    // one: a note never competes with the review for the token, it competes
    // with the review's **arm**. So what varies per instance is whether the
    // delivery arrives before a consumer decides the review — and every
    // delivery that does land leaves a sibling token behind, which is how the
    // storm gets two tokens in one scope, a re-armed subscription, and a
    // completion that has to wait for work its host knows nothing about. The
    // side path's service task is claimed by the same pull consumers, so its
    // topic is declared rather than handled. Process id is already
    // `casefile`; no rename.
    setup.declare_topic("file_note").await.unwrap();
    setup
        .deploy(
            &fixture("accept/33-non-interrupting-message-boundary.bpmn"),
            &rbpmn_core::Bindings::new().correlation("note_received", "case.id"),
        )
        .await
        .unwrap();
    // The repeating timer (slice 3): the late-fee cycle at the one-minute
    // floor, renamed so it does not collide with fixture 29's `ticket`. It is
    // the one construct the storm drives through the *schedulers* rather than
    // through a verb: an armed occurrence is backdated and three scheduler
    // loops race to claim it. Its side task is the consumers' to complete.
    setup.declare_topic("add_late_fee").await.unwrap();
    setup
        .deploy(
            // Its process id is `ticket`, like fixture 29's, so the rename is
            // spelled out rather than going through `with_process_id`.
            &fixture("accept/40-late-fee-cycle.bpmn")
                .replace("R/P7D", "R/PT1M")
                .replace("id=\"ticket\"", "id=\"billing\"")
                .replace("bpmnElement=\"ticket\"", "bpmnElement=\"billing\""),
            &rbpmn_core::Bindings::new().correlation("await_payment", "ticket.reference"),
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
                for topic in [
                    "ta",
                    "tb",
                    "ut",
                    "t_esc",
                    "c",
                    "count",
                    "ship",
                    "handle_contest",
                    "review",
                    "file_note",
                    "add_late_fee",
                ] {
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
    let mut cycle_backdates: u32 = 0;
    let (mut notes_delivered, mut notes_refused) = (0u32, 0u32);
    for round in 0..rounds {
        let node = &nodes[round as usize % nodes.len()];
        let empty = serde_json::json!({});
        for key in ["par", "timed", "nested"] {
            let id = node.start(key, None, empty.clone()).await.unwrap().id;
            instances.push((id, empty.clone()));
        }
        let vars = serde_json::json!({ "order": { "id": format!("o-{round}") } });
        let id = node.start("msg", None, vars.clone()).await.unwrap().id;
        instances.push((id, vars));

        // The message boundary, driven per instance: two of every three
        // tickets get their payment *while* the consumers above are claiming
        // and completing `handle_contest`, in the same loop rather than after
        // it, so the two verbs really are in flight together. Whichever wins,
        // the other must be refused typed — and both must happen across the
        // rounds, which the non-vacuity assertion below insists on.
        let reference = format!("t-{round}");
        let vars = serde_json::json!({ "ticket": { "reference": reference.clone() } });
        let id = node.start("ticket", None, vars.clone()).await.unwrap().id;
        instances.push((id, vars));
        if !round.is_multiple_of(3) {
            match node
                .correlate("PAID", &reference, serde_json::json!({}))
                .await
            {
                Ok(_) => {}
                // The consumer got there first: the completion withdrew the
                // arm (404), or closed the instance under the delivery's
                // re-check (409). Both are the loser's typed answer.
                Err(rbpmn_engine::EngineError::NoSubscription { .. })
                | Err(rbpmn_engine::EngineError::InstanceNotActive(..)) => {}
                Err(e) => panic!("correlate PAID {reference}: {e}"),
            }
        }

        // The non-interrupting boundary, driven the same way: zero, one or
        // two notes per case, delivered while the consumers above are
        // claiming and completing `review`. A note does not close the host,
        // so the second one exercises the *re-arm* — without it the delivery
        // that consumed the first subscription would have left the boundary
        // dead and this would be a 404. Every third case takes no note at
        // all, which is what keeps both sides of the non-vacuity assertion
        // below present rather than assumed.
        let case = format!("c-{round}");
        let vars = serde_json::json!({ "case": { "id": case.clone() } });
        let id = node.start("casefile", None, vars.clone()).await.unwrap().id;
        instances.push((id, vars));
        for _ in 0..(round % 3) {
            match node.correlate("NOTE", &case, serde_json::json!({})).await {
                Ok(_) => notes_delivered += 1,
                // The reviewer decided first: the completion withdrew the arm
                // (404), or closed the instance under the delivery's re-check
                // (409). Legal outcomes, and the reason the counts below are
                // read from the log rather than from this loop.
                Err(rbpmn_engine::EngineError::NoSubscription { .. })
                | Err(rbpmn_engine::EngineError::InstanceNotActive(..)) => notes_refused += 1,
                Err(e) => panic!("correlate NOTE {case}: {e}"),
            }
        }

        // The repeating timer, driven through the schedulers. Backdating the
        // armed occurrence makes all three loops see it due at once and race
        // for it — advisory try-lock, NOWAIT on the instance row, re-check of
        // the timer row — and the winner's re-arm lands on the grid of the
        // previous due at or after now, a minute out. So one backdate is
        // exactly one fire, and the count settling at one (not two, not zero)
        // is the claim path's exactly-once under competing schedulers, for a
        // row that re-creates itself in the firing transaction. Every other
        // instance is backdated twice, so the re-armed occurrence fires too.
        // PAID then ends the host, which cancels the cycle and leaves the fee
        // items to the consumers.
        let reference = format!("b-{round}");
        let vars = serde_json::json!({ "ticket": { "reference": reference.clone() } });
        let id = node.start("billing", None, vars.clone()).await.unwrap().id;
        instances.push((id, vars));
        for fire in 1..=(1 + round % 2) {
            sqlx::query(
                "update rbpmn_timer set due_at = now() - interval '90 minutes' \
                 where instance_id = $1",
            )
            .bind(id)
            .execute(&db.pool)
            .await
            .unwrap();
            cycle_backdates += 1;
            let mut fired = 0;
            for _ in 0..600 {
                fired = fires_of(&db.pool, id).await;
                if fired >= fire {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(
                fired, fire,
                "billing {reference}: a backdated occurrence fires exactly once across \
                 the competing schedulers (saw {fired} after backdate {fire})"
            );
        }
        node.correlate("PAID", &reference, serde_json::json!({}))
            .await
            .unwrap_or_else(|e| panic!("correlate PAID {reference}: {e}"));
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
    // ...and the same statement for the message boundary: some tickets were
    // paid out from under an open task, some were decided before the payment
    // arrived. One-sided means the workload stopped racing.
    let boundary_fired = count(
        &db.pool,
        "select count(*) from rbpmn_event where kind = 'message-received' \
         and element_id = 'paid_during_contest'",
    )
    .await;
    let boundary_withdrawn = count(
        &db.pool,
        "select count(*) from rbpmn_event where kind = 'subscription-cancelled' \
         and element_id = 'paid_during_contest'",
    )
    .await;
    assert!(
        boundary_fired > 0 && boundary_withdrawn > 0,
        "the message-boundary race never went both ways ({boundary_fired} delivered, \
         {boundary_withdrawn} withdrawn by a completion) — the storm is not \
         exercising the interleaving spec/BoundaryExit.tla is about"
    );

    // ...and the same statement for the *non-interrupting* boundary, whose
    // two sides are different events. A note that landed while the review was
    // open is a `message-received` at the boundary; a review decided without
    // one is a completed case whose arm was withdrawn and that never received
    // anything. One-sided means the driver stopped racing the arm — and a run
    // in which no note ever landed would leave the re-arm, the sibling token
    // and "the instance outlives its host" entirely untested while staying
    // green.
    let notes = count(
        &db.pool,
        "select count(*) from rbpmn_event where kind = 'message-received' \
         and element_id = 'note_received'",
    )
    .await;
    let quiet_cases = count(
        &db.pool,
        "select count(*) from rbpmn_instance i where i.definition_key = 'casefile' \
           and i.status = 'completed' \
           and exists (select 1 from rbpmn_event e where e.instance_id = i.id \
                 and e.kind = 'subscription-cancelled' and e.element_id = 'note_received') \
           and not exists (select 1 from rbpmn_event e where e.instance_id = i.id \
                 and e.kind = 'message-received')",
    )
    .await;
    assert!(
        notes > 0 && quiet_cases > 0,
        "the non-interrupting boundary never went both ways ({notes} notes landed \
         on an open review, {quiet_cases} reviews decided without one) — nothing \
         here exercised the re-arm or the sibling token"
    );

    // Two exact identities, which is what a non-interrupting boundary lets a
    // storm assert that an interrupting one cannot. One side token per
    // delivery — no more (a delivery that also interrupted would leave the
    // host's continuation *and* the sibling) and no fewer. And one arm per
    // review entered plus exactly one re-arm per note: a boundary that failed
    // to re-arm, or re-armed twice, is off by the number of notes.
    let side_paths = count(
        &db.pool,
        "select count(*) from rbpmn_event where kind = 'element-started' \
         and element_id = 'file_note'",
    )
    .await;
    assert_eq!(side_paths, notes, "one side token per delivered note");
    let cases = count(
        &db.pool,
        "select count(*) from rbpmn_instance where definition_key = 'casefile'",
    )
    .await;
    let arms = count(
        &db.pool,
        "select count(*) from rbpmn_event where kind = 'message-subscribed' \
         and element_id = 'note_received'",
    )
    .await;
    assert_eq!(
        arms,
        cases + notes,
        "one arm per review entered ({cases}) plus one re-arm per note ({notes})"
    );
    // The repeating timer: each backdate fired exactly once under three
    // competing schedulers (asserted per fire in the loop); this is the shape
    // of the whole run. One side token per fire; one arm per instance plus one
    // re-arm per fire — a cycle that failed to re-arm, or re-armed twice, is
    // off by the fire count; one cancel per instance, because PAID ended
    // every host with an occurrence still armed; and at least one instance
    // whose *re-armed* occurrence fired, or the re-arm was never stormed.
    // `>=` on the fires, not `==`: the re-arm lands a minute out, and a run
    // cranked past a minute may see one fire on its own.
    let cycle_fires = count(
        &db.pool,
        "select count(*) from rbpmn_event where kind = 'timer-fired' \
         and element_id = 'late_fee_due'",
    )
    .await;
    assert!(
        cycle_fires >= cycle_backdates as i64,
        "{cycle_backdates} backdated occurrences, {cycle_fires} fires"
    );
    let fees = count(
        &db.pool,
        "select count(*) from rbpmn_event where kind = 'element-started' \
         and element_id = 'add_late_fee'",
    )
    .await;
    assert_eq!(fees, cycle_fires, "one side token per cycle fire");
    let billings = count(
        &db.pool,
        "select count(*) from rbpmn_instance where definition_key = 'billing'",
    )
    .await;
    let cycle_arms = count(
        &db.pool,
        "select count(*) from rbpmn_event where kind = 'timer-armed' \
         and element_id = 'late_fee_due'",
    )
    .await;
    assert_eq!(
        cycle_arms,
        billings + cycle_fires,
        "one arm per instance ({billings}) plus one re-arm per fire ({cycle_fires})"
    );
    let cycle_cancels = count(
        &db.pool,
        "select count(*) from rbpmn_event where kind = 'timer-cancelled' \
         and element_id = 'late_fee_due'",
    )
    .await;
    assert_eq!(
        cycle_cancels, billings,
        "PAID cancelled exactly one armed occurrence per instance"
    );
    let rearmed_and_fired = count(
        &db.pool,
        "select count(*) from (select instance_id from rbpmn_event \
         where kind = 'timer-fired' and element_id = 'late_fee_due' \
         group by 1 having count(*) >= 2) x",
    )
    .await;
    assert!(
        rearmed_and_fired > 0,
        "no instance fired a re-armed occurrence — the re-arm was never stormed"
    );
    let stuck = count(
        &db.pool,
        "select count(*) from rbpmn_instance where status = 'active'",
    )
    .await;
    // ...and that subprocesses really executed, or the scope invariants in the
    // fsck are checking a table that stayed empty all run.
    let scoped = count(
        &db.pool,
        "select count(*) from rbpmn_event \
         where element_id in ('outer', 'inner') and kind = 'element-started'",
    )
    .await;
    assert!(
        scoped > 0,
        "no subprocess was ever entered — the scope projection is untested here"
    );
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
        "  notes:    {notes_delivered} delivered, {notes_refused} refused by a \
         decided review; {notes} landed per the log, {quiet_cases} quiet cases, \
         {arms} arms over {cases} cases"
    );
    println!(
        "  cycles:   {cycle_backdates} backdated, {cycle_fires} fired, {fees} fees, \
         {cycle_arms} arms and {cycle_cancels} cancels over {billings} instances, \
         {rearmed_and_fired} fired a re-armed occurrence"
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

/// `timer-fired` events of one instance — how the cycle driver waits for the
/// schedulers to claim the occurrence it just backdated.
async fn fires_of(pool: &PgPool, instance: Uuid) -> u32 {
    let n: i64 = sqlx::query_scalar(
        "select count(*) from rbpmn_event where instance_id = $1 and kind = 'timer-fired'",
    )
    .bind(instance)
    .fetch_one(pool)
    .await
    .unwrap();
    n as u32
}
