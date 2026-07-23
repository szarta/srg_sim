//! Build script: stamp the git commit into the binary/WASM so the CLI (`srg info`)
//! and the WASM `WasmSession.version()` report the exact source commit. The frontend
//! asserts the backend `srg` binary and the vendored `web/src/pkg` were built from
//! the same commit (no enriched-deck schema skew). See `FRONTEND_INTEGRATION_BRIEF.md`.

use std::process::Command;

fn main() {
    // Re-run when HEAD moves (new commit / branch switch) so the stamp stays current.
    println!("cargo:rerun-if-changed=.git/HEAD");
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=SRG_GIT_COMMIT={commit}");
}
