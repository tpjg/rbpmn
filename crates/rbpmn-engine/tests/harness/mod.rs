//! Shared machinery for the Postgres-layer tests: the fsck, replay
//! verification, and the fixtures both `storm.rs` and `chaos.rs` drive
//! (docs/stress-testing.md §4 and §5).

// Shared by several test binaries; each uses a different part.
#![allow(dead_code)]

use rbpmn_core::{Command, Event, ExecutableProcess, InstanceState, InstanceStatus};
use rbpmn_engine::Engine;
use sqlx::{PgPool, Row};
use std::fs;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

pub fn fixture(name: &str) -> String {
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
        // What makes the loader's element-qualified lookup sound
        // (`load_instance_nowait`, the `message` arm): a waiting token is
        // matched to its arm by `(token_no, element_id)`, so *exactly one*
        // row must answer that — zero would leave the token unresolvable,
        // several would make the resolution a guess. Counting only rows at
        // the token's own element is the point: a message boundary puts a
        // second subscription on the same token, legitimately, and this must
        // not read that as a violation.
        "a message-waiting token has no unique subscription at its own element",
        "select t.instance_id::text from rbpmn_token t \
         left join rbpmn_subscription s \
           on s.instance_id = t.instance_id and s.token_no = t.token_no \
              and s.element_id = t.element_id \
         where t.wait_kind = 'message' \
         group by t.instance_id, t.token_no having count(s.subscription_no) <> 1",
    ),
    (
        // The same statement for timers. A timer catch hosts nothing, so
        // there has only ever been one row — the invariant is stated anyway,
        // because the lookup is element-qualified either way and an
        // unstated invariant is one nothing checks.
        "a timer-waiting token has no unique timer at its own element",
        "select t.instance_id::text from rbpmn_token t \
         left join rbpmn_timer m \
           on m.instance_id = t.instance_id and m.token_no = t.token_no \
              and m.element_id = t.element_id \
         where t.wait_kind = 'timer' \
         group by t.instance_id, t.token_no having count(m.timer_no) <> 1",
    ),
    (
        // Per *scope instance* since phase 6: joins count within their scope,
        // so the same join element may legitimately hold tokens in two live
        // scopes. Grouping without scope_no would be a false positive the day
        // one subprocess node has two concurrent instances.
        "two tokens parked at one join via the same flow in one scope",
        "select instance_id::text from rbpmn_token where wait_kind = 'join' \
         group by instance_id, scope_no, element_id, arrived_via having count(*) > 1",
    ),
    (
        "completed/terminated instances still hold scopes",
        "select s.instance_id::text from rbpmn_scope s \
         join rbpmn_instance i on i.id = s.instance_id \
         where i.status in ('completed', 'terminated')",
    ),
    (
        "a scope's parked parent token is missing or not waiting on it",
        "select s.instance_id::text from rbpmn_scope s \
         left join rbpmn_token t \
           on t.instance_id = s.instance_id and t.token_no = s.token_no \
         where t.token_no is null or t.wait_kind <> 'scope'",
    ),
    (
        "a token waits on a subprocess with no scope open for it",
        "select t.instance_id::text from rbpmn_token t \
         left join rbpmn_scope s \
           on s.instance_id = t.instance_id and s.token_no = t.token_no \
         where t.wait_kind = 'scope' and s.scope_no is null",
    ),
    (
        "a scope's parent scope does not exist",
        "select s.instance_id::text from rbpmn_scope s \
         left join rbpmn_scope p \
           on p.instance_id = s.instance_id and p.scope_no = s.parent_scope_no \
         where s.parent_scope_no <> 0 and p.scope_no is null",
    ),
    (
        "a token lives in a scope that does not exist",
        "select t.instance_id::text from rbpmn_token t \
         left join rbpmn_scope s \
           on s.instance_id = t.instance_id and s.scope_no = t.scope_no \
         where t.scope_no <> 0 and s.scope_no is null",
    ),
    (
        // At least one, not exactly one: the freeze parks every token that
        // was still in flight (a parallel sibling mid-advance) as an
        // incident too, so token conservation survives the freeze.
        "a failed instance is not frozen at an incident token",
        "select i.id::text from rbpmn_instance i where i.status = 'failed' \
           and not exists (select 1 from rbpmn_token t \
                where t.instance_id = i.id and t.wait_kind = 'incident')",
    ),
    (
        "a work item is locked without a live lease or an owner",
        "select id::text from rbpmn_work_item \
         where state = 'locked' and (lock_owner is null or lock_until is null)",
    ),
    (
        // Same shape as the foreign key below: the CHECK constraints are
        // *what makes* a cycle's fire count positive and confined to a cycle
        // row, so the invariant worth asserting is that they are still there.
        // The loader refuses such a row rather than clamping it, and a
        // database with the checks dropped is the only way one could exist.
        "a rbpmn_timer.remaining check constraint is missing",
        "select c.name::text from (values ('rbpmn_timer_remaining_check'), \
                                          ('rbpmn_timer_remaining_kind_check')) as c(name) \
         where not exists (select 1 from pg_constraint where conname = c.name)",
    ),
    (
        // The constraint, not an anti-join over the largest table in the
        // schema: the FK is *what makes* "no event outlives its instance"
        // true, so scanning rbpmn_event to re-derive a constant answer buys
        // nothing an O(1) catalogue lookup does not — and fsck is meant to be
        // runnable against a production-sized database. This is the invariant
        // `delete_definition` leans on when it proves "nothing references
        // this" from an indexed instance lookup.
        "the event -> instance foreign key is missing",
        "select 'rbpmn_event_instance_fk'::text where not exists \
           (select 1 from pg_constraint where conname = 'rbpmn_event_instance_fk')",
    ),
];

pub async fn fsck(pool: &PgPool) -> Vec<String> {
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
pub async fn pinned_process(pool: &PgPool, instance: Uuid) -> ExecutableProcess {
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
pub async fn core_events(pool: &PgPool, instance: Uuid) -> Vec<Event> {
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
///
/// **Decisions are refused rather than skipped.** A business-rule task is not
/// a stimulus — the engine answers it from its own evaluation inside the step
/// — so replaying one means feeding the *recorded* answer back in, which this
/// flat `Vec<Command>` shape cannot express: the answer has to arrive when the
/// token parks, not before. Nothing in the corpus has one today. The `_ => {}`
/// below would quietly drop it and hand back a command sequence that replays
/// to a different history, so the day a business-rule fixture joins the storm
/// this says so instead.
pub fn commands_from(events: &[Event]) -> Vec<Command> {
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
            Event::DecisionEvaluated { element, .. } => panic!(
                "replay cannot reconstruct the decision at {element:?}: a decision's \
                 answer is not a stimulus, it is read back from `decision-evaluated` \
                 when the token parks. See this function's doc comment."
            ),
            _ => {}
        }
    }
    commands
}

/// **Replay verification.** Re-derive the instance's history by feeding its
/// reconstructed stimuli to the pure core, and assert the core produces
/// exactly the trace the database recorded. This is the systematic form of
/// "the Postgres layer is a projection of this core".
pub async fn replay_verify(
    pool: &PgPool,
    instance: Uuid,
    initial: &serde_json::Value,
) -> Result<usize, String> {
    let recorded = core_events(pool, instance).await;
    if recorded.is_empty() {
        return Err("instance has no core events at all".into());
    }
    let proc = pinned_process(pool, instance).await;

    // The initial variables come from the **log**, not from the caller. That
    // is the property `instance-started` was given a payload for: a history
    // that cannot be replayed without a value kept outside it is not a
    // history. `initial` is still taken, and cross-checked below, so a payload
    // that recorded the wrong document fails here rather than replaying
    // consistently against itself.
    let logged = recorded
        .iter()
        .find_map(|e| match e {
            Event::InstanceStarted { variables } => Some(variables.clone()),
            _ => None,
        })
        .ok_or("history has no instance-started to start from")?;
    if &logged != initial {
        return Err(format!(
            "instance-started recorded variables the instance did not start with\n  \
             logged: {logged}\n  actual: {initial}"
        ));
    }

    let mut state = InstanceState::new();
    let mut replayed: Vec<Event> =
        step_or(&proc, &mut state, Command::Start { variables: logged })?;
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

pub fn step_or(
    proc: &ExecutableProcess,
    state: &mut InstanceState,
    command: Command,
) -> Result<Vec<Event>, String> {
    rbpmn_core::step(proc, state, command.clone())
        .map_err(|e| format!("replaying {command:?}: {e}"))
}

// --------------------------------------------------------------------- setup

pub async fn engine_on(pool: PgPool) -> Engine {
    Engine::builder(pool).retry_backoff(Duration::ZERO).build()
}

/// A boundary timer that actually fires during the test, so timer claims race
/// completions on the same instance — the interleaving `spec/LockOrder.tla`
/// says is the interesting one.
pub fn racing_timer_xml() -> String {
    fixture("accept/09-timer-boundary.bpmn").replace("PT1H", "PT0S")
}

pub async fn open_items(pool: &PgPool, instance: Uuid) -> Vec<(Uuid, String)> {
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

pub fn with_process_id(xml: &str, id: &str) -> String {
    xml.replace("id=\"p\"", &format!("id=\"{id}\""))
        .replace("bpmnElement=\"p\"", &format!("bpmnElement=\"{id}\""))
}

pub async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query(sql).fetch_one(pool).await.unwrap().get(0)
}

/// Postgres counts deadlocks per database, so this observes them even though
/// the worker and scheduler loops handle their own errors. The design brief
/// claims the shipped lock order has none; `spec/LockOrder.tla` proves it of
/// the protocol, and this checks the implementation agrees.
pub async fn deadlocks(pool: &PgPool) -> i64 {
    count(
        pool,
        "select deadlocks from pg_stat_database where datname = current_database()",
    )
    .await
}
