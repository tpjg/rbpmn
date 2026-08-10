use rbpmn_model::Diagnostic;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    /// The front door: lint errors and wiring gaps (`unresolved-topic`),
    /// machine-readable, exactly what the design brief promises.
    #[error("deployment rejected ({} diagnostics)", .0.len())]
    Rejected(Vec<Diagnostic>),
    #[error("BPMN XML does not parse: {0}")]
    Xml(#[from] rbpmn_model::ParseError),
    #[error("a deployment must contain exactly one process, found {0}")]
    NotExactlyOneProcess(usize),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("no deployed definition with key '{0}'")]
    UnknownDefinition(String),
    #[error("no work item {0}")]
    UnknownWorkItem(Uuid),
    #[error("instance {0} is not active (status: {1})")]
    InstanceNotActive(Uuid, String),
    #[error("instance {0} has an open incident; resolve it first")]
    IncidentOpen(Uuid),
    #[error("definition no longer compiles: {0}")]
    Compile(#[from] rbpmn_core::CompileError),
    #[error("semantic core rejected the step: {0}")]
    Step(#[from] rbpmn_core::StepError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Idempotent deploy result: `reused` is true when the content hash matched
/// the latest version and no new version row was created.
#[derive(Debug)]
pub struct Deployment {
    pub definition_id: Uuid,
    pub key: String,
    pub version: i32,
    pub reused: bool,
    /// Warn-severity diagnostics; errors would have rejected the deploy.
    pub warnings: Vec<Diagnostic>,
}

#[derive(Debug)]
pub struct StartedInstance {
    pub id: Uuid,
    pub events: Vec<rbpmn_core::Event>,
}

/// Completion is exactly-once as a *state transition*; a repeat is a distinct
/// no-op result, not an error (handlers are at-least-once and must retry).
#[derive(Debug)]
pub enum Completion {
    Advanced(Vec<rbpmn_core::Event>),
    AlreadyClosed { state: String },
}

#[derive(Debug, PartialEq, Eq)]
pub enum FailOutcome {
    /// The item went back to `available` with one fewer retry.
    Retrying { retries_left: i32 },
    /// Retry budget exhausted: the work item is failed and the instance is in
    /// the incident state (no error boundary matched — none are executable
    /// yet; boundary matching lands with the phase-2 follow-up milestone).
    IncidentRaised,
}
