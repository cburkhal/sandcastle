//! Core: config, state and box lifecycle.
//!
//! Hard rule: nothing in here runs `container`, `opencode` or `bw`. The core
//! goes through the module traits in `runtime`, `agent` and (later) `secrets`.

pub mod config;
pub mod lifecycle;
pub mod state;
