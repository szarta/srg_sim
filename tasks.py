"""Invoke tasks for srg-core development.

Usage:
    invoke check          # pre-commit hooks only (fmt + clippy + knots) — the fast gate
    invoke test           # cargo test — the suite (separate from check)
    invoke build          # cargo build (--release optional)
    invoke overrides      # regen overrides.ir.json from overrides.yaml (self-contained)
    invoke cards-ir       # regen the parser golden fixtures/parser/cards.ir.json (Rust)
    invoke grammar-catalog# regen docs/development/grammar-catalog.md + rule_index.json
    invoke bump-version   # bump the crate version in Cargo.toml (dry-run with no args)
    invoke release-web    # rebuild the srg binary + WASM pkg from one commit (pre-push)
    invoke deploy         # on prod: build/copy the no-root steps, print the sudo ones
    invoke verify-deployment  # confirm binary/pkg/service agree (no schema skew)

Install invoke: pip install invoke   (or use the shared venv's copy)
"""

import filecmp
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

from invoke import task

SEMVER = r"\d+\.\d+\.\d+"

# Prod topology (docs/development/releasing.rst). All overridable by env so the tasks
# stay usable off the canonical host.
INSTALL_BIN = os.environ.get("SRG_INSTALL_BIN", "/usr/local/bin/srg")
BACKEND_SERVICE = os.environ.get("SRG_SERVICE", "srg-backend.service")
# The committed WASM artifact the frontend vendors, and where it is vendored to.
PKG_DIR = Path("web/src/pkg")
PKG_FILES = ("srg_core_bg.wasm", "srg_core.js")


def _read_cargo_version() -> str:
    cargo = Path("Cargo.toml").read_text()
    match = re.search(r'^version = "([^"]+)"', cargo, re.MULTILINE)
    if not match:
        raise RuntimeError("could not find package version in Cargo.toml")
    return match.group(1)


def _frontend_dir(frontend: str | None) -> Path:
    """The frontend checkout that vendors the WASM pkg (prod: ~/srg_card_search_website
    /frontend). Overridable by `--frontend` or the `SRG_FRONTEND` env var."""
    raw = frontend or os.environ.get(
        "SRG_FRONTEND", str(Path.home() / "srg_card_search_website" / "frontend")
    )
    return Path(raw).expanduser()


def _source_effect_ir_schema() -> int:
    """The effect_ir SCHEMA_VERSION this checkout compiles (src/ir.rs) — the contract
    number the deployed binary and WASM pkg must both carry."""
    m = re.search(r"SCHEMA_VERSION:\s*i64\s*=\s*(\d+)", Path("src/ir.rs").read_text())
    if not m:
        raise RuntimeError("could not find SCHEMA_VERSION in src/ir.rs")
    return int(m.group(1))


def _head_commit() -> str:
    return subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()


def _resolve_srg() -> str | None:
    """Resolve the `srg` binary the backend would shell (rib_engine._srg_bin order):
    `SRG_BIN`, then `srg` on PATH, then a dev-checkout target/ fallback."""
    if pinned := os.environ.get("SRG_BIN"):
        return pinned
    if found := shutil.which("srg"):
        return found
    for profile in ("release", "debug"):
        cand = Path("target") / profile / "srg"
        if cand.exists():
            return str(cand)
    return None


def _srg_info(binary: str) -> dict:
    """Parse `<binary> info` (commit + schema versions)."""
    out = subprocess.run(
        [binary, "info"], capture_output=True, text=True, check=True
    ).stdout
    return json.loads(out)


@task
def check(c):
    """Run the fast gate: pre-commit hooks only (fmt + clippy + knots + file checks).

    Tests are deliberately NOT run here — use `invoke test` for those. The full
    pre-commit gate is `invoke check && invoke test`.
    """
    c.run("pre-commit run --all-files", pty=True)


@task
def test(c):
    """Run the test suite (`cargo test`)."""
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


@task(name="grammar-catalog")
def grammar_catalog(c):
    """Regenerate the grammar catalog: `docs/development/grammar-catalog.md` (a readable
    per-rule reference with real DB example clauses) and `fixtures/parser/rule_index.json`
    (the DB-free rule inventory).

    Run after adding or changing a grammar rule, then review the diff. The inventory is
    gated by `tests/grammar_catalog.rs`, so a rule change without regenerating fails the
    suite. The catalog is the "what shapes are already handled?" reference for the
    coverage grind. Requires a built binary.
    """
    c.run("cargo run --quiet --features cli -- grammar-catalog", pty=True)


@task(name="parser-fixture")
def parser_fixture(c):
    """Refresh the curated parser regression sample `fixtures/parser/clauses.json`.

    Recomputes each case's `expected` IR + the `coverage_golden` from the live Rust
    parser, preserving the sample's inputs (db_uuid/source/text, coverage_records).
    Run alongside `cards-ir` after a grammar/override change, then review the diff:
    `tests/parser.rs` holds the parser to this sample. Requires a built binary.
    """
    c.run("cargo run --quiet --features cli -- parser-fixture", pty=True)


@task(name="scripted-fixture")
def scripted_fixture(c):
    """Regenerate the scripted-match snapshot fixture `web/src/sample/scripted_match.json`.

    Drives the deterministic scripted match (seat A takes `legal[0]`, seat B heuristic)
    to `Done` and rewrites the ordered `Step` sequence the Run It Back frontend replays.
    Run after an intended change to the Step shape, deck coverage, or the heuristic
    policy, then review the diff: `tests/scripted_match.rs` holds the engine to it.
    """
    c.run(
        "cargo test --test scripted_match scripted_match_matches_fixture",
        pty=True,
        env={"BLESS_SCRIPTED": "1"},
    )


@task
def build(c, release=False):
    """Build the crate (debug by default; --release for optimized)."""
    c.run("cargo build --release" if release else "cargo build", pty=True)


@task
def wasm(c):
    """Build the web WASM package: srg-core (wasm feature) -> web/src/pkg (wasm-bindgen).

    Needs the wasm32 target (`rustup target add wasm32-unknown-unknown`) and a
    `wasm-bindgen` CLI matching the wasm-bindgen crate version
    (`cargo install wasm-bindgen-cli --version <v>`). The output (`web/src/pkg`) is
    committed so the frontend can vendor a known-good artifact without a local Rust
    toolchain; refresh it with `invoke release-web` and commit the result.
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


@task(name="release-web")
def release_web(c):
    """Build the `srg` release binary AND the WASM pkg from the *same* commit.

    The backend shells the `srg` binary to enrich decks; the frontend vendors
    `web/src/pkg` to play them. Both must come from one tree so the enriched-deck
    schema matches (FRONTEND_INTEGRATION_BRIEF.md). Run this, commit the refreshed
    `web/src/pkg`, and ship the `srg` binary from the same commit — `srg info` and
    the WASM `version()` will then report the same `commit` stamp (no skew).
    """
    c.run("cargo build --release --bin srg", pty=True)
    wasm(c)
    print("\nBuilt from commit:")
    c.run("./target/release/srg info", pty=True)
    print(
        "\nCommit `web/src/pkg` and ship ./target/release/srg from the same commit."
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


@task
def deploy(c, frontend=None, rebuild_wasm=False):
    """Deploy this checkout on the prod host: do every step that needs NO root, then
    print the two `sudo` commands for the operator to run.

    Run it on the prod host (`ssh get-diced`) after `git pull`, from the engine
    checkout. The non-root steps it performs:

      1. Build the native `srg` release binary (`cargo install` -> ~/.cargo/bin/srg).
      2. Publish the frontend: copy the committed `web/src/pkg` into the frontend's
         vendored `src/runitback/pkg`, then `npm run build` (nginx serves `dist/`).

    Then it prints the root steps you must run to finish (install the binary onto the
    service PATH and restart the backend). Runbook: docs/development/releasing.rst.

    Flags:
      --frontend PATH   frontend checkout (default $SRG_FRONTEND or
                        ~/srg_card_search_website/frontend).
      --rebuild-wasm    rebuild the WASM pkg here at the pulled commit (kills the
                        one-commit lag of the committed pkg) — needs the wasm toolchain.
    """
    if not Path("Cargo.toml").exists() or not PKG_DIR.exists():
        raise SystemExit("run from the srg-core checkout root (Cargo.toml + web/src/pkg)")
    fe = _frontend_dir(frontend)
    vendored = fe / "src" / "runitback" / "pkg"
    if not fe.exists():
        raise SystemExit(f"frontend not found: {fe} (set --frontend / $SRG_FRONTEND)")

    commit, schema = _head_commit(), _source_effect_ir_schema()
    print(f"Deploying commit {commit} (effect_ir schema {schema})\n")

    # 1. Native binary -> ~/.cargo/bin/srg (no root; reuses the build cache).
    print("[1/3] Building the native srg binary (cargo install)…")
    c.run("cargo install --path . --bin srg", pty=True)

    # 2. Optionally rebuild the WASM pkg here so it carries the pulled commit exactly.
    if rebuild_wasm:
        print("\n[2/3] Rebuilding the WASM pkg at this commit…")
        wasm(c)
    else:
        print("\n[2/3] Using the committed WASM pkg (pass --rebuild-wasm to rebuild).")

    # 3. Publish the frontend: vendor the pkg + build.
    print(f"\n[3/3] Publishing the frontend at {fe}…")
    vendored.mkdir(parents=True, exist_ok=True)
    for name in PKG_FILES:
        src = PKG_DIR / name
        if not src.exists():
            raise SystemExit(f"missing pkg artifact: {src} (run `invoke release-web`)")
        shutil.copy2(src, vendored / name)
    print(f"  copied {', '.join(PKG_FILES)} -> {vendored}")
    c.run("npm run build", pty=True, cwd=str(fe))

    home = Path.home()
    print(
        "\n"
        "──────────────────────────────────────────────────────────────────────\n"
        "  Non-root steps done. Run these two as root to finish the deploy:\n"
        "──────────────────────────────────────────────────────────────────────\n"
        f"  sudo install -m 755 {home / '.cargo' / 'bin' / 'srg'} {INSTALL_BIN}\n"
        f"  sudo systemctl restart {BACKEND_SERVICE}\n"
        "──────────────────────────────────────────────────────────────────────\n"
        f"  Then: invoke verify-deployment   (confirms {INSTALL_BIN} reports\n"
        f"  commit {commit} / effect_ir {schema} and the service is live).\n"
    )


@task(name="verify-deployment")
def verify_deployment(c, frontend=None):
    """Confirm a deploy landed with no schema skew: the installed binary, the vendored
    WASM pkg, and the backend service all agree with this checkout.

    Checks (a failing one exits non-zero):
      * the PATH-resolved `srg` (what the backend shells) reports the checkout's
        effect_ir schema — the wire contract that must not skew;
      * the frontend's vendored pkg is byte-identical to the committed `web/src/pkg`
        (so nginx serves the schema that was deployed);
      * `<service>` is active.
    The binary's commit and the pkg's one-commit lag are reported, not enforced.
    Runbook: docs/development/releasing.rst.
    """
    want_schema, want_commit = _source_effect_ir_schema(), _head_commit()
    fe = _frontend_dir(frontend)
    vendored = fe / "src" / "runitback" / "pkg"
    print(f"Expecting effect_ir schema {want_schema}, commit {want_commit}\n")

    results: list[tuple[bool, str]] = []

    # 1. Backend binary — schema is the hard gate; commit is advisory.
    binary = _resolve_srg()
    if not binary:
        results.append((False, "backend binary: `srg` not found on PATH/SRG_BIN/target"))
    else:
        try:
            info = _srg_info(binary)
            got_schema = info.get("schemas", {}).get("effect_ir")
            got_commit = info.get("commit")
            ok = got_schema == want_schema
            results.append(
                (
                    ok,
                    f"backend binary ({binary}): effect_ir {got_schema} "
                    f"{'==' if ok else '!='} {want_schema}, commit {got_commit}",
                )
            )
            if ok and got_commit != want_commit:
                print(
                    f"  note: binary commit {got_commit} != HEAD {want_commit} — "
                    "reinstall the binary if this is not intentional.\n"
                )
        except (subprocess.CalledProcessError, json.JSONDecodeError) as e:
            results.append((False, f"backend binary ({binary}): `info` failed — {e}"))

    # 2. Frontend pkg — vendored copy must match the committed artifact byte-for-byte.
    for name in PKG_FILES:
        src, dst = PKG_DIR / name, vendored / name
        if not dst.exists():
            results.append((False, f"frontend pkg: {dst} missing (run `invoke deploy`)"))
        elif filecmp.cmp(src, dst, shallow=False):
            results.append((True, f"frontend pkg: {name} matches committed web/src/pkg"))
        else:
            results.append((False, f"frontend pkg: {name} DIFFERS from web/src/pkg — re-copy"))

    # 3. Backend service — active? (`is-active` needs no root.)
    svc = c.run(
        f"systemctl is-active {BACKEND_SERVICE}", hide=True, warn=True, pty=False
    )
    state = svc.stdout.strip() or "unknown"
    results.append((state == "active", f"service {BACKEND_SERVICE}: {state}"))

    print("Results:")
    for ok, msg in results:
        print(f"  [{'PASS' if ok else 'FAIL'}] {msg}")
    if all(ok for ok, _ in results):
        print("\nDeployment verified — binary, pkg, and service agree. No skew.")
    else:
        raise SystemExit("\nDeployment verification FAILED (see above).")
