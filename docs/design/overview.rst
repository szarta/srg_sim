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
   * - ``cards``
     - Domain model — ``Card``, ``Competitor``, ``EntranceCard``, ``Deck``, enums.
   * - ``ir``
     - **Effect IR** — ``Trigger``, ``Condition``, ``Action``, ``Effect``, ``Unsupported``.
   * - ``conditions``
     - Condition evaluation against ``GameState`` (``holds``, card/filter matching).
   * - ``parser``
     - ``rules_text`` → ``[Effect]``: grammar + ``overrides.yaml`` + coverage report.
   * - ``state``
     - ``GameState`` / ``PlayerState`` with serializable snapshots + ``observable``.
   * - ``engine``
     - Turn loop, effect executor, stop resolution, finish sequence.
   * - ``session``
     - Resumable state machine over ``engine`` — ``open`` / ``submit`` / ``snapshot``.
   * - ``finish``
     - **Ported** from ``fae_comp`` — finish/breakout math.
   * - ``stops``
     - **Ported** from ``fae_comp`` — skill-stop online logic.
   * - ``rng``
     - Seeded portable PRNG (splitmix64): ``roll()``, ``shuffle()``, ``reveal()``.
   * - ``policy``
     - ``Policy`` trait + ``RandomPolicy``, ``HeuristicPolicy`` — where player skill lives.
   * - ``gamelog``
     - Game-log event schema, JSONL read/write, replay/verify.
   * - ``console`` (bin ``srg``)
     - CLI over ``srg-core``: ``srg play | coverage | analyze | replay | session | cards-ir``.

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

   Fold the relevant parts of the root ``DESIGN.md`` into this section now that
   the engine has landed, so the docs become the living reference and
   ``DESIGN.md`` settles into a historical review record.
