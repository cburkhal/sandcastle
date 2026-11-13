//! sandcastle: one OpenCode instance per Git repo, each in its own VM.
//!
//! The library holds everything; the `sandcastle` binary is the CLI on top,
//! and the app in `ui/` is a second front end.

pub mod agent;
pub mod core;
pub mod firewall;
pub mod relay;
pub mod runtime;
pub mod secrets;
