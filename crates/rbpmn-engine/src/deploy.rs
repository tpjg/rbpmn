//! Atomic deploy: one call carries the definition, its bindings manifest and
//! any DMN artifacts it invokes, validated together against the environment as
//! it exists at that moment. Idempotent by content: same key + byte-identical
//! bundle returns the existing version; changed content allocates the next.
//!
//! [`Bundle`] is that triple, and it *is* the HTTP body — the server
//! deserializes this exact struct, so the library path and the wire path
//! cannot drift into validating different things (the design brief's
//! "one `DeploymentManifest` struct internally").

use crate::{DeployError, Deployment, Engine};
use rbpmn_core::{Bindings, ExecutableProcess, IndexScope};
use rbpmn_model::{Diagnostic, Severity, rule};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

/// Definitions whose manifests still matter: the latest version of every
/// key, plus any version with active instances — the same set startup
/// re-validation and `undeclare_topic` reason over. Bindings only; no
/// `bpmn_xml`, because nothing here compiles.
pub(crate) const RELEVANT_DEFINITIONS: &str = "select d.key, d.version, d.bindings from rbpmn_definition d \
     where d.id in (select definition_id from rbpmn_instance where status = 'active') \
        or (d.key, d.version) in \
           (select key, max(version) from rbpmn_definition group by key) \
     order by d.key, d.version";

/// Everything one deploy carries. Serializes 1:1 to `POST /v1/definitions`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bundle {
    /// The process definition.
    pub bpmn: String,
    /// Its wiring: topics, correlations, indexes, decision bindings, task config.
    #[serde(default)]
    pub bindings: Bindings,
    /// The DMN artifacts its business-rule tasks invoke, as raw XML.
    ///
    /// They travel *inside* the deployment rather than being registered
    /// separately, which is what makes an instance pin its decisions the way
    /// it pins its process: the alternative is a multi-call registration
    /// dance with partially-wired states in the middle, which is the
    /// "seems to run" failure this design exists to kill. The cost, accepted:
    /// a decision table shared by two processes is deployed with each, and
    /// changing a rule is a redeploy.
    #[serde(default)]
    pub decisions: Vec<String>,
}

impl Bundle {
    pub fn new(bpmn: impl Into<String>) -> Self {
        Self {
            bpmn: bpmn.into(),
            ..Default::default()
        }
    }

    pub fn bindings(mut self, bindings: Bindings) -> Self {
        self.bindings = bindings;
        self
    }

    /// Add a DMN artifact. Order is preserved: artifacts may import each
    /// other, so it is not ours to shuffle.
    pub fn decision(mut self, dmn: impl Into<String>) -> Self {
        self.decisions.push(dmn.into());
        self
    }
}

impl Engine {
    /// Deploy a process and its manifest, with no decision artifacts.
    ///
    /// A thin spelling of [`Engine::deploy_bundle`], not a second
    /// implementation — the overwhelmingly common case should not have to
    /// name an empty bundle.
    pub async fn deploy(&self, xml: &str, bindings: &Bindings) -> Result<Deployment, DeployError> {
        self.deploy_bundle(&Bundle::new(xml).bindings(bindings.clone()))
            .await
    }

    /// Deploy a whole bundle: process, manifest and decision artifacts,
    /// validated together and persisted in one transaction.
    pub async fn deploy_bundle(&self, bundle: &Bundle) -> Result<Deployment, DeployError> {
        let xml = &bundle.bpmn;
        let bindings = &bundle.bindings;
        // Parse, the one-process rule, lint and compile-against-manifest, all
        // of it shared with the editor's WASM path (rbpmn_core::check) so the
        // two surfaces can never reach different verdicts. Only the
        // environment link below needs this process's registration state.
        let checked =
            match rbpmn_core::check_deployable(xml, bindings, &bundle.decisions, &validator()) {
                rbpmn_core::DeployCheck::Unparseable(e) => return Err(DeployError::Xml(e)),
                rbpmn_core::DeployCheck::NotExactlyOneProcess(n) => {
                    return Err(DeployError::NotExactlyOneProcess(n));
                }
                rbpmn_core::DeployCheck::Checked(checked) => checked,
            };
        let key = checked.key.clone();

        // Manifest index declarations are validated up front (fail early,
        // before anything persists) and applied after the commit — a
        // CONCURRENTLY build cannot run inside the deploy transaction.
        crate::tasks::validate_index_declarations(&key, &bindings.indexes)
            .map_err(|e| DeployError::InvalidManifest(e.to_string()))?;

        // The manifest is stored as jsonb, and PostgreSQL cannot represent a
        // NUL in a string. Checked here rather than left to the insert, which
        // would answer a well-formed request with a raw database error and an
        // HTTP 500. Every other JSON-into-jsonb boundary asks the same
        // question; this one became reachable when `Bindings::config` made the
        // manifest carry arbitrary application strings — topics resolve
        // against a NUL-free declared set, correlations are parsed FEEL
        // qualified names, and index fields are identifier-validated.
        let bindings_json = serde_json::to_value(bindings).expect("bindings serialize");
        if crate::runtime::contains_nul(&bindings_json) {
            return Err(DeployError::InvalidManifest(
                "the manifest must not contain \\u0000 in any string                  (PostgreSQL jsonb cannot store it)"
                    .to_string(),
            ));
        }

        if !checked.ok() {
            return Err(DeployError::Rejected(checked.diagnostics));
        }

        // The link step: every service-task topic must be covered by the
        // environment as registered *right now*.
        let covered = self.covered_topics().await?;
        let gaps = checked.unresolved_topics(|topic| covered.contains(topic));
        if !gaps.is_empty() {
            return Err(DeployError::Rejected(gaps));
        }
        let warnings = checked.diagnostics;

        let mut hasher = Sha256::new();
        hasher.update(xml.as_bytes());
        hasher.update(bindings_json.to_string().as_bytes());
        // Decisions are part of the content, so changing a rule allocates a
        // new version exactly as changing the diagram does. The length prefix
        // keeps two artifacts from hashing the same as one concatenation of
        // them.
        for dmn in &bundle.decisions {
            hasher.update(dmn.len().to_le_bytes());
            hasher.update(dmn.as_bytes());
        }
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
        // Same transaction as the definition row: a version that exists
        // without the decisions it was validated with is a version that would
        // run something else.
        for (ordinal, dmn) in bundle.decisions.iter().enumerate() {
            sqlx::query(
                "insert into rbpmn_definition_decision (definition_id, ordinal, dmn_xml) \
                 values ($1, $2, $3)",
            )
            .bind(id)
            .bind(ordinal as i32)
            .bind(dmn)
            .execute(&mut *tx)
            .await?;
        }
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
    ///
    /// One field at a time: each build takes its own advisory lock and
    /// releases it before the next, so two locks are never held at once.
    async fn apply_manifest_indexes(
        &self,
        key: &str,
        bindings: &Bindings,
    ) -> Result<(), DeployError> {
        if bindings.indexes.is_empty() {
            return Ok(());
        }
        self.warn_on_index_scope_conflicts(key, bindings).await?;
        for decl in &bindings.indexes {
            let built = match decl.scope {
                IndexScope::Definition => self.declare_index(key, &decl.field).await,
                IndexScope::Shared => self.declare_shared_index(&decl.field).await,
            };
            built.map_err(|e| match e {
                crate::EngineError::Db(db) => DeployError::Db(db),
                other => DeployError::InvalidManifest(other.to_string()),
            })?;
        }
        Ok(())
    }

    /// The one thing rbpmn *can* observe about a shared declaration: another
    /// definition indexing the same field at the other scope.
    ///
    /// A log line, deliberately — not a `Diagnostic` and never an error. It
    /// is not an error because the configuration is legal and sometimes
    /// optimal: measured, a `TaskFilter` served only by a shared index
    /// degrades to a `BitmapAnd` against the definition-key index, so a
    /// definition that also filters internally has a real reason to declare
    /// both. And it is not a `Diagnostic` because diagnostics carry an
    /// element id and feed a rule catalogue the playground reproduces from
    /// XML alone — this is an operator fact about *other* deployed
    /// definitions, which no offline surface can ever see.
    ///
    /// What it cannot see at all is the thing actually worth worrying about:
    /// whether the field *means* the same in both. Nothing can.
    async fn warn_on_index_scope_conflicts(
        &self,
        key: &str,
        bindings: &Bindings,
    ) -> Result<(), DeployError> {
        let ours: std::collections::BTreeMap<&str, IndexScope> = bindings
            .indexes
            .iter()
            .map(|d| (d.field.as_str(), d.scope))
            .collect();
        for row in sqlx::query(RELEVANT_DEFINITIONS)
            .fetch_all(self.pool())
            .await?
        {
            let other_key: String = row.get("key");
            // Our own superseded versions are not a conflict: the deploy in
            // hand supersedes them, and warning about them would fire on
            // every redeploy of a definition that changed its own scope.
            if other_key == key {
                continue;
            }
            let Ok(other) = serde_json::from_value::<Bindings>(row.get("bindings")) else {
                // An unreadable manifest is `check_active_definitions`'
                // problem, not a reason to fail a deploy over a log line.
                continue;
            };
            let version: i32 = row.get("version");
            for decl in &other.indexes {
                if let Some(&scope) = ours.get(decl.field.as_str())
                    && scope != decl.scope
                {
                    tracing::warn!(
                        field = %decl.field,
                        scope = %scope.as_str(),
                        other = %format!("{other_key} v{version}"),
                        other_scope = %decl.scope.as_str(),
                        "index field declared at two scopes across definitions — two \
                         indexes over one expression, which may be deliberate; rbpmn \
                         cannot check the field means the same thing in both"
                    );
                }
            }
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
            "select distinct d.id, d.key, d.version, d.bpmn_xml, d.bindings from rbpmn_definition d \
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
            // Decisions persist with the definition, but *what validates them*
            // is code — so a binary rebuilt without the `dmn` feature, or a
            // dsntk upgrade that stopped accepting an artifact, is exactly the
            // drift this pass exists to catch. Same argument as handler
            // drift, one layer over.
            let decisions: Vec<String> = sqlx::query_scalar(
                "select dmn_xml from rbpmn_definition_decision \
                 where definition_id = $1 order by ordinal",
            )
            .bind(row.get::<Uuid, _>("id"))
            .fetch_all(self.pool())
            .await?;
            let decided = rbpmn_core::DecisionValidator::check(&validator(), &decisions);
            for diagnostic in decided.diagnostics {
                out.push(Diagnostic {
                    message: format!("definition '{key}' v{version}: {}", diagnostic.message),
                    ..diagnostic
                });
            }

            let Ok(defs) = rbpmn_model::parse(&row.get::<String, _>("bpmn_xml")) else {
                out.push(Diagnostic::error(
                    rule::BPMN_STRUCTURE,
                    &key,
                    format!("stored definition {key} v{version} no longer parses"),
                ));
                continue;
            };
            // The manifest half deploy checks, on the path that re-checks
            // stored definitions. Config has no default, so an entry that
            // stopped binding a task — a row edited by hand, a migration, a
            // future `Bindings` shape — would deliver nothing and say
            // nothing, which is the one failure `config-binds-task` exists to
            // prevent. It does not gate the compile below, for the reason
            // `check_deployable` does not either.
            if let Some(process) = defs.processes.first() {
                for diagnostic in rbpmn_core::config_bindings(&bindings, process) {
                    out.push(Diagnostic {
                        message: format!("definition '{key}' v{version}: {}", diagnostic.message),
                        ..diagnostic
                    });
                }
            }
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

/// The DMN validator this build carries — the same choice `rbpmn-wasm` makes,
/// for the same reason: deploy and the editor must reach one verdict.
///
/// Without the `dmn` feature this is [`rbpmn_core::NoDecisions`], which
/// **refuses** a bundle carrying decision artifacts. That is deliberate: a
/// binary that cannot validate decisions must not accept a deployment
/// containing them.
#[cfg(feature = "dmn")]
fn validator() -> impl rbpmn_core::DecisionValidator {
    rbpmn_dmn::Validator
}

#[cfg(not(feature = "dmn"))]
fn validator() -> impl rbpmn_core::DecisionValidator {
    rbpmn_core::NoDecisions
}
