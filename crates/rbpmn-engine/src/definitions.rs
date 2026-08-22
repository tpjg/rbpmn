//! The published read surface over deployed definitions.
//!
//! The other four views answer what is *happening*; this one answers what is
//! *deployed*. The question behind it is reconciliation — "is the model
//! running here the one in git?" — which is `content_hash` against a hash of
//! the bundle, and `deployed_at` for when it landed. A definition is not a
//! diagram, though: it is a diagram plus its manifest plus the decisions it
//! invokes, so the artifacts are part of the surface too.
//!
//! Two views rather than one, for the reason the subscription view leaves
//! ambiguity to a query: `bpmn_xml` and `bindings` are 1:1 with a definition
//! and sit in [`DEFINITION_VIEW`], while the DMN artifacts are 0..N and would
//! need an aggregate to fold in — which would stop the view being an inlinable
//! projection. They get [`DEFINITION_DECISION_VIEW`], keyed by both the
//! surrogate and the stable pair so a caller joins whichever way it holds.

use crate::Engine;

/// The published definition view. Named here, beside the other view
/// constants, so callers build SQL against the contract rather than rbpmn's
/// table names.
pub const DEFINITION_VIEW: &str = "rbpmn_v_definition";

/// The DMN artifacts a definition was deployed with. Read ordered by
/// `ordinal`: artifacts may import one another, so deployment order is part
/// of the deployment.
pub const DEFINITION_DECISION_VIEW: &str = "rbpmn_v_definition_decision";

impl Engine {
    /// What is deployed right now — the latest version of every key, with the
    /// hash to reconcile against and nothing heavy in the select list.
    ///
    /// `distinct on` rather than a window function or a correlated subquery:
    /// it walks the `(key, version)` unique index backwards and stops at the
    /// first row per key. Note it is a *query*, not a view — `distinct on` is
    /// a distinct, and a view carrying one would stop being an inlinable
    /// projection.
    ///
    /// Deliberately selects no XML. `bpmn_xml` is a whole document, and a
    /// deployment inventory that dragged every model across the wire would be
    /// the wrong default for the one question asked most often.
    ///
    /// ```sql
    /// select distinct on (key) key, version, content_hash, deployed_at
    ///   from rbpmn_v_definition
    ///  order by key, version desc
    /// ```
    pub const DEPLOYED_NOW_SQL: &'static str = "select distinct on (key) key, version, content_hash, deployed_at \
         from rbpmn_v_definition order by key, version desc";
}
