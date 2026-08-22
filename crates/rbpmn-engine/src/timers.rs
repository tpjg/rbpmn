//! The published read surface over armed timers — the third wait state,
//! beside [`crate::instances`] and [`crate::work_items`].
//!
//! The question it answers is *when does this next happen?* A deadline an
//! application shows a user — a renewal date, a payment reminder, an
//! escalation that has not fired yet — is a value it has to render next to
//! its own row, which makes it a join, which makes it SQL. The same surface
//! answers what is armed at all for one instance (support) and how much is
//! past due (health).
//!
//! There is deliberately **no typed call here**, unlike the other two
//! modules. `find_by_shared_index` exists because an index-backed lookup has
//! a precondition worth refusing loudly; `queue_depths` exists because a
//! two-bucket aggregate over a claimability rule is fiddly enough to get
//! wrong. "The next deadline for this instance" is `order by due_at limit 1`
//! — a query with no rule in it, that an application will want joined to its
//! own row anyway. A typed call would be the weakest possible form of this
//! surface and one more thing to keep in step with the view.

use crate::Engine;

/// The published timer view. Named here, beside [`crate::INSTANCE_VIEW`] and
/// [`crate::WORK_ITEM_VIEW`], so callers build SQL against the contract
/// rather than rbpmn's table names — a rename is then a compile error for
/// them instead of a runtime surprise.
pub const TIMER_VIEW: &str = "rbpmn_v_timer";

impl Engine {
    /// The soonest armed deadline for one instance, as SQL an application can
    /// paste and extend — `order by due_at limit 1`, never `min(due_at)`.
    ///
    /// Not a method, because it should not be one: an application wants this
    /// value *joined to its own row*, and a call returning a bare timestamp
    /// would push it back into N+1 round trips. What it needs from rbpmn is
    /// the contract and the shape, which is what this is.
    ///
    /// ```sql
    /// select t.due_at, t.element_id, t.due_spec
    ///   from rbpmn_v_timer t
    ///  where t.instance_id = $1
    ///  order by t.due_at
    ///  limit 1
    /// ```
    ///
    /// `min(due_at)` over the view plans a hash join across two sequential
    /// scans — the aggregate-to-index-scan rewrite is refused across a join,
    /// before indexes are considered. Measured on a 50 000-instance probe: 6
    /// buffers against 733. [`Engine::next_due_in`] carries the same finding
    /// for the scheduler's own query.
    pub const NEXT_DEADLINE_SQL: &'static str = "select t.due_at, t.element_id, t.due_spec \
         from rbpmn_v_timer t where t.instance_id = $1 order by t.due_at limit 1";
}
