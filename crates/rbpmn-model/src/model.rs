//! The internal model: what the parser extracts from BPMN XML.
//!
//! Deliberately permissive — unsupported constructs are *represented* (as
//! `Unsupported`, `InclusiveGateway`, `CallActivity`, ...) so the linter can
//! point at them with precise element ids instead of the parser failing.

use serde::{Deserialize, Serialize};

pub type Id = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Definitions {
    pub processes: Vec<Process>,
    pub messages: Vec<MessageDef>,
    pub errors: Vec<ErrorDef>,
    /// Synthesized ids of elements that were missing the required `id` attribute.
    pub missing_ids: Vec<Id>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDef {
    pub id: Id,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorDef {
    pub id: Id,
    pub name: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Process {
    pub id: Id,
    pub name: Option<String>,
    pub body: FlowScope,
}

/// A flow container: a process body or an embedded subprocess body.
/// Sequence flows never cross scope boundaries.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FlowScope {
    pub nodes: Vec<FlowNode>,
    pub flows: Vec<SequenceFlow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceFlow {
    pub id: Id,
    pub source: Id,
    pub target: Id,
    /// Raw condition expression text; parsed/validated by the linter against
    /// the tiny condition grammar.
    pub condition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowNode {
    pub id: Id,
    pub name: Option<String>,
    pub kind: NodeKind,
    pub loop_kind: Option<LoopKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopKind {
    MultiInstance,
    Standard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    Start(StartTrigger),
    End(EndKind),
    Catch(CatchTrigger),
    Throw(ThrowKind),
    Boundary(BoundaryData),
    ServiceTask(ServiceBinding),
    UserTask,
    ReceiveTask(MessageBinding),
    ExclusiveGateway { default_flow: Option<Id> },
    ParallelGateway,
    EventBasedGateway,
    InclusiveGateway,
    CallActivity,
    SubProcess(SubProcessData),
    Unsupported { tag: String },
}

impl NodeKind {
    pub fn describe(&self) -> &'static str {
        match self {
            NodeKind::Start(_) => "start event",
            NodeKind::End(_) => "end event",
            NodeKind::Catch(_) => "intermediate catch event",
            NodeKind::Throw(_) => "intermediate throw event",
            NodeKind::Boundary(_) => "boundary event",
            NodeKind::ServiceTask(_) => "service task",
            NodeKind::UserTask => "user task",
            NodeKind::ReceiveTask(_) => "receive task",
            NodeKind::ExclusiveGateway { .. } => "exclusive gateway",
            NodeKind::ParallelGateway => "parallel gateway",
            NodeKind::EventBasedGateway => "event-based gateway",
            NodeKind::InclusiveGateway => "inclusive gateway",
            NodeKind::CallActivity => "call activity",
            NodeKind::SubProcess(_) => "subprocess",
            NodeKind::Unsupported { .. } => "unsupported element",
        }
    }

    pub fn is_gateway(&self) -> bool {
        matches!(
            self,
            NodeKind::ExclusiveGateway { .. }
                | NodeKind::ParallelGateway
                | NodeKind::EventBasedGateway
                | NodeKind::InclusiveGateway
        )
    }

    /// Activities that can host boundary events (v1 subset).
    pub fn is_supported_boundary_host(&self) -> bool {
        matches!(
            self,
            NodeKind::ServiceTask(_)
                | NodeKind::UserTask
                | NodeKind::ReceiveTask(_)
                | NodeKind::SubProcess(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StartTrigger {
    None,
    Message(MessageBinding),
    Timer(TimerSpec),
    Unsupported { tag: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EndKind {
    None,
    Terminate,
    Message(MessageBinding),
    Unsupported { tag: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CatchTrigger {
    Message(MessageBinding),
    Timer(TimerSpec),
    Unsupported { tag: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ThrowKind {
    None,
    Message(MessageBinding),
    Unsupported { tag: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundaryData {
    pub attached_to: Option<Id>,
    pub cancel_activity: bool,
    pub trigger: BoundaryTrigger,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BoundaryTrigger {
    Timer(TimerSpec),
    Error { error_ref: Option<Id> },
    Message(MessageBinding),
    None,
    Unsupported { tag: String },
}

/// How a message start/catch/throw binds to a message and its correlation key.
/// The correlation key is an rbpmn:correlationKey attribute holding a JSON
/// pointer into the instance variable document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageBinding {
    pub message_ref: Option<Id>,
    pub correlation_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TimerSpec {
    Date(String),
    Duration(String),
    Cycle(String),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceBinding {
    /// rbpmn:topic — which work-item topic handlers/workers subscribe to.
    pub topic: Option<String>,
    /// Vendor-namespace bindings we detected but ignore (e.g. "camunda:topic").
    pub foreign: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubProcessData {
    pub triggered_by_event: bool,
    pub body: Box<FlowScope>,
}
