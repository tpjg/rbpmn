//! Gate 0b: does the dsntk DMN stack *work* inside a wasm VM, not merely
//! link?
//!
//! Throwaway scaffolding, superseded by `crates/rbpmn-dmn` at P1. The
//! exported entry points take no arguments and return integers, so the module
//! can be driven by a bare `WebAssembly.instantiate` with no bindgen glue
//! between the runtime and the thing being proven.

use dsntk_feel::context::FeelContext;
use dsntk_feel::values::Value;
use dsntk_feel::{FeelNumber, Name};
use dsntk_model_evaluator::ModelEvaluator;
use std::sync::Arc;
use wasm_bindgen::prelude::*;

const DECISION: &str = include_str!("../fixtures/decision.dmn");

fn build() -> Result<Arc<ModelEvaluator>, String> {
    let definitions = dsntk_model::parse(DECISION).map_err(|e| e.to_string())?;
    ModelEvaluator::new(&[definitions]).map_err(|e| e.to_string())
}

/// Parse the DMN document and build its evaluators — the deploy-time check.
/// Returns the number of invocables, or a negative code.
#[wasm_bindgen]
pub fn compile() -> i32 {
    match build() {
        Ok(evaluator) => evaluator.invocables().len() as i32,
        Err(_) => -1,
    }
}

/// Evaluate the decision table for an input amount and return the discount,
/// scaled by 100 so an integer can carry it across the boundary.
///
/// This is the real proof: a decision table's unary tests, FEEL arithmetic
/// and the substituted decimal type, all executing in WebAssembly.
#[wasm_bindgen]
pub fn evaluate(amount: i32) -> i32 {
    let Ok(evaluator) = build() else { return -1 };
    let mut input = FeelContext::default();
    input.set_entry(&Name::from("Amount"), Value::Number(FeelNumber::from(amount)));
    // `evaluate_invocable` is the public entry point — `DecisionEvaluator`
    // takes a `DefKey`, which the crate does not export. Worth knowing at P1.
    match evaluator.evaluate_invocable("https://rbpmn.example", "gate0", "Discount", &input) {
        Value::Number(n) => i32::try_from(n * FeelNumber::from(100)).unwrap_or(-3),
        _ => -4,
    }
}

/// The diagnostics path P1 actually needs: a *bad* document must come back
/// with the parser's complaint, not a panic.
#[wasm_bindgen]
pub fn compile_error(dmn: &str) -> String {
    match dsntk_model::parse(dmn).map_err(|e| e.to_string()).and_then(|d| {
        ModelEvaluator::new(&[d]).map(|_| "ok".to_string()).map_err(|e| e.to_string())
    }) {
        Ok(s) => s,
        Err(e) => e,
    }
}
