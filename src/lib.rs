//! A persistent, tool-using agent. `main.rs` is wiring only; everything it
//! wires lives here so integration tests in `tests/` can reach it.

pub mod agent;
pub mod cli;
pub mod runner;
pub mod store;
