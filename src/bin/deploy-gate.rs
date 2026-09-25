//! Wiring only: the real VM, rooted at `/`. The logic is `athena::gate`.

use athena::gate::{self, Gate, RealSystem};

fn main() {
    // A CI connection that drops mid-deploy sends SIGHUP. Dying then could
    // leave the units stopped, so finish the operation (and its rollback).
    // SAFETY: called before any other thread exists; SIG_IGN is a valid
    // disposition.
    unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
    // Lossy on purpose: a non-UTF-8 word cannot match the whitelist, so it
    // is rejected like any other unknown word.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let ssh_command =
        std::env::var_os("SSH_ORIGINAL_COMMAND").map(|c| c.to_string_lossy().into_owned());
    let gate = Gate::new("/", &RealSystem);
    let code = gate::main(&args, ssh_command.as_deref(), &gate, &mut std::io::stdout());
    std::process::exit(code);
}
