Analyzing a matchup
===================

Where ``play`` runs one game, ``analyze`` runs *many* — a batch of seeded games
for a fixed pairing of two decks and two policies — and aggregates them into a
single :class:`~srg_sim.analysis.MatchupReport`. This is the M2 analysis bench:
it turns "who wins this matchup, and how" from an anecdote into a number with a
confidence interval (:file:`DESIGN.md` §10 M2).

Because every game is a **pure function of its seed** (all randomness flows
through the seeded RNG), a batch is reproducible and order-independent: game *i*
uses seed ``seed-start + i`` and depends on nothing else. Re-running the same
command yields byte-identical results, and spreading the games across processes
changes nothing but the wall-clock.

Running a batch
---------------

Point ``analyze`` at two decklists and ask for ``N`` games::

    srg-sim analyze decks/bull.yaml decks/fae.yaml --games 500

::

    analyze: The Bull (heuristic) vs Fae Dragon (heuristic) — 500 games (seeds 0-499)
      wins: A 271 (54.2%, CI 49.8%-58.5%)  B 229 (45.8%, CI 41.5%-50.2%)  draw 0
      reasons: finish 468, count_out 20, turn_cap 12
      finish types: Grapple 190, Strike 168, Submission 110
      length (turns): min 9  mean 34.1  median 33  max 78
      stops/game: A 3.10  B 2.88

Useful flags:

.. list-table::
   :header-rows: 1
   :widths: 24 76

   * - Flag
     - Effect
   * - ``--games N``
     - Number of seeded games to play (default 100).
   * - ``--seed-start S``
     - First seed; the batch uses ``S .. S+N-1`` (default 0). Shift it to draw a
       fresh, non-overlapping sample of the same matchup.
   * - ``--policy-a`` / ``--policy-b``
     - Which policy plays each side (``random``, ``heuristic``, ``aggressive``,
       ``smart``, ``newbie``). Pit two skill levels against each other to isolate
       a deck's floor from its ceiling.
   * - ``--jobs J``
     - Fan the games out over ``J`` worker processes. Results stay ordered by
       seed, so a parallel run reproduces the serial one exactly; ``J`` = 1 (the
       default) stays in-process. Use it for large ``N``.
   * - ``--json PATH`` / ``--csv PATH``
     - Also write the full report as JSON, or as a long-format
       (``metric,value``) CSV for a notebook.

An A/B deck diff is just two runs that hold the policies fixed and vary one
deck: compare the win-rate confidence intervals to see whether a build change
moved the needle beyond noise.

The MatchupReport schema
------------------------

:meth:`MatchupReport.from_outcomes <srg_sim.analysis.MatchupReport.from_outcomes>`
builds the report from a batch that kept its logs
(``run_batch(..., keep_logs=True)``); every log-derived metric needs the event
stream, so an outcome without a log is rejected.
:meth:`~srg_sim.analysis.MatchupReport.to_dict` yields the JSON-ready view the
``--json`` export writes:

.. list-table::
   :header-rows: 1
   :widths: 24 76

   * - Field
     - Meaning
   * - ``games``
     - Number of games in the batch.
   * - ``wins``
     - ``{"A", "B", "draw"}`` → win counts.
   * - ``win_rate``
     - Per side, wins as a fraction of all games.
   * - ``win_ci``
     - Per side, the Wilson 95% score interval ``[lo, hi]`` for the win rate —
       preferred over the normal approximation at the small ``N`` and extreme
       rates a lopsided matchup produces.
   * - ``reasons``
     - How games ended: ``finish`` | ``count_out`` | ``disqualification`` |
       ``pinfall`` | ``turn_cap`` → count.
   * - ``finish_types``
     - Among finish wins, the winning finish's attack type (``Strike`` |
       ``Grapple`` | ``Submission``) → count.
   * - ``length``
     - ``min`` | ``max`` | ``mean`` | ``median`` of game length in turns.
   * - ``stops``
     - Mean number of stops each side plays per game.
   * - ``crowd_meter_curve``
     - Mean crowd-meter value by turn index across games — a *ragged* curve, since
       later indices average only the games that reached that turn.

The programmatic entry point mirrors the CLI::

    from srg_sim.analysis import Matchup, MatchupReport, run_batch, seed_range
    from srg_sim.loader import CardIndex, load_deck
    from srg_sim import rules_parser as rp
    from srg_sim.policy import HeuristicPolicy, SmartPasser

    index = CardIndex.from_yaml()                 # defaults to the card-DB snapshot
    overrides = rp.load_overrides()
    deck = lambda ref: rp.enrich_deck(load_deck(ref, index).deck, overrides)

    matchup = Matchup(
        deck("decks/bull.yaml"), deck("decks/fae.yaml"),
        policy_a=HeuristicPolicy, policy_b=SmartPasser,   # factories, not instances
    )
    outcomes = run_batch(matchup, seed_range(500), keep_logs=True, jobs=4)
    report = MatchupReport.from_outcomes(outcomes)
    print(report.win_rate, report.win_ci)

Policies are supplied as **factories** (not instances) so every game gets a
fresh pair — determinism holds even for a policy that carries per-game state.

Exporting the decisions
-----------------------

The same batch that answers "who wins" also records **how** each side played:
every ``decision`` event carries the observable state, the legal set, and the
chosen action — the free imitation-learning signal (:file:`DESIGN.md` §7/§8). The
``export`` command flattens one or more logs to newline-delimited JSON, one
honest training example per decision::

    srg-sim export game1.jsonl game2.jsonl --player A --out decisions.ndjson

Each line is ``{observable_state, legal, chosen, policy, point, player, turn}`` —
the per-seat :meth:`~srg_sim.state.GameState.observable` view only, with **no**
oracle leak, so the training signal never sees a hidden zone. This is the M4
dataset ``LearnedPolicy`` consumes; see also :doc:`playing` for the richer,
oracle-carrying ``review`` records used for post-game critique.
