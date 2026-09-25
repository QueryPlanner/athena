//! A persistent, tool-using agent. `main.rs` is wiring only; everything it
//! wires lives here so integration tests in `tests/` can reach it.

pub mod agent;
pub mod cli;
pub mod dotenv;
pub mod http;
pub mod policy;
pub mod runner;
pub mod sandbox;
pub mod service;
pub mod shutdown;
pub mod store;
pub mod telegram;
