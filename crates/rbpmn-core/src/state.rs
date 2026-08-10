//! Instance runtime state — pure data, serializable, deterministic.
//!
//! Between steps the state is always *quiescent*: every token sits at a wait
//! position (parked at a parallel join or behind a work item). The step
//! function advances tokens synchronously until quiescence — the projection
//! layer's transaction boundary.

use crate::compile::{FlowIx, NodeIx, WorkKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TokenId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkItemId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceStatus {
    Created,
    Active,
    Completed,
    Terminated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub node: NodeIx,
    pub wait: WaitKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WaitKind {
    /// Parked at a parallel join, holding the incoming flow it arrived on.
    Join { arrived_via: FlowIx },
    /// Parked behind an open work item (service/user task).
    WorkItem(WorkItemId),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceState {
    pub status: InstanceStatus,
    pub variables: Value,
    pub(crate) tokens: BTreeMap<TokenId, Token>,
    pub(crate) work_items: BTreeMap<WorkItemId, WorkItemState>,
    next_token: u64,
    next_work_item: u64,
}

impl InstanceState {
    pub fn new() -> Self {
        InstanceState {
            status: InstanceStatus::Created,
            variables: Value::Null,
            tokens: BTreeMap::new(),
            work_items: BTreeMap::new(),
            next_token: 0,
            next_work_item: 0,
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

    /// Token ids are allocated when a token starts moving; the token is only
    /// materialized in the map when it parks at a wait position.
    pub(crate) fn next_token_id(&mut self) -> TokenId {
        let id = TokenId(self.next_token);
        self.next_token += 1;
        id
    }

    pub(crate) fn alloc_work_item(&mut self, item: WorkItemState) -> WorkItemId {
        let id = WorkItemId(self.next_work_item);
        self.next_work_item += 1;
        self.work_items.insert(id, item);
        id
    }
}

impl Default for InstanceState {
    fn default() -> Self {
        Self::new()
    }
}
