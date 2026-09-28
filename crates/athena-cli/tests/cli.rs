//! The real binary: arguments in, exit code and output out.

use std::process::Command;

fn cli(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_athena-cli"))
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
    )
}

#[test]
fn the_binary_prints_its_version_and_refuses_what_it_does_not_know() {
    let (code, out) = cli(&["--version"]);
    assert_eq!(
        (code, out.trim()),
        (0, concat!("athena-cli ", env!("CARGO_PKG_VERSION")))
    );
    let (code, out) = cli(&["nope"]);
    assert_eq!(code, 2);
    assert!(out.contains("usage:"), "{out}");
}
