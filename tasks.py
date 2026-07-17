"""Invoke tasks for srg-core development.

Usage:
    invoke check          # pre-commit hooks (fmt + clippy + knots) + tests — the CI gate
    invoke test           # cargo test
    invoke build          # cargo build (--release optional)
    invoke bump-version   # bump the crate version in Cargo.toml (dry-run with no args)

Install invoke: pip install invoke   (or use the shared venv's copy)
"""

import re
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
def build(c, release=False):
    """Build the crate (debug by default; --release for optimized)."""
    c.run("cargo build --release" if release else "cargo build", pty=True)


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
