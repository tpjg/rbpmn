//! What a rendered document must be true of, whatever data went into it.
//!
//! Two families. The **structural** ones pin the security posture: one
//! executable script, every script hash listed in the policy the document
//! carries, no subresources, and — for the inspector — no WebAssembly
//! permission it does not need. The **escaping** ones are the reason this
//! crate is allowed to inline business data at all: an order note containing
//! `</script>` must render as text, in every field, every time.
//!
//! Escaping is not sanitization. Nothing here is removed or rewritten; the
//! round-trip assertions exist to prove exactly that — the document shows the
//! operator the real value, byte for byte, without it becoming markup.

use rbpmn_engine::{EventView, InstanceInspection, TokenView, WorkItemView};
use rbpmn_ui::testing::{empty_inspection, sha256_base64};
use rbpmn_ui::{render_editor, render_inspection};

/// Every shape that has ever been used to break out of an inline script.
const HOSTILE: &[&str] = &[
    "</script><img src=x onerror=alert(1)>",
    "</SCRIPT ><script>alert(1)</script>",
    "<!--<script>",
    "]]><![CDATA[",
    "\u{2028}alert(1)\u{2029}",
    "&lt;already escaped&gt;",
    "\"'`",
];

fn extract_csp(html: &str) -> String {
    let marker = "<meta http-equiv=\"Content-Security-Policy\" content=\"";
    let start = html.find(marker).expect("policy present") + marker.len();
    let rest = &html[start..];
    rest[..rest.find('"').expect("policy terminated")].to_string()
}

/// (content, is_executable) for every script element in the document.
fn scripts(html: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(open) = rest.find("<script") {
        let after = &rest[open..];
        let tag_end = after.find('>').expect("script tag closes");
        let executable = !after[..tag_end].contains("type=");
        let body = &after[tag_end + 1..];
        let end = body.find("</script>").expect("script closes");
        out.push((body[..end].to_string(), executable));
        rest = &body[end..];
    }
    out
}

/// `wasm` and `fetches` are the two capabilities a document may hold; every
/// other directive is identical for both, and locked.
fn assert_structure(html: &str, wasm: bool, fetches: bool) {
    let found = scripts(html);
    assert_eq!(
        found.iter().filter(|(_, executable)| *executable).count(),
        1,
        "a document has exactly one executable script"
    );

    let csp = extract_csp(html);
    for directive in [
        "default-src 'none'",
        "base-uri 'none'",
        "form-action 'none'",
        "img-src data:",
        "font-src data:",
    ] {
        assert!(
            csp.contains(directive),
            "policy is missing {directive}: {csp}"
        );
    }
    assert_eq!(
        csp.contains("'wasm-unsafe-eval'"),
        wasm,
        "WebAssembly permission must be granted only where it is used: {csp}"
    );
    // Both directions matter. A document that never fetches must not be
    // allowed to, and one that does must actually be able to: a policy that
    // blocks the page's own feature is not stricter, it is broken — which is
    // how the editor shipped with `connect-src 'none'` until a browser tried
    // the button.
    assert!(
        csp.contains(if fetches {
            "connect-src 'self'"
        } else {
            "connect-src 'none'"
        }),
        "wrong connect-src for a document that {} fetch: {csp}",
        if fetches { "does" } else { "does not" }
    );

    // Every script the document carries is pinned by the policy it carries.
    for (content, _) in &found {
        let hash = sha256_base64(content);
        assert!(
            csp.contains(&format!("'sha256-{hash}'")),
            "a script's hash is missing from the policy"
        );
    }

    // Our own markup references nothing external. (The bundles legitimately
    // contain URL *strings* — BPMN namespace URIs — so this checks the
    // constructs that would cause a fetch, not the substring "http".)
    assert!(!html.contains("<link "), "no stylesheet links");
    assert!(!html.contains("<script src"), "no external scripts");
    assert!(!html.contains("<iframe"), "no frames");
    assert!(
        !html.contains("<base "),
        "no base element (and base-uri forbids it)"
    );
}

#[test]
fn inspector_document_structure() {
    assert_structure(&render_inspection(&empty_inspection()), false, false);
}

#[test]
fn editor_document_structure() {
    assert_structure(&render_editor(), true, true);
}

/// The editor carries no instance data — which is what lets it be mounted
/// somewhere an inspector must not be. Checked structurally: the bundle
/// mentions the JSON media type in its own fetch headers, so a substring
/// search would prove nothing.
#[test]
fn editor_carries_no_data_block() {
    let found = scripts(&render_editor());
    assert!(
        found.iter().all(|(_, executable)| *executable),
        "the editor document carries a data block"
    );
    assert!(!render_editor().contains("id=\"rbpmn-data\""));
}

/// The load-bearing one: hostile text in any field cannot become markup, and
/// still arrives intact.
#[test]
fn hostile_data_cannot_escape_the_data_block() {
    for hostile in HOSTILE {
        let mut inspection = empty_inspection();
        inspection.definition_key = (*hostile).to_string();
        inspection.status = (*hostile).to_string();
        inspection.bpmn_xml = format!("<definitions>{hostile}</definitions>");
        inspection.variables = serde_json::json!({
            *hostile: { "nested": [*hostile, { "deeper": *hostile }] },
            "plain": *hostile,
        });
        // The manifest is application text too, and `config` is the field
        // that made that unmissable: free JSON of the application's own
        // shape, nested as deep as it likes, rendered in the element pane.
        inspection.bindings = rbpmn_engine::Bindings::new()
            .topic(*hostile, *hostile)
            .config(
                *hostile,
                serde_json::json!({ *hostile: [*hostile, { "deeper": *hostile }] }),
            );
        inspection.tokens = vec![TokenView {
            element_id: (*hostile).to_string(),
            wait_kind: (*hostile).to_string(),
            scope_no: 0,
        }];
        inspection.work_items = vec![WorkItemView {
            id: "11111111-1111-1111-1111-111111111111".parse().unwrap(),
            element_id: (*hostile).to_string(),
            state: "failed".to_string(),
            topic: (*hostile).to_string(),
            kind: "service".to_string(),
            retries: 0,
            last_failure: Some((*hostile).to_string()),
        }];
        inspection.events = vec![EventView {
            kind: (*hostile).to_string(),
            element_id: Some((*hostile).to_string()),
            display: (*hostile).to_string(),
            // The new incident-reason field is hostile input here too: it
            // reaches the document the same way `display` does.
            detail: Some((*hostile).to_string()),
        }];

        let html = render_inspection(&inspection);

        // Exactly the closing tags we emit — the payload contributed none.
        assert_eq!(
            html.matches("</script>").count(),
            2,
            "hostile payload {hostile:?} produced a stray script close"
        );

        // The strong form: the data block carries no markup-significant
        // character at all, so no tokenizer state can be reached from it —
        // not the end tag, not a comment, not CDATA.
        let raw = raw_data_block(&html);
        for forbidden in ['<', '>', '&', '\u{2028}', '\u{2029}'] {
            assert!(
                !raw.contains(forbidden),
                "hostile payload {hostile:?} left {forbidden:?} unescaped in the data block"
            );
        }
        assert_structure(&html, false, false);

        // ... and the operator still sees the real value.
        let parsed = data_block(&html);
        assert_eq!(parsed["definitionKey"], *hostile);
        assert_eq!(parsed["variables"]["plain"], *hostile);
        assert_eq!(parsed["variables"][*hostile]["nested"][0], *hostile);
        assert_eq!(parsed["workItems"][0]["lastFailure"], *hostile);
        assert_eq!(parsed["events"][0]["display"], *hostile);
        assert_eq!(parsed["bpmnXml"], inspection.bpmn_xml);
        // The manifest too, and at depth. Without this the escaping check
        // above passes when the payload is simply *gone* — a raw-character
        // scan cannot tell "escaped correctly" from "dropped on the way in".
        assert_eq!(parsed["bindings"]["topics"][*hostile], *hostile);
        assert_eq!(
            parsed["bindings"]["config"][*hostile][*hostile][0],
            *hostile
        );
        assert_eq!(
            parsed["bindings"]["config"][*hostile][*hostile][1]["deeper"],
            *hostile
        );
    }
}

/// The title is HTML character data rather than DOM text, so it is the one
/// place the renderer escapes markup itself.
#[test]
fn hostile_definition_key_cannot_break_the_title() {
    let mut inspection = empty_inspection();
    inspection.definition_key = "</title><script>alert(1)</script>".to_string();
    let html = render_inspection(&inspection);
    let title_start = html.find("<title>").unwrap() + "<title>".len();
    let title_end = html.find("</title>").unwrap();
    let title = &html[title_start..title_end];
    assert!(
        !title.contains('<'),
        "unescaped markup in the title: {title}"
    );
    assert!(
        title.contains("&lt;/title&gt;"),
        "the real value is still shown: {title}"
    );
    assert_structure(&html, false, false);
}

/// The whole value, unmodified — the round-trip that proves the escaping is
/// lossless rather than a filter.
#[test]
fn the_data_block_round_trips_the_inspection() {
    let mut inspection = empty_inspection();
    inspection.variables = serde_json::json!({
        "unicode": "héllo ☃ 🎉",
        "control": "tab\there\nnewline",
        "number": 1.5,
        "null": null,
        "bool": true,
    });
    let html = render_inspection(&inspection);
    let expected: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&inspection).unwrap()).unwrap();
    assert_eq!(data_block(&html), expected);
}

/// The data block exactly as it sits in the file, before any JSON parsing.
fn raw_data_block(html: &str) -> String {
    scripts(html)
        .into_iter()
        .find(|(_, executable)| !executable)
        .expect("a data block")
        .0
}

fn data_block(html: &str) -> serde_json::Value {
    serde_json::from_str(&raw_data_block(html)).expect("the data block is valid JSON")
}

/// A document is a snapshot; rendering the same value twice must produce the
/// same bytes, or the hashes in the policy would be a moving target.
#[test]
fn rendering_is_deterministic() {
    let inspection = empty_inspection();
    assert_eq!(
        render_inspection(&inspection),
        render_inspection(&inspection)
    );
    assert_eq!(render_editor(), render_editor());
}

/// Guards the claim in the crate docs: an application that must not show
/// variables to tier-1 support strips the field before rendering, and needs
/// no feature from us to do it.
#[test]
fn an_application_can_redact_before_rendering() {
    let mut inspection: InstanceInspection = empty_inspection();
    inspection.variables = serde_json::json!({ "iban": "NL91ABNA0417164300" });
    assert!(render_inspection(&inspection).contains("NL91ABNA0417164300"));

    inspection.variables = serde_json::json!({ "iban": "[redacted]" });
    let html = render_inspection(&inspection);
    assert!(!html.contains("NL91ABNA0417164300"));
    assert!(html.contains("[redacted]"));
}
