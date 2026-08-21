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
    /// The XML carries no binding: the work-item topic is bound at engine
    /// registration time (`Bindings::topic`, default: element id). `foreign` lists
    /// vendor-namespace bindings we detected but ignore (e.g. "camunda:topic").
    ServiceTask {
        foreign: Vec<String>,
    },
    UserTask,
    /// Invokes a decision from the deployment's bundled DMN artifacts. Which
    /// decision, and where its answer lands, are manifest data
    /// (`Bindings::decision`) — never in the XML, exactly like a service
    /// task's topic.
    BusinessRuleTask,
    ReceiveTask {
        message_ref: Option<Id>,
    },
    ExclusiveGateway {
        default_flow: Option<Id>,
    },
    ParallelGateway,
    EventBasedGateway,
    InclusiveGateway,
    CallActivity,
    SubProcess(SubProcessData),
    Unsupported {
        tag: String,
    },
}

impl NodeKind {
    pub fn describe(&self) -> &'static str {
        match self {
            NodeKind::Start(_) => "start event",
            NodeKind::End(_) => "end event",
            NodeKind::Catch(_) => "intermediate catch event",
            NodeKind::Throw(_) => "intermediate throw event",
            NodeKind::Boundary(_) => "boundary event",
            NodeKind::ServiceTask { .. } => "service task",
            NodeKind::UserTask => "user task",
            NodeKind::BusinessRuleTask => "business rule task",
            NodeKind::ReceiveTask { .. } => "receive task",
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

    /// The hosts [`Self::is_supported_boundary_host`] accepts, spelled for a
    /// modeller. It lives beside the predicate — and is the only place the
    /// list is written in prose — because the linter's message, the
    /// predicate and the compiler's "survived lint" guard had already drifted
    /// apart once.
    pub const SUPPORTED_BOUNDARY_HOSTS: &'static str =
        "service task, user task, receive task, embedded subprocess";

    /// Elements that open a message subscription when entered or armed: an
    /// intermediate catch, a receive task, a message boundary. The one
    /// definition on the *model* side — `rbpmn_core::ExecKind::message_arm`
    /// is its counterpart on the compiled side — so a lint rule about
    /// message arms and the compiler's subscribe path name the same set.
    pub fn is_message_arm(&self) -> bool {
        match self {
            NodeKind::Catch(CatchTrigger::Message(_)) | NodeKind::ReceiveTask { .. } => true,
            NodeKind::Boundary(b) => matches!(b.trigger, BoundaryTrigger::Message(_)),
            _ => false,
        }
    }

    /// Where a repeating `timeCycle` is actually executed: a **non-interrupting
    /// timer boundary**, and nowhere else. On an intermediate catch or an
    /// interrupting boundary the first occurrence ends the wait, so "fire
    /// once, drop the rest" would be the silent reinterpretation this linter
    /// exists to refuse.
    ///
    /// **The** answer, for the same reason as
    /// [`Self::is_supported_boundary_host`]: the linter's `check_timer` asks
    /// it to decide whether to accept the element, and the compiler asks it
    /// at its one timer chokepoint to decide whether a cycle survived lint.
    /// Written twice, the two drift, and the drift is a `timeCycle` that
    /// deploys and then fires once.
    pub fn executes_cycle(&self) -> bool {
        matches!(self, NodeKind::Boundary(b)
            if !b.cancel_activity && matches!(b.trigger, BoundaryTrigger::Timer(_)))
    }

    /// Activities that can host boundary events (v1 subset). **The** answer:
    /// the linter asks it, and so does the compiler's guard, against this
    /// same model kind.
    ///
    /// Deliberately **not** the business rule task: a decision is answered
    /// inside the transaction that parks its token, so a boundary there is
    /// armed and cancelled in one step and can never fire. It was accepted
    /// once, and produced exactly the "seems to run" this linter exists to
    /// kill (docs/design/boundary-messages.md, finding 3).
    pub fn is_supported_boundary_host(&self) -> bool {
        matches!(
            self,
            NodeKind::ServiceTask { .. }
                | NodeKind::UserTask
                | NodeKind::ReceiveTask { .. }
                | NodeKind::SubProcess(_)
        )
    }
}

/// Message events carry only the `messageRef`. Correlation bindings are
/// registered in code (`Bindings::correlation`, FEEL qualified names) and checked
/// at deploy — never declared in the XML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StartTrigger {
    None,
    Message(Option<Id>),
    Timer(TimerSpec),
    Unsupported { tag: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EndKind {
    None,
    Terminate,
    Message(Option<Id>),
    Unsupported { tag: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CatchTrigger {
    Message(Option<Id>),
    Timer(TimerSpec),
    Unsupported { tag: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ThrowKind {
    None,
    Message(Option<Id>),
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
    Message(Option<Id>),
    None,
    Unsupported { tag: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TimerSpec {
    Date(String),
    Duration(String),
    Cycle(String),
    Missing,
}

impl TimerSpec {
    /// The literal-first rule's one table: what the XML calls this spec, its
    /// text, and whether that text parses as the literal — the linter
    /// accepts with it and the compiler classifies with it, so the two
    /// cannot disagree about what is a literal and what is a variable name.
    /// `None` for a missing definition. Whether a cycle is *allowed* where it
    /// stands is the caller's question, not the text's.
    pub fn literal_check(&self) -> Option<(&'static str, &str, Result<(), String>)> {
        Some(match self {
            TimerSpec::Date(s) => ("timeDate", s.as_str(), crate::iso8601::validate_datetime(s)),
            TimerSpec::Duration(s) => (
                "timeDuration",
                s.as_str(),
                crate::iso8601::validate_duration(s),
            ),
            TimerSpec::Cycle(s) => ("timeCycle", s.as_str(), crate::iso8601::validate_cycle(s)),
            TimerSpec::Missing => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubProcessData {
    pub triggered_by_event: bool,
    pub body: Box<FlowScope>,
}
