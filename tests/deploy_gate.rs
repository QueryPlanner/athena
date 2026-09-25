//! The real `deploy-gate` binary, only with inputs it must reject before
//! doing anything: it is rooted at `/` and would otherwise drive this
//! machine's systemd. Everything it does is tested in `src/gate/` against
//! a fake VM.

use serde_json::Value;
use std::process::{Command, Output};

fn gate(args: &[&str], ssh_command: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_deploy-gate"));
    cmd.args(args).env_remove("SSH_ORIGINAL_COMMAND");
    if let Some(line) = ssh_command {
        cmd.env("SSH_ORIGINAL_COMMAND", line);
    }
    cmd.output().unwrap()
}

#[test]
fn an_injection_attempt_is_rejected_with_exit_2_and_one_json_line() {
    let out = gate(&["--key-env", "staging"], Some("deploy prod; rm -rf /"));
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["ok"], false);
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.starts_with("rejected: "), "{stderr}");
}

#[test]
fn a_key_without_a_command_is_rejected() {
    let out = gate(&["--key-env", "prod"], None);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no SSH_ORIGINAL_COMMAND"));
}

#[test]
fn version_prints_the_package_version() {
    let out = gate(&["--version"], None);
    assert_eq!(out.status.code(), Some(0));
    let expected = format!("deploy-gate {}\n", env!("CARGO_PKG_VERSION"));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), expected);
}
