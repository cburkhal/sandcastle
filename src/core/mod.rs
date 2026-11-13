//! Core: config, state and box lifecycle.
//!
//! Hard rule: nothing in here runs `container`, `opencode` or `bw`. The core
//! goes through the module traits in `runtime`, `agent` and (later) `secrets`.

pub mod config;
pub mod git;
pub mod lifecycle;
pub mod session;
pub mod state;
pub mod up;
