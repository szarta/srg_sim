"""Invoke tasks for srg-core development.

Usage:
    invoke check          # pre-commit hooks (fmt + clippy + knots) + tests — the CI gate
    invoke test           # cargo test
    invoke build          # cargo build (--release optional)
    invoke overrides      # regen overrides.ir.json from overrides.yaml (self-contained)
    invoke cards-ir       # regen the parser golden fixtures/parser/cards.ir.json (Rust)
    invoke bump-version   # bump the crate version in Cargo.toml (dry-run with no args)

Install invoke: pip install invoke   (or use the shared venv's copy)
"""

import re
import sys
from pathlib import Path

from invoke import task

SEMVER = r"\d+\.\d+\.\d+"


def _read_cargo_version() -> str:
    cargo = Path("Cargo.toml").read_text()
    match = re.search(r'^version = "([^"]+)"', cargo, re.MULTILINE)
    if not match:
        raise RuntimeError("could not find package version in Cargo.toml")
    return match.group(1)


@task
def check(c):
    """Run the full gate: pre-commit hooks (fmt + clippy + knots), then tests."""
    c.run("pre-commit run --all-files", pty=True)
    c.run("cargo test", pty=True)


@task
def test(c):
    """Run the test suite."""
    c.run("cargo test", pty=True)


@task
def overrides(c):
    """Regenerate overrides.ir.json from this repo's overrides.yaml (the source of truth).

    The coverage-growth loop: model a card/competitor gimmick in `overrides.yaml` (repo
    root), run this to refresh the embedded Rust table, and rebuild. Expansion uses the
    in-repo IR tooling (`scripts/srg_ir/`) to fill defaults + canonicalize — Python, but
    self-contained (the retired `srg_sim_python` oracle is no longer consulted).
    """
    c.run(f"{sys.executable} scripts/gen_overrides_ir.py overrides.ir.json", pty=True)


@task(name="cards-ir")
def cards_ir(c):
    """Regenerate the parser golden `fixtures/parser/cards.ir.json` from the Rust parser.

    Run after a deliberate parser change or a card-DB update, then review the diff:
    `tests/parser_parity.rs` holds the parser to this committed corpus. The Rust-native
    replacement for the retired `scripts/gen_cards_ir.py` (which drove the Python
    parser oracle). Requires a built binary — builds it first.
    """
    c.run("cargo run --quiet --features cli -- cards-ir", pty=True)


@task(name="parser-fixture")
def parser_fixture(c):
    """Refresh the curated parser regression sample `fixtures/parser/clauses.json`.

    Recomputes each case's `expected` IR + the `coverage_golden` from the live Rust
    parser, preserving the sample's inputs (db_uuid/source/text, coverage_records).
    Run alongside `cards-ir` after a grammar/override change, then review the diff:
    `tests/parser.rs` holds the parser to this sample. Requires a built binary.
    """
    c.run("cargo run --quiet --features cli -- parser-fixture", pty=True)


@task
def build(c, release=False):
    """Build the crate (debug by default; --release for optimized)."""
    c.run("cargo build --release" if release else "cargo build", pty=True)


@task
def wasm(c):
    """Build the web WASM package: srg-core (wasm feature) -> web/src/pkg (wasm-bindgen).

    Needs the wasm32 target (`rustup target add wasm32-unknown-unknown`) and a
    `wasm-bindgen` CLI matching the wasm-bindgen crate version
    (`cargo install wasm-bindgen-cli --version <v>`). The output (`web/src/pkg`) is a
    generated artifact — git-ignored, rebuilt from the crate, never vendored.
    """
    c.run(
        "cargo build --lib --release --no-default-features --features wasm "
        "--target wasm32-unknown-unknown",
        pty=True,
    )
    c.run(
        "wasm-bindgen --target web --no-typescript --out-dir web/src/pkg "
        "target/wasm32-unknown-unknown/release/srg_core.wasm",
        pty=True,
    )


@task(name="bump-version")
def bump_version(c, new_version=None):
    """Bump the crate version in Cargo.toml. With no --new-version, prints current."""
    current = _read_cargo_version()
    if not new_version:
        print(f"Current version (Cargo.toml): {current}")
        print("Run: invoke bump-version --new-version X.Y.Z")
        return
    if not re.fullmatch(SEMVER, new_version):
        raise SystemExit(f"--new-version must look like X.Y.Z, got '{new_version}'")
    path = Path("Cargo.toml")
    text = path.read_text()
    updated = re.sub(
        r'^(version = ")' + SEMVER + r'(")',
        rf"\g<1>{new_version}\g<2>",
        text,
        count=1,
        flags=re.MULTILINE,
    )
    if updated == text:
        print("No version string changed.")
        return
    path.write_text(updated)
    print(f"Bumped {current} -> {new_version} in Cargo.toml")
