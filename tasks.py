"""
Invoke tasks for srg_sim development.

Usage:
    invoke check          # pre-commit hooks + type check + tests (the CI gate)
    invoke build          # build the sdist and wheel into dist/
    invoke test           # run the test suite
    invoke docs           # build HTML documentation
    invoke clean          # remove build/test artifacts
    invoke bump-version   # bump version across all files (reads pyproject.toml)

Tasks shell out to the interpreter running invoke (the shared venv at
~/data/stars/venv), so they work whether or not that venv is on PATH.

Install invoke: pip install invoke
"""

import re
import sys
from pathlib import Path

from invoke import task

# The interpreter running invoke — i.e. the venv's python. Everything is invoked
# as `python -m <module>` so tasks don't depend on the venv being activated.
PY = sys.executable

# A semver core like 0.1.0 (no pre-release / build metadata).
SEMVER = r"\d+\.\d+\.\d+"

# Files that embed this project's own version, with the pattern that locates it
# and a replacement template ({new} = new version). pyproject.toml is the source
# of truth; the others must be kept in lock-step.
VERSION_FILES = [
    # (path, pattern, replacement-template)
    ("pyproject.toml", r'^(version = ")' + SEMVER + r'(")', r"\g<1>{new}\g<2>"),
    ("srg_sim/__init__.py", r'(__version__ = ")' + SEMVER + r'(")', r"\g<1>{new}\g<2>"),
    ("docs/conf.py", r'(release = ")' + SEMVER + r'(")', r"\g<1>{new}\g<2>"),
]


def _read_version():
    """Read the source-of-truth version from pyproject.toml."""
    pyproject = Path("pyproject.toml").read_text()
    match = re.search(r'^version = "([^"]+)"', pyproject, re.MULTILINE)
    if not match:
        raise RuntimeError("Could not find version in pyproject.toml")
    return match.group(1)


@task
def bump_version(c, new_version=None):
    """Bump this project's version across every file that embeds it.

    Reads the current version from pyproject.toml. With no --new-version, prints
    the current version and the files that would change (dry run). Otherwise
    rewrites pyproject.toml, srg_sim/__init__.py, and docs/conf.py.

    Args:
        new_version: Target version string, e.g. 0.1.0 (no leading 'v').
    """
    current = _read_version()

    if not new_version:
        print(f"Current version (pyproject.toml): {current}")
        print("\nFiles that would be updated:")
        for path, *_ in VERSION_FILES:
            print(f"  {path}")
        print("\nRun: invoke bump-version --new-version X.Y.Z")
        return

    if not re.fullmatch(SEMVER, new_version):
        raise SystemExit(f"--new-version must look like X.Y.Z, got '{new_version}'")

    changed = []
    for path, pattern, tmpl in VERSION_FILES:
        p = Path(path)
        if not p.exists():
            continue
        text = p.read_text()
        updated = re.sub(pattern, tmpl.format(new=new_version), text, flags=re.MULTILINE)
        if updated != text:
            p.write_text(updated)
            changed.append(path)

    if not changed:
        print(f"No occurrences of {current} found — nothing changed.")
        return

    print(f"Bumped {current} -> {new_version} in:")
    for path in changed:
        print(f"  {path}")
    print(
        "\nNext: review `git diff`, commit, then "
        f"`git tag v{new_version} && git push && git push origin v{new_version}`"
    )


@task
def check(c):
    """Run the full gate: pre-commit hooks, type check, then tests."""
    c.run(f"{PY} -m pre_commit run --all-files", pty=True)
    c.run(f"{PY} -m mypy srg_sim", pty=True)
    c.run(f"{PY} -m pytest", pty=True)


@task
def build(c):
    """Build the sdist and wheel into dist/."""
    c.run(f"{PY} -m build", pty=True)


@task
def test(c):
    """Run the test suite."""
    c.run(f"{PY} -m pytest", pty=True)


@task
def docs(c, open_browser=False):
    """Build HTML documentation with Sphinx.

    Args:
        open_browser: Open the result in a browser after building.
    """
    c.run(f"{PY} -m sphinx -b html docs docs/_build/html", pty=True)
    if open_browser:
        c.run("xdg-open docs/_build/html/index.html", warn=True)
    else:
        print("Docs built: docs/_build/html/index.html")


@task(
    help={
        "a": "first competitor (name, substring, or db_uuid)",
        "b": "second competitor (name, substring, or db_uuid)",
        "pdf": "also build the xelatex PDF",
    }
)
def report(c, a, b, pdf=False):
    """Build a 2-competitor matchup report (Sphinx HTML, optional PDF).

    Example: ``invoke report --a Soborno --b "Mrs. Apocalypse" --pdf``
    """
    flag = " --pdf" if pdf else ""
    c.run(f'{PY} -m srg_sim.cli report "{a}" "{b}"{flag}', pty=True)


@task
def clean(c):
    """Remove build and test artifacts."""
    c.run("rm -rf dist build ./*.egg-info docs/_build .pytest_cache .mypy_cache .ruff_cache")
    print("Removed build/test artifacts.")
