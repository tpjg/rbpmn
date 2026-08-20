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
