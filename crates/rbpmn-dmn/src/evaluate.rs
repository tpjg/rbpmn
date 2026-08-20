//! Evaluating a decision.
//!
//! [`Decisions`] is what P3 caches per definition version: building the
//! evaluators parses every expression in every artifact, which is far too
//! expensive to repeat per instance, and a deployed version is immutable so
//! the cache never needs invalidating.
//!
//! It lives here rather than in `rbpmn-engine` for the same reason the
//! validator does — this is the crate where dsntk is allowed. And it is
//! called from the *projection*, never from `step`: the pure core parks at
//! the business-rule task, the engine evaluates inside the same transaction,
//! and the result re-enters as command data. That is what lets a history
//! replay without an evaluator at all.

use crate::value::{self, Outcome};
use dsntk_model_evaluator::ModelEvaluator;
use rbpmn_core::Invocable;
use rbpmn_model::Diagnostic;
use serde_json::Value as Json;
use std::sync::Arc;

/// A compiled set of DMN artifacts, ready to answer.
#[derive(Clone)]
pub struct Decisions {
    evaluator: Arc<ModelEvaluator>,
    invocables: Vec<Invocable>,
}

impl std::fmt::Debug for Decisions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decisions")
            .field("invocables", &self.invocables.len())
            .finish()
    }
}

impl Decisions {
    /// Compile artifacts that have already been validated.
    ///
    /// Returns the same diagnostics [`crate::check`] would, so a caller that
    /// skipped validation still cannot proceed on a broken model — there is
    /// no path here that yields a half-working evaluator.
    pub fn compile(artifacts: &[String]) -> Result<Self, Vec<Diagnostic>> {
        let check = crate::check(artifacts);
        if rbpmn_model::has_errors(&check.diagnostics) {
            return Err(check.diagnostics);
        }
        let parsed: Result<Vec<_>, _> = artifacts.iter().map(|a| dsntk_model::parse(a)).collect();
        let parsed = parsed.map_err(|e| {
            vec![Diagnostic::error(
                rbpmn_model::rule::DMN_VALIDATES,
                "decision artifacts",
                e.to_string(),
            )]
        })?;
        let evaluator = ModelEvaluator::new(&parsed).map_err(|e| {
            vec![Diagnostic::error(
                rbpmn_model::rule::DMN_VALIDATES,
                "decision artifacts",
                e.to_string(),
            )]
        })?;
        Ok(Self {
            evaluator,
            invocables: check.invocables,
        })
    }

    /// What a business-rule task can bind to.
    pub fn invocables(&self) -> &[Invocable] {
        &self.invocables
    }

    /// Evaluate one decision against a variable document.
    ///
    /// `input` is the instance's variables: FEEL addresses inputs by name, so
    /// the whole document goes in and the model's `InputData` picks what it
    /// declared. The engine never interprets the document, here or anywhere.
    pub fn evaluate(&self, invocable: &Invocable, input: &Json) -> Outcome {
        let context = match value::to_context(input) {
            Ok(context) => context,
            Err(e) => return Outcome::Null { reason: Some(e) },
        };
        // An unknown invocable comes back as a null carrying a reason — the
        // same shape a legal empty answer has, which is why deploy's
        // `unresolved-decision` (P2) is what actually prevents it rather than
        // anything inspectable here.
        let answer = self.evaluator.evaluate_invocable(
            &invocable.namespace,
            &invocable.model,
            &invocable.name,
            &context,
        );
        value::to_outcome(&answer)
    }
}
