//! What a decision actually answers, pinned from the outside.
//!
//! `docs/dmn.md` set this crate one job before any other: establish whether
//! "no rule matched" and "your decision blew up" are distinguishable, because
//! if they are not, an incomplete decision table becomes an incident or an
//! incident becomes an ordinary branch.
//!
//! **They are not.** These fixtures measured it, and the answer shaped the
//! `Outcome` type: every null dsntk produces carries a reason, and the *same*
//! reason covers a legal gap and a type error. The tests below are what keeps
//! that finding true — if a dsntk upgrade ever separates the two, they fail
//! and the ruling can be revisited on evidence.

use rbpmn_dmn::{Decisions, Outcome};
use serde_json::json;
use std::fs;
use std::path::Path;

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn decisions(name: &str) -> Decisions {
    Decisions::compile(&[fixture(name)]).unwrap_or_else(|d| panic!("{name} should compile: {d:?}"))
}

fn invocable(decisions: &Decisions, name: &str) -> rbpmn_core::Invocable {
    decisions
        .invocables()
        .iter()
        .find(|i| i.name == name)
        .unwrap_or_else(|| panic!("no invocable named {name} in {:?}", decisions.invocables()))
        .clone()
}

/// A grade table covering 80 and up answers normally where it has rules.
#[test]
fn a_matching_rule_answers_with_its_output() {
    let decisions = decisions("accept/07-incomplete-table.dmn");
    let grade = invocable(&decisions, "Grade");

    assert_eq!(
        decisions.evaluate(&grade, &json!({ "Score": 95 })),
        Outcome::Value(json!("A"))
    );
    assert_eq!(
        decisions.evaluate(&grade, &json!({ "Score": 85 })),
        Outcome::Value(json!("B"))
    );
}

/// **The finding.** A legal gap in an incomplete table, a wrong input type, a
/// missing input and an explicit null all produce the same shape — a null
/// carrying a reason — and the first three produce the *identical* reason.
///
/// So nothing here can tell "the model says nothing applies" from "the input
/// was wrong". Any code that branched on the reason would be guessing, which
/// is why `Outcome` exposes it as text and P3 treats a null as an answer.
#[test]
fn every_null_carries_a_reason_and_they_are_not_distinguishable() {
    let decisions = decisions("accept/07-incomplete-table.dmn");
    let grade = invocable(&decisions, "Grade");

    let mut reasons = Vec::new();
    for input in [
        json!({ "Score": 12 }),             // a legal gap: no rule covers it
        json!({ "Score": "not a number" }), // a type error
        json!({}),                          // the input is not there at all
        json!({ "Score": null }),
    ] {
        let Outcome::Null { reason } = decisions.evaluate(&grade, &input) else {
            panic!("expected a null answer for {input}");
        };
        reasons.push(reason.expect("dsntk attaches a reason to every null"));
    }

    assert!(
        reasons.windows(2).all(|w| w[0] == w[1]),
        "if these ever differ, the distinction is back and the ruling in \
         docs/dmn.md can be revisited: {reasons:?}"
    );
    assert!(reasons[0].contains("no rules matched"), "{}", reasons[0]);
}

/// A different *kind* of failure does get a different reason — the text is
/// informative, it just is not a signal.
#[test]
fn a_reason_says_something_useful_even_though_it_decides_nothing() {
    let decisions = decisions("accept/01-literal-expression.dmn");
    let greeting = invocable(&decisions, "Greeting");

    assert_eq!(
        decisions.evaluate(&greeting, &json!({ "Applicant": "Ada" })),
        Outcome::Value(json!("Hello Ada"))
    );
    let Outcome::Null { reason } = decisions.evaluate(&greeting, &json!({ "Applicant": 42 }))
    else {
        panic!("a string concatenation with a number should not produce a value");
    };
    assert!(
        reason.is_some_and(|r| r.contains("string")),
        "the reason should describe the type mismatch"
    );
}

/// Naming a decision that does not exist is *also* just a null with a reason
/// — which is precisely why deploy has to be the thing that prevents it
/// (`unresolved-decision`, P2) rather than anything observed at runtime.
#[test]
fn an_unknown_invocable_is_indistinguishable_at_runtime_too() {
    let decisions = decisions("accept/07-incomplete-table.dmn");
    let missing = rbpmn_core::Invocable {
        namespace: "https://rbpmn.example/07-incomplete-table".to_string(),
        model: "07-incomplete-table".to_string(),
        name: "NoSuchDecision".to_string(),
    };
    let Outcome::Null { reason } = decisions.evaluate(&missing, &json!({ "Score": 95 })) else {
        panic!("expected a null");
    };
    assert!(
        reason.is_some_and(|r| r.contains("NoSuchDecision")),
        "the reason should at least name it"
    );
}

/// The input document is the instance's variables and the engine never
/// interprets it — but it must be an object, because FEEL addresses inputs by
/// name.
#[test]
fn a_non_object_input_is_refused_with_a_reason() {
    let decisions = decisions("accept/07-incomplete-table.dmn");
    let grade = invocable(&decisions, "Grade");
    let Outcome::Null { reason } = decisions.evaluate(&grade, &json!([1, 2, 3])) else {
        panic!("a non-object input must not produce a value");
    };
    assert!(reason.is_some_and(|r| r.contains("object")));
}

/// Precision is the reason `arbitrary_precision` went in: a decision that
/// computes on a 30-digit number must not hand back an `f64`.
#[test]
fn a_decision_answers_in_full_precision() {
    let decisions = decisions("accept/02-decision-table.dmn");
    let discount = invocable(&decisions, "Discount");

    let big: serde_json::Value =
        serde_json::from_str(r#"{ "Amount": 123456789012345678901234567890 }"#).unwrap();
    let Outcome::Value(answer) = decisions.evaluate(&discount, &big) else {
        panic!("should have produced a value");
    };
    // Amount * 0.1, exactly — not 1.2345678901234568e+28.
    assert_eq!(answer.to_string(), "12345678901234567890123456789");
}

/// Every accepted fixture must actually be invocable end to end. A model that
/// validates but cannot be called is not a model rbpmn can deploy.
#[test]
fn every_accepted_fixture_compiles_and_exposes_an_invocable() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/accept");
    let mut checked = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "dmn") {
            continue;
        }
        let xml = fs::read_to_string(&path).unwrap();
        let compiled = Decisions::compile(&[xml])
            .unwrap_or_else(|d| panic!("{} should compile: {d:?}", path.display()));
        assert!(
            !compiled.invocables().is_empty(),
            "{} exposes nothing to bind to",
            path.display()
        );
        checked += 1;
    }
    assert!(checked >= 8, "expected the accept corpus, found {checked}");
}

/// What actually makes an input visible to a FEEL expression — measured,
/// because getting it wrong is silent.
///
/// This is the trap the editor's starter decision, the demo fixture and
/// `docs/dmn.md` all point at from different directions, and the one users
/// hit first. A decision reads `risk.score`; `risk` resolves to null unless
/// the model says the decision *requires* it, and `null > 10` is null, so the
/// `if` takes its else branch and answers something plausible and wrong. No
/// diagnostic anywhere: `feel-parses` checks that an expression parses, not
/// that its names resolve.
///
/// Three separate things have to be true, and each one alone is not enough.
/// Pinned here so a dsntk upgrade that changes the resolution rules fails a
/// test rather than quietly changing what every bundled decision answers.
#[test]
fn an_input_is_visible_only_when_required_and_typed_for_its_shape() {
    fn band(inputs: &str, requirements: &str) -> Outcome {
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<definitions xmlns="https://www.omg.org/spec/DMN/20191111/MODEL/"
             namespace="https://rbpmn.dev/inputs" name="Inputs" id="_inputs">
  {inputs}
  <decision id="d" name="Band">
    <variable name="Band" typeRef="string" />
    {requirements}
    <literalExpression id="e">
      <text>if risk.score &gt; 10 then "seen" else "blind"</text>
    </literalExpression>
  </decision>
</definitions>
"#
        );
        let decisions = Decisions::compile(&[xml]).expect("should compile");
        let band = invocable(&decisions, "Band");
        decisions.evaluate(&band, &json!({ "risk": { "score": 82 } }))
    }

    const REQUIRES: &str = r##"<informationRequirement id="r">
        <requiredInput href="#in_risk" /></informationRequirement>"##;
    const DECLARED: &str =
        r#"<inputData id="in_risk" name="risk"><variable name="risk" typeRef="Any" /></inputData>"#;

    // Everything present: the name resolves and the expression sees its input.
    assert_eq!(band(DECLARED, REQUIRES), Outcome::Value(json!("seen")));

    // No `inputData` at all — and note this is a *value*, not an error. The
    // whole variable document is passed in, so the data is right there; the
    // model simply never said it wanted it.
    assert_eq!(band("", ""), Outcome::Value(json!("blind")));

    // Declared but not wired. The `informationRequirement` is the binding, not
    // the declaration — an `inputData` sitting unconnected on the DRD is
    // decoration, which is exactly what it looks like on the canvas.
    assert_eq!(band(DECLARED, ""), Outcome::Value(json!("blind")));

    // Wired but typed as a number, while the data is a context. dmn-js
    // defaults new input data to `Any` and the type-ref dropdown offers eight
    // scalar types beside it, so this is one click away at all times.
    let mistyped = r#"<inputData id="in_risk" name="risk"><variable name="risk" typeRef="number" /></inputData>"#;
    assert_eq!(band(mistyped, REQUIRES), Outcome::Value(json!("blind")));

    // The `<variable>` is what names it, not the `inputData`. dmn-js keeps the
    // two in step when you rename on the canvas; a hand-written file need not.
    let renamed = r#"<inputData id="in_risk" name="Risk data">
        <variable name="risk" typeRef="Any" /></inputData>"#;
    assert_eq!(band(renamed, REQUIRES), Outcome::Value(json!("seen")));
}
