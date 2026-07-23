//! Build script: stamp the git commit into the binary/WASM so the CLI (`srg info`)
//! and the WASM `WasmSession.version()` report the exact source commit. The frontend
//! asserts the backend `srg` binary and the vendored `web/src/pkg` were built from
//! the same commit (no enriched-deck schema skew). See `FRONTEND_INTEGRATION_BRIEF.md`.

use std::process::Command;

fn main() {
    // Re-run when HEAD moves (branch switch) so the stamp stays current — and when the
    // branch HEAD points at moves, which is what a new commit on this branch touches.
    // Records embed this stamp as replay provenance (DESIGN.md §8.1), so a stale one is
    // a wrong claim, not a cosmetic wart.
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Some(git_ref) = std::fs::read_to_string(".git/HEAD")
        .ok()
        .and_then(|head| head.strip_prefix("ref: ").map(|r| r.trim().to_owned()))
    {
        // Only watch it if it exists as a loose ref: a missing path reads as
        // "changed" to cargo and would rebuild the crate on every invocation.
        let path = format!(".git/{git_ref}");
        if std::path::Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=SRG_GIT_COMMIT={commit}");
}
