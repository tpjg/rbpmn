//! The value bridge, given hostile input.
//!
//! This is where the application's variable document becomes a decision's
//! input and a decision's answer becomes business data. Both directions are
//! lossy in principle, so the losses get pinned rather than discovered: what
//! survives exactly, what changes shape, and what is refused outright.
//!
//! The same treatment `rbpmn-ui`'s `documents.rs` gives to escaping, and for
//! the same reason — one mistake here lands in everybody's data.

use rbpmn_dmn::value::{to_feel, to_json};
use serde_json::{Value, json};

/// JSON in, FEEL, JSON out — the round trip a decision's inputs make when the
/// model just passes a value through.
fn round_trip(text: &str) -> String {
    let json: Value = serde_json::from_str(text).expect("test input should be JSON");
    to_json(&to_feel(&json))
        .unwrap_or_else(|e| panic!("`{text}` should survive the bridge: {e}"))
        .to_string()
}

#[test]
fn scalars_survive_exactly() {
    for text in ["null", "true", "false", "0", "-1", "\"\"", "\"hello\""] {
        assert_eq!(round_trip(text), text, "{text}");
    }
}

/// The whole reason numbers cross as text: `f64` would quietly round these.
#[test]
fn numbers_wider_than_f64_survive_exactly() {
    for text in [
        "0.3333333333333333333333333333333333",
        "123456789012345678901234567890",
        "9999999999999999999999999999999999",
        "1.50",
        "-0.0000000000000000000000000000001",
    ] {
        assert_eq!(round_trip(text), text, "{text}");
    }
}

#[test]
fn structures_survive_including_the_awkward_keys() {
    for text in [
        r#"{}"#,
        r#"[]"#,
        r#"[1,2,3]"#,
        r#"{"a":{"b":{"c":[1,{"d":null}]}}}"#,
        // Keys FEEL names would not normally allow. The document is the
        // application's and the bridge does not get to rename anything.
        r#"{"with space":1}"#,
        r#"{"üñïçøde":1}"#,
        r#"{"":1}"#,
    ] {
        assert_eq!(round_trip(text), text, "{text}");
    }
}

/// Strings are data, never markup and never code. The inspector inlines this
/// document into HTML, so anything that survives here is escaped there — but
/// it must survive *unchanged* first.
#[test]
fn hostile_strings_are_carried_not_interpreted() {
    for text in [
        r#"{"a":"</script><script>alert(1)</script>"}"#,
        r#"{"a":"<!-- -->"}"#,
        r#"{"a":"  "}"#,
        r#"{"a":"now()"}"#,
        r#"{"a":"\" or 1=1 --"}"#,
        r#"{"a":"line\nbreak\ttab"}"#,
        r#"{"a":"\\"}"#,
    ] {
        let json: Value = serde_json::from_str(text).unwrap();
        let back = to_json(&to_feel(&json)).unwrap();
        assert_eq!(back, json, "{text}");
    }
}

/// A decision's answer can be a temporal value, which JSON has no type for.
/// It becomes its canonical FEEL spelling — lossy in type, exact in value,
/// and it reads back through `date("…")`.
#[test]
fn temporal_answers_become_their_canonical_text() {
    use dsntk_feel::values::Value as FeelValue;
    use std::str::FromStr;

    let date = FeelValue::Date(dsntk_feel_temporal::FeelDate::from_str("2026-08-14").unwrap());
    assert_eq!(to_json(&date).unwrap(), json!("2026-08-14"));
}

/// What the variable document cannot hold is refused, not approximated.
/// Dropping a decision's answer silently would be worse than saying so.
#[test]
fn unrepresentable_answers_are_refused_with_a_reason() {
    use dsntk_feel::FeelNumber;
    use dsntk_feel::values::Value as FeelValue;

    let range = FeelValue::Range(
        Box::new(FeelValue::Number(FeelNumber::from(1))),
        dsntk_feel::IntervalType::Closed,
        Box::new(FeelValue::Number(FeelNumber::from(10))),
        dsntk_feel::IntervalType::Closed,
    );
    let error = to_json(&range).expect_err("a range is not JSON");
    assert!(error.contains("range"), "{error}");

    // A list of representable values is fine, though.
    let list = FeelValue::List(vec![
        FeelValue::Number(FeelNumber::from(1)),
        FeelValue::String("two".into()),
    ]);
    assert_eq!(to_json(&list).unwrap(), json!([1, "two"]));
}

/// A nested unrepresentable value must not be smuggled through inside a
/// container that *is* representable.
#[test]
fn an_unrepresentable_value_inside_a_list_still_refuses() {
    use dsntk_feel::FeelNumber;
    use dsntk_feel::values::Value as FeelValue;

    let list = FeelValue::List(vec![
        FeelValue::Number(FeelNumber::from(1)),
        FeelValue::Range(
            Box::new(FeelValue::Number(FeelNumber::from(1))),
            dsntk_feel::IntervalType::Closed,
            Box::new(FeelValue::Number(FeelNumber::from(2))),
            dsntk_feel::IntervalType::Closed,
        ),
    ]);
    assert!(to_json(&list).is_err(), "a bad element must fail the list");
}
