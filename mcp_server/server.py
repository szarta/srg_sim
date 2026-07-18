"""MCP server wrapping the `srg` console engine (task 77).

Follows the house pattern (cf. `todo-sqlite-cli/mcp_server`): a thin FastMCP server
that shells out to the Rust `srg` binary and returns its JSON — no engine logic here,
and no MCP/async deps in the Rust tree.

A match is *stateful* across decisions, but a `Session` is a pure function of its
serializable snapshot (`srg session …` is stateless and snapshot-threaded). So this
server keeps the snapshot per session in memory, keyed by a `session_id` it hands the
caller, and threads it back into `submit`/`observe` — the model shuttles a small id, not
a whole board.

Resolution:
  * binary  — `SRG_BIN` env var, else `srg` on PATH.
  * card DB — `SRG_CARDS` env var (passed as `--cards`), else the `srg` default snapshot.
"""

import json
import os
import subprocess
import uuid

from mcp.server.fastmcp import FastMCP

BIN = os.environ.get("SRG_BIN", "srg")
CARDS = os.environ.get("SRG_CARDS")

mcp = FastMCP("srg-supershow")

# session_id -> the latest SessionSnapshot (dict), threaded back into the CLI.
_SESSIONS: dict[str, dict] = {}


def _run(*args: str, stdin: str | None = None) -> str:
    """Run `srg <args>`, raise RuntimeError on non-zero exit, return stdout."""
    result = subprocess.run(
        [BIN, *args], input=stdin, capture_output=True, text=True
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or f"srg exited {result.returncode}")
    return result.stdout.strip()


def _cards() -> list[str]:
    return ["--cards", CARDS] if CARDS else []


# ---------------------------------------------------------------------------
# Interactive session (the decision protocol)
# ---------------------------------------------------------------------------


@mcp.tool()
def open_session(
    deck_a: str,
    deck_b: str,
    seed: int = 0,
    seat_a: str = "remote",
    seat_b: str = "heuristic",
) -> str:
    """Open a match between two decklists and return {session_id, step} as JSON.

    deck_a / deck_b: paths to decklist YAML files.
    seat_a / seat_b: "remote" (a human/agent answers via submit) or a policy name
        (random|heuristic|aggressive|smart|newbie) for a local AI opponent.
    seed: RNG seed for a reproducible match.

    `step` is either {"kind":"decision","request":{viewer, point, legal, ...}} — the
    outstanding choice, whose `legal` list is indexed by `submit` — or
    {"kind":"done","result":{winner, reason, turns}} if no seat is remote.
    """
    out = json.loads(
        _run("session", "open", deck_a, deck_b, "--seed", str(seed),
             "--seat-a", seat_a, "--seat-b", seat_b, *_cards())
    )
    session_id = uuid.uuid4().hex
    _SESSIONS[session_id] = out["snapshot"]
    return json.dumps({"session_id": session_id, "step": out["step"]})


@mcp.tool()
def submit(session_id: str, choice_index: int) -> str:
    """Answer the outstanding decision with option `choice_index` of its `legal` list.

    Returns {session_id, step} for the next decision (or the final result). The
    session's snapshot is advanced in place.
    """
    snapshot = _session(session_id)
    out = json.loads(
        _run("session", "submit", "--choice-index", str(choice_index),
             stdin=json.dumps(snapshot))
    )
    _SESSIONS[session_id] = out["snapshot"]
    return json.dumps({"session_id": session_id, "step": out["step"]})


@mcp.tool()
def observe(session_id: str) -> str:
    """Re-fetch the session's current `step` (the outstanding decision or result).

    Read-only: does not advance the match. Returns the `step` JSON, including the
    deciding player's observable state.
    """
    snapshot = _session(session_id)
    out = json.loads(_run("session", "observe", stdin=json.dumps(snapshot)))
    return json.dumps(out["step"])


def _session(session_id: str) -> dict:
    snapshot = _SESSIONS.get(session_id)
    if snapshot is None:
        raise RuntimeError(f"unknown session_id {session_id!r} (open a session first)")
    return snapshot


# ---------------------------------------------------------------------------
# Batch analysis (stateless)
# ---------------------------------------------------------------------------


@mcp.tool()
def analyze(
    deck_a: str,
    deck_b: str,
    games: int = 100,
    seed_start: int = 0,
    policy_a: str = "heuristic",
    policy_b: str = "heuristic",
) -> str:
    """Batch `games` seeded matches between two decklists; return the win-rate summary.

    policy_a / policy_b: random|heuristic|aggressive|smart|newbie.
    """
    return _run(
        "analyze", deck_a, deck_b, "--games", str(games),
        "--seed-start", str(seed_start), "--policy-a", policy_a,
        "--policy-b", policy_b, *_cards(),
    )


@mcp.tool()
def coverage(top96: bool = False) -> str:
    """Rules-parser coverage report over the card DB (grammar/override/unsupported).

    top96: also report the top-96 competitor subset.
    """
    args = ["coverage", *(["--top96"] if top96 else []), *_cards()]
    return _run(*args)


def main() -> None:
    mcp.run()


if __name__ == "__main__":
    main()
