//! DMN and full FEEL for rbpmn.
//!
//! **This is the only crate where dsntk is allowed**, and nothing upstream of
//! it may depend on it (`docs/dmn.md`, D1). `rbpmn-model` and `rbpmn-core`
//! keep their dependency sets exactly as they are, because the editor's L2
//! check, the playground and the bpmnlint plugin all ride on those staying
//! wasm-clean. What crosses the boundary is a trait — `DecisionValidator`,
//! defined in `rbpmn-core` and implemented here — so `Engine::deploy` and the
//! editor's WASM export can pass *the same* implementation and cannot reach
//! different verdicts.
//!
//! dsntk itself only reaches wasm32 because `[patch.crates-io]` substitutes
//! `crates/rbpmn-feel-number` for the C-backed decimal it publishes. That
//! substitution is verified by `just number-parity` and `just dmn-tck`; see
//! `docs/dmn.md` for what it cost.
//!
//! What P1 provides:
//!
//! * [`Validator`] — `dmn-validates`, `feel-parses`, `feel-deterministic`.
//! * [`value`] — the bridge between the variable document and FEEL values,
//!   including the distinction between "no rule matched" and "the decision
//!   failed", which dsntk reports as the same FEEL null.
//! * [`expressions`] and [`determinism`] — the walks those rest on, written
//!   so that a dsntk upgrade breaks the build rather than widening what rbpmn
//!   accepts.
//!
//! Evaluation itself is P3's, and it happens in `rbpmn-engine`, never in the
//! pure core: `step` parks at the business-rule task, the projection
//! evaluates inside the same transaction, and the result re-enters as command
//! data. That is what keeps replay working without an evaluator.

#![forbid(unsafe_code)]

pub mod determinism;
mod evaluate;
pub mod expressions;
mod validate;
pub mod value;

pub use evaluate::Decisions;
pub use validate::{Validator, check};
pub use value::Outcome;
