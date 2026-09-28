//! Wiring only: the Athena runtime, running this agent.

fn main() -> anyhow::Result<()> {
    athena_core::app::main(@NAME@::agent::spec())
}
