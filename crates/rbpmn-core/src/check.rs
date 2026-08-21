//! The deploy verdict, minus the database.
//!
//! `deploy` decides six things without touching Postgres — is this BPMN at
//! all, is there exactly one process, do the bundled DMN artifacts validate,
//! do the decision bindings resolve against them, does the linter pass, and
//! does the model compile against its bindings manifest — and exactly one
//! thing with it: are the resolved service topics covered by the environment
//! as registered right now (`unresolved-topic`).
//!
//! That split lives here so both callers share it. `Engine::deploy` runs this
//! and then the environment link; the editor runs this in WASM and does the
//! link itself against a covered-topic set it fetched. Neither reimplements
//! the other, which is the same guarantee `just parity` buys for the linter:
//! a surface that reports a verdict must report *the* verdict.
//!
//! Bundled DMN artifacts are validated here too, through an injected
//! [`DecisionValidator`] — the core has no DMN model type and must not
//! acquire one (`docs/dmn.md`, D1). Deploy passes `rbpmn_dmn`'s
//! implementation and the editor passes the same one, which is what stops
//! the two surfaces reaching different verdicts about the same bundle.
//!
//! Not covered here: manifest index declarations, whose validation is SQL
//! identifier policy and stays in the engine. They are a performance
//! declaration and cannot make a model unexecutable.

use crate::compile::{Bindings, CompileError, ExecutableProcess};
use crate::decisions::{DecisionValidator, Invocable};
use rbpmn_model::{Diagnostic, ParseError, rule};

/// What can be known about a deployment before the environment is consulted.
#[derive(Debug)]
pub enum DeployCheck {
    /// Not a BPMN XML document at all.
    Unparseable(ParseError),
    /// A deployment is one process; zero or several is a packaging mistake
    /// rather than a modelling one, so it is not a diagnostic.
    NotExactlyOneProcess(usize),
    /// Parsed, linted and compiled as far as is possible without the
    /// environment. `diagnostics` still decides the verdict: any error
    /// severity means deploy would reject.
    Checked(Checked),
}

#[derive(Debug)]
pub struct Checked {
    /// The definition key — the id of the single process.
    pub key: String,
    /// Lint diagnostics plus any compile failure, mapped to the same rule
    /// ids `deploy` reports, so the two are comparable set-for-set.
    pub diagnostics: Vec<Diagnostic>,
    /// `(element id, resolved topic)` for every service task, empty when the
    /// model did not compile. This is the input to the environment link the
    /// caller performs: any topic outside the covered set is an
    /// `unresolved-topic` error.
    pub topics: Vec<(String, String)>,
    /// What the bundled decision artifacts expose, empty when there are none
    /// or they did not compile. P2 matches manifest bindings against this for
    /// `unresolved-decision`; unlike topics it needs no environment, because
    /// the artifacts travel inside the deployment.
    pub invocables: Vec<Invocable>,
}

impl Checked {
    /// True when no diagnostic is an error — i.e. deploy would proceed to the
    /// environment link.
    pub fn ok(&self) -> bool {
        !rbpmn_model::has_errors(&self.diagnostics)
    }

    /// The environment link, as `deploy` performs it: every resolved service
    /// topic must be covered by a registered handler or a declared
    /// external-worker topic. Returns the `unresolved-topic` errors, if any.
    ///
    /// Kept here rather than at each call site because the message is part of
    /// the rule's contract — a caller that phrased it differently would be
    /// telling the modeler about a different rule than the one that will
    /// reject the deploy.
    pub fn unresolved_topics<F>(&self, covered: F) -> Vec<Diagnostic>
    where
        F: Fn(&str) -> bool,
    {
        self.topics
            .iter()
            .filter(|(_, topic)| !covered(topic))
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
            .collect()
    }
}

/// Run every deploy check that does not need a database.
///
/// `decisions` is the DMN artifacts the deployment bundles, as raw XML, and
/// `validator` is what knows how to read them. They are checked
/// *independently* of the process: a broken decision table and a broken
/// diagram are separate problems, and making someone fix one to discover the
/// other wastes a round trip.
pub fn check_deployable(
    xml: &str,
    bindings: &Bindings,
    decisions: &[String],
    validator: &dyn DecisionValidator,
) -> DeployCheck {
    let decided = validator.check(decisions);

    let defs = match rbpmn_model::parse(xml) {
        Ok(defs) => defs,
        Err(e) => return DeployCheck::Unparseable(e),
    };
    if defs.processes.len() != 1 {
        return DeployCheck::NotExactlyOneProcess(defs.processes.len());
    }
    let key = defs.processes[0].id.clone();

    let mut diagnostics = decided.diagnostics;
    diagnostics.extend(decision_bindings(bindings, &decided.invocables));
    diagnostics.extend(rbpmn_model::lint(&defs));
    // Compilation re-lints, so running it over a model the linter already
    // rejected would only restate those errors. Stop at the first gate, the
    // way deploy does.
    if rbpmn_model::has_errors(&diagnostics) {
        return DeployCheck::Checked(Checked {
            key,
            diagnostics,
            topics: Vec::new(),
            invocables: decided.invocables,
        });
    }

    // Phase gating + condition/topic/correlation resolution. The mappings
    // below are the contract: a compile failure is reported as the rule a
    // modeler can act on, never as a raw error string.
    match ExecutableProcess::compile(&defs, &key, bindings) {
        Ok(proc) => {
            let topics = proc
                .service_topics()
                .map(|(element, topic)| (element.to_string(), topic.to_string()))
                .collect();
            DeployCheck::Checked(Checked {
                key,
                diagnostics,
                topics,
                invocables: decided.invocables,
            })
        }
        Err(e) => {
            diagnostics.extend(compile_diagnostics(&key, e));
            DeployCheck::Checked(Checked {
                key,
                diagnostics,
                topics: Vec::new(),
                invocables: decided.invocables,
            })
        }
    }
}

/// `decision-has-binding` and `unresolved-decision`, over the manifest.
///
/// This is the decision half of the link step, and unlike `unresolved-topic`
/// it is *complete here*: topics are an environment question, but a
/// deployment's decisions travel inside it, so the editor can answer this
/// offline with no server and no upload.
///
/// The "a business-rule task must *have* a binding" half is not here: it needs
/// the compiled process to know which elements are business-rule tasks, so it
/// lives in `compile` and reports the same rule id. Both halves are
/// `decision-has-binding`; this one checks the binding is well-formed and
/// resolves, that one checks it exists.
fn decision_bindings(bindings: &Bindings, invocables: &[Invocable]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (element, binding) in &bindings.decisions {
        if let Err(e) = rbpmn_model::condition::parse_qname(&binding.result) {
            diagnostics.push(Diagnostic::error(
                rule::DECISION_HAS_BINDING,
                element,
                format!(
                    "the decision result path is not a FEEL qualified name: {e} \
                     (it names where the answer lands in the variable document, \
                     like `order.discount`)"
                ),
            ));
        }
        let matches: Vec<&Invocable> = invocables
            .iter()
            .filter(|i| i.name == binding.decision)
            .collect();
        match matches.as_slice() {
            [_] => {}
            [] => diagnostics.push(Diagnostic::error(
                rule::UNRESOLVED_DECISION,
                element,
                format!(
                    "no decision named '{}' in the bundled DMN artifacts{}",
                    binding.decision,
                    if invocables.is_empty() {
                        " — the deployment bundles none".to_string()
                    } else {
                        format!(
                            " (available: {})",
                            invocables
                                .iter()
                                .map(|i| i.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                ),
            )),
            // Refused, never picked: delivering to "one of them" would be a
            // guess, and a deployment that guesses is one that runs the wrong
            // decision on a Tuesday.
            several => diagnostics.push(Diagnostic::error(
                rule::UNRESOLVED_DECISION,
                element,
                format!(
                    "'{}' is defined by {} bundled artifacts ({}) — rename one, or \
                     bundle only the artifact this deployment means",
                    binding.decision,
                    several.len(),
                    several
                        .iter()
                        .map(|i| format!("{}/{}", i.namespace, i.model))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        }
    }
    diagnostics
}

/// One compile failure -> the diagnostics deploy reports for it.
fn compile_diagnostics(key: &str, e: CompileError) -> Vec<Diagnostic> {
    match e {
        CompileError::MissingCorrelation(elements) => elements
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
        CompileError::MissingDecision(elements) => elements
            .iter()
            .map(|el| {
                Diagnostic::error(
                    rule::DECISION_HAS_BINDING,
                    el,
                    "business-rule task has no decision binding — bind it with \
                     Bindings::decision(element_id, decision_name, result_path). \
                     There is no default: guessing a decision by element id would \
                     invoke business logic nobody chose",
                )
            })
            .collect(),
        CompileError::InvalidDecision { element, reason } => {
            vec![Diagnostic::error(
                rule::DECISION_HAS_BINDING,
                element,
                format!("decision binding is not usable: {reason}"),
            )]
        }
        // One diagnostic per offending element of every group, like
        // `MissingCorrelation`: the modeller has to look at each arm, and an
        // editor highlights what it is told to highlight. The sentence is
        // built once per group, not once per element — every element of a
        // group is told the same thing.
        CompileError::AmbiguousMessageArm(groups) => groups
            .iter()
            .flat_map(|g| {
                let quantifier = if g.elements.len() == 2 { "both" } else { "all" };
                let text = format!(
                    "{} {quantifier} catch '{}' correlated by the same key ('{}') and are \
                     live at the same time, so every delivery would be ambiguous — give one \
                     arm a different message, or a different correlation binding",
                    crate::compile::and_list(&g.elements),
                    g.message,
                    g.binding,
                );
                g.elements
                    .iter()
                    .map(move |el| Diagnostic::error(rule::AMBIGUOUS_MESSAGE_ARM, el, text.clone()))
            })
            .collect(),
        CompileError::InvalidCorrelation { element, reason } => {
            vec![Diagnostic::error(
                rule::MESSAGE_HAS_CORRELATION,
                element,
                format!("correlation binding is not a FEEL qualified name: {reason}"),
            )]
        }
        // RejectedByLint cannot reach here (the gate above returned already),
        // and the rest — phase gating, unknown process, internals — are
        // reported against the process itself as an unsupported element.
        e => vec![Diagnostic::error(
            rule::NO_UNSUPPORTED_ELEMENT,
            key,
            e.to_string(),
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="defs">
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"><bpmn:outgoing>f1</bpmn:outgoing></bpmn:startEvent>
    <bpmn:serviceTask id="st"><bpmn:incoming>f1</bpmn:incoming><bpmn:outgoing>f2</bpmn:outgoing></bpmn:serviceTask>
    <bpmn:endEvent id="end"><bpmn:incoming>f2</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="st" />
    <bpmn:sequenceFlow id="f2" sourceRef="st" targetRef="end" />
  </bpmn:process>
</bpmn:definitions>"#;

    fn checked(xml: &str, bindings: &Bindings) -> Checked {
        match check_deployable(xml, bindings, &[], &crate::NoDecisions) {
            DeployCheck::Checked(c) => c,
            other => panic!("expected Checked, got {other:?}"),
        }
    }

    #[test]
    fn clean_model_resolves_its_topics() {
        let c = checked(MINIMAL, &Bindings::new().topic("st", "payments"));
        assert!(c.ok(), "{:?}", c.diagnostics);
        assert_eq!(c.key, "p");
        assert_eq!(c.topics, vec![("st".to_string(), "payments".to_string())]);
    }

    /// The default topic is the element id — the rule that makes an unmapped
    /// service task resolvable at all.
    #[test]
    fn unmapped_service_task_defaults_to_its_element_id() {
        let c = checked(MINIMAL, &Bindings::new());
        assert_eq!(c.topics, vec![("st".to_string(), "st".to_string())]);
    }

    #[test]
    fn environment_link_is_the_caller_s_half() {
        let c = checked(MINIMAL, &Bindings::new().topic("st", "payments"));
        assert!(c.unresolved_topics(|t| t == "payments").is_empty());
        let gaps = c.unresolved_topics(|_| false);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].rule, rule::UNRESOLVED_TOPIC);
        assert_eq!(gaps[0].element, "st");
    }

    #[test]
    fn not_bpmn_at_all() {
        assert!(matches!(
            check_deployable("<not-xml", &Bindings::new(), &[], &crate::NoDecisions),
            DeployCheck::Unparseable(_)
        ));
    }

    #[test]
    fn a_deployment_is_one_process() {
        let two = MINIMAL.replace(
            "</bpmn:definitions>",
            r#"<bpmn:process id="q" isExecutable="true">
                 <bpmn:startEvent id="s2"><bpmn:outgoing>g1</bpmn:outgoing></bpmn:startEvent>
                 <bpmn:endEvent id="e2"><bpmn:incoming>g1</bpmn:incoming></bpmn:endEvent>
                 <bpmn:sequenceFlow id="g1" sourceRef="s2" targetRef="e2" />
               </bpmn:process></bpmn:definitions>"#,
        );
        assert!(matches!(
            check_deployable(&two, &Bindings::new(), &[], &crate::NoDecisions),
            DeployCheck::NotExactlyOneProcess(2)
        ));
    }

    /// `ambiguous-message-arm`: two message boundaries on one host, both
    /// catching PAID by the same key. They are armed together and withdrawn
    /// together, so *every* delivery would be ambiguous — a freeze that is
    /// certain at deploy belongs at deploy.
    const TWO_BOUNDARIES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="defs">
  <bpmn:message id="m" name="PAID" />
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"><bpmn:outgoing>f1</bpmn:outgoing></bpmn:startEvent>
    <bpmn:userTask id="ut"><bpmn:incoming>f1</bpmn:incoming><bpmn:outgoing>f2</bpmn:outgoing></bpmn:userTask>
    <bpmn:boundaryEvent id="b1" attachedToRef="ut">
      <bpmn:outgoing>f3</bpmn:outgoing><bpmn:messageEventDefinition messageRef="m" />
    </bpmn:boundaryEvent>
    <bpmn:boundaryEvent id="b2" attachedToRef="ut">
      <bpmn:outgoing>f4</bpmn:outgoing><bpmn:messageEventDefinition messageRef="m" />
    </bpmn:boundaryEvent>
    <bpmn:endEvent id="end"><bpmn:incoming>f2</bpmn:incoming></bpmn:endEvent>
    <bpmn:endEvent id="e1"><bpmn:incoming>f3</bpmn:incoming></bpmn:endEvent>
    <bpmn:endEvent id="e2"><bpmn:incoming>f4</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ut" />
    <bpmn:sequenceFlow id="f2" sourceRef="ut" targetRef="end" />
    <bpmn:sequenceFlow id="f3" sourceRef="b1" targetRef="e1" />
    <bpmn:sequenceFlow id="f4" sourceRef="b2" targetRef="e2" />
  </bpmn:process>
</bpmn:definitions>"#;

    /// A receive task waiting for PAID with a PAID boundary on itself: the
    /// host's own arm and the boundary's, on one token.
    const HOST_AND_ITS_BOUNDARY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="defs">
  <bpmn:message id="m" name="PAID" />
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"><bpmn:outgoing>f1</bpmn:outgoing></bpmn:startEvent>
    <bpmn:receiveTask id="rt" messageRef="m"><bpmn:incoming>f1</bpmn:incoming><bpmn:outgoing>f2</bpmn:outgoing></bpmn:receiveTask>
    <bpmn:boundaryEvent id="b" attachedToRef="rt">
      <bpmn:outgoing>f3</bpmn:outgoing><bpmn:messageEventDefinition messageRef="m" />
    </bpmn:boundaryEvent>
    <bpmn:endEvent id="end"><bpmn:incoming>f2</bpmn:incoming></bpmn:endEvent>
    <bpmn:endEvent id="e1"><bpmn:incoming>f3</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="rt" />
    <bpmn:sequenceFlow id="f2" sourceRef="rt" targetRef="end" />
    <bpmn:sequenceFlow id="f3" sourceRef="b" targetRef="e1" />
  </bpmn:process>
</bpmn:definitions>"#;

    /// A PAID boundary on a subprocess and a PAID catch two scopes down: the
    /// parent's arm is live for the whole life of the body, so depth changes
    /// nothing.
    const BOUNDARY_AND_A_CATCH_INSIDE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="defs">
  <bpmn:message id="m" name="PAID" />
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"><bpmn:outgoing>f1</bpmn:outgoing></bpmn:startEvent>
    <bpmn:subProcess id="sp">
      <bpmn:incoming>f1</bpmn:incoming><bpmn:outgoing>f2</bpmn:outgoing>
      <bpmn:startEvent id="s2"><bpmn:outgoing>g1</bpmn:outgoing></bpmn:startEvent>
      <bpmn:subProcess id="sp2">
        <bpmn:incoming>g1</bpmn:incoming><bpmn:outgoing>g2</bpmn:outgoing>
        <bpmn:startEvent id="s3"><bpmn:outgoing>h1</bpmn:outgoing></bpmn:startEvent>
        <bpmn:receiveTask id="inner" messageRef="m"><bpmn:incoming>h1</bpmn:incoming><bpmn:outgoing>h2</bpmn:outgoing></bpmn:receiveTask>
        <bpmn:endEvent id="e3"><bpmn:incoming>h2</bpmn:incoming></bpmn:endEvent>
        <bpmn:sequenceFlow id="h1" sourceRef="s3" targetRef="inner" />
        <bpmn:sequenceFlow id="h2" sourceRef="inner" targetRef="e3" />
      </bpmn:subProcess>
      <bpmn:endEvent id="e2"><bpmn:incoming>g2</bpmn:incoming></bpmn:endEvent>
      <bpmn:sequenceFlow id="g1" sourceRef="s2" targetRef="sp2" />
      <bpmn:sequenceFlow id="g2" sourceRef="sp2" targetRef="e2" />
    </bpmn:subProcess>
    <bpmn:boundaryEvent id="b" attachedToRef="sp">
      <bpmn:outgoing>f3</bpmn:outgoing><bpmn:messageEventDefinition messageRef="m" />
    </bpmn:boundaryEvent>
    <bpmn:endEvent id="end"><bpmn:incoming>f2</bpmn:incoming></bpmn:endEvent>
    <bpmn:endEvent id="e1"><bpmn:incoming>f3</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="sp" />
    <bpmn:sequenceFlow id="f2" sourceRef="sp" targetRef="end" />
    <bpmn:sequenceFlow id="f3" sourceRef="b" targetRef="e1" />
  </bpmn:process>
</bpmn:definitions>"#;

    /// A non-interrupting NOTE boundary whose side path waits for NOTE
    /// again, under the same key. The boundary re-arms and spawns the side
    /// token in one step, so the catch subscribes into an arm that is
    /// already open: the *first* delivery freezes the instance.
    ///
    /// `{ARM}` is the side path's waiting element and `{ARM_BINDING}` the key
    /// it is bound to, so the three tests below differ by one substitution
    /// each rather than by a copy of the whole model.
    const SIDE_PATH_ARM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="defs">
  <bpmn:message id="m" name="NOTE" />
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"><bpmn:outgoing>f1</bpmn:outgoing></bpmn:startEvent>
    <bpmn:userTask id="ut"><bpmn:incoming>f1</bpmn:incoming><bpmn:outgoing>f2</bpmn:outgoing></bpmn:userTask>
    <bpmn:boundaryEvent id="b" cancelActivity="false" attachedToRef="ut">
      <bpmn:outgoing>f3</bpmn:outgoing><bpmn:messageEventDefinition messageRef="m" />
    </bpmn:boundaryEvent>
    {ARM}
    <bpmn:endEvent id="end"><bpmn:incoming>f2</bpmn:incoming></bpmn:endEvent>
    <bpmn:endEvent id="e1"><bpmn:incoming>f4</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ut" />
    <bpmn:sequenceFlow id="f2" sourceRef="ut" targetRef="end" />
    <bpmn:sequenceFlow id="f3" sourceRef="b" targetRef="{ENTRY}" />
    <bpmn:sequenceFlow id="f4" sourceRef="{EXIT}" targetRef="e1" />
  </bpmn:process>
</bpmn:definitions>"#;

    /// The catch sits directly on the side path.
    const FLAT_ARM: &str = r#"<bpmn:intermediateCatchEvent id="catch_ack">
      <bpmn:incoming>f3</bpmn:incoming><bpmn:outgoing>f4</bpmn:outgoing>
      <bpmn:messageEventDefinition messageRef="m" />
    </bpmn:intermediateCatchEvent>"#;

    /// ...and the same catch one scope down, inside a subprocess on the side
    /// path — which is the repair `boundary-side-path` recommends for a
    /// parallel block, so it must not be a way to smuggle the arm past this.
    const NESTED_ARM: &str = r#"<bpmn:subProcess id="sp">
      <bpmn:incoming>f3</bpmn:incoming><bpmn:outgoing>f4</bpmn:outgoing>
      <bpmn:startEvent id="s2"><bpmn:outgoing>g1</bpmn:outgoing></bpmn:startEvent>
      <bpmn:intermediateCatchEvent id="catch_ack">
        <bpmn:incoming>g1</bpmn:incoming><bpmn:outgoing>g2</bpmn:outgoing>
        <bpmn:messageEventDefinition messageRef="m" />
      </bpmn:intermediateCatchEvent>
      <bpmn:endEvent id="e2"><bpmn:incoming>g2</bpmn:incoming></bpmn:endEvent>
      <bpmn:sequenceFlow id="g1" sourceRef="s2" targetRef="catch_ack" />
      <bpmn:sequenceFlow id="g2" sourceRef="catch_ack" targetRef="e2" />
    </bpmn:subProcess>"#;

    fn side_path_model(arm: &str, entry: &str) -> String {
        SIDE_PATH_ARM
            .replace("{ARM}", arm)
            .replace("{ENTRY}", entry)
            .replace("{EXIT}", entry)
    }

    #[test]
    fn a_side_path_arm_for_the_boundary_s_own_pair_is_ambiguous() {
        let d = ambiguity(
            &side_path_model(FLAT_ARM, "catch_ack"),
            &Bindings::new()
                .correlation("b", "case.id")
                .correlation("catch_ack", "case.id"),
        );
        let elements: Vec<&str> = d.iter().map(|d| d.element.as_str()).collect();
        assert_eq!(elements, vec!["b", "catch_ack"], "{d:?}");
    }

    /// The negative, and the same one that makes the rule L2: a different
    /// binding is a different key, and "each activation acknowledges its own
    /// note" is exactly how this shape is meant to be written.
    #[test]
    fn a_side_path_arm_under_a_different_binding_is_fine() {
        let c = checked(
            &side_path_model(FLAT_ARM, "catch_ack"),
            &Bindings::new()
                .correlation("b", "case.id")
                .correlation("catch_ack", "note.id"),
        );
        assert!(c.ok(), "{:?}", c.diagnostics);
    }

    #[test]
    fn a_side_path_arm_inside_a_subprocess_is_ambiguous_too() {
        let d = ambiguity(
            &side_path_model(NESTED_ARM, "sp"),
            &Bindings::new()
                .correlation("b", "case.id")
                .correlation("catch_ack", "case.id"),
        );
        let elements: Vec<&str> = d.iter().map(|d| d.element.as_str()).collect();
        assert_eq!(elements, vec!["b", "catch_ack"], "{d:?}");
    }

    fn ambiguity(xml: &str, bindings: &Bindings) -> Vec<Diagnostic> {
        let c = checked(xml, bindings);
        c.diagnostics
            .into_iter()
            .filter(|d| d.rule == rule::AMBIGUOUS_MESSAGE_ARM)
            .collect()
    }

    #[test]
    fn two_message_boundaries_on_one_host_are_ambiguous() {
        let d = ambiguity(
            TWO_BOUNDARIES,
            &Bindings::new()
                .correlation("b1", "order.id")
                .correlation("b2", "order.id"),
        );
        let elements: Vec<&str> = d.iter().map(|d| d.element.as_str()).collect();
        assert_eq!(elements, vec!["b1", "b2"], "{d:?}");
    }

    #[test]
    fn a_boundary_catching_its_host_s_own_message_is_ambiguous() {
        let d = ambiguity(
            HOST_AND_ITS_BOUNDARY,
            &Bindings::new()
                .correlation("rt", "order.id")
                .correlation("b", "order.id"),
        );
        let elements: Vec<&str> = d.iter().map(|d| d.element.as_str()).collect();
        assert_eq!(elements, vec!["rt", "b"], "{d:?}");
    }

    #[test]
    fn a_subprocess_boundary_and_a_catch_inside_it_are_ambiguous() {
        let d = ambiguity(
            BOUNDARY_AND_A_CATCH_INSIDE,
            &Bindings::new()
                .correlation("inner", "order.id")
                .correlation("b", "order.id"),
        );
        // Declaration order, which puts the parent scope's boundary before a
        // node two scopes down — a stable order, and the one an editor will
        // list the two highlights in.
        let elements: Vec<&str> = d.iter().map(|d| d.element.as_str()).collect();
        assert_eq!(elements, vec!["b", "inner"], "{d:?}");
    }

    /// Two sequential user tasks, each carrying two PAID boundaries under one
    /// binding: two ambiguities, and a modeller who is told about one fixes
    /// it and is refused again.
    const TWO_AMBIGUOUS_HOSTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="defs">
  <bpmn:message id="m" name="PAID" />
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"><bpmn:outgoing>f1</bpmn:outgoing></bpmn:startEvent>
    <bpmn:userTask id="ut1"><bpmn:incoming>f1</bpmn:incoming><bpmn:outgoing>f2</bpmn:outgoing></bpmn:userTask>
    <bpmn:boundaryEvent id="b1" attachedToRef="ut1">
      <bpmn:outgoing>f5</bpmn:outgoing><bpmn:messageEventDefinition messageRef="m" />
    </bpmn:boundaryEvent>
    <bpmn:boundaryEvent id="b2" attachedToRef="ut1">
      <bpmn:outgoing>f6</bpmn:outgoing><bpmn:messageEventDefinition messageRef="m" />
    </bpmn:boundaryEvent>
    <bpmn:userTask id="ut2"><bpmn:incoming>f2</bpmn:incoming><bpmn:outgoing>f3</bpmn:outgoing></bpmn:userTask>
    <bpmn:boundaryEvent id="b3" attachedToRef="ut2">
      <bpmn:outgoing>f7</bpmn:outgoing><bpmn:messageEventDefinition messageRef="m" />
    </bpmn:boundaryEvent>
    <bpmn:boundaryEvent id="b4" attachedToRef="ut2">
      <bpmn:outgoing>f8</bpmn:outgoing><bpmn:messageEventDefinition messageRef="m" />
    </bpmn:boundaryEvent>
    <bpmn:endEvent id="end"><bpmn:incoming>f3</bpmn:incoming></bpmn:endEvent>
    <bpmn:endEvent id="e1"><bpmn:incoming>f5</bpmn:incoming></bpmn:endEvent>
    <bpmn:endEvent id="e2"><bpmn:incoming>f6</bpmn:incoming></bpmn:endEvent>
    <bpmn:endEvent id="e3"><bpmn:incoming>f7</bpmn:incoming></bpmn:endEvent>
    <bpmn:endEvent id="e4"><bpmn:incoming>f8</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ut1" />
    <bpmn:sequenceFlow id="f2" sourceRef="ut1" targetRef="ut2" />
    <bpmn:sequenceFlow id="f3" sourceRef="ut2" targetRef="end" />
    <bpmn:sequenceFlow id="f5" sourceRef="b1" targetRef="e1" />
    <bpmn:sequenceFlow id="f6" sourceRef="b2" targetRef="e2" />
    <bpmn:sequenceFlow id="f7" sourceRef="b3" targetRef="e3" />
    <bpmn:sequenceFlow id="f8" sourceRef="b4" targetRef="e4" />
  </bpmn:process>
</bpmn:definitions>"#;

    /// Every group, not the first one found — and the groups stay *per host*:
    /// the two tasks run one after the other, so `b1`'s arm is never live
    /// beside `b3`'s and no diagnostic may claim it is.
    #[test]
    fn every_ambiguous_host_is_reported() {
        let d = ambiguity(
            TWO_AMBIGUOUS_HOSTS,
            &Bindings::new()
                .correlation("b1", "order.id")
                .correlation("b2", "order.id")
                .correlation("b3", "order.id")
                .correlation("b4", "order.id"),
        );
        let elements: Vec<&str> = d.iter().map(|d| d.element.as_str()).collect();
        assert_eq!(elements, vec!["b1", "b2", "b3", "b4"], "{d:?}");
        for (i, host_pair) in [("b1", "b2"), ("b3", "b4")].iter().enumerate() {
            let (first, second) = *host_pair;
            let text = &d[i * 2].message;
            assert_eq!(
                text,
                &d[i * 2 + 1].message,
                "one group, one sentence: {d:?}"
            );
            assert!(
                text.contains(&format!("'{first}' and '{second}' both catch")),
                "{text}"
            );
        }
    }

    /// Three arms on one host: the sentence is a list, not "'b1' and 'b2' and
    /// 'b3' … while both are live".
    #[test]
    fn three_ambiguous_arms_read_as_a_list() {
        let three = TWO_BOUNDARIES.replace(
            r#"<bpmn:endEvent id="end">"#,
            r#"<bpmn:boundaryEvent id="b3" attachedToRef="ut">
      <bpmn:outgoing>f5</bpmn:outgoing><bpmn:messageEventDefinition messageRef="m" />
    </bpmn:boundaryEvent>
    <bpmn:endEvent id="e3"><bpmn:incoming>f5</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="f5" sourceRef="b3" targetRef="e3" />
    <bpmn:endEvent id="end">"#,
        );
        let d = ambiguity(
            &three,
            &Bindings::new()
                .correlation("b1", "order.id")
                .correlation("b2", "order.id")
                .correlation("b3", "order.id"),
        );
        let elements: Vec<&str> = d.iter().map(|d| d.element.as_str()).collect();
        assert_eq!(elements, vec!["b1", "b2", "b3"], "{d:?}");
        assert!(
            d[0].message.contains("'b1', 'b2' and 'b3' all catch"),
            "{}",
            d[0].message
        );
    }

    /// The negative that makes the rule L2 rather than L1: the same message
    /// under *different* bindings resolves to different keys, both arms may
    /// legitimately be live, and only the manifest could ever have told.
    #[test]
    fn the_same_message_under_different_bindings_is_fine() {
        let c = checked(
            TWO_BOUNDARIES,
            &Bindings::new()
                .correlation("b1", "order.id")
                .correlation("b2", "order.replacementId"),
        );
        assert!(c.ok(), "{:?}", c.diagnostics);
    }

    /// A lint error stops the pipeline before compilation, so the verdict
    /// carries the linter's diagnostics and no topics.
    #[test]
    fn lint_errors_short_circuit() {
        let broken = MINIMAL.replace(r#"targetRef="end""#, r#"targetRef="nowhere""#);
        let c = checked(&broken, &Bindings::new());
        assert!(!c.ok());
        assert!(c.topics.is_empty());
    }
}
