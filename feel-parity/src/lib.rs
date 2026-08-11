//! Bridge between the two evaluators. The check itself is `tests/parity.rs`.

use dsntk_feel::context::FeelContext;
use dsntk_feel::values::Value as FeelValue;
use dsntk_feel::{FeelNumber, FeelScope, Name};
use dsntk_feel_evaluator::prepare;
use dsntk_feel_parser::parse_expression;
use serde_json::Value;
use std::str::FromStr;

/// What an evaluator answered, in the only vocabulary both share.
#[derive(Debug, PartialEq)]
pub enum Answer {
    /// A definite boolean.
    Bool(bool),
    /// FEEL's null — "unknown". Collapses to false at the root of a condition.
    Null,
    /// Refused the expression (parse error, or a result outside the subset).
    Refused(String),
}

impl Answer {
    /// The root collapse: a sequence-flow condition is taken iff the
    /// expression evaluates to exactly `true`.
    pub fn taken(&self) -> Option<bool> {
        match self {
            Answer::Bool(b) => Some(*b),
            Answer::Null => Some(false),
            Answer::Refused(_) => None,
        }
    }
}

fn json_to_feel(value: &Value) -> FeelValue {
    match value {
        Value::Null => FeelValue::Null(None),
        Value::Bool(b) => FeelValue::Boolean(*b),
        // Via the string form: FEEL numbers are decimal128, so going through
        // f64 would introduce a difference this check exists to detect.
        Value::Number(n) => match FeelNumber::from_str(&n.to_string()) {
            Ok(n) => FeelValue::Number(n),
            Err(e) => FeelValue::Null(Some(format!("{e}"))),
        },
        Value::String(s) => FeelValue::String(s.clone()),
        Value::Object(map) => {
            let mut ctx = FeelContext::new();
            for (k, v) in map {
                ctx.set_entry(&Name::from(k.as_str()), json_to_feel(v));
            }
            FeelValue::Context(ctx)
        }
        Value::Array(_) => FeelValue::Null(Some("arrays are outside the subset".into())),
    }
}

/// Evaluate with dsntk — the TCK-verified reference.
pub fn dsntk(src: &str, variables: &Value) -> Answer {
    let scope = FeelScope::default();
    if let Some(obj) = variables.as_object() {
        for (k, v) in obj {
            scope.set_value(&Name::from(k.as_str()), json_to_feel(v));
        }
    }
    let node = match parse_expression(&scope, src, false) {
        Ok(node) => node,
        Err(e) => return Answer::Refused(format!("parse: {e}")),
    };
    match prepare(&node)(&scope) {
        FeelValue::Boolean(b) => Answer::Bool(b),
        FeelValue::Null(_) => Answer::Null,
        other => Answer::Refused(format!("non-boolean result: {other}")),
    }
}

/// Evaluate with ours. `eval` already applies the root collapse, so a `false`
/// here is indistinguishable from a null — which is exactly the contract:
/// what must match is whether the flow is taken.
pub fn rbpmn(src: &str, variables: &Value) -> Answer {
    match rbpmn_model::condition::parse(src) {
        Ok(expr) => Answer::Bool(rbpmn_model::condition::eval(&expr, variables)),
        Err(e) => Answer::Refused(format!("parse: {e}")),
    }
}
