//! Atomic deploy: one call carries the definition and its bindings manifest,
//! validated together against the environment as it exists at that moment.
//! Idempotent by content: same key + byte-identical XML + bindings returns
//! the existing version; changed content allocates the next version.

use crate::{DeployError, Deployment, Engine};
use rbpmn_core::{Bindings, ExecutableProcess};
use rbpmn_model::{Diagnostic, Severity, rule};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

impl Engine {
    pub async fn deploy(&self, xml: &str, bindings: &Bindings) -> Result<Deployment, DeployError> {
        let defs = rbpmn_model::parse(xml)?;
        if defs.processes.len() != 1 {
            return Err(DeployError::NotExactlyOneProcess(defs.processes.len()));
        }
        let key = defs.processes[0].id.clone();

        // Manifest index declarations are validated up front (fail early,
        // before anything persists) and applied after the commit — a
        // CONCURRENTLY build cannot run inside the deploy transaction.
        for field in &bindings.indexes {
            crate::tasks::validate_index_declaration(&key, field)
                .map_err(|e| DeployError::InvalidManifest(e.to_string()))?;
        }

        let diagnostics = rbpmn_model::lint(&defs);
        if rbpmn_model::has_errors(&diagnostics) {
            return Err(DeployError::Rejected(diagnostics));
        }
        let warnings = diagnostics;

        // Phase gating + condition/topic/correlation resolution — a
        // definition that deploys is guaranteed executable and fully wired.
        let proc = match ExecutableProcess::compile(&defs, &key, bindings) {
            Ok(proc) => proc,
            Err(rbpmn_core::CompileError::MissingCorrelation(elements)) => {
                return Err(DeployError::Rejected(
                    elements
                        .iter()
                        .map(|el| {
                            Diagnostic::error(
                                rule::MESSAGE_HAS_CORRELATION,
                                el,
                                "message element has no correlation binding — bind it \
                                 with Bindings::correlation(element_id, feel_qualified_name)",
                            )
                        })
                        .collect(),
                ));
            }
            Err(rbpmn_core::CompileError::InvalidCorrelation { element, reason }) => {
                return Err(DeployError::Rejected(vec![Diagnostic::error(
                    rule::MESSAGE_HAS_CORRELATION,
                    element,
                    format!("correlation binding is not a FEEL qualified name: {reason}"),
                )]));
            }
            Err(e) => {
                return Err(DeployError::Rejected(vec![Diagnostic::error(
                    rule::NO_UNSUPPORTED_ELEMENT,
                    &key,
                    e.to_string(),
                )]));
            }
        };

        // The link step: every service-task topic must be covered by the
        // environment as registered *right now*.
        let covered = self.covered_topics().await?;
        let gaps: Vec<Diagnostic> = proc
            .service_topics()
            .filter(|(_, topic)| !covered.contains(*topic))
            .map(|(element, topic)| {
                Diagnostic::error(
                    rule::UNRESOLVED_TOPIC,
                    element,
                    format!(
                        "topic '{topic}' has no registered handler and no declared \
                         external-worker topic — register it before deploying \
                         (the environment can grow at any time)"
                    ),
                )
            })
            .collect();
        if !gaps.is_empty() {
            return Err(DeployError::Rejected(gaps));
        }

        let bindings_json = serde_json::to_value(bindings).expect("bindings serialize");
        let mut hasher = Sha256::new();
        hasher.update(xml.as_bytes());
        hasher.update(bindings_json.to_string().as_bytes());
        let content_hash = format!("{:x}", hasher.finalize());

        let mut tx = self.pool().begin().await?;
        // Serialize deploys per key so concurrent identical deploys stay
        // idempotent instead of racing the unique (key, version) constraint.
        sqlx::query("select pg_advisory_xact_lock(hashtext($1))")
            .bind(&key)
            .execute(&mut *tx)
            .await?;

        let latest = sqlx::query(
            "select id, version, content_hash from rbpmn_definition \
             where key = $1 order by version desc limit 1",
        )
        .bind(&key)
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(row) = &latest
            && row.get::<String, _>("content_hash") == content_hash
        {
            tx.commit().await?;
            // Idempotent re-deploy re-applies the declarations too — this
            // is what makes deploy re-runnable at startup.
            self.apply_manifest_indexes(&key, bindings).await?;
            return Ok(Deployment {
                definition_id: row.get("id"),
                key,
                version: row.get("version"),
                reused: true,
                warnings,
            });
        }
        let version: i32 = latest.map(|r| r.get::<i32, _>("version") + 1).unwrap_or(1);

        let id: Uuid = sqlx::query(
            "insert into rbpmn_definition (key, version, content_hash, bpmn_xml, bindings) \
             values ($1, $2, $3, $4, $5) returning id",
        )
        .bind(&key)
        .bind(version)
        .bind(&content_hash)
        .bind(xml)
        .bind(&bindings_json)
        .fetch_one(&mut *tx)
        .await?
        .get("id");
        tx.commit().await?;
        self.apply_manifest_indexes(&key, bindings).await?;

        Ok(Deployment {
            definition_id: id,
            key,
            version,
            reused: false,
            warnings,
        })
    }

    /// Build the manifest's declared indexes, after the deploy commit
    /// (CONCURRENTLY cannot run in a transaction). Everything here is
    /// idempotent, so a failure is safely retried by re-deploying.
    async fn apply_manifest_indexes(
        &self,
        key: &str,
        bindings: &Bindings,
    ) -> Result<(), DeployError> {
        for field in &bindings.indexes {
            self.declare_index(key, field).await.map_err(|e| match e {
                crate::EngineError::Db(db) => DeployError::Db(db),
                other => DeployError::InvalidManifest(other.to_string()),
            })?;
        }
        Ok(())
    }

    /// Startup re-validation: definitions persist across restarts but the
    /// environment is rebuilt from code/config — re-check every definition
    /// that can still produce work (the latest version per key, plus any
    /// version with active instances) against the current registration state.
    /// Call after wiring the initial environment; fail loudly on diagnostics.
    pub async fn check_active_definitions(&self) -> Result<Vec<Diagnostic>, sqlx::Error> {
        let rows = sqlx::query(
            "select distinct d.key, d.version, d.bpmn_xml, d.bindings from rbpmn_definition d \
             where d.id in (select definition_id from rbpmn_instance where status = 'active') \
                or (d.key, d.version) in \
                   (select key, max(version) from rbpmn_definition group by key) \
             order by d.key, d.version",
        )
        .fetch_all(self.pool())
        .await?;

        let covered = self.covered_topics().await?;
        let mut out = Vec::new();
        for row in rows {
            let key: String = row.get("key");
            let version: i32 = row.get("version");
            let bindings: Bindings =
                match serde_json::from_value(row.get::<serde_json::Value, _>("bindings")) {
                    Ok(b) => b,
                    Err(e) => {
                        out.push(Diagnostic::error(
                            rule::BPMN_STRUCTURE,
                            &key,
                            format!(
                                "stored bindings manifest of {key} v{version} does not \
                             deserialize ({e}) — refusing to guess"
                            ),
                        ));
                        continue;
                    }
                };
            let Ok(defs) = rbpmn_model::parse(&row.get::<String, _>("bpmn_xml")) else {
                out.push(Diagnostic::error(
                    rule::BPMN_STRUCTURE,
                    &key,
                    format!("stored definition {key} v{version} no longer parses"),
                ));
                continue;
            };
            let proc = match ExecutableProcess::compile(&defs, &key, &bindings) {
                Ok(proc) => proc,
                Err(e) => {
                    // Deploy validated it once; if it stopped compiling the
                    // engine itself changed — say so, never skip silently.
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        &key,
                        format!("stored definition {key} v{version} no longer compiles: {e}"),
                    ));
                    continue;
                }
            };
            for (element, topic) in proc.service_topics() {
                if !covered.contains(topic) {
                    out.push(Diagnostic {
                        rule: rule::UNRESOLVED_TOPIC.to_string(),
                        element: element.to_string(),
                        message: format!(
                            "definition '{key}' v{version}: topic '{topic}' is no longer \
                             covered by the environment — a handler or declared topic \
                             disappeared since deploy"
                        ),
                        severity: Severity::Error,
                    });
                }
            }
        }
        Ok(out)
    }
}
