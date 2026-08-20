//! The DMN half of the deploy verdict — as a trait, not a dependency.
//!
//! `rbpmn-core` must keep compiling to wasm32 and must keep its dependency
//! set (`rbpmn-model + serde + serde_json + thiserror`) exactly as it is:
//! the editor's L2 check, the playground and the bpmnlint plugin all ride on
//! that. dsntk lives in `rbpmn-dmn` and nothing upstream of it may depend on
//! it (`docs/dmn.md`, D1).
//!
//! So the core states what a DMN validator must answer and lets the caller
//! supply one. `Engine::deploy` passes `rbpmn_dmn`'s implementation and the
//! editor's WASM export passes **the same one**, which is what keeps
//! `just parity` meaningful: a surface that reports a verdict must report
//! *the* verdict, and two implementations of a rule are two verdicts waiting
//! to disagree.

use rbpmn_model::Diagnostic;

/// One thing a bundled DMN artifact offers a business-rule task: a decision
/// or decision service that can be invoked by name.
///
/// The triple is what dsntk needs to invoke it
/// (`evaluate_invocable(namespace, model_name, invocable)`), and it is what
/// P2's `unresolved-decision` compares a manifest binding against. Carried
/// through the verdict rather than recomputed, because parsing a DMN document
/// twice to answer two questions is how the two answers drift.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Invocable {
    /// The `namespace` attribute of the artifact that defines it.
    pub namespace: String,
    /// The artifact's model name.
    pub model: String,
    /// The invocable's own name, as a manifest would bind it.
    pub name: String,
}

/// What a validator answers about a set of DMN artifacts.
#[derive(Debug, Clone, Default)]
pub struct DecisionCheck {
    /// `dmn-validates`, `feel-parses`, `feel-deterministic`. Any error
    /// severity means deploy would reject.
    pub diagnostics: Vec<Diagnostic>,
    /// Every invocable the artifacts expose, empty when they did not compile.
    /// Sorted, so a caller may binary-search and a test may compare.
    pub invocables: Vec<Invocable>,
}

/// Validate DMN artifacts without knowing how.
///
/// Implemented by `rbpmn_dmn::Validator`. Deliberately takes the artifacts as
/// raw XML: the core has no DMN model type and must not acquire one.
pub trait DecisionValidator {
    fn check(&self, artifacts: &[String]) -> DecisionCheck;
}

/// The validator used when a build has no DMN support compiled in.
///
/// It **refuses** artifacts rather than ignoring them: a deployment that
/// bundles decisions this binary cannot validate is not a deployment that
/// "has no decisions", and quietly dropping them is precisely the
/// seems-to-run failure the manifest design exists to kill. With no artifacts
/// it is silent, so a build without DMN behaves exactly as before.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDecisions;

impl DecisionValidator for NoDecisions {
    fn check(&self, artifacts: &[String]) -> DecisionCheck {
        let diagnostics = if artifacts.is_empty() {
            Vec::new()
        } else {
            vec![Diagnostic::error(
                rbpmn_model::rule::DMN_VALIDATES,
                "definitions",
                format!(
                    "this build has no DMN support, so the {} bundled decision artifact(s) \
                     cannot be validated — rebuild with the `dmn` feature, or deploy \
                     without them",
                    artifacts.len()
                ),
            )]
        };
        DecisionCheck {
            diagnostics,
            invocables: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_decisions_is_silent_without_artifacts() {
        assert!(NoDecisions.check(&[]).diagnostics.is_empty());
    }

    /// The half that matters: artifacts a build cannot check are refused, not
    /// skipped.
    #[test]
    fn no_decisions_refuses_artifacts_it_cannot_check() {
        let check = NoDecisions.check(&["<definitions/>".to_string()]);
        assert_eq!(check.diagnostics.len(), 1);
        assert_eq!(check.diagnostics[0].rule, rbpmn_model::rule::DMN_VALIDATES);
        assert!(rbpmn_model::has_errors(&check.diagnostics));
    }
}
