Matchup reports
===============

A **two-competitor matchup scorecard** — card art, turn-roll win %, per-Crowd-Meter
finish odds with finish images, the skill stops that come online in *this* matchup,
the most-open finish line, and the skill-requirement payoff cards a competitor
uniquely enables. All of the odds reuse the validated finish/breakout and skill-stop
math the engine runs — the report never re-derives it.

.. admonition:: Status — a consumer feature, not in ``srg`` today
   :class: important

   The scorecard was built by the **Python** ``srg-sim report`` command
   (``srg_sim.report/``). Per the substrate split it is a **consumer**
   (presentation, not rules), so it lives *outside* the ``srg-core`` engine
   crate; it was retired with the rest of the Python CLI surface at the Rust
   migration (task #79) and is **not** a ``srg`` subcommand. It is slated to be
   rebuilt as a consumer on top of ``srg-core``'s public API — most naturally in
   the **web presentation layer** (see :doc:`../design/substrate-split`,
   "Consumers"). The validated math it needs — turn-roll enumeration, finish and
   breakout odds, skill-stop evaluation — already lives in ``srg-core``; a rebuilt
   report calls the engine rather than re-deriving anything. The generated
   scorecards under ``docs/reports/<slug>/`` (git-ignored, excluded from the
   developer-docs build, embedding converted card art) are historical outputs of
   the Python generator, kept for reference.

The rest of this page is the **spec** the rebuild should honor.

What each section reports
-------------------------

- **Turn roll %** — the chance to win the opening roll-off. When neither competitor
  has an effect that touches a roll, it is an **exact** 6×6 face enumeration; when
  either does (a lowest-wins flip, a persistent skill buff, a comeback), it falls
  back to a seeded engine Monte-Carlo over the roll-off so every gimmick is honored.
- **Finish odds (CM0–5)** — for **every** signature finish, the success probability
  at each Crowd Meter (the engine's finish math), alongside the finish's card image
  and combo bonus. A **better logoless alternative** is listed only when a generic
  ``Logoless`` finish beats the signature over the **early** Crowd Meter (CM0–2),
  where finishes are actually contested — past CM2 everything saturates. The
  competitor's stats and gimmick text are *not* printed (they're on the card image).
- **Skill stops / most-open line** — whether the *defender* can skill-stop each
  attack type (the engine's stop evaluation), and the strongest line to throw (best
  odds among open lanes, at the contested Crowd Meter).
- **Key skill-requirement cards** — a hand-curated priority set (a
  ``skill_cards.yaml``): auto-include payoffs first, then the Equal-8 skill stops
  (critical in the equal-stat matchups). For each the report shows the requirement,
  whether the competitor can run it, and whether its stop is **live** for this stat
  line / matchup. Deckbuilding allows only two such cards, so only the top few are
  shown; the no-requirement disruption Leads (Apocalypse / Rejected!) are a standing
  note.

Honesty about coverage
----------------------

If a competitor's gimmick isn't yet modeled by the rules parser, the report must say
so in a prominent warning and note that the turn-roll odds and comp-type reflect the
**base stat line only** — the gimmick's raw text is still shown. Nothing is silently
dropped (:file:`DESIGN.md` §4).

Deferred
--------

Curated free-form notes, a "notable cards" list, and a full sample decklist are
authored per competitor (keyed by name or uuid); the comp-type label is auto-derived
and can be overridden there. A full 30-card sample-decklist generator is its own
later task — the report shows the signature + logoless finish pool in the meantime.
