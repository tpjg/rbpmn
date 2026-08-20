//! The bridge between the variable document and FEEL values.
//!
//! Both directions are lossy in ways worth naming, because this is where a
//! decision's answer becomes business data and stays there.
//!
//! **In:** JSON numbers cross as *text*, never as `f64`. The variable document
//! now holds arbitrary-precision numbers (see `docs/dmn.md`, the
//! `arbitrary_precision` spike) and FEEL numbers are decimal128, so going
//! through `f64` would throw away digits both sides can represent.
//!
//! **Out:** FEEL has types JSON does not — dates, times, durations, ranges,
//! functions. Dates and durations become their canonical strings, which is
//! lossy in *type* but not in value and is what every other DMN engine does.
//! Everything else is refused rather than approximated.
//!
//! A note on the wildcard at the bottom of `to_json`: unlike the walks in
//! `expressions` and `determinism`, where a missed variant would silently
//! *allow* something, a missed variant here is refused. Defaulting to refusal
//! is safe, so it does not need the compiler standing over it.

use dsntk_feel::context::FeelContext;
use dsntk_feel::values::Value as FeelValue;
use dsntk_feel::{FeelNumber, Name};
use serde_json::Value as Json;
use std::str::FromStr;

/// What came back from evaluating a decision.
///
/// **There are two null cases in DMN and dsntk does not distinguish them.**
/// `docs/dmn.md` assumed it did — that a legitimate "no rule matched" would
/// be a bare null and a failure would carry a reason — and the fixtures in
/// `tests/outcomes.rs` measured the opposite:
///
/// ```text
/// incomplete UNIQUE table, no rule matches -> Null("no rules matched, no output value defined")
/// wrong input type for the same table      -> Null("no rules matched, no output value defined")
/// a literal expression given a bad input   -> Null("expected string as a second argument in addition")
/// ```
///
/// Every null carries a reason, the *same* reason covers a legal gap and a
/// type error, and there is no bare null to be found. So the reason is
/// **diagnostic text, not a failure signal**, and this type does not pretend
/// otherwise: deciding from it which nulls are incidents would freeze an
/// instance on a perfectly ordinary incomplete decision table.
///
/// What P3 does with that is recorded in `docs/dmn.md`: a null is an answer,
/// the reason is carried into the event trace, and wiring that is genuinely
/// broken is refused at deploy where this project puts that class of error.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A value the variable document can hold.
    Value(Json),
    /// FEEL null: the decision's answer is "nothing". `reason` is dsntk's
    /// explanation when it gave one — for the history, not for a branch.
    Null { reason: Option<String> },
    /// The decision produced a value with no representation in the variable
    /// document — a function, a range. Unlike a null this *is* unambiguous,
    /// and silently dropping a decision's answer would be worse than
    /// refusing it.
    Unrepresentable(String),
}

/// Variable document -> FEEL input context.
///
/// Only an object can be a decision's input: FEEL addresses inputs by name.
pub fn to_context(variables: &Json) -> Result<FeelContext, String> {
    let Some(object) = variables.as_object() else {
        return Err(format!(
            "decision input must be an object, got {}",
            describe(variables)
        ));
    };
    let mut context = FeelContext::default();
    for (key, value) in object {
        context.set_entry(&Name::from(key.as_str()), to_feel(value));
    }
    Ok(context)
}

/// One JSON value -> one FEEL value.
pub fn to_feel(value: &Json) -> FeelValue {
    match value {
        Json::Null => FeelValue::Null(None),
        Json::Bool(b) => FeelValue::Boolean(*b),
        // Through the text, not through f64: `to_string` on an
        // arbitrary-precision number is the literal the application wrote.
        Json::Number(n) => match FeelNumber::from_str(&n.to_string()) {
            Ok(number) => FeelValue::Number(number),
            Err(e) => FeelValue::Null(Some(format!("{n} is not a FEEL number: {e}"))),
        },
        Json::String(s) => FeelValue::String(s.clone()),
        Json::Array(items) => FeelValue::List(items.iter().map(to_feel).collect()),
        Json::Object(entries) => {
            let mut context = FeelContext::default();
            for (key, value) in entries {
                context.set_entry(&Name::from(key.as_str()), to_feel(value));
            }
            FeelValue::Context(context)
        }
    }
}

/// A decision's answer -> the variable document.
pub fn to_outcome(value: &FeelValue) -> Outcome {
    match value {
        FeelValue::Null(reason) => Outcome::Null {
            reason: reason.clone(),
        },
        other => match to_json(other) {
            Ok(json) => Outcome::Value(json),
            Err(e) => Outcome::Unrepresentable(e),
        },
    }
}

/// One FEEL value -> one JSON value, or why it cannot be one.
pub fn to_json(value: &FeelValue) -> Result<Json, String> {
    match value {
        FeelValue::Null(_) => Ok(Json::Null),
        FeelValue::Boolean(b) => Ok(Json::Bool(*b)),
        // Through the text again, so a 34-digit decimal survives.
        FeelValue::Number(n) => serde_json::from_str(&n.to_string())
            .map_err(|e| format!("number {n} is not representable in JSON: {e}")),
        FeelValue::String(s) => Ok(Json::String(s.clone())),
        FeelValue::List(items) => items
            .iter()
            .map(to_json)
            .collect::<Result<_, _>>()
            .map(Json::Array),
        FeelValue::Context(context) => {
            let mut object = serde_json::Map::new();
            for key in context.keys() {
                let Some(entry) = context.get_entry(key) else {
                    continue;
                };
                object.insert(key.to_string(), to_json(entry)?);
            }
            Ok(Json::Object(object))
        }
        // Temporal values keep their canonical FEEL spelling. Lossy in type,
        // exact in value, and it round-trips back through `date("…")`.
        FeelValue::Date(v) => Ok(Json::String(v.to_string())),
        FeelValue::Time(v) => Ok(Json::String(v.to_string())),
        FeelValue::DateTime(v) => Ok(Json::String(v.to_string())),
        FeelValue::DaysAndTimeDuration(v) => Ok(Json::String(v.to_string())),
        FeelValue::YearsAndMonthsDuration(v) => Ok(Json::String(v.to_string())),
        other => Err(format!(
            "a decision returned {}, which the variable document cannot hold",
            describe_feel(other)
        )),
    }
}

fn describe(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "an array",
        Json::Object(_) => "an object",
    }
}

fn describe_feel(value: &FeelValue) -> String {
    match value {
        FeelValue::Range(..) => "a range".to_string(),
        FeelValue::FunctionDefinition(..) | FeelValue::BuiltInFunction(_) => {
            "a function".to_string()
        }
        FeelValue::ExternalJavaFunction(..) | FeelValue::ExternalPmmlFunction(..) => {
            "an external function".to_string()
        }
        FeelValue::FeelType(t) => format!("the type `{t}`"),
        other => format!("`{other}`"),
    }
}
