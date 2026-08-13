//! WASM surface for the rbpmn front door. One source of truth: the
//! playground, the editor and the bpmnlint plugin call this — never a JS
//! reimplementation of the rules.
//!
//! Two exports, because there are two questions:
//!
//! * [`lint`] — *is the model legal?* Model only, no manifest. This is what
//!   bpmnlint asks, and what the playground shows.
//! * [`check_deployable`] — *would this deploy?* Model **and** bindings
//!   manifest, i.e. everything `Engine::deploy` decides without a database.
//!   The one remaining deploy step, the environment link (`unresolved-topic`),
//!   needs registration state, so the resolved topics are handed back for the
//!   caller to check against a covered-topic set it fetched.
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

use rbpmn_core::{Bindings, DeployCheck, check_deployable as core_check};
use std::collections::BTreeMap;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn lint(xml: &str) -> String {
    lint_json(xml)
}

/// `bindings_json` is a bindings manifest as JSON (`"{}"` for none).
#[wasm_bindgen]
pub fn check_deployable(xml: &str, bindings_json: &str) -> String {
    check_json(xml, bindings_json)
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
pub fn check_json(xml: &str, bindings_json: &str) -> String {
    let bindings: Bindings = match serde_json::from_str(bindings_json) {
        Ok(bindings) => bindings,
        Err(e) => {
            return serde_json::json!({
                "ok": false,
                "parseError": null,
                "bindingsError": e.to_string(),
                "processCount": null,
                "key": null,
                "diagnostics": [],
                "topics": {},
            })
            .to_string();
        }
    };

    let value = match core_check(xml, &bindings) {
        DeployCheck::Unparseable(e) => serde_json::json!({
            "ok": false,
            "parseError": e.to_string(),
            "bindingsError": null,
            "processCount": null,
            "key": null,
            "diagnostics": [],
            "topics": {},
        }),
        DeployCheck::NotExactlyOneProcess(n) => serde_json::json!({
            "ok": false,
            "parseError": null,
            "bindingsError": null,
            "processCount": n,
            "key": null,
            "diagnostics": [],
            "topics": {},
        }),
        DeployCheck::Checked(checked) => {
            let topics: BTreeMap<String, String> = checked.topics.iter().cloned().collect();
            serde_json::json!({
                "ok": checked.ok(),
                "parseError": null,
                "bindingsError": null,
                "processCount": null,
                "key": checked.key,
                "diagnostics": checked.diagnostics,
                "topics": topics,
            })
        }
    };
    value.to_string()
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
        ));
        assert_eq!(out["ok"], true);
        assert_eq!(out["key"], "p");
        assert_eq!(out["topics"]["st"], "payments");
    }

    /// The editor's whole reason for existing: the manifest is what makes an
    /// unmapped task's topic knowable, and it defaults to the element id.
    #[test]
    fn check_defaults_an_unmapped_topic_to_the_element_id() {
        let out = parse(&super::check_json(MINIMAL, "{}"));
        assert_eq!(out["topics"]["st"], "st");
    }

    #[test]
    fn check_reports_a_bad_manifest_separately_from_a_bad_model() {
        let out = parse(&super::check_json(MINIMAL, "not json"));
        assert_eq!(out["ok"], false);
        assert!(out["bindingsError"].is_string());
        assert!(out["parseError"].is_null());

        let out = parse(&super::check_json("<not-xml", "{}"));
        assert!(out["parseError"].is_string());
        assert!(out["bindingsError"].is_null());
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

        let out = parse(&super::check_json(&xml, "{}"));
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
        ));
        assert_eq!(out["ok"], true, "{:?}", out["diagnostics"]);
    }
}
