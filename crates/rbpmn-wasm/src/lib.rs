//! WASM surface for rbpmn-model. One source of truth: the playground and the
//! bpmnlint plugin call this — never a JS reimplementation of the rules.
//!
//! JSON strings cross the boundary (identical serialization on both sides of
//! the parity check):
//!
//! ```json
//! { "ok": bool, "parseError": string|null,
//!   "diagnostics": [{ "rule", "element", "message", "severity" }] }
//! ```

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn lint(xml: &str) -> String {
    lint_json(xml)
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

#[cfg(test)]
mod tests {
    #[test]
    fn lint_json_shape() {
        let out = super::lint_json("<not-xml");
        assert!(out.contains("parseError"));
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["ok"], false);
    }
}
