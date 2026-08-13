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

use rbpmn_engine::{
    Bindings, EventView, InstanceInspection, SubscriptionView, TimerView, TokenView, WorkItemView,
};
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

/// A frozen instance: the case the inspector exists for.
fn sample() -> InstanceInspection {
    InstanceInspection {
        id: "3f2504e0-4f89-11d3-9a0c-0305e82c3301"
            .parse()
            .expect("uuid"),
        definition_key: "orders".to_string(),
        status: "active".to_string(),
        variables: serde_json::json!({
            "order": { "id": "o-4711", "total": 129.95, "currency": "EUR" },
            "customer": { "tier": "gold" },
        }),
        bpmn_xml: include_str!("../../rbpmn-model/tests/fixtures/accept/07-task-kinds.bpmn")
            .to_string(),
        bindings: Bindings::new()
            .topic("st", "payments")
            .topic("ut", "review-queue")
            .correlation("rt", "order.id"),
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
            last_failure: "handler answered 502".to_string().into(),
        }],
        timers: vec![TimerView {
            element_id: "rt".to_string(),
            due_spec: "P3D".to_string(),
            due_at: "2026-08-16T09:00:00Z".to_string(),
        }],
        subscriptions: vec![SubscriptionView {
            element_id: "rt".to_string(),
            message_name: "ShipmentConfirmed".to_string(),
            correlation_key: "o-4711".to_string(),
        }],
        events: vec![
            EventView {
                kind: "instance-started".to_string(),
                element_id: None,
                display: "instance-started".to_string(),
            },
            EventView {
                kind: "work-item-created".to_string(),
                element_id: Some("st".to_string()),
                display: "work-item-created st (payments)".to_string(),
            },
            EventView {
                kind: "work-item-failed".to_string(),
                element_id: Some("st".to_string()),
                display: "work-item-failed st — handler answered 502".to_string(),
            },
        ],
    }
}
