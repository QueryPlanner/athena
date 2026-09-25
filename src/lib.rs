//! A persistent, tool-using agent. `main.rs` is wiring only; everything it
//! wires lives here so integration tests in `tests/` can reach it.

pub mod agent;
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
pub mod store;
pub mod telegram;
pub mod telemetry;
