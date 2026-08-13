//! Loading a benchmark model: the bytes, their hash, and the element lists
//! the harness needs to bind topics and size worker pools.
//!
//! Benchmark models live in `benchmarks/models/` and are deliberately *not*
//! part of `crates/rbpmn-model/tests/fixtures/`. The fixture corpus is the
//! specification — adding a model there because a benchmark wanted it would
//! make the corpus grow for performance reasons rather than semantic ones.
//! They are still ordinary models: same linter, same compile, same deploy.

use rbpmn_model::model::{Definitions, FlowScope, NodeKind};
use sha2::{Digest, Sha256};
use std::path::Path;

pub struct Model {
    pub file: String,
    /// The definition key — the single process's id. Taken from the model
    /// rather than from the scenario file so the two cannot disagree, and
    /// resolved **once**: it used to be re-derived per instance, which meant
    /// re-parsing the whole document on every `start`.
    pub key: String,
    pub xml: String,
    pub sha256: String,
    /// Element ids in document order — the order the manifest is built in,
    /// so a rebuilt manifest hashes the same.
    pub service_tasks: Vec<String>,
    pub user_tasks: Vec<String>,
    /// Present so a scenario cannot quietly stop exercising the scheduler:
    /// the result file records it, and the report prints it.
    pub timers: usize,
    pub message_catches: usize,
    pub subprocesses: usize,
    pub elements: usize,
}

impl Model {
    pub fn load(path: &Path) -> Result<Model, String> {
        let xml = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let definitions =
            rbpmn_model::parse(&xml).map_err(|e| format!("{}: parse: {e}", path.display()))?;
        let process = single_process(&definitions, path)?;
        let mut model = Model {
            file: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            key: process.id.clone(),
            sha256: format!("{:x}", Sha256::digest(xml.as_bytes())),
            xml,
            service_tasks: Vec::new(),
            user_tasks: Vec::new(),
            timers: 0,
            message_catches: 0,
            subprocesses: 0,
            elements: 0,
        };
        model.walk(&process.body);
        Ok(model)
    }

    fn walk(&mut self, scope: &FlowScope) {
        use rbpmn_model::model::{BoundaryTrigger, CatchTrigger};
        for node in &scope.nodes {
            self.elements += 1;
            match &node.kind {
                NodeKind::ServiceTask { .. } => self.service_tasks.push(node.id.clone()),
                NodeKind::UserTask => self.user_tasks.push(node.id.clone()),
                NodeKind::Catch(CatchTrigger::Timer(_)) => self.timers += 1,
                NodeKind::Catch(CatchTrigger::Message(_)) => self.message_catches += 1,
                NodeKind::Boundary(data) => match data.trigger {
                    BoundaryTrigger::Timer(_) => self.timers += 1,
                    BoundaryTrigger::Message(_) => self.message_catches += 1,
                    _ => {}
                },
                NodeKind::SubProcess(data) => {
                    self.subprocesses += 1;
                    self.walk(&data.body);
                }
                _ => {}
            }
        }
    }
}

fn single_process<'a>(
    definitions: &'a Definitions,
    path: &Path,
) -> Result<&'a rbpmn_model::model::Process, String> {
    match definitions.processes.as_slice() {
        [process] => Ok(process),
        other => Err(format!(
            "{}: a deployment is exactly one process, found {}",
            path.display(),
            other.len()
        )),
    }
}

/// The deploy verdict minus the environment — exactly what `Engine::deploy`
/// and the editor both run (`rbpmn_core::check_deployable`). Running it
/// offline is what makes `rbpmn-bench check` useful with no database at all:
/// a broken benchmark model is caught before Docker is started.
pub fn check(model: &Model, bindings: &rbpmn_core::Bindings) -> Result<Vec<String>, String> {
    match rbpmn_core::check_deployable(&model.xml, bindings) {
        rbpmn_core::DeployCheck::Unparseable(e) => Err(format!("{}: {e}", model.file)),
        rbpmn_core::DeployCheck::NotExactlyOneProcess(n) => {
            Err(format!("{}: expected one process, found {n}", model.file))
        }
        rbpmn_core::DeployCheck::Checked(checked) => {
            let errors: Vec<String> = checked
                .diagnostics
                .iter()
                .filter(|d| d.severity == rbpmn_model::Severity::Error)
                .map(|d| format!("{} [{}] {}", d.element, d.rule, d.message))
                .collect();
            if !errors.is_empty() {
                return Err(format!(
                    "{} does not lint clean:\n  {}",
                    model.file,
                    errors.join("\n  ")
                ));
            }
            Ok(checked
                .diagnostics
                .iter()
                .map(|d| format!("{} [{}] {}", d.element, d.rule, d.message))
                .collect())
        }
    }
}
