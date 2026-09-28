//! Wiring only: real arguments, a real shell, real output.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = athena_cli::main(&args, &mut athena_cli::RealShell, &mut std::io::stdout());
    std::process::exit(code);
}
