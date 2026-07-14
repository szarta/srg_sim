Architecture at a glance
========================

A one-screen orientation. For the full, authoritative treatment read
:file:`DESIGN.md`.

The shape of the system
-----------------------

.. list-table::
   :header-rows: 1
   :widths: 20 80

   * - Module
     - Responsibility
   * - ``cards.py``
     - Domain model — ``Card``, ``Competitor``, ``EntranceCard``, ``Deck``, enums.
   * - ``loader.py``
     - Card DB → index; resolve a decklist file into a ``Deck``.
   * - ``effects.py``
     - **Effect IR** — ``Trigger``, ``Condition``, ``Action``, ``Effect``, ``Unsupported``.
   * - ``rules_parser.py``
     - ``rules_text`` → ``[Effect]``: grammar + ``overrides.yaml`` + coverage report.
   * - ``state.py``
     - ``GameState`` / ``PlayerState`` with serializable snapshots.
   * - ``engine.py``
     - Turn loop, effect executor, stop resolution, finish sequence.
   * - ``finish.py``
     - **Ported** from ``fae_comp/supershow.py`` — finish/breakout math.
   * - ``stops.py``
     - **Ported** from ``fae_comp/skill_stops.py`` — skill-stop online logic.
   * - ``rng.py``
     - Seeded RNG wrapper: ``roll()``, ``shuffle()``, ``reveal()``.
   * - ``policy.py``
     - ``Policy`` ABC + ``RandomPolicy``, ``HeuristicPolicy`` — where player skill lives.
   * - ``gamelog.py``
     - Game-log event schema, JSONL read/write, replay/verify.
   * - ``cli.py``
     - ``srg-sim play | coverage | replay``.

Two decisions everything hinges on
----------------------------------

**The Effect IR** (:file:`DESIGN.md` §3). Cards, competitor gimmicks, and
Entrance effects all compile to one typed ``(trigger, condition, actions)`` IR.
The engine executes *only* IR, never raw text.

**The game-log schema** (:file:`DESIGN.md` §8). One JSON-Lines schema serves
both simulated and recorded-human games, so a real match can be transcribed in
the same format and later used to fit a human-like policy. ``decision`` events
(legal set + chosen action + observable state) are the imitation-learning
dataset.

.. todo::

   Fold the relevant parts of the root ``DESIGN.md`` into this section as the
   engine lands, so the docs become the living reference and ``DESIGN.md``
   settles into a historical review record.
