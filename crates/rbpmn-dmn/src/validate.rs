//! The deploy verdict for a set of DMN artifacts.
//!
//! Three rules, in the order that produces the most useful message:
//!
//! * `dmn-validates` — the document is DMN, and its decision logic builds.
//! * `feel-parses` — every FEEL expression in it parses.
//! * `feel-deterministic` — none of them reads a clock or calls out of the
//!   process.
//!
//! Building the evaluator is what `deploy` does, so it is what validation
//! does: `ModelEvaluator::new` parses every expression in the model **with the
//! scope the model defines**, which is the only way to parse FEEL correctly —
//! names may contain spaces and operators, so where one ends depends on which
//! names exist (see [`crate::determinism`] for what that cost the first
//! attempt).
//!
//! That makes dsntk the authority on `feel-parses`, and it reports one error
//! for the whole build. Our own walk runs afterwards, *only* to attribute
//! that error to an element: a re-parse without the model scope can be wrong
//! in both directions, so it is never allowed to cause a rejection — only to
//! locate one that already happened. When it cannot, the diagnostic names the
//! model and carries dsntk's message, which quotes the offending expression.

use crate::expressions::{self, Grammar};
use dsntk_feel::FeelScope;
use dsntk_model::{Definitions, NamedElement};
use dsntk_model_evaluator::ModelEvaluator;
use rbpmn_core::{DecisionCheck, DecisionValidator, Invocable};
use rbpmn_model::{Diagnostic, has_errors, rule};

/// The DMN half of the deploy verdict. `Engine::deploy` and the editor's WASM
/// export pass *this* — one implementation, so two surfaces cannot disagree.
#[derive(Debug, Clone, Copy, Default)]
pub struct Validator;

impl DecisionValidator for Validator {
    fn check(&self, artifacts: &[String]) -> DecisionCheck {
        check(artifacts)
    }
}

/// Validate DMN artifacts. Plain function so native tests and the WASM export
/// share it exactly, the way `lint_json` does for the linter.
pub fn check(artifacts: &[String]) -> DecisionCheck {
    let mut diagnostics = Vec::new();
    let mut parsed: Vec<Definitions> = Vec::new();

    for (index, artifact) in artifacts.iter().enumerate() {
        match dsntk_model::parse(artifact) {
            Ok(definitions) => {
                diagnostics.extend(expression_diagnostics(&definitions));
                parsed.push(definitions);
            }
            Err(e) => diagnostics.push(Diagnostic::error(
                rule::DMN_VALIDATES,
                artifact_label(artifact, index),
                format!("not a valid DMN document: {e}"),
            )),
        }
    }

    // Building the evaluators is the expensive half and the one whose errors
    // are least specific, so it only runs once the artifacts are known to be
    // parseable and their expressions sound. Reporting "decision logic failed
    // to build" on top of the syntax error that caused it helps nobody.
    if has_errors(&diagnostics) {
        return DecisionCheck {
            diagnostics,
            invocables: Vec::new(),
        };
    }

    match ModelEvaluator::new(&parsed) {
        Ok(evaluator) => {
            let mut invocables: Vec<Invocable> = evaluator
                .invocables()
                .list()
                .into_iter()
                .map(|(namespace, model, name)| Invocable {
                    namespace,
                    model,
                    name,
                })
                .collect();
            invocables.sort();
            DecisionCheck {
                diagnostics,
                invocables,
            }
        }
        Err(e) => {
            diagnostics.push(build_failure(&parsed, &e.to_string()));
            DecisionCheck {
                diagnostics,
                invocables: Vec::new(),
            }
        }
    }
}

/// Classify the one error `ModelEvaluator::new` gives us, and locate it.
///
/// dsntk tags its errors in the message — `<ParserError>` for FEEL that does
/// not parse, `<ModelParserError>` and friends for a model that does not hang
/// together. Keying the *rule id* off that text is a soft spot, and it is
/// bounded on purpose: both classifications reject the deploy, so the worst a
/// wrong guess does is name the less apt rule. The fixture corpus pins both
/// (`20-feel-syntax-error`, `28-dangling-reference`), which is what would
/// catch an upstream wording change.
fn build_failure(parsed: &[Definitions], message: &str) -> Diagnostic {
    if !message.contains("ParserError") {
        return Diagnostic::error(
            rule::DMN_VALIDATES,
            model_label(parsed),
            format!("decision logic does not build: {message}"),
        );
    }
    // A FEEL syntax error. Try to say *where*: re-parse each expression and
    // report the first that also fails. This can only narrow the location of
    // a rejection dsntk already decided on.
    let scope = FeelScope::default();
    for definitions in parsed {
        for expression in expressions::collect(definitions).expressions {
            let reparsed = match expression.grammar {
                Grammar::Expression => {
                    dsntk_feel_parser::parse_expression(&scope, &expression.text, false)
                }
                Grammar::UnaryTests => {
                    dsntk_feel_parser::parse_unary_tests(&scope, &expression.text, false)
                }
            };
            if reparsed.is_err() {
                return Diagnostic::error(
                    rule::FEEL_PARSES,
                    &expression.element,
                    format!("{} does not parse: {message}", expression.slot),
                );
            }
        }
    }
    Diagnostic::error(rule::FEEL_PARSES, model_label(parsed), message.to_string())
}

/// `feel-deterministic`, per expression.
fn expression_diagnostics(definitions: &Definitions) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let found = expressions::collect(definitions);

    for external in &found.externals {
        diagnostics.push(Diagnostic::error(
            rule::FEEL_DETERMINISTIC,
            &external.element,
            format!(
                "external {} function: a decision must be self-contained, and dsntk \
                 evaluates these by calling out of the process — move the logic into \
                 the model, or compute it in application code and pass the result in \
                 as an input",
                external.kind
            ),
        ));
    }

    for expression in &found.expressions {
        for name in crate::determinism::banned_calls(&expression.text) {
            diagnostics.push(Diagnostic::error(
                rule::FEEL_DETERMINISTIC,
                &expression.element,
                format!(
                    "{} calls {name}(): decisions must be deterministic, so the same \
                     inputs replay to the same answer and a retry cannot change one — \
                     and dsntk answers {name}() from the *node's* local timezone. Pass \
                     the time in as an input instead.",
                    expression.slot
                ),
            ));
        }
    }
    diagnostics
}

/// Something to point at when the document did not parse and therefore has no
/// elements. The `name` attribute is the modeler's own label and survives a
/// document too broken to build a model from; failing that, its position in
/// the bundle.
fn artifact_label(artifact: &str, index: usize) -> String {
    artifact
        .split_once("name=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(name, _)| name.to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("decision artifact {}", index + 1))
}

/// The models did parse, so name them rather than their position.
fn model_label(parsed: &[Definitions]) -> String {
    parsed
        .first()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|| "decision artifacts".to_string())
}
