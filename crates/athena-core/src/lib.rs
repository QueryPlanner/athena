//! The runtime every Athena agent shares. An agent's crate writes an
//! [`spec::AgentSpec`] and calls [`app::main`]; everything that is the same for
//! every agent lives here.

pub mod app;
pub mod bench;
pub mod cli;
pub mod dotenv;
pub mod eval;
pub mod flags;
pub mod gate;
pub mod http;
pub mod ops;
pub mod policy;
pub mod runner;
pub mod sandbox;
pub mod service;
pub mod shutdown;
pub mod spec;
pub mod store;
pub mod telegram;
pub mod telemetry;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use spec::{AgentSpec, SandboxTools};
