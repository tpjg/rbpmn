//! Instance runtime state — pure data, serializable, deterministic.
//!
//! Between steps the state is always *quiescent*: every token sits at a wait
//! position (parked at a parallel join or behind a work item). The step
//! function advances tokens synchronously until quiescence — the projection
//! layer's transaction boundary.

use crate::compile::{FlowIx, NodeIx, TimerDue, WorkKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TokenId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkItemId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TimerId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SubscriptionId(pub u64);

/// A **runtime** scope instance. `ScopeId(0)` is the instance root and is
/// never stored; every entered subprocess allocates a fresh one, so a
/// subprocess inside a loop gets a new scope per iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ScopeId(pub u64);

impl ScopeId {
    pub const ROOT: ScopeId = ScopeId(0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceStatus {
    Created,
    Active,
    Completed,
    Terminated,
    /// Incident: a raised error matched no boundary. The instance is frozen
    /// as-is (tokens and closed items stay put) for later repair — nothing
    /// is torn down, unlike terminate.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub node: NodeIx,
    /// The runtime scope this token belongs to — what makes join counting
    /// local ("one token per incoming flow *within its scope*") and
    /// teardown precise.
    pub scope: ScopeId,
    pub wait: WaitKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WaitKind {
    /// Parked at a parallel join, holding the incoming flow it arrived on.
    Join { arrived_via: FlowIx },
    /// Parked behind an open work item (service/user task).
    WorkItem(WorkItemId),
    /// Parked at a timer intermediate catch, waiting for its timer to fire.
    Timer(TimerId),
    /// Parked at a message catch (or receive task), waiting for delivery.
    Message(SubscriptionId),
    /// Parked at an event-based gateway; the armed timers/subscriptions
    /// point back at this token and race — first to fire wins.
    EventGateway,
    /// Frozen by an incident at this element. Every incident converges on
    /// this shape — token parked where it failed, in-flight arms withdrawn —
    /// so inspection always shows *where*, and a future repair API has one
    /// state to resume from.
    Incident,
    /// Parked at a subprocess, waiting for the child scope it opened to
    /// empty. Resumed when the last token inside that scope is consumed.
    Scope(ScopeId),
    /// Parked at a business-rule task while its decision is evaluated.
    ///
    /// Transient by design: the projection evaluates and resumes the token
    /// inside the *same* transaction, so this is never a state an operator
    /// observes or a scheduler has to find. It exists because the pure core
    /// must be able to say "waiting for an answer" without being able to
    /// compute one (`docs/dmn.md`, D3).
    Decision,
}

/// An open subprocess scope instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopeState {
    /// The subprocess node that owns it.
    pub element: NodeIx,
    /// The enclosing runtime scope (`ScopeId::ROOT` for a top-level one).
    pub parent: ScopeId,
    /// The parked token that will resume when this scope completes.
    pub token: TokenId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItemState {
    pub element: NodeIx,
    pub token: TokenId,
    pub kind: WorkKind,
    pub topic: String,
    /// Completed/cancelled items stay (closed) so a late completion gets the
    /// typed "not open" result instead of "unknown".
    pub open: bool,
}

/// An armed timer. `element` is where it conceptually sits: the catch event
/// itself, or the boundary event when armed on a task's token. Fired and
/// cancelled timers are removed — a claimed timer row deleting in the same
/// transaction as the step is what makes firing exactly-once.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimerState {
    pub element: NodeIx,
    pub token: TokenId,
    pub due: TimerDue,
    /// For a cycle: fires left, the armed one included, so `Some(1)` is the
    /// last. `None` is an unbounded cycle — or no cycle at all; the `due`
    /// says which, and only the cycle path reads this.
    ///
    /// `default` for the same reason the sibling fields of
    /// [`crate::Event::TimerArmed`] carry it: this is a public,
    /// `Deserialize` shape that a caller may have persisted before the field
    /// existed, and "absent" and "unbounded" are the same answer here.
    #[serde(default)]
    pub remaining: Option<u32>,
}

/// An open message subscription. `key` is the correlation key **value**,
/// evaluated from the variables when the subscription was armed (the FEEL
/// qualified name to evaluate comes from the bindings manifest).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionState {
    pub element: NodeIx,
    pub token: TokenId,
    pub message: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceState {
    pub status: InstanceStatus,
    pub variables: Value,
    pub(crate) tokens: BTreeMap<TokenId, Token>,
    pub(crate) work_items: BTreeMap<WorkItemId, WorkItemState>,
    pub(crate) timers: BTreeMap<TimerId, TimerState>,
    pub(crate) subscriptions: BTreeMap<SubscriptionId, SubscriptionState>,
    pub(crate) scopes: BTreeMap<ScopeId, ScopeState>,
    next_token: u64,
    next_work_item: u64,
    next_timer: u64,
    next_subscription: u64,
    next_scope: u64,
}

impl InstanceState {
    pub fn new() -> Self {
        InstanceState {
            status: InstanceStatus::Created,
            variables: Value::Null,
            tokens: BTreeMap::new(),
            work_items: BTreeMap::new(),
            timers: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            scopes: BTreeMap::new(),
            next_token: 0,
            next_work_item: 0,
            next_timer: 0,
            next_subscription: 0,
            next_scope: 1, // 0 is the implicit root
        }
    }

    pub fn tokens(&self) -> impl Iterator<Item = (TokenId, &Token)> {
        self.tokens.iter().map(|(id, t)| (*id, t))
    }

    pub fn work_items(&self) -> impl Iterator<Item = (WorkItemId, &WorkItemState)> {
        self.work_items.iter().map(|(id, w)| (*id, w))
    }

    pub fn open_work_items(&self) -> impl Iterator<Item = (WorkItemId, &WorkItemState)> {
        self.work_items().filter(|(_, w)| w.open)
    }

    /// The open work item parked on `element_id`, if any — how external
    /// callers (tests, scenarios, the task API) address work by element.
    pub fn open_work_item_at(&self, element: NodeIx) -> Option<WorkItemId> {
        self.open_work_items()
            .find(|(_, w)| w.element == element)
            .map(|(id, _)| id)
    }

    pub fn timers(&self) -> impl Iterator<Item = (TimerId, &TimerState)> {
        self.timers.iter().map(|(id, t)| (*id, t))
    }

    pub fn subscriptions(&self) -> impl Iterator<Item = (SubscriptionId, &SubscriptionState)> {
        self.subscriptions.iter().map(|(id, s)| (*id, s))
    }

    /// Open subprocess scope instances (the root is implicit, never listed).
    pub fn scopes(&self) -> impl Iterator<Item = (ScopeId, &ScopeState)> {
        self.scopes.iter().map(|(id, s)| (*id, s))
    }

    /// Every scope in `scope`'s subtree, itself included — the set a
    /// cancelled subprocess tears down.
    pub(crate) fn scope_subtree(&self, scope: ScopeId) -> Vec<ScopeId> {
        let mut out = vec![scope];
        let mut i = 0;
        while i < out.len() {
            let parent = out[i];
            out.extend(
                self.scopes
                    .iter()
                    .filter(|(_, s)| s.parent == parent)
                    .map(|(id, _)| *id),
            );
            i += 1;
        }
        out
    }

    /// The armed timer sitting on `element`, if any (catch or boundary
    /// event) — how scenarios address timers.
    pub fn armed_timer_at(&self, element: NodeIx) -> Option<TimerId> {
        self.timers()
            .find(|(_, t)| t.element == element)
            .map(|(id, _)| id)
    }

    /// The open subscription on `element`, if any.
    pub fn armed_subscription_at(&self, element: NodeIx) -> Option<SubscriptionId> {
        self.subscriptions()
            .find(|(_, s)| s.element == element)
            .map(|(id, _)| id)
    }

    /// Token ids are allocated when a token starts moving; the token is only
    /// materialized in the map when it parks at a wait position.
    pub(crate) fn next_token_id(&mut self) -> TokenId {
        let id = TokenId(self.next_token);
        self.next_token += 1;
        id
    }

    /// Rebuild a quiescent state from projected rows (the Postgres layer's
    /// loader). Rows are the runtime truth; this is the inverse of writing
    /// them. Only *open* work items need to be supplied — closed ones are
    /// answered from rows before the core is ever invoked; fired/cancelled
    /// timers and subscriptions have no rows at all.
    #[allow(clippy::too_many_arguments)]
    pub fn rehydrate(
        status: InstanceStatus,
        variables: Value,
        tokens: impl IntoIterator<Item = (TokenId, Token)>,
        work_items: impl IntoIterator<Item = (WorkItemId, WorkItemState)>,
        timers: impl IntoIterator<Item = (TimerId, TimerState)>,
        subscriptions: impl IntoIterator<Item = (SubscriptionId, SubscriptionState)>,
        scopes: impl IntoIterator<Item = (ScopeId, ScopeState)>,
        counters: Counters,
    ) -> Self {
        InstanceState {
            status,
            variables,
            tokens: tokens.into_iter().collect(),
            work_items: work_items.into_iter().collect(),
            timers: timers.into_iter().collect(),
            subscriptions: subscriptions.into_iter().collect(),
            scopes: scopes.into_iter().collect(),
            next_token: counters.next_token,
            next_work_item: counters.next_work_item,
            next_timer: counters.next_timer,
            next_subscription: counters.next_subscription,
            next_scope: counters.next_scope.max(1),
        }
    }

    pub fn counters(&self) -> Counters {
        Counters {
            next_token: self.next_token,
            next_work_item: self.next_work_item,
            next_timer: self.next_timer,
            next_subscription: self.next_subscription,
            next_scope: self.next_scope,
        }
    }

    pub(crate) fn alloc_work_item(&mut self, item: WorkItemState) -> WorkItemId {
        let id = WorkItemId(self.next_work_item);
        self.next_work_item += 1;
        self.work_items.insert(id, item);
        id
    }

    pub(crate) fn alloc_timer(&mut self, timer: TimerState) -> TimerId {
        let id = TimerId(self.next_timer);
        self.next_timer += 1;
        self.timers.insert(id, timer);
        id
    }

    pub(crate) fn alloc_scope(&mut self, scope: ScopeState) -> ScopeId {
        let id = ScopeId(self.next_scope);
        self.next_scope += 1;
        self.scopes.insert(id, scope);
        id
    }

    pub(crate) fn alloc_subscription(&mut self, sub: SubscriptionState) -> SubscriptionId {
        let id = SubscriptionId(self.next_subscription);
        self.next_subscription += 1;
        self.subscriptions.insert(id, sub);
        id
    }
}

/// The per-instance id allocators, persisted as instance columns so ids stay
/// stable across rehydration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    pub next_token: u64,
    pub next_work_item: u64,
    pub next_timer: u64,
    pub next_subscription: u64,
    pub next_scope: u64,
}

impl Default for InstanceState {
    fn default() -> Self {
        Self::new()
    }
}
