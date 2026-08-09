//! rbpmn-model: BPMN parsing, validation and linting for the rbpmn engine.
//!
//! This crate is the engine's front door: it parses a BPMN 2.0 XML document
//! into an internal model and lints it against the supported subset. It is
//! deliberately dependency-light — no IO, no async runtime, no database — so
//! it can compile to WASM for the linter playground and back the published
//! bpmnlint plugin with the exact same rules and rule IDs.
//!
//! Guiding principle: it is better to loudly reject a model than to silently
//! execute it wrong.

#![forbid(unsafe_code)]

pub mod condition;
pub mod diagnostics;
pub mod iso8601;
mod lint;
pub mod model;
mod parse;

pub use diagnostics::{CATALOGUE, Diagnostic, RuleInfo, Severity, has_errors, rule};
pub use lint::lint;
pub use parse::{BPMN_NS, ParseError, RBPMN_NS, parse};

/// Result of [`check`]: the parsed model plus all diagnostics.
/// `ok` is false if any diagnostic has error severity — deploy must refuse.
#[derive(Debug)]
pub struct Checked {
    pub ok: bool,
    pub definitions: model::Definitions,
    pub diagnostics: Vec<Diagnostic>,
}

/// Parse and lint in one step. `Err` only for malformed XML / non-BPMN input;
/// everything else is reported through diagnostics.
pub fn check(xml: &str) -> Result<Checked, ParseError> {
    let definitions = parse(xml)?;
    let diagnostics = lint(&definitions);
    Ok(Checked {
        ok: !has_errors(&diagnostics),
        definitions,
        diagnostics,
    })
}
