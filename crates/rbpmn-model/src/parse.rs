//! BPMN 2.0 XML -> internal model, via roxmltree.
//!
//! The parser only fails on malformed XML or a non-BPMN root. Everything else
//! — unknown elements, missing attributes, vendor namespaces — is captured in
//! the model so the linter can report it with precise element ids.

use crate::model::*;
use roxmltree::{Document, Node};

pub const BPMN_NS: &str = "http://www.omg.org/spec/BPMN/20100524/MODEL";

/// Vendor namespaces whose attributes/extension elements we detect (to warn
/// that rbpmn ignores them — wiring is bound at registration time, never in
/// the XML) but never execute. rbpmn defines no extension namespace of its
/// own: BPMN files stay 100% standard.
const VENDOR_NS: &[(&str, &str)] = &[
    ("camunda", "http://camunda.org/schema/1.0/bpmn"),
    ("zeebe", "http://camunda.org/schema/zeebe/1.0"),
    ("flowable", "http://flowable.org/bpmn"),
    ("activiti", "http://activiti.org/bpmn"),
];

/// Non-executable content we silently ignore inside a flow scope (diagram-only
/// or spec baggage per the design brief). `incoming`/`outgoing` appear here
/// because a subProcess is both a flow node (carrying them as children) and a
/// scope container — connectivity is derived from sequence flows instead.
const IGNORED_SCOPE_CHILDREN: &[&str] = &[
    "incoming",
    "outgoing",
    "documentation",
    "extensionElements",
    "laneSet",
    "dataObject",
    "dataObjectReference",
    "dataStoreReference",
    "association",
    "textAnnotation",
    "group",
    "ioSpecification",
    "monitoring",
    "auditing",
    "property",
    "category",
];

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("invalid XML: {0}")]
    Xml(#[from] roxmltree::Error),
    #[error("not a BPMN document: root element must be bpmn:definitions")]
    NotBpmn,
}

pub fn parse(xml: &str) -> Result<Definitions, ParseError> {
    let doc = Document::parse(xml)?;
    let root = doc.root_element();
    if bpmn_tag(root) != Some("definitions") {
        return Err(ParseError::NotBpmn);
    }

    let mut p = Parser::default();
    let mut defs = Definitions {
        processes: Vec::new(),
        messages: Vec::new(),
        errors: Vec::new(),
        missing_ids: Vec::new(),
    };

    for child in elements(root) {
        match bpmn_tag(child) {
            Some("process") => {
                let id = p.id_of(child, "process");
                defs.processes.push(Process {
                    id,
                    name: attr_string(child, "name"),
                    body: p.scope(child),
                });
            }
            Some("message") => defs.messages.push(MessageDef {
                id: p.id_of(child, "message"),
                name: attr_string(child, "name"),
            }),
            Some("error") => defs.errors.push(ErrorDef {
                id: p.id_of(child, "error"),
                name: attr_string(child, "name"),
                code: attr_string(child, "errorCode"),
            }),
            // Collaborations, diagrams (bpmndi), foreign namespaces: not
            // executable content for us.
            _ => {}
        }
    }

    defs.missing_ids = p.missing_ids;
    Ok(defs)
}

#[derive(Default)]
struct Parser {
    seq: u32,
    missing_ids: Vec<String>,
}

impl Parser {
    fn id_of(&mut self, n: Node, tag: &str) -> String {
        match n.attribute("id") {
            Some(id) => id.to_string(),
            None => {
                self.seq += 1;
                let synth = format!("__missing-id-{}-{}", self.seq, tag);
                self.missing_ids.push(synth.clone());
                synth
            }
        }
    }

    fn scope(&mut self, container: Node) -> FlowScope {
        let mut scope = FlowScope::default();
        for c in elements(container) {
            let Some(tag) = bpmn_tag(c) else { continue };
            if tag == "sequenceFlow" {
                scope.flows.push(self.flow(c));
            } else if !IGNORED_SCOPE_CHILDREN.contains(&tag) {
                scope.nodes.push(self.flow_node(c, tag));
            }
        }
        scope
    }

    fn flow(&mut self, c: Node) -> SequenceFlow {
        let condition = elements(c)
            .find(|e| bpmn_tag(*e) == Some("conditionExpression"))
            .map(|e| e.text().unwrap_or("").trim().to_string());
        SequenceFlow {
            id: self.id_of(c, "sequenceFlow"),
            source: attr_string(c, "sourceRef").unwrap_or_default(),
            target: attr_string(c, "targetRef").unwrap_or_default(),
            condition,
        }
    }

    fn flow_node(&mut self, c: Node, tag: &str) -> FlowNode {
        let id = self.id_of(c, tag);
        let loop_kind = elements(c).find_map(|e| match bpmn_tag(e) {
            Some("multiInstanceLoopCharacteristics") => Some(LoopKind::MultiInstance),
            Some("standardLoopCharacteristics") => Some(LoopKind::Standard),
            _ => None,
        });

        let kind = match tag {
            "startEvent" => NodeKind::Start(self.start_trigger(c)),
            "endEvent" => NodeKind::End(self.end_kind(c)),
            "intermediateCatchEvent" => NodeKind::Catch(self.catch_trigger(c)),
            "intermediateThrowEvent" => NodeKind::Throw(self.throw_kind(c)),
            "boundaryEvent" => NodeKind::Boundary(BoundaryData {
                attached_to: attr_string(c, "attachedToRef"),
                cancel_activity: c.attribute("cancelActivity") != Some("false"),
                trigger: self.boundary_trigger(c),
            }),
            "serviceTask" => NodeKind::ServiceTask {
                foreign: foreign_bindings(c),
            },
            "userTask" => NodeKind::UserTask,
            "businessRuleTask" => NodeKind::BusinessRuleTask,
            "receiveTask" => NodeKind::ReceiveTask {
                message_ref: attr_string(c, "messageRef"),
            },
            "exclusiveGateway" => NodeKind::ExclusiveGateway {
                default_flow: attr_string(c, "default"),
            },
            "parallelGateway" => NodeKind::ParallelGateway,
            "eventBasedGateway" => NodeKind::EventBasedGateway,
            "inclusiveGateway" => NodeKind::InclusiveGateway,
            "callActivity" => NodeKind::CallActivity,
            "subProcess" => NodeKind::SubProcess(SubProcessData {
                triggered_by_event: c.attribute("triggeredByEvent") == Some("true"),
                body: Box::new(self.scope(c)),
            }),
            other => NodeKind::Unsupported {
                tag: other.to_string(),
            },
        };

        FlowNode {
            id,
            name: attr_string(c, "name"),
            kind,
            loop_kind,
        }
    }

    fn start_trigger(&mut self, c: Node) -> StartTrigger {
        match single_event_def(c) {
            EventDefs::None => StartTrigger::None,
            EventDefs::One(def, "messageEventDefinition") => {
                StartTrigger::Message(message_ref(def))
            }
            EventDefs::One(def, "timerEventDefinition") => StartTrigger::Timer(timer_spec(def)),
            EventDefs::One(_, tag) => StartTrigger::Unsupported {
                tag: tag.to_string(),
            },
            EventDefs::Multiple => StartTrigger::Unsupported {
                tag: "multiple event definitions".to_string(),
            },
        }
    }

    fn end_kind(&mut self, c: Node) -> EndKind {
        match single_event_def(c) {
            EventDefs::None => EndKind::None,
            EventDefs::One(_, "terminateEventDefinition") => EndKind::Terminate,
            EventDefs::One(def, "messageEventDefinition") => EndKind::Message(message_ref(def)),
            EventDefs::One(_, tag) => EndKind::Unsupported {
                tag: tag.to_string(),
            },
            EventDefs::Multiple => EndKind::Unsupported {
                tag: "multiple event definitions".to_string(),
            },
        }
    }

    fn catch_trigger(&mut self, c: Node) -> CatchTrigger {
        match single_event_def(c) {
            EventDefs::One(def, "messageEventDefinition") => {
                CatchTrigger::Message(message_ref(def))
            }
            EventDefs::One(def, "timerEventDefinition") => CatchTrigger::Timer(timer_spec(def)),
            EventDefs::One(_, tag) => CatchTrigger::Unsupported {
                tag: tag.to_string(),
            },
            EventDefs::None => CatchTrigger::Unsupported {
                tag: "missing event definition".to_string(),
            },
            EventDefs::Multiple => CatchTrigger::Unsupported {
                tag: "multiple event definitions".to_string(),
            },
        }
    }

    fn throw_kind(&mut self, c: Node) -> ThrowKind {
        match single_event_def(c) {
            EventDefs::None => ThrowKind::None,
            EventDefs::One(def, "messageEventDefinition") => ThrowKind::Message(message_ref(def)),
            EventDefs::One(_, tag) => ThrowKind::Unsupported {
                tag: tag.to_string(),
            },
            EventDefs::Multiple => ThrowKind::Unsupported {
                tag: "multiple event definitions".to_string(),
            },
        }
    }

    fn boundary_trigger(&mut self, c: Node) -> BoundaryTrigger {
        match single_event_def(c) {
            EventDefs::One(def, "timerEventDefinition") => BoundaryTrigger::Timer(timer_spec(def)),
            EventDefs::One(def, "errorEventDefinition") => BoundaryTrigger::Error {
                error_ref: attr_string(def, "errorRef"),
            },
            EventDefs::One(def, "messageEventDefinition") => {
                BoundaryTrigger::Message(message_ref(def))
            }
            EventDefs::One(_, tag) => BoundaryTrigger::Unsupported {
                tag: tag.to_string(),
            },
            EventDefs::None => BoundaryTrigger::None,
            EventDefs::Multiple => BoundaryTrigger::Unsupported {
                tag: "multiple event definitions".to_string(),
            },
        }
    }
}

enum EventDefs<'a, 'i> {
    None,
    One(Node<'a, 'i>, &'a str),
    Multiple,
}

fn single_event_def<'a, 'i>(c: Node<'a, 'i>) -> EventDefs<'a, 'i> {
    let mut defs = elements(c).filter(|e| {
        e.tag_name().namespace() == Some(BPMN_NS)
            && e.tag_name().name().ends_with("EventDefinition")
    });
    match (defs.next(), defs.next()) {
        (None, _) => EventDefs::None,
        (Some(d), None) => EventDefs::One(d, d.tag_name().name()),
        (Some(_), Some(_)) => EventDefs::Multiple,
    }
}

fn message_ref(def: Node) -> Option<String> {
    attr_string(def, "messageRef")
}

fn timer_spec(def: Node) -> TimerSpec {
    for c in elements(def) {
        let text = || c.text().unwrap_or("").trim().to_string();
        match bpmn_tag(c) {
            Some("timeDate") => return TimerSpec::Date(text()),
            Some("timeDuration") => return TimerSpec::Duration(text()),
            Some("timeCycle") => return TimerSpec::Cycle(text()),
            _ => {}
        }
    }
    TimerSpec::Missing
}

fn foreign_bindings(c: Node) -> Vec<String> {
    let mut foreign = Vec::new();
    for a in c.attributes() {
        if let Some(prefix) = vendor_prefix(a.namespace().unwrap_or("")) {
            foreign.push(format!("{prefix}:{}", a.name()));
        }
    }
    for ext in elements(c).filter(|e| bpmn_tag(*e) == Some("extensionElements")) {
        for d in ext.descendants().filter(Node::is_element) {
            if let Some(prefix) = vendor_prefix(d.tag_name().namespace().unwrap_or("")) {
                foreign.push(format!("{prefix}:{}", d.tag_name().name()));
            }
        }
    }
    foreign.dedup();
    foreign
}

fn vendor_prefix(ns: &str) -> Option<&'static str> {
    VENDOR_NS
        .iter()
        .find(|(_, uri)| *uri == ns)
        .map(|(prefix, _)| *prefix)
}

fn elements<'a, 'i>(n: Node<'a, 'i>) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children().filter(Node::is_element)
}

fn bpmn_tag<'a>(n: Node<'a, '_>) -> Option<&'a str> {
    if n.tag_name().namespace() == Some(BPMN_NS) {
        Some(n.tag_name().name())
    } else {
        None
    }
}

fn attr_string(n: Node, name: &str) -> Option<String> {
    n.attribute(name).map(str::to_string)
}
