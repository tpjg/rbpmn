//! rbpmn-core: the pure semantic core of the engine.
//!
//! `step(process, state, command) -> events` — **no IO in the semantics**.
//! The Postgres layer (phase 2) is a projection of this core: load the
//! affected state, run the pure transition, write rows + events, all in one
//! transaction. Purity is what makes property tests and exhaustive semantic
//! tests cheap, and it is why the core can never read a wall clock or run an
//! evaluator. **Time and decisions both enter as command data**: the current
//! time arrives with `FireTimer`, which is what makes "years-long sleep"
//! fixtures trivial to simulate, and a decision's answer arrives with
//! `CompleteDecision`, which is what lets a replay re-derive a history through
//! a core that cannot evaluate DMN at all.
//!
//! Executable subset: none start/end, sequence flow, exclusive split/join
//! (FEEL-subset conditions + default flow), parallel split/join, service/user
//! tasks as work-item wait states, business-rule tasks as decision waits,
//! terminate end, error boundaries, timer/message intermediate catch (+
//! receive task), interrupting timer boundaries, the event-based gateway, and
//! embedded subprocesses. Everything the linter accepts but no phase can
//! execute yet is refused at [`compile`] time with a phase pointer — fail
//! early, never "seems to run".

#![forbid(unsafe_code)]

mod check;
mod compile;
mod decisions;
mod event;
mod merge_patch;
mod state;
mod step;

pub use check::{Checked, DeployCheck, check_deployable, config_bindings};
pub use compile::{
    Bindings, CompileError, ExecKind, ExecutableProcess, FlowIx, IndexDeclaration, IndexScope,
    NodeIx, ScopeIx, TimerDue, WorkKind,
};
pub use decisions::{DecisionCheck, DecisionValidator, Invocable, NoDecisions};
pub use event::Event;
pub use merge_patch::merge_patch;
pub use state::{
    Counters, InstanceState, InstanceStatus, ScopeId, ScopeState, SubscriptionId,
    SubscriptionState, TimerId, TimerState, Token, TokenId, WaitKind, WorkItemId, WorkItemState,
};
pub use step::{Command, StepError, step};
