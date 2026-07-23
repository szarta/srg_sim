Coverage grind playbook
=======================

The parser maps card ``rules_text`` to the Effect IR; anything it cannot map
becomes an explicit ``Unsupported`` node that surfaces in the coverage report
(never silently dropped — CLAUDE.md ground rule). *Grinding* that tail means
picking a recurring mechanic family (see :file:`docs/coverage-tail-audit.md` for
the ranked buckets) and either teaching the parser new grammar or modeling a
specific card in :file:`overrides.yaml`.

This page is the accumulated procedure and the traps — the things that are not
obvious from the code and cost a cycle to rediscover. It pairs with the audit:
the audit finds the families, this is how you land one.

.. contents::
   :local:
   :depth: 1


The override-authoring loop
---------------------------

An *override* models one card's gimmick as hand-written Effect IR, keyed by
``db_uuid``, when the parser grammar does not (yet) cover it. The single source
of truth is :file:`overrides.yaml` at the repo root.

#. Model the gimmick's Effect IR in :file:`overrides.yaml`. Author YAML with
   ``yaml.dump(..., sort_keys=False)`` — hand-emitters forget to quote the
   ``@type`` keys.
#. ``invoke overrides`` — regenerates the embedded :file:`overrides.ir.json`
   from :file:`overrides.yaml`.
#. ``invoke cards-ir`` — regenerates the whole-DB parser golden
   :file:`fixtures/parser/cards.ir.json`.
#. ``invoke parser-fixture`` — refreshes :file:`fixtures/parser/clauses.json`
   in place (keeps the curated inputs, recomputes the ``expected`` IR and the
   ``coverage_golden`` counts).
#. Review the fixture diffs, then ``invoke check`` (the CI gate).

.. warning::

   **The rebuild trap.** :file:`overrides.ir.json` is baked into the ``srg``
   binary via ``include_str!`` (:file:`src/console/loader.rs`) at *compile*
   time. A bare ``invoke overrides`` does **not** rebuild, so ``srg play`` /
   ``srg coverage`` keep emitting byte-identical output — the override looks
   like it "didn't apply". Run ``cargo build`` (or ``invoke check``, which
   rebuilds) before trusting behavior.

.. note::

   An override **replaces all of a card's clauses**. If a card has a trailing
   clause you are not modeling (``Shuffle your deck``, a play-order line), you
   must reproduce it or you silently drop it. Before accepting a
   ``grammar → 0`` coverage drop, confirm each dropped clause is a genuine
   no-op (e.g. ``act_search`` already shuffles).

Use ``~/data/stars/venv/bin/invoke`` — the ``invoke`` on ``PATH`` runs under an
interpreter with no ``yaml`` and dies on import. Pass ``--policy-a random``
(uniform over legal moves) to force an otherwise-unpicked ``Choice`` branch to
execute; the default heuristic always takes ``legal[0]``.


Adding an IR node or field
--------------------------

The Effect IR and game-log schema are cross-language contracts
(:file:`schemas/v1/`). Any change hits the CLAUDE.md **§3 review gate** and must
touch every mirror below or a test — or, worse, the other engine — drifts.

#. **DESIGN.md §3** — propose the node/field.
#. **schemas/v1/effect_ir.schema.json** — add the ``$def``, add a ``oneOf`` ref
   at *every* union site, and bump the internal ``"version"``. The schema is
   canonical ``json.dumps(indent=2, sort_keys=True)`` + trailing newline; edit
   it programmatically and re-dump. Nothing reads ``"version"`` at runtime — it
   is a pure contract marker, so the bump is always safe. ``oneOf`` ref counts
   by node kind (grep to confirm):

   .. list-table::
      :header-rows: 1
      :widths: 20 12 68

      * - Node kind
        - Ref sites
        - Where
      * - Condition
        - 7
        - IrNode + ``Condition`` union + ``And``-items + ``Or``-items +
          ``Not``-item + ``SetFinishRoll.condition`` + ``AlsoLead.condition``
      * - Action
        - 3
        - ``ChoiceOption.actions`` + ``Effect.actions`` + IRNode
      * - Trigger
        - 2
        - ``Trigger`` union + IrNode

#. **fixtures/ir/all_nodes.json** — add an instance, and bump the ``NN`` count
   in ``tests/ir_roundtrip.rs`` (it value-compares, so order is free).
#. **scripts/srg_ir/effects.py** ``SAMPLES`` list + import — the Python IR
   expander's node-registry guard. This is the test that breaks *first* and is
   easiest to forget.
#. **Both engines** — node defs + evaluator/dispatch. A marker action read
   outside ``apply_action`` still needs adding to the passive no-op list.
#. Override + ``invoke overrides``.

.. note::

   **Condition variants live in two Rust places** — the ``Condition`` enum
   *and* the flat ``IrNode`` union — with separate variant lists. A
   ``replace_all`` that hits only the enum leaves ``IrNode`` short and
   ``ir_roundtrip`` fails with ``unknown variant X``. Triggers likewise appear
   in a ``Trigger`` union and in ``IrNode`` (:file:`ir.rs` carries two copies of
   the OnStop-style envelope).

A **new enum variant** (not a node, not a field — e.g. ``Dest::DECK_TOP``,
``Duration::WHILE_IN_DISCARD``) has *zero* fixture churn: ``ir_roundtrip``
checks node types, not enum values, and existing instances keep their value.

**State fields** need extra care. A ``GameState`` or ``PlayerState`` field
(``last_roll_winner``, ``reroll_grants``, ``chosen_name``, ``timed_buffs``) must
land in both engines' state with ``#[serde(default)]`` (Rust) + a dataclass
default (Python) *and* be swept into every position of
:file:`fixtures/state/positions.json` (canonical ``sort_keys=False``, dataclass
field order) or ``tests/state.rs::snapshot_round_trips`` fails. An **engine-only
transient** (``hit_card``, ``stopped_card``, ``pending_roll_boost``,
``turn_bumped``) is not serialized — no ``positions.json`` churn. ``flags``-based
ad-hoc state (``flags["swap_grant_next"]``) is a serialized map, also no churn.

All Option fields serialize explicitly as ``null`` — this project has **no**
``skip_serializing_if`` (:file:`src/ir.rs` documents the convention). Use
``#[serde(default)]`` + a dataclass default so a field is optional on input but
always emitted.


Fixture-sweep map
-----------------

These committed fixtures embed IR. When you add a field to an existing node, how
many you touch depends entirely on how ubiquitous that node is — **measure, do
not assume** (``grep -o '"@type": *"X"' fixtures/**/*.json``).

.. list-table::
   :header-rows: 1
   :widths: 34 22 44

   * - Fixture
     - Dump convention
     - Guard test
   * - ``fixtures/ir/all_nodes.json``
     - insertion order
     - ``ir_roundtrip`` (value compare)
   * - ``fixtures/ir/deck_effects.json``
     - ``sort_keys=True``
     - ``decks_round_trip`` / parse tests
   * - ``fixtures/parser/clauses.json``
     - ``sort_keys=True``
     - ``parser.rs::parse_text_matches_oracle``
   * - ``fixtures/state/positions.json``
     - ``sort_keys=False`` (field order)
     - ``state.rs::snapshot_round_trips``
   * - ``fixtures/conformance/*.json`` (6 decks)
     - ``sort_keys=False`` (field order)
     - ``cards.rs::decks_round_trip``
   * - ``fixtures/parser/cards.ir.json`` (~7 MB)
     - regen via ``srg cards-ir``
     - ``parser_parity.rs`` (regression golden)

Churn tiers:

- **Additive node / new variant** — ``all_nodes`` +1 (and the roundtrip count).
  Frozen corpus stays byte-identical.
- **Rare-node field-add (3 fixtures)** — ``all_nodes`` + ``deck_effects`` +
  ``clauses.json``; no conformance or positions sweep. Applies to Bury, Discard,
  FinishRollBonus, Reroll, OnStop, ShuffleHandDraw (each only a handful of
  instances; OnStop and Reroll are 1 apiece).
- **Ubiquitous-node field-add (6 fixtures)** — script the field recursively into
  ``all_nodes`` + ``deck_effects`` + ``clauses.json`` + ``positions.json`` + the
  6 conformance decks. Applies to CardFilter (~217), OnHit (~165), Stop (~316),
  Effect, FrequencyGuard. Walk the JSON recursively — nodes nested inside
  ``Effect.actions`` / ``AddText.effects`` are easy to miss.

.. warning::

   Fixtures are **not** uniformly key-ordered. ``deck_effects`` and
   ``clauses.json`` round-trip through ``sort_keys=True`` (a new field lands in
   alphabetical position — ``last_turn_bumped`` after ``last_roll_winner``), but
   ``positions.json`` and the conformance decks use serde **declaration** order
   (insert the new key at the right struct position, e.g. after ``raw``). A
   ubiquitous-node field-add therefore needs byte-exact insertion across mixed
   orderings — a standing reason to prefer a sibling marker action over a field
   (see :ref:`patterns <coverage-grind:reusable patterns>`).

``engine_conformance.rs`` only *replays* recorded events, so it never re-emits a
CardFilter/node — only ``decks_round_trip`` needs the field in the conformance
decks. If an override lands on a card that appears in one of the six reference
decks, or the parser IR shape changes, regenerate ``clauses.json``; if the
override's uuid is in its 113-case sample, update that case's ``expected`` and
bump ``coverage_golden`` (override ±1 / unsupported ∓1). Most uuids are not in
the sample — check with a script.


Cross-language mirror gotchas
-----------------------------

:file:`src/ir.rs` is authoritative; ``scripts/srg_ir/effects.py`` (the lenient
override expander) must stay in lockstep. A Rust-native expander was tried and
failed — Rust IR deserialization is strict (the schema marks all
default-fillable fields required), so the default-filling front-end genuinely
needs Python ``from_dict``.

- A field named ``from`` collides with the Python keyword. ``IRNode.to_dict``
  emits ``f.name`` verbatim with no alias mechanism, so rename in **both**
  engines (``reveal_from``, ``from_skill``).
- ``&`` binds **looser** than ``==`` in Python: ``x & want == want`` parses as
  ``x & (want == want)``. Write ``(x & want) == want``.
- ``_buff_sources`` is a ``GameState`` method — call
  ``self.state._buff_sources(...)``, not ``self.``.
- ``yaml.dump`` emits ``&id001`` / ``*id001`` aliases when a generator reuses one
  dict across effects. Build fresh dicts per effect, or use a dumper with
  ``ignore_aliases`` — but note intentional YAML anchors (``&esh2_choice``)
  resolve on load and are fine.
- Shared helpers diverge tolerant-vs-strict: Rust ``bury_cards`` / ``pick_from``
  tolerate a miss; Python ``.remove()`` raises and ``_pick_from`` auto-takes
  ``len == 1``. When mirroring, check the *shared* helper's behavior, not just
  the new code (also present in ``add_from_discard``).
- Inline Rust test JSON for an action with no serde default must spell out every
  field — a ``Draw`` needs ``"per": null, "per_who": "SELF"`` or Deck
  deserialization fails (``overrides.ir.json`` is fine — Python always writes
  them).
- ``competitor`` / ``entrance`` are **frozen** dataclasses — mutate via
  ``replace(competitor, effects=...)``.
- Enum casing on the wire: ``AtkType`` serializes PascalCase (``Strike``,
  ``None``); domain/order enums are ``SCREAMING_SNAKE`` ``$defs``.
- ``Skill::ALL`` order equals Python ``list(Skill)`` order (Power, Agility,
  Technique, Submission, Grapple, Strike) — a serialized bitmask matches across
  engines only because of this.
- ``negate`` / ``flip_signs`` (Cassandra) must enumerate *every* signed field on
  a node — a new ``FinishRollBonus.when_base_le`` or ``MinHandSize`` each needs
  adding to ``negate_action`` and Python ``_SIGNED_DELTA``.
- Python's ``_ACTIONS`` dispatch captures ``Engine._method`` at class-def time —
  monkeypatching an instance does not intercept; patch ``_ACTIONS[fx.Node]``.


.. _coverage-grind:reusable patterns:

Reusable patterns
-----------------

Prefer these before reaching for new machinery.

- **Add a discriminant field to a near-twin node, not a new node.** When a
  one-off is a variation on something built last time, a field is cheaper churn
  and keeps the engines symmetric: ``RevealForDraw.match_on`` (STOP vs
  ROLLED_SKILL), ``Bury.source`` (DISCARD/HAND), ``Draw.cap`` /
  ``per_excludes_trigger``, ``FinishRollBonus.per``.
- **A sibling marker action beats a field on a ubiquitous node.** To gate an
  existing high-instance node without the six-fixture sweep, add a passive
  marker action beside it (no-op in ``apply_action``, read by a scan) — e.g.
  ``StopRequiresTag`` beside ``Stop`` instead of ``Stop.attacker_tag``.
- **The synthetic-tag pattern.** Fold a card DB attribute into a synthetic tag
  at *load* time (``card_tags`` in :file:`src/console/loader.rs`) and match it
  through the existing ``CardFilter{tag}`` predicate — zero schema bump, zero
  struct field, zero frozen-deck sweep (frozen decks are deserialized, not
  re-run through ``card_tags``). Used for ``"Spotlight"`` (``spotlight: true``)
  and ``"SkillRequirement"`` (a ``requirements:`` block). The go-to for any new
  boolean card attribute the engine must read.
- **Structural-scan actions read outside** ``apply_action`` — a no-op in the
  dispatcher, scanned at roll/finish points (``offer_rerolls``,
  ``offer_switches``, ``SwitchRolledSkill``). Filter the scan by discriminant: a
  ``Reroll`` with ``when=NEXT`` is a grant and must not fire *this* turn.
- **Never treat a bare gimmick** ``OnHit`` **as "when you hit a card".** The
  parser produces bare ``OnHit`` as fragment misattributions (a stray "Draw 1
  card" line). A source-based fix (``eff.source != Card``) fired those
  fragments on every hit. Gate real standing behavior behind an explicit
  override-only flag (``OnHit.on_any``) the parser never sets.
- **A trigger disjunction is two Effects sharing one action list** via a YAML
  anchor (``&esh2_choice``) — no OR-trigger node. A "choose 1 of N" card is
  ``ChooseName`` (StartOfMatch, binds ``PlayerState.chosen_name``) plus one
  ``Static`` per branch gated on ``ChosenNameIs``.
- **A derived-stats fold beats a reactive trigger.** Mimic ("when the opponent
  increases, you increase") is ``effective_stats`` folding
  ``max(0, opp_base_eff - opp_base)`` per skill — no event hook. Read the
  opponent's ``effective_stats_base`` to avoid recursion.
- **Persistent buffs live on the target** (``PlayerState.timed_buffs``,
  ``pending_text``, poison), not the granter — so the effect outlives the source
  card going to discard, and the single ``effective_stats`` chokepoint applies it
  to turn / Finish / breakout alike. Stacking identity is the granting
  ``raw_clause``.
- **Reuse a field an action already accepts but ignores** before extending —
  ``BuffSkill.cap``, ``act_blank_gimmick``'s ``duration`` were present-but-unread.

.. note::

   **The Engine holds no card index.** ``Engine::new`` takes only the two decks;
   there is no runtime cross-competitor lookup. Absorb-family gimmicks must be
   *baked* into the override (``AbsorbGimmick`` — re-bake when the absorbed card
   is later modeled); copy / live-target cards resolve live against the opponent
   via an all-players declaration scan. Global/match conditions (DQ, count-out,
   ``SwapCrowdMeter``) belong on the **entrance** (survives a gimmick blank), not
   ``competitor.effects`` (blankable).

Directional ``who``-dispatch that scans both players is the shared shape for
``run_on_stop_gimmicks`` (YOURS/THEIRS), ``run_on_bury``, ``OnBreakout``,
``OnHit.who``, ``OnShuffle``, ``OnDiscardMove`` — ``who`` is the pile/roll owner
from the effect owner's point of view. Preserve the ordering subtleties:
OnBreakout fires before ``discard_in_play``; the higher roll resolves first
(ties stable A-then-B); ``record_roll_ctx`` populates ``roll_ctx[key].skill``
before OnRoll fires; ``last_turn_bumped`` is read before ``finish_roll_off`` sets
it. Many mechanical paths bypass the action chokepoints for free
(pass-and-recycle ``bury_cards``, mulligan ``deck.extend``, hand-cap trim, bump
``self.draw()``), so OnBury / OnShuffle / SuppressOpponentDraw naturally do not
fire there.


Testing and verification gotchas
--------------------------------

.. warning::

   **Cross-engine ad-hoc log diffs do not match.** Rust and Python resolve and
   shuffle *non-frozen* decks differently — a baseline bull-vs-d2 game diverges
   from turn 0. The oracle is ``invoke conformance`` (parser parity + frozen
   whole-engine replay), **not** an ad-hoc game. Verify each engine fires a
   mechanic separately.

- **Never verify the gate through** ``head``. ``invoke check | grep | head`` has
  truncated before a failing test binary and reported a red run green. Redirect
  to a file and grep the whole thing, or trust the exit code.
- A reused smoke deck can emit a **colliding node** — the bull main-deck also
  emits ``OnRoll → ModifyRoll(delta=0)`` and a bare-``OnHit`` fragment. Filter
  behavioral assertions by the override's signature (delta / when).
- A reused ``ReplayDecider`` only works if the mechanic needs no decision;
  discard-exit paths hit ``target`` / ``bury`` / ``discard`` points and yield a
  ``DecisionRequest``. Add a local ``FirstLegal`` decider and stock hands (an
  empty-hand action no-ops → the test can pass vacuously — watch for that).
- **RNG-fragile assertions**: "the shed card is now in the deck" can be shuffled
  back and re-drawn — assert ``hand.len()`` instead. Some flags are inert in the
  bull smoke deck (no opponent-draw riders, no in-roll effects) → verify with a
  deterministic direct ``_act_*`` call with the flag injected.
- **Key a decision's zone lookup on the option's** ``owner``, **not the acting
  player** — pools can span the opponent's zone (``at_bury`` panicked
  "chosen card is in discard" on a ``who=OPP`` bury; same class as the
  ``bury_hand`` vs ``bury_opp_hand`` split). Every candidate option carries an
  ``owner``.
- **clippy**: the 7-argument limit (group into a ``DrawSpec`` /
  ``TimedBuff`` struct); ``doc_lazy_continuation`` — a doc line starting with
  ``+`` reads as a list item (write "plus"). Both run as pre-commit hooks.
- An edit-script that does several in-memory replacements then writes **once**
  silently discards earlier edits if a later replacement raises. Write after each
  edit, or grep-verify every intended edit landed.
- **Nothing validates fixtures against the schema** — a real gap
  (``OnHit`` never listing ``on_any`` under ``additionalProperties: false``)
  went unnoticed. Check the schema by hand when adding a field.


Finding the next family
-----------------------

- ``srg coverage --top96`` reports only clause *shapes*, not names. To recover
  names + uuids, cross-reference ``cards.yaml``
  (``~/data/srg_card_search_website/backend/app/cards.yaml``; the ``division``
  field marks top-96 = ``World Championship`` | ``Underworld``) against the
  parser IR :file:`fixtures/parser/cards.ir.json`, keyed by ``db_uuid``. Tally
  the ``Unsupported`` nodes (each carries a ``raw_text`` field) and group by
  normalized shape (digits → ``N``, skill names → ``<S>``).
- **Synthetic test deck**: copy any :file:`decks/*.yaml`, swap ``competitor:`` to
  the target (the engine does not enforce faction), ``srg play``, grep the JSONL
  log for the gimmick firing. Use Python ``re.sub`` for names containing ``&`` —
  ``sed`` treats ``&`` as the whole match.
- **Split-header clauses**: the parser splits multi-line ``rules_text``; a bare
  header like ``Once per turn roll:`` is a trigger *fragment* whose real effect
  is the body on the next line. Read the full ``rules_text`` before modeling.
- **Look for pure-override wins first**: ``+1 <skill> on turn rolls`` is
  ``InRoll(skill, SELF) + ModifyRoll(SELF, +1, THIS)``; ``+N until start of next
  turn`` is ``BuffSkill``; ``when you roll X, <action>`` is ``OnRoll``. A whole
  card override side-steps a grammar rule entirely — both parsers stay in parity
  with no grammar change.

.. warning::

   **Audit clause counts are inflated ~7×** by keyword conflation — a bucket
   lumps in a different mechanic (hand-disruption's "1451" was really ~200;
   #49/#39/#121 were all inflated by crowd-meter / name / capped / recur variants
   that belong to other buckets). Classify a phrase-matched set by *mechanic*
   before quoting its seam size, and ask for exemplars rather than trusting a
   keyword sweep.


Confirmed rules-calls
---------------------

Disambiguations the engine must honor (see also :file:`docs/` design notes and
the ``srg-rules`` references):

- **Numer01** is a *this-roll* modifier (the in-progress roll-off, not the
  recorded post-resolve context).
- **Name/text substring matching** is case-insensitive, pure substring (no word
  boundary — "Table" matches "Stable"), over both the card title and
  ``rules_text``. Extract keywords from ``rules_text`` at parse time; the
  ``wants_keywords`` DB field is not authoritative.
- **No-DQ scope**: SELF vs MATCH matters — self-immunity must not protect the
  opponent; a MATCH rule protects both. In-play-scoped, stacking in last-played
  order. A **stop** means a stop *card*; breaking out is a separate defense — do
  not conflate ``OnStop`` with breakout.
- **Scry / reveal**: the actor (gimmick owner) chooses even on the opponent's
  deck, and buries the *best* card there (sabotage); model look (count only) vs
  reveal (public ids) visibility.
- **OnShuffle** fires on any effect-caused shuffle (explicit or incidental
  post-search / tutor / shuffle-into-deck) but not the match-start setup shuffle
  nor the private bury-ordering shuffle.
- **Discard-pile bury** is a choice over the whole pile (a discard pile has no
  meaningful order; "random" picks at random).
- **Minimum hand size** is a floor on the *maximum*, not a draw-up floor:
  ``max(base + max_mods, MIN_HAND_SIZE(3) + min_mods)``. Default minimum is 3;
  ``HAND_CAP`` (max) base is 10.
- **Finish roll** in card text is the base roll (die → skill → competitor stat),
  pre-bonus. ``if your Finish roll is N or less`` gates the base; ``it is +N`` is
  a signed additive bonus (may be negative); a bare ``N`` is a *set* (distinct,
  left ``Unsupported``). Additive Finish consequents require an explicit sign.
- **Count-out is a win** (empty deck + hand on a won turn); ``CountOutRule``
  immunity mirrors DQ.
- **Poison** stays active until fulfilled even if the source card leaves the
  board; queue it on the target. "Their next turn" is the next turn the target
  is the active player.
- **SwitchRolledSkill** applies to the turn roll or the Finish roll, unlimited
  frequency, and happens before boosts / in-roll mods land.
