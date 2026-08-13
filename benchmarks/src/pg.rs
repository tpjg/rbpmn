//! What the database was, and what it did.
//!
//! Two jobs. Before a run: capture the settings that decide the numbers, so
//! nothing is a hidden default — the curated list is what `compose.yml`
//! tunes, and the catch-all sweeps up everything whose source is not
//! `default`, which is what catches a setting the tuning file grew after
//! this list was written. During a run: the samples the monitor takes.

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;

/// Settings whose values a reader needs in order to interpret a number.
/// Captured by name even when they hold the server default, because "we ran
/// on the default" is itself a fact about the measurement.
const CURATED: &[&str] = &[
    "server_version",
    "shared_buffers",
    "work_mem",
    "maintenance_work_mem",
    "effective_cache_size",
    "max_connections",
    "max_worker_processes",
    "wal_level",
    "wal_buffers",
    "max_wal_size",
    "min_wal_size",
    "checkpoint_timeout",
    "checkpoint_completion_target",
    "synchronous_commit",
    "fsync",
    "full_page_writes",
    "random_page_cost",
    "seq_page_cost",
    "effective_io_concurrency",
    "autovacuum",
    "autovacuum_naptime",
    "autovacuum_max_workers",
    "autovacuum_vacuum_scale_factor",
    "autovacuum_vacuum_threshold",
    "autovacuum_vacuum_cost_delay",
    "autovacuum_vacuum_cost_limit",
    "track_io_timing",
    "jit",
    "max_locks_per_transaction",
    "default_transaction_isolation",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresFacts {
    pub version: String,
    /// Whether the harness and the database share a machine. Never mix the
    /// two in one comparison — the README says so and the report prints it.
    pub local: bool,
    pub connection_host: String,
    /// How this database got here: `compose` (the pinned image in
    /// `benchmarks/compose.yml`) or `external` (a URL the caller supplied).
    pub provisioned_by: String,
    pub settings: BTreeMap<String, String>,
    /// Everything the server is *not* running at its built-in default,
    /// whatever the reason (config file, command line, ALTER SYSTEM).
    pub non_default_settings: BTreeMap<String, String>,
    /// Per-table storage parameters — where the churn-heavy tables'
    /// autovacuum settings live. The engine's migrations set none; the
    /// benchmark applies `benchmarks/tuning.sql`, which is why the applied
    /// values are recorded rather than assumed.
    pub table_options: BTreeMap<String, Vec<String>>,
}

pub async fn facts(
    pool: &PgPool,
    connection_host: &str,
    provisioned_by: &str,
) -> Result<PostgresFacts, sqlx::Error> {
    let version: String = sqlx::query_scalar("select version()")
        .fetch_one(pool)
        .await?;

    let mut settings = BTreeMap::new();
    let rows = sqlx::query(
        "select name, setting, coalesce(unit, '') as unit from pg_settings \
         where name = any($1)",
    )
    .bind(CURATED)
    .fetch_all(pool)
    .await?;
    for row in rows {
        let name: String = row.get("name");
        let setting: String = row.get("setting");
        let unit: String = row.get("unit");
        settings.insert(
            name,
            if unit.is_empty() {
                setting
            } else {
                format!("{setting}{unit}")
            },
        );
    }

    let mut non_default = BTreeMap::new();
    let rows = sqlx::query(
        "select name, setting, source from pg_settings \
         where source not in ('default', 'client') order by name",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        let name: String = row.get("name");
        let setting: String = row.get("setting");
        let source: String = row.get("source");
        non_default.insert(name, format!("{setting} ({source})"));
    }

    let mut table_options = BTreeMap::new();
    let rows = sqlx::query(
        "select relname, coalesce(reloptions, '{}') as reloptions from pg_class \
         where relname like 'rbpmn\\_%' and relkind = 'r' order by relname",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        let name: String = row.get("relname");
        let options: Vec<String> = row.get("reloptions");
        table_options.insert(name, options);
    }

    Ok(PostgresFacts {
        version,
        local: is_local(connection_host),
        connection_host: connection_host.to_string(),
        provisioned_by: provisioned_by.to_string(),
        settings,
        non_default_settings: non_default,
        table_options,
    })
}

pub fn is_local(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "") || host.starts_with('/')
}

/// One monitor observation. Everything is a raw counter or gauge as the
/// database reports it: deltas are the reader's business, and a harness that
/// pre-differenced them would throw away the ability to notice a counter
/// reset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// Seconds since the monitor started.
    pub at_secs: f64,
    pub active_instances: i64,
    pub completed_instances: i64,
    pub failed_instances: i64,
    /// Work items claimable right now — the backlog a worker sees.
    pub claimable_work_items: i64,
    /// The same backlog broken down by topic. One number for the whole queue
    /// hides the case that matters: a drain that has stalled because *one*
    /// topic's consumers cannot keep up, which reads identically to a slow
    /// engine until you split it out.
    pub claimable_by_topic: BTreeMap<String, i64>,
    pub locked_work_items: i64,
    pub armed_timers: i64,
    pub open_subscriptions: i64,
    pub live_tokens: i64,
    pub events: i64,
    /// Instances that reached a terminal state since the previous sample,
    /// and their latency percentiles — computed in the database, over
    /// database timestamps, so no client clock enters a latency number.
    pub completed_since_last: i64,
    pub latency_p50_ms: Option<f64>,
    pub latency_p95_ms: Option<f64>,
    pub latency_p99_ms: Option<f64>,
    pub database: DatabaseStats,
    pub tables: BTreeMap<String, TableStats>,
    pub connections: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseStats {
    pub xact_commit: i64,
    pub xact_rollback: i64,
    pub blks_read: i64,
    pub blks_hit: i64,
    pub tup_inserted: i64,
    pub tup_updated: i64,
    pub tup_deleted: i64,
    pub deadlocks: i64,
    pub temp_files: i64,
    pub temp_bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableStats {
    pub seq_scan: i64,
    pub idx_scan: i64,
    pub n_live_tup: i64,
    pub n_dead_tup: i64,
    pub total_bytes: i64,
    pub autovacuum_count: i64,
}

/// Tables whose churn the design brief calls out (`work_item`, `token`) plus
/// the ones whose *size* is the growth story (`event`, `instance`).
const WATCHED: &[&str] = &[
    "rbpmn_work_item",
    "rbpmn_token",
    "rbpmn_instance",
    "rbpmn_event",
    "rbpmn_timer",
    "rbpmn_subscription",
    "rbpmn_scope",
];

pub async fn sample(
    pool: &PgPool,
    at_secs: f64,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<(Sample, chrono::DateTime<chrono::Utc>), sqlx::Error> {
    // One statement for the runtime counts: several would let a step commit
    // between them and produce a sample that never existed.
    let row = sqlx::query(
        "select \
           (select count(*) from rbpmn_instance where status = 'active') as active, \
           (select count(*) from rbpmn_instance where status in ('completed','terminated')) \
             as completed, \
           (select count(*) from rbpmn_instance where status = 'failed') as failed, \
           (select count(*) from rbpmn_work_item w join rbpmn_instance i on i.id = w.instance_id \
             where (w.state = 'available' or (w.state = 'locked' and w.lock_until < now())) \
               and (w.retry_at is null or w.retry_at <= now()) and i.status = 'active') \
             as claimable, \
           (select count(*) from rbpmn_work_item \
             where state = 'locked' and lock_until >= now()) as locked, \
           (select count(*) from rbpmn_timer) as timers, \
           (select count(*) from rbpmn_subscription) as subscriptions, \
           (select count(*) from rbpmn_token) as tokens, \
           (select count(*) from rbpmn_event) as events, \
           clock_timestamp() as now",
    )
    .fetch_one(pool)
    .await?;
    let now: chrono::DateTime<chrono::Utc> = row.get("now");

    // Latency over the instances that finished in this window only, so a
    // long run's percentiles do not flatten into its own history.
    let latency = sqlx::query(
        "select count(*) as n, \
           percentile_cont(0.50) within group \
             (order by (extract(epoch from (completed_at - created_at)) * 1000)::float8) as p50, \
           percentile_cont(0.95) within group \
             (order by (extract(epoch from (completed_at - created_at)) * 1000)::float8) as p95, \
           percentile_cont(0.99) within group \
             (order by (extract(epoch from (completed_at - created_at)) * 1000)::float8) as p99 \
         from rbpmn_instance \
         where completed_at > $1 and completed_at <= $2 and completed_at is not null",
    )
    .bind(since)
    .bind(now)
    .fetch_one(pool)
    .await?;

    let db = sqlx::query(
        "select xact_commit, xact_rollback, blks_read, blks_hit, tup_inserted, \
                tup_updated, tup_deleted, deadlocks, temp_files, temp_bytes \
         from pg_stat_database where datname = current_database()",
    )
    .fetch_one(pool)
    .await?;

    let mut tables = BTreeMap::new();
    let rows = sqlx::query(
        "select relname, seq_scan, coalesce(idx_scan, 0) as idx_scan, \
                n_live_tup, n_dead_tup, autovacuum_count, \
                pg_total_relation_size(relid) as total_bytes \
         from pg_stat_user_tables where relname = any($1)",
    )
    .bind(WATCHED)
    .fetch_all(pool)
    .await?;
    for row in rows {
        tables.insert(
            row.get::<String, _>("relname"),
            TableStats {
                seq_scan: row.get("seq_scan"),
                idx_scan: row.get("idx_scan"),
                n_live_tup: row.get("n_live_tup"),
                n_dead_tup: row.get("n_dead_tup"),
                total_bytes: row.get("total_bytes"),
                autovacuum_count: row.get("autovacuum_count"),
            },
        );
    }

    let mut claimable_by_topic = BTreeMap::new();
    let rows = sqlx::query(
        "select w.topic, count(*) as n from rbpmn_work_item w \
         join rbpmn_instance i on i.id = w.instance_id \
         where (w.state = 'available' or (w.state = 'locked' and w.lock_until < now())) \
           and (w.retry_at is null or w.retry_at <= now()) and i.status = 'active' \
         group by w.topic",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        claimable_by_topic.insert(row.get::<String, _>("topic"), row.get::<i64, _>("n"));
    }

    let mut connections = BTreeMap::new();
    let rows = sqlx::query(
        "select coalesce(state, 'unknown') as state, count(*) as n from pg_stat_activity \
         where datname = current_database() group by 1",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        connections.insert(row.get::<String, _>("state"), row.get::<i64, _>("n"));
    }

    let sample = Sample {
        at_secs,
        active_instances: row.get("active"),
        completed_instances: row.get("completed"),
        failed_instances: row.get("failed"),
        claimable_work_items: row.get("claimable"),
        claimable_by_topic,
        locked_work_items: row.get("locked"),
        armed_timers: row.get("timers"),
        open_subscriptions: row.get("subscriptions"),
        live_tokens: row.get("tokens"),
        events: row.get("events"),
        completed_since_last: latency.get("n"),
        latency_p50_ms: latency.get("p50"),
        latency_p95_ms: latency.get("p95"),
        latency_p99_ms: latency.get("p99"),
        database: DatabaseStats {
            xact_commit: db.get("xact_commit"),
            xact_rollback: db.get("xact_rollback"),
            blks_read: db.get("blks_read"),
            blks_hit: db.get("blks_hit"),
            tup_inserted: db.get("tup_inserted"),
            tup_updated: db.get("tup_updated"),
            tup_deleted: db.get("tup_deleted"),
            deadlocks: db.get("deadlocks"),
            temp_files: db.get("temp_files"),
            temp_bytes: db.get("temp_bytes"),
        },
        tables,
        connections,
    };
    Ok((sample, now))
}

/// Bytes written into the event table, and how many rows — the measurement
/// that stands in for the history-level axis until per-definition event-kind
/// filtering exists (see `scenario::History`).
pub async fn event_volume(pool: &PgPool) -> Result<(i64, i64), sqlx::Error> {
    let row = sqlx::query(
        "select count(*) as rows, \
                coalesce(sum(pg_column_size(payload) + 64), 0)::bigint as bytes \
         from rbpmn_event",
    )
    .fetch_one(pool)
    .await?;
    Ok((row.get("rows"), row.get("bytes")))
}
