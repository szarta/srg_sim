Analyzing a matchup
===================

Where ``play`` runs one game, ``analyze`` runs *many* — a batch of seeded games
for a fixed pairing of two decks and two policies — and aggregates them into a
win-rate summary. This is the M2 analysis bench: it turns "who wins this
matchup, and how" from an anecdote into a number (:file:`DESIGN.md` §10 M2).

Because every game is a **pure function of its seed** (all randomness flows
through the seeded RNG), a batch is reproducible and order-independent: game *i*
uses seed ``seed-start + i`` and depends on nothing else. Re-running the same
command yields identical results.

Running a batch
---------------

Point ``analyze`` at two decklists and ask for ``N`` games::

    srg analyze decks/bull.yaml decks/warehouse.yaml --games 500

::

    analyze: The Bull (heuristic) vs Warehouse (heuristic) — 500 games (seeds 0..500)
      A wins     271  (54.2%)
      B wins     229  (45.8%)
      draws        0  (0.0%)
      avg turns 34.1
      by reason:
          468  finish
           20  count_out
           12  turn_cap

Flags:

.. list-table::
   :header-rows: 1
   :widths: 26 74

   * - Flag
     - Effect
   * - ``--games N``
     - Number of seeded games to play (default 100).
   * - ``--seed-start S``
     - First seed; the batch uses ``S .. S+N-1`` (default 0). Shift it to draw a
       fresh, non-overlapping sample of the same matchup.
   * - ``--policy-a`` / ``--policy-b``
     - Which policy plays each side (``random``, ``heuristic``, ``aggressive``,
       ``smart``, ``newbie``; default ``heuristic``). Pit two skill levels
       against each other to isolate a deck's floor from its ceiling.
   * - ``--cards PATH``
     - Card-DB snapshot to resolve against (defaults to the bundled
       ``cards.yaml``).

An A/B deck diff is just two runs that hold the policies fixed and vary one
deck: compare the win rates to see whether a build change moved the needle.

.. note::

   **Not yet ported from the M2 Python bench.** The retired Python engine's
   ``analyze`` also emitted a structured ``MatchupReport`` (``--json`` / ``--csv``)
   with Wilson win-rate confidence intervals, finish-type breakdowns,
   stops-per-game, and a crowd-meter-by-turn curve, plus a ``--jobs`` fan-out
   across processes. The Rust CLI currently prints only the win/reason/length
   summary above. The richer report is tracked M2 work (task #18, the A/B deck
   diff); it is not implemented in ``srg`` today.

Exporting the decisions
-----------------------

The engine records **how** each side played, not just who won: every decision
point carries the observable state, the legal set, and the chosen action — the
free imitation-learning signal (:file:`DESIGN.md` §7/§8). Today that stream is
reachable one step at a time through the **decision protocol**: ``srg session``
(``open`` / ``submit`` / ``observe``) drives a resumable match over the same
wire the web frontend uses, and ``srg repl --transcript FILE`` writes a
Claude-observable JSONL feed of an interactive game. The per-seat observable
view carries **no** oracle leak, so a training signal built from it never sees a
hidden zone.

.. note::

   **Not yet ported.** The retired Python engine had an ``export`` command that
   flattened game logs to newline-delimited per-seat training examples
   (``{observable_state, legal, chosen, policy, point, player, turn}``) — the M4
   ``LearnedPolicy`` dataset. There is no bulk ``srg export`` yet; the decision
   protocol above is the current source of the same signal. See :doc:`playing`
   for interactive play and match-record capture.
