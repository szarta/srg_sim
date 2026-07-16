Matchup reports
===============

``srg-sim report`` builds a **two-competitor matchup scorecard** — a self-contained
Sphinx project rendered to HTML and, with ``--pdf``, a xelatex PDF. It is the
expanded successor to the old ``fae_comp`` scorecards: for each competitor it pulls
the card art, the turn-roll win %, per-Crowd-Meter finish odds with finish images,
the skill stops that come online in *this* matchup, the most-open finish line, and
the skill-requirement payoff cards the competitor uniquely enables. All of the odds
reuse the validated finish/breakout and skill-stop math the engine runs — the
report never re-derives it.

Building a report
-----------------

::

    srg-sim report "Soborno" "Mrs. Apocalypse" --pdf

::

    report: docs/reports/soborno-vs-mrs-apocalypse
      html: docs/reports/soborno-vs-mrs-apocalypse/_build/html/index.html
      pdf:  docs/reports/soborno-vs-mrs-apocalypse/_build/latex/matchup.pdf

Each competitor is resolved by name (exact, then unique substring) or ``db_uuid``,
so ``"Mrs. Apocalypse"`` and ``"Soborno"`` both work, and an ambiguous fragment
lists its candidates. Reports are generated under ``docs/reports/<slug>/`` — a
directory that is git-ignored and excluded from the developer-docs build, since it
embeds converted card art (not vendored; see :file:`CLAUDE.md`).

Flags:

.. list-table::
   :header-rows: 1
   :widths: 22 78

   * - Flag
     - Effect
   * - ``--pdf``
     - Also build the xelatex PDF (needs ``xelatex`` + ImageMagick ``convert`` on PATH).
   * - ``--no-html``
     - Skip the HTML build (just write ``index.rst`` + ``conf.py``).
   * - ``--cm 1-5``
     - Crowd-Meter range (or ``--cm 1,3,5`` list) for the finish-odds columns.
   * - ``--mc N``
     - Monte-Carlo rolls for the turn-odds sim fallback (default 50 000).
   * - ``--out DIR``
     - Output root (default ``docs/reports``).
   * - ``--cards PATH``
     - Card export to resolve against (default: the DB snapshot).

An ``invoke`` wrapper mirrors the CLI::

    invoke report --a Soborno --b "Mrs. Apocalypse" --pdf

What each section reports
-------------------------

- **Turn roll %** — the chance to win the opening roll-off. When neither competitor
  has an effect that touches a roll, it is an **exact** 6×6 face enumeration; when
  either does (a lowest-wins flip, a persistent skill buff, a comeback), it falls
  back to a seeded engine Monte-Carlo (``Engine._turn_roll``) so every gimmick is
  honored.
- **Finish odds (CM1–5)** — for each signature finish, the success probability at
  each Crowd Meter from :func:`srg_sim.finish.finish_odds`, alongside the finish's
  card image and combo bonus. A **better logoless alternative** is listed only when
  a generic ``Logoless`` finish beats the signature across the whole CM curve.
- **Skill stops / most-open line** — whether the *defender* can skill-stop each
  attack type (:func:`srg_sim.stops.evaluate_stop`), and the strongest line to throw
  (best odds among open lanes).
- **Key skill-requirement cards** — the payoff cards gated behind a
  ``Skill Requirement:`` the competitor satisfies, ranked to lean into its standout
  skills.

Honesty about coverage
-----------------------

If a competitor's gimmick isn't yet modeled by the rules parser, the report says
so in a prominent warning and notes that the turn-roll odds and comp-type reflect
the **base stat line only** — the gimmick's raw text is still shown. Nothing is
silently dropped (:file:`DESIGN.md` §4).

Deferred (Phase 2)
------------------

Curated free-form notes, a "notable cards" list, and a full sample decklist are
authored per competitor in ``srg_sim/report/overrides.yaml`` (keyed by name or
uuid); the comp-type label is auto-derived now and can be overridden there. A full
30-card sample-decklist generator is its own later task — the report shows the
signature + logoless finish pool in the meantime.

Programmatic entry point
------------------------

::

    from srg_sim.report.carddb import ReportCardDB
    from srg_sim.report.model import build_matchup

    db = ReportCardDB.from_yaml()               # defaults to the card-DB snapshot
    data = build_matchup(db, "Soborno", "Mrs. Apocalypse")
    print(data.turn.method, data.a.turn_win, data.b.turn_win)
    for line in data.a.finish_lines:
        print(line.atk_type, line.best and line.best.finish.name, line.open_lane)

:func:`srg_sim.report.build.build_report` wraps this with image conversion and the
Sphinx HTML/PDF build.
