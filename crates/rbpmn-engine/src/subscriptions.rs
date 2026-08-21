//! The published read surface over armed message subscriptions — the fourth
//! wait state, and the one that completes the set beside
//! [`crate::instances`], [`crate::work_items`] and [`crate::timers`].
//!
//! The question it answers arrives in one shape: someone quotes a business
//! identifier — an order number, a ticket reference — and asks what is
//! waiting on it. Nobody asks by instance id; if the instance id were known
//! the answer would already be at hand. So `correlation_key` is the column
//! this surface exists to be searched by, and
//! `rbpmn_subscription_by_key` (migration 0017) is what keeps that search
//! index-driven on every Postgres the engine supports, and independent of how
//! many distinct message names a deployment has.
//!
//! No typed call, for the reason [`crate::timers`] gives: these are queries
//! with no rule in them that an application wants joined to its own row. What
//! rbpmn owes here is the contract and the shapes, which is what this is.

use crate::Engine;

/// The published subscription view. Named here, beside the other three, so
/// callers build SQL against the contract rather than rbpmn's table names.
pub const SUBSCRIPTION_VIEW: &str = "rbpmn_v_subscription";

impl Engine {
    /// What is waiting on one business identifier — the support question,
    /// written out.
    ///
    /// Searched by `correlation_key` alone on purpose: a message name is
    /// something the *application* knows, and the person asking usually does
    /// not. `instance_status` comes back with it because the row found may be
    /// exactly the one `correlate` is ignoring — a frozen instance keeps its
    /// subscriptions, and they neither answer for a key nor block delivery to
    /// a live instance sharing it. One column is the difference between
    /// "nothing is waiting" and "the thing waiting is frozen".
    ///
    /// ```sql
    /// select instance_id, definition_key, element_id, message_name,
    ///        instance_status, created_at
    ///   from rbpmn_v_subscription
    ///  where correlation_key = $1
    ///  order by created_at
    /// ```
    pub const WAITING_ON_KEY_SQL: &'static str = "select instance_id, definition_key, element_id, message_name, \
         instance_status, created_at from rbpmn_v_subscription \
         where correlation_key = $1 order by created_at";

    /// The query to run after a `409 AmbiguousCorrelation`: which
    /// (message, key) pairs have more than one live subscription, and whose.
    ///
    /// This is the one fact about subscriptions that is genuinely not
    /// derivable from a single row, and it is deliberately a query rather than
    /// a column — seeing it needs an aggregate over the whole table, and an
    /// aggregate would stop the view being an inlinable projection.
    ///
    /// `correlate` refuses a duplicate rather than delivering to an arbitrary
    /// one, so this returns exactly the pairs it will refuse.
    pub const AMBIGUOUS_CORRELATIONS_SQL: &'static str = "select message_name, correlation_key, count(*) as waiting, \
         array_agg(instance_id order by created_at) as instances \
         from rbpmn_v_subscription where instance_status = 'active' \
         group by message_name, correlation_key having count(*) > 1";
}
