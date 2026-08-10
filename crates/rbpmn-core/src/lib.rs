//! rbpmn-core: the pure semantic core of the engine.
//!
//! `step(process, state, command) -> events` — **no IO in the semantics**.
//! The Postgres layer (phase 2) is a projection of this core: load the
//! affected state, run the pure transition, write rows + events, all in one
//! transaction. Purity is what makes property tests and exhaustive semantic
//! tests cheap, and it is why the core can never read a wall clock — when
//! timers arrive (phase 3), the current time enters as command data, which is
//! what makes "years-long sleep" fixtures trivial to simulate.
//!
//! Phase 1 executable subset: none start/end, sequence flow, exclusive
//! split/join (FEEL-subset conditions + default flow), parallel split/join,
//! service/user tasks as work-item wait states, terminate end. Everything the
//! linter accepts but this phase cannot execute is refused at [`compile`]
//! time with a phase pointer — fail early, never "seems to run".

#![forbid(unsafe_code)]

mod compile;
mod event;
mod merge_patch;
mod state;
mod step;

pub use compile::{Bindings, CompileError, ExecKind, ExecutableProcess, FlowIx, NodeIx, WorkKind};
pub use event::Event;
pub use merge_patch::merge_patch;
pub use state::{
    InstanceState, InstanceStatus, Token, TokenId, WaitKind, WorkItemId, WorkItemState,
};
pub use step::{Command, StepError, step};
