Working with Claude / coding agents
===================================

This project is built with heavy use of coding agents (Claude Code and
friends). :file:`CLAUDE.md` in the repo root is the short, always-loaded brief;
this page is the longer version.

Ground rules
------------

* **Read** :file:`DESIGN.md` **first.** It is the review gate. The Effect IR
  (§3) and game-log schema (§8) are the two expensive-to-change decisions —
  never re-derive or quietly alter them; propose changes against the doc.
* **Do not re-derive the math.** Finish/breakout and skill-stop logic are
  ported *verbatim* from the validated ``fae_comp`` modules (see the sources
  listed in :file:`README.md`). Port with their self-checks; do not reinvent.
* **Never silently drop a rule.** Anything the parser cannot map becomes an
  explicit ``Unsupported`` sentinel that shows up in the coverage report — no
  gimmick is ever silently mis-played.
* **Use the shared venv** at ``~/data/stars/venv``; do not create another.

Task tracking
-------------

Use ``todo-sqlite-cli`` (marker + DB already committed) as the working task
list. The agent-friendly flow::

    todo-sqlite-cli next                 # what to work on
    todo-sqlite-cli start <id>           # mark in-progress
    todo-sqlite-cli done <id>            # mark complete

Keep tasks scoped to the milestones in :file:`DESIGN.md` §10.

Before you commit
-----------------

Run ``make check`` (lint + typecheck + test) and ``make precommit``. Both must
be green. The ``knots`` complexity gate will reject overly complex functions —
prefer small, well-named helpers that read like the surrounding code.
