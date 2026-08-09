The rule pipeline: clause → IR → engine
=======================================

The **map** of how a card's rules text becomes engine behavior, and where each
kind of change lands. It answers, for a clause in front of you: *what do I touch,
and what do I read first?* It pairs with two companions:

- :doc:`coverage-grind` — the *procedure and traps* (how to actually land a
  mechanic family, and the things that cost a cycle to rediscover).
- :file:`docs/development/grammar-catalog.md` — the generated *reference* of every
  grammar rule (regex + real example clauses). **Grep it before modeling a shape.**

.. contents::
   :local:
   :depth: 1

The flow
--------

A card's ``rules_text`` becomes engine behavior through five stages, all in
:file:`src/parser.rs` until the last::

    rules_text
      │  split_clauses            sentence / newline split
      ▼
    clauses ──► parse_text        strips frequency / window / metadata headers;
      │           runs the reveal-then / choice / compound composers; otherwise
      │           hands each clause to match_grammar
      ▼
    match_grammar ──► RULES       first regex match wins; RULES is the flattened
      │                           13 domain sub-tables (domain_tables)
      ▼
    Effect (IR)                   src/ir.rs — Trigger + Condition + [Action…] +
      │                           Duration + Frequency
      ▼
    engine executes the IR        src/engine/mod.rs — apply_action dispatch

Overrides short-circuit stages 2–3: a card keyed by ``db_uuid`` in
:file:`overrides.yaml` supplies its hand-authored IR directly (see
:doc:`coverage-grind`). Anything the parser cannot map **must** surface as an
``Action::Unsupported`` node — never a silent drop (CLAUDE.md ground rule) — and
shows up in the coverage report.

Where a change lands
--------------------

Pick the layer from the clause, not the card:

- **Recurring clause shape, many cards** → a new **grammar rule** (plus, as needed,
  a helper, an IR node, an engine handler, and a test). Covered in *every* deck that
  uses the shape, keyed by ``db_uuid``.
- **One-off or irregular card** → a bespoke **override** in :file:`overrides.yaml`
  (then ``invoke overrides``).
- **The shape needs a behavior the IR cannot express** → a new **IR node** + engine
  handler (+ a schema bump). See :doc:`coverage-grind`, "Adding an IR node or field".
- **The IR already expresses it but the engine mishandles it** → an **engine fix**
  only; no parser or IR change.

The five landing spots
----------------------

Grammar rules — regex → Effect
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

:file:`src/parser.rs`, ``domain_tables()`` → the 13 ``build_*_rules`` sub-tables
(``skill_buff``, ``draw_search``, ``turn_roll``, ``dq_loss``,
``flip_crowd_reroll``, ``flip_trigger``, ``bury_discard``, ``removal_hand``,
``recur``, ``unstoppable_draw``, ``reveal_alsolead``, ``finish_breakout``,
``stop_trigger``). Order is **precedence** — the first matching rule wins — so add
a rule next to its siblings of the same shape, and never reorder without
re-checking the parser golden. **Read first:** the grammar catalog; the phrasing
often already parses under a broader pattern.

Helper builders — the small constructors
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

:file:`src/parser.rs`, the 16 concern sections above ``domain_tables`` (effect /
trigger, card filters, reveal, draw/discard, roll mods, flip, scry, search, bury,
skill buffs, re-rolls, DQ/lose, hand size, conditions, trigger-body & gates, text
util). A rule closure builds its ``Effect`` from these; reuse an existing builder
(``draw``, ``bury``, ``buff``, ``reroll``, ``stop_eff``, …) before writing a new
one — scan the section for your concern.

Effect IR — the contract
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

:file:`src/ir.rs`: ``Action`` (the ~78-variant verb set), ``Effect`` (the wrapper),
``Trigger`` (when), ``Condition`` (gate), ``Duration``, ``Who``, ``CardFilter``,
and friends. The Effect IR and the game log are **cross-language contracts**
(:file:`schemas/v1/effect_ir.schema.json`, DESIGN.md §3): do not re-derive or quietly
alter them — a change bumps ``ir::SCHEMA_VERSION`` and the schema, and goes through
the DESIGN.md review gate.

Engine — executes the IR
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

:file:`src/engine/mod.rs`, one ``impl Engine``: ``apply_action`` dispatches each
``Action`` to an ``act_*`` method; the roll / finish / breakout / stop machinery
lives in the same impl (``roll_off``, ``finish_sequence``, ``breakout``,
``apply_stop``, …). A new ``Action`` variant means a new match arm **and** an
``act_*`` method.

Tests & goldens
~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

- **Unit tests:** :file:`src/engine/tests.rs` — the ``*_tests`` modules; add one
  that drives your behavior end to end.
- **Goldens** (regression, replayed inside ``cargo test``): the parser golden
  :file:`fixtures/parser/cards.ir.json` (``tests/parser_parity.rs``); the
  whole-engine conformance corpus :file:`fixtures/conformance/`
  (``tests/engine_conformance.rs``); the rule inventory
  :file:`fixtures/parser/rule_index.json` (``tests/grammar_catalog.rs``).

A clause end to end
-------------------

*"Draw 1 card"*:

1. **parse** — matches ``Draw (\d+) cards?`` in the ``draw_search`` sub-table
   (``build_draw_search_rules``).
2. **helper** — the closure builds the action via ``draw()`` (Draw & discard
   section).
3. **IR** — an ``Effect`` (Trigger ``OnPlay``, ``Duration::Instant``) carrying
   ``Action::Draw``.
4. **engine** — ``apply_action`` routes it to ``act_draw``, which moves cards to
   the hand.
5. **test** — an engine ``*_tests`` module asserts the hand grew; the parser golden
   pins the compiled IR so the mapping can't drift.

After a change: regenerate
--------------------------

- **Parser / grammar change** → ``invoke cards-ir`` (parser golden) **and**
  ``invoke grammar-catalog`` (readable catalog + gated ``rule_index.json``); review
  both diffs.
- **Curated parser sample** → ``invoke parser-fixture``.
- **IR node / schema change** → bump ``ir::SCHEMA_VERSION`` and the matching file in
  :file:`schemas/v1/`; the DESIGN.md review gate applies.
- **Gate** — ``invoke check && invoke test`` must be green before committing.

See also
--------

- :doc:`coverage-grind` — the procedure, the traps, and finding the next family.
- :doc:`/coverage-tail-audit` — the ranked ``Unsupported`` buckets.
- :file:`docs/development/grammar-catalog.md` — the generated grammar reference.
- ``DESIGN.md`` §3 (Effect IR), §6 (finish / stops / engine), §8 (game log).
