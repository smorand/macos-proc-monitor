//! CLI end-to-end tests: run the actual compiled binary and assert on output.

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_lists_collection_and_web_flags() {
    Command::cargo_bin("macos-proc-monitor")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--interval"))
        .stdout(predicate::str::contains("--port"))
        .stdout(predicate::str::contains("--bind"))
        .stdout(predicate::str::contains("--data-retention"));
}

#[test]
fn version_matches_crate_version() {
    Command::cargo_bin("macos-proc-monitor")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}
