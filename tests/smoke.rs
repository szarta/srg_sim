//! Scaffold smoke test: the `srg` binary builds and runs (task #65).
//!
//! Real behaviour lands with the M-R1 consumer task (#76); this just proves the
//! lib/bin wiring is intact so CI has something to run from day one.

use std::process::Command;

#[test]
fn info_subcommand_runs() {
    let output = Command::new(env!("CARGO_BIN_EXE_srg"))
        .arg("info")
        .output()
        .expect("failed to run the srg binary");
    assert!(output.status.success(), "`srg info` exited non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // `srg info` emits the machine-readable version stamp the frontend asserts against.
    let info: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("info is not JSON: {e}\n{stdout}"));
    assert!(info["engine"].is_string(), "info.engine missing: {stdout}");
    assert!(info["commit"].is_string(), "info.commit missing: {stdout}");
    assert!(
        info["schemas"]["effect_ir"].is_i64(),
        "info.schemas.effect_ir missing: {stdout}"
    );
}

#[test]
fn version_flag_runs() {
    let output = Command::new(env!("CARGO_BIN_EXE_srg"))
        .arg("--version")
        .output()
        .expect("failed to run the srg binary");
    assert!(output.status.success(), "`srg --version` exited non-zero");
}
