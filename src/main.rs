//! Wiring only: the runtime, for this agent.

fn main() -> anyhow::Result<()> {
    athena_core::app::main(athena::agent::spec())
}
