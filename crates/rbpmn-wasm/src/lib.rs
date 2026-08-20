//! WASM surface for the rbpmn front door. One source of truth: the
//! playground, the editor and the bpmnlint plugin call this — never a JS
//! reimplementation of the rules.
//!
//! Three exports for three questions, plus [`catalogue`] — the rule ids and
//! their severities, so a surface can render a legend without hard-coding a
//! list that would drift from the one the rules actually use:
//!
//! * [`lint`] — *is the model legal?* Model only, no manifest. This is what
//!   bpmnlint asks, and what the playground shows.
//! * [`check_deployable`] — *would this deploy?* Model, bindings manifest
//!   **and any bundled DMN artifacts**, i.e. everything `Engine::deploy`
//!   decides without a database. The one remaining deploy step, the
//!   environment link (`unresolved-topic`), needs registration state, so the
//!   resolved topics are handed back for the caller to check against a
//!   covered-topic set it fetched. Decisions need no such round trip: the
//!   artifacts travel inside the deployment, so the verdict on them is
//!   complete offline — a confidential decision table can be validated
//!   against nothing but this module.
//! * [`evaluate_decision`] — *what does this decision answer?* For the
//!   editor's try-it pane. Only present with the `dmn` feature.
//!
//! JSON strings cross the boundary (identical serialization on both sides of
//! the parity check):
//!
//! ```json
//! { "ok": bool, "parseError": string|null,
//!   "diagnostics": [{ "rule", "element", "message", "severity" }] }
//! ```
//!
//! ```json
//! { "ok": bool, "parseError": string|null, "bindingsError": string|null,
//!   "processCount": number|null, "key": string|null,
//!   "diagnostics": [ ... ], "topics": { "<elementId>": "<topic>" } }
//! ```

use rbpmn_core::{Bindings, DecisionValidator, DeployCheck, check_deployable as core_check};
use std::collections::BTreeMap;
use wasm_bindgen::prelude::*;

/// The DMN validator this build carries.
///
/// With the `dmn` feature it is the real one; without it,
/// [`rbpmn_core::NoDecisions`], which **refuses** bundled artifacts rather
/// than ignoring them. A linter-only build that quietly accepted a
/// deployment's decisions would be telling the modeler a verdict it did not
/// reach.
#[cfg(feature = "dmn")]
fn validator() -> impl DecisionValidator {
    rbpmn_dmn::Validator
}

#[cfg(not(feature = "dmn"))]
fn validator() -> impl DecisionValidator {
    rbpmn_core::NoDecisions
}

#[wasm_bindgen]
pub fn lint(xml: &str) -> String {
    lint_json(xml)
}

/// `bindings_json` is a bindings manifest as JSON (`"{}"` for none).
/// `decisions_json` is a JSON array of DMN documents (`"[]"` or omitted for
/// none) — optional so the playground and the bpmnlint plugin, which have no
/// decisions to pass, keep calling this with two arguments.
#[wasm_bindgen]
pub fn check_deployable(xml: &str, bindings_json: &str, decisions_json: Option<String>) -> String {
    check_json(
        xml,
        bindings_json,
        decisions_json.as_deref().unwrap_or("[]"),
    )
}

/// Evaluate one decision from a bundle, for the editor's try-it pane.
///
/// Returns `{ "outcome": "value" | "null" | "unrepresentable", "value": …,
/// "reason": … }`. A null answer is an outcome, not an error: see
/// `docs/dmn.md`, "What P1 measured" — dsntk cannot distinguish a legal
/// "no rule matched" from a broken evaluation, so neither does this.
#[cfg(feature = "dmn")]
#[wasm_bindgen]
pub fn evaluate_decision(
    decisions_json: &str,
    namespace: &str,
    model: &str,
    name: &str,
    input_json: &str,
) -> String {
    evaluate_json(decisions_json, namespace, model, name, input_json)
}

#[wasm_bindgen]
pub fn catalogue() -> String {
    serde_json::to_string(rbpmn_model::CATALOGUE).expect("catalogue serializes")
}

/// Plain-Rust core so native tests and the parity dump share the exact
/// serialization with the WASM export.
pub fn lint_json(xml: &str) -> String {
    let value = match rbpmn_model::check(xml) {
        Ok(checked) => serde_json::json!({
            "ok": checked.ok,
            "parseError": null,
            "diagnostics": checked.diagnostics,
        }),
        Err(e) => serde_json::json!({
            "ok": false,
            "parseError": e.to_string(),
            "diagnostics": [],
        }),
    };
    value.to_string()
}

/// Plain-Rust twin of [`check_deployable`], for the same reason as
/// [`lint_json`].
///
/// `ok` means "deploy would proceed to the environment link" — never "deploy
/// would succeed". A caller that stops here and reports success is skipping
/// `unresolved-topic`, which is exactly the wiring gap the manifest exists to
/// make visible.
pub fn check_json(xml: &str, bindings_json: &str, decisions_json: &str) -> String {
    let refused = |message: String| {
        serde_json::json!({
            "ok": false,
            "parseError": null,
            "bindingsError": message,
            "processCount": null,
            "key": null,
            "diagnostics": [],
            "topics": {},
            "invocables": [],
        })
        .to_string()
    };
    let bindings: Bindings = match serde_json::from_str(bindings_json) {
        Ok(bindings) => bindings,
        Err(e) => return refused(e.to_string()),
    };
    let decisions: Vec<String> = match serde_json::from_str(decisions_json) {
        Ok(decisions) => decisions,
        Err(e) => {
            return refused(format!(
                "decisions must be a JSON array of DMN documents: {e}"
            ));
        }
    };

    let value = match core_check(xml, &bindings, &decisions, &validator()) {
        DeployCheck::Unparseable(e) => serde_json::json!({
            "ok": false,
            "parseError": e.to_string(),
            "bindingsError": null,
            "processCount": null,
            "key": null,
            "diagnostics": [],
            "topics": {},
            "invocables": [],
        }),
        DeployCheck::NotExactlyOneProcess(n) => serde_json::json!({
            "ok": false,
            "parseError": null,
            "bindingsError": null,
            "processCount": n,
            "key": null,
            "diagnostics": [],
            "topics": {},
            "invocables": [],
        }),
        DeployCheck::Checked(checked) => {
            let topics: BTreeMap<String, String> = checked.topics.iter().cloned().collect();
            let invocables: Vec<serde_json::Value> = checked
                .invocables
                .iter()
                .map(|i| {
                    serde_json::json!({
                        "namespace": i.namespace,
                        "model": i.model,
                        "name": i.name,
                    })
                })
                .collect();
            serde_json::json!({
                "ok": checked.ok(),
                "parseError": null,
                "bindingsError": null,
                "processCount": null,
                "key": checked.key,
                "diagnostics": checked.diagnostics,
                "topics": topics,
                "invocables": invocables,
            })
        }
    };
    value.to_string()
}

/// Plain-Rust twin of [`evaluate_decision`], for the same reason as the
/// others: native tests and the WASM export must share one serialization.
#[cfg(feature = "dmn")]
pub fn evaluate_json(
    decisions_json: &str,
    namespace: &str,
    model: &str,
    name: &str,
    input_json: &str,
) -> String {
    use rbpmn_dmn::Outcome;

    let fail =
        |message: String| serde_json::json!({ "outcome": "error", "reason": message }).to_string();
    let decisions: Vec<String> = match serde_json::from_str(decisions_json) {
        Ok(decisions) => decisions,
        Err(e) => {
            return fail(format!(
                "decisions must be a JSON array of DMN documents: {e}"
            ));
        }
    };
    let input: serde_json::Value = match serde_json::from_str(input_json) {
        Ok(input) => input,
        Err(e) => return fail(format!("input must be JSON: {e}")),
    };
    let compiled = match rbpmn_dmn::Decisions::compile(&decisions) {
        Ok(compiled) => compiled,
        Err(diagnostics) => {
            return serde_json::json!({
                "outcome": "error",
                "reason": "the decision artifacts do not compile",
                "diagnostics": diagnostics,
            })
            .to_string();
        }
    };
    let invocable = rbpmn_core::Invocable {
        namespace: namespace.to_string(),
        model: model.to_string(),
        name: name.to_string(),
    };
    match compiled.evaluate(&invocable, &input) {
        Outcome::Value(value) => serde_json::json!({ "outcome": "value", "value": value }),
        // A null is an answer, not an error — see docs/dmn.md, "What P1
        // measured". The reason is shown as explanation, never as a verdict.
        Outcome::Null { reason } => serde_json::json!({ "outcome": "null", "reason": reason }),
        Outcome::Unrepresentable(reason) => {
            serde_json::json!({ "outcome": "unrepresentable", "reason": reason })
        }
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    const MINIMAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="defs">
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"><bpmn:outgoing>f1</bpmn:outgoing></bpmn:startEvent>
    <bpmn:serviceTask id="st"><bpmn:incoming>f1</bpmn:incoming><bpmn:outgoing>f2</bpmn:outgoing></bpmn:serviceTask>
    <bpmn:endEvent id="end"><bpmn:incoming>f2</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="st" />
    <bpmn:sequenceFlow id="f2" sourceRef="st" targetRef="end" />
  </bpmn:process>
</bpmn:definitions>"#;

    fn parse(json: &str) -> serde_json::Value {
        serde_json::from_str(json).expect("export emits JSON")
    }

    #[test]
    fn lint_json_shape() {
        let out = super::lint_json("<not-xml");
        assert!(out.contains("parseError"));
        assert_eq!(parse(&out)["ok"], false);
    }

    #[test]
    fn check_resolves_topics_through_the_manifest() {
        let out = parse(&super::check_json(
            MINIMAL,
            r#"{"topics":{"st":"payments"}}"#,
            "[]",
        ));
        assert_eq!(out["ok"], true);
        assert_eq!(out["key"], "p");
        assert_eq!(out["topics"]["st"], "payments");
    }

    /// The editor's whole reason for existing: the manifest is what makes an
    /// unmapped task's topic knowable, and it defaults to the element id.
    #[test]
    fn check_defaults_an_unmapped_topic_to_the_element_id() {
        let out = parse(&super::check_json(MINIMAL, "{}", "[]"));
        assert_eq!(out["topics"]["st"], "st");
    }

    #[test]
    fn check_reports_a_bad_manifest_separately_from_a_bad_model() {
        let out = parse(&super::check_json(MINIMAL, "not json", "[]"));
        assert_eq!(out["ok"], false);
        assert!(out["bindingsError"].is_string());
        assert!(out["parseError"].is_null());

        let out = parse(&super::check_json("<not-xml", "{}", "[]"));
        assert!(out["parseError"].is_string());
        assert!(out["bindingsError"].is_null());
    }

    // `r##` because the document contains `"#` in `href="#amount"`.
    const DECISION: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<definitions xmlns="https://www.omg.org/spec/DMN/20191111/MODEL/"
             namespace="https://rbpmn.example/t" name="t" id="_t">
  <inputData name="Amount" id="amount"><variable name="Amount" typeRef="number"/></inputData>
  <decision name="Doubled" id="doubled">
    <variable name="Doubled" typeRef="number"/>
    <informationRequirement><requiredInput href="#amount"/></informationRequirement>
    <literalExpression><text>Amount * 2</text></literalExpression>
  </decision>
</definitions>"##;

    fn bundle(dmn: &str) -> String {
        serde_json::to_string(&vec![dmn]).unwrap()
    }

    /// The editor's whole reason for wanting this: a bundled decision is
    /// validated, and what it exposes comes back so a manifest can bind to it
    /// — with no server involved. A confidential decision table never leaves
    /// the browser.
    #[cfg(feature = "dmn")]
    #[test]
    fn check_validates_bundled_decisions_offline() {
        let out = parse(&super::check_json(MINIMAL, "{}", &bundle(DECISION)));
        assert_eq!(out["ok"], true, "{:?}", out["diagnostics"]);
        let invocables = out["invocables"].as_array().unwrap();
        assert!(
            invocables.iter().any(|i| i["name"] == "Doubled"),
            "{invocables:?}"
        );
    }

    /// A decision that reads the clock is refused here exactly as deploy
    /// refuses it — same rule id, same element.
    #[cfg(feature = "dmn")]
    #[test]
    fn check_refuses_a_nondeterministic_decision() {
        let stamped = DECISION.replace("Amount * 2", "now()");
        let out = parse(&super::check_json(MINIMAL, "{}", &bundle(&stamped)));
        assert_eq!(out["ok"], false);
        let diagnostics = out["diagnostics"].as_array().unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|d| d["rule"] == "feel-deterministic" && d["element"] == "doubled"),
            "{diagnostics:?}"
        );
    }

    /// Without the feature the artifacts are refused, not ignored. A build
    /// that quietly accepted them would report a verdict it never reached.
    #[cfg(not(feature = "dmn"))]
    #[test]
    fn a_build_without_dmn_refuses_decisions_rather_than_ignoring_them() {
        let out = parse(&super::check_json(MINIMAL, "{}", &bundle(DECISION)));
        assert_eq!(out["ok"], false);
        assert!(
            out["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["rule"] == "dmn-validates")
        );
    }

    /// Decisions travel inside the deployment, so an empty bundle is the
    /// ordinary case and must stay silent.
    #[test]
    fn no_decisions_is_silent() {
        let out = parse(&super::check_json(MINIMAL, "{}", "[]"));
        assert_eq!(out["ok"], true, "{:?}", out["diagnostics"]);
        assert_eq!(out["invocables"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_malformed_decision_bundle_is_reported_separately() {
        let out = parse(&super::check_json(MINIMAL, "{}", "not json"));
        assert_eq!(out["ok"], false);
        assert!(out["bindingsError"].as_str().unwrap().contains("decisions"));
    }

    #[cfg(feature = "dmn")]
    #[test]
    fn evaluate_answers_and_says_why_when_it_cannot() {
        let out = parse(&super::evaluate_json(
            &bundle(DECISION),
            "https://rbpmn.example/t",
            "t",
            "Doubled",
            r#"{"Amount": 21}"#,
        ));
        assert_eq!(out["outcome"], "value");
        assert_eq!(out["value"], 42);

        // A null is an outcome, not an error (docs/dmn.md, "What P1 measured").
        let out = parse(&super::evaluate_json(
            &bundle(DECISION),
            "https://rbpmn.example/t",
            "t",
            "Doubled",
            r#"{"Amount": "twenty-one"}"#,
        ));
        assert_eq!(out["outcome"], "null");
        assert!(out["reason"].is_string());

        // Artifacts that do not compile are an error, and they say so.
        let out = parse(&super::evaluate_json(
            &bundle("<not-dmn"),
            "n",
            "m",
            "d",
            "{}",
        ));
        assert_eq!(out["outcome"], "error");
    }

    /// A message catch with no correlation binding is the manifest gap that
    /// only this export can see — `lint` passes the same document.
    #[test]
    fn check_sees_manifest_gaps_that_lint_cannot() {
        let xml = MINIMAL.replace(
            r#"<bpmn:serviceTask id="st"><bpmn:incoming>f1</bpmn:incoming><bpmn:outgoing>f2</bpmn:outgoing></bpmn:serviceTask>"#,
            r#"<bpmn:intermediateCatchEvent id="cm"><bpmn:incoming>f1</bpmn:incoming><bpmn:outgoing>f2</bpmn:outgoing>
                 <bpmn:messageEventDefinition id="med" messageRef="m" /></bpmn:intermediateCatchEvent>"#,
        )
        .replace(
            r#"<bpmn:process id="p""#,
            r#"<bpmn:message id="m" name="Paid" /><bpmn:process id="p""#,
        )
        .replace(r#"targetRef="st""#, r#"targetRef="cm""#)
        .replace(r#"sourceRef="st""#, r#"sourceRef="cm""#);

        assert_eq!(parse(&super::lint_json(&xml))["ok"], true);

        let out = parse(&super::check_json(&xml, "{}", "[]"));
        assert_eq!(out["ok"], false);
        let diagnostics = out["diagnostics"].as_array().unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|d| d["rule"] == "message-has-correlation" && d["element"] == "cm"),
            "{diagnostics:?}"
        );

        // ... and binding it closes the gap.
        let out = parse(&super::check_json(
            &xml,
            r#"{"correlations":{"cm":"order.id"}}"#,
            "[]",
        ));
        assert_eq!(out["ok"], true, "{:?}", out["diagnostics"]);
    }
}
