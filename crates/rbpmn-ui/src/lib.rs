//! rbpmn-ui: the two documents.
//!
//! * [`render_inspection`] — a **read-only** view of one instance, with the
//!   data inlined. No API, no credentials in the browser, no writes. The
//!   embedding application's authorization check is the only gate, which is
//!   what "the application handles auth" has to mean in order to be true.
//! * [`render_editor`] — the model **and manifest** editor. A `.bpmn` is not
//!   deployable on its own (rbpmn keeps wiring out of the XML), and no
//!   bpmn-io tool knows the other half exists.
//!
//! Both are self-contained HTML: one inline stylesheet, one inline script,
//! and a Content-Security-Policy the document carries itself. They work from
//! `file://`, they can be attached to a support ticket, and they cannot
//! phone home.
//!
//! The library boundary is a **value, not an endpoint**:
//! `Engine::inspect_instance` already returns an [`InstanceInspection`], and
//! rendering is a pure function over it. So an application that must not show
//! variables to tier-1 support strips the field before calling — that door
//! stays open for free, and it is the only redaction this crate will grow.
//!
//! ```no_run
//! # async fn example(engine: rbpmn_engine::Engine, id: uuid::Uuid)
//! # -> Result<(), Box<dyn std::error::Error>> {
//! let inspection = engine.inspect_instance(id).await?;
//! let html = rbpmn_ui::render_inspection(&inspection);
//! # let _ = html;
//! # Ok(())
//! # }
//! ```
//!
//! What the application still owes its users is in `docs/http-security.md`:
//! authenticate the viewer, embed in a `sandbox="allow-scripts"` iframe
//! *without* `allow-same-origin`, and serve with `Cache-Control: no-store`.

#![forbid(unsafe_code)]

mod document;
#[cfg(feature = "axum")]
mod routes;

use document::{Document, escape_json_for_html};
use rbpmn_engine::InstanceInspection;

#[cfg(feature = "axum")]
pub use routes::{
    EDITOR_API_PREFIX, editor_router, editor_slash_redirect, environment_router, inspector_router,
};

const INSPECTOR_JS: &str = include_str!("../assets/inspector.js");
const INSPECTOR_CSS: &str = include_str!("../assets/inspector.css");
const EDITOR_JS: &str = include_str!("../assets/editor.js");
const EDITOR_CSS: &str = include_str!("../assets/editor.css");

/// Render one instance as a self-contained HTML document.
///
/// Pure: no IO, no clock, no network. Whatever is in the value is what the
/// document shows.
pub fn render_inspection(inspection: &InstanceInspection) -> String {
    let json = serde_json::to_string(inspection).expect("InstanceInspection serializes");
    let title = format!("{} · {}", inspection.definition_key, inspection.id);
    Document {
        title: &title,
        css: INSPECTOR_CSS,
        js: INSPECTOR_JS,
        data: Some(escape_json_for_html(&json)),
        // No validator in this document: it renders a model that already
        // deployed, and half a megabyte of linter would be dead weight in a
        // file that gets emailed around.
        wasm: false,
    }
    .render()
}

/// Render the model+manifest editor as a self-contained HTML document.
///
/// Stateless and constant: it carries no instance data and reaches nothing on
/// its own. The single optional call it can make — the covered-topic set for
/// the `unresolved-topic` check — is a button the user presses, resolved
/// relative to wherever the document was served from.
pub fn render_editor() -> String {
    Document {
        title: "rbpmn editor",
        css: EDITOR_CSS,
        js: EDITOR_JS,
        data: None,
        // The editor runs the real linter, compiled to wasm32.
        wasm: true,
    }
    .render()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inlining a bundle into `<script>` is only safe while the bundle cannot
    /// interfere with the HTML tokenizer's script-data states. Two sequences
    /// can, and the distinction matters because one of them is common in
    /// serializer code and harmless:
    ///
    /// * `</script` ends the element wherever it appears — the document would
    ///   be truncated mid-bundle.
    /// * `<!--` only switches to *script data escaped*, where `</script>`
    ///   still closes normally. It becomes fatal only if `<script` follows
    ///   before the script ends: that reaches *script data double escaped*,
    ///   where our real closing tag stops closing anything and the rest of
    ///   the document is swallowed into the script.
    ///
    /// So `<!--` is allowed (bpmn-moddle's XML serializer contains it in a
    /// string literal and a regex) and `<script` in any form is not — the
    /// conservative half of the pair, and the one no bundle has a reason to
    /// carry. Asserted rather than assumed: a future dependency could ship
    /// either, and the failure mode is a document that silently renders as
    /// text.
    #[test]
    fn bundles_cannot_disturb_the_script_tokenizer() {
        for (name, js) in [("inspector", INSPECTOR_JS), ("editor", EDITOR_JS)] {
            let lowered = js.to_ascii_lowercase();
            assert!(
                !lowered.contains("</script"),
                "{name}.js can close its own tag"
            );
            assert!(
                !lowered.contains("<script"),
                "{name}.js can reach the double-escaped state"
            );
        }
        for (name, css) in [("inspector", INSPECTOR_CSS), ("editor", EDITOR_CSS)] {
            assert!(
                !css.to_ascii_lowercase().contains("</style"),
                "{name}.css can close its own tag"
            );
        }
    }
}

/// Fixtures and helpers shared with the suites in `tests/`.
#[doc(hidden)]
pub mod testing {
    use rbpmn_engine::{Bindings, InstanceInspection};

    /// The same digest the renderer puts in the policy, so a test can check
    /// that what a document declares matches what it actually carries.
    pub fn sha256_base64(content: &str) -> String {
        crate::document::sha256_base64(content)
    }

    /// A minimal inspection with no runtime rows — the base every escaping
    /// fixture mutates.
    pub fn empty_inspection() -> InstanceInspection {
        InstanceInspection {
            id: uuid_nil(),
            definition_key: "orders".to_string(),
            status: "active".to_string(),
            variables: serde_json::json!({}),
            bpmn_xml: "<definitions/>".to_string(),
            bindings: Bindings::default(),
            tokens: Vec::new(),
            scopes: Vec::new(),
            work_items: Vec::new(),
            timers: Vec::new(),
            subscriptions: Vec::new(),
            events: Vec::new(),
        }
    }

    fn uuid_nil() -> uuid::Uuid {
        "00000000-0000-0000-0000-000000000000".parse().expect("nil")
    }
}
