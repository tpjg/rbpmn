//! Writes both documents to disk (`just ui-dist`).
//!
//! The editor is the point: it is a complete tool in one file, so opening the
//! written `editor.html` straight from the filesystem — no server, no
//! install, no network — must work. That property is easy to lose and this is
//! how you check it by hand.
//!
//! The inspector is written with a small synthetic instance, because a real
//! one needs a database. It is enough to see the layout, the diagnosis line
//! and the manifest pane.

use rbpmn_engine::{Bindings, EventView, InstanceInspection, TokenView, WorkItemView};
use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist");
    std::fs::create_dir_all(&out)?;

    let editor = out.join("editor.html");
    std::fs::write(&editor, rbpmn_ui::render_editor())?;
    println!("{}", editor.display());

    let inspector = out.join("inspector.html");
    std::fs::write(&inspector, rbpmn_ui::render_inspection(&sample()))?;
    println!("{}", inspector.display());
    Ok(())
}

/// A frozen instance — the case the inspector exists for, and the same one
/// `just demo` produces against a real engine: 'Charge card' kept answering
/// 502 and raised `GATEWAY_TIMEOUT`, which the error boundary (listening for
/// `PAYMENT_FAILED`) does not catch, so the retry budget ran out and the
/// instance froze instead of taking the recovery path.
///
/// Kept in step with `e2e/demo.py` by hand. For the live version with a real
/// engine and a clickable link, run `just demo`.
fn sample() -> InstanceInspection {
    InstanceInspection {
        id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301"
            .parse()
            .expect("uuid"),
        definition_key: "p".to_string(),
        status: "failed".to_string(),
        variables: serde_json::json!({
            "order": {
                "id": "o-4711",
                "total": 129.95,
                "currency": "EUR",
                "lines": [
                    { "sku": "RB-100", "qty": 2, "price": 49.95 },
                    { "sku": "RB-205", "qty": 1, "price": 30.05 },
                ],
            },
            "customer": { "id": "c-88", "tier": "gold", "email": "ada@example.com" },
            "payment": { "method": "card", "last4": "4242", "attempts": 3 },
        }),
        bpmn_xml: include_str!("../../rbpmn-model/tests/fixtures/accept/10-error-boundary.bpmn")
            .to_string(),
        // `t_fix` is on the recovery path the token never took, so its queue
        // is visible here and nowhere else — the manifest is not in the XML,
        // and no work item was ever created to carry it.
        bindings: Bindings::new()
            .topic("st", "payments")
            .topic("t_fix", "payment-recovery")
            // The other half of "why did this element do that": the topic
            // says which handler ran, the config says what it was told.
            .config(
                "st",
                serde_json::json!({ "gateway": "acquirer-a", "retries": 3 }),
            ),
        tokens: vec![TokenView {
            element_id: "st".to_string(),
            wait_kind: "incident".to_string(),
            scope_no: 0,
        }],
        scopes: Vec::new(),
        work_items: vec![WorkItemView {
            id: "9f1c1b52-0b1f-4d3a-9a2e-2f0b8c5d7e10"
                .parse()
                .expect("uuid"),
            element_id: "st".to_string(),
            state: "failed".to_string(),
            topic: "payments".to_string(),
            kind: "service".to_string(),
            retries: 0,
            last_failure: Some("handler answered 502 (Bad Gateway), attempt 3".to_string()),
        }],
        timers: Vec::new(),
        subscriptions: Vec::new(),
        events: vec![
            EventView {
                kind: "instance-started".to_string(),
                element_id: None,
                display: "instance-started".to_string(),
                detail: None,
            },
            EventView {
                kind: "work-item-created".to_string(),
                element_id: Some("st".to_string()),
                display: "work-item-created st (payments)".to_string(),
                detail: None,
            },
            EventView {
                kind: "work-item-retrying".to_string(),
                element_id: Some("st".to_string()),
                display: "work-item-retrying st (2 left) \u{2014} attempt 1".to_string(),
                detail: None,
            },
            EventView {
                kind: "work-item-retrying".to_string(),
                element_id: Some("st".to_string()),
                display: "work-item-retrying st (1 left) \u{2014} attempt 2".to_string(),
                detail: None,
            },
            EventView {
                kind: "work-item-failed".to_string(),
                element_id: Some("st".to_string()),
                display: "work-item-failed st GATEWAY_TIMEOUT".to_string(),
                detail: None,
            },
            EventView {
                kind: "incident-raised".to_string(),
                element_id: Some("st".to_string()),
                display: "incident-raised st \u{2014} no boundary matches GATEWAY_TIMEOUT"
                    .to_string(),
                detail: None,
            },
        ],
    }
}
