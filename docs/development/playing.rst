Playing a match and reviewing it
================================

|project| can be driven by a human at the terminal, not only by two policies.
You play one side seeing **only what a player at the table would know**; the
engine plays the other. Afterwards a **review** pass reconstructs, for every
decision you made, both that redacted view *and* the full oracle state — the
opponent's hand, deck order and all — so a stronger line can be judged in
hindsight without having influenced the live choice.

This is the loop behind todo #42: *learn how a human plays, and critique the
decisions after the fact.* Nothing here changes the game-log schema — a human
match is an ordinary log with ``kind: "real"`` and ``policy: "human"`` (see
:file:`DESIGN.md` §7/§8).

The two information views
-------------------------

Every position has two projections of one shared ``GameState``:

.. list-table::
   :header-rows: 1
   :widths: 18 82

   * - View
     - What it shows
   * - **Player view**
     - ``GameState.observable(you)`` — your own hand in full; the opponent's
       hand and **both** decks as *counts only* (deck order is hidden from
       everyone, owner included). This is what ``HumanPolicy`` renders, so you
       can never accidentally act on hidden state.
   * - **Oracle view**
     - ``GameState.to_dict()`` — the complete position: both hands, deck order,
       crowd meter, and the RNG state. What the engine sees, and what the review
       recovers for critique.

Playing a match
---------------

Pass ``human`` as a policy to the ``play`` command. You play that side; the other
side is whichever engine policy you name (``smart`` is the strongest built-in).
Write the log with ``--out`` so you can review it later::

    srg-sim play decks/bull.yaml decks/fae.yaml \
        --seed 5 --policy-a human --policy-b smart \
        --out game.jsonl

At each of your decisions you are shown the player view and a numbered menu of
the legal options; type the option number. For example::

    ── turn 2  ·  crowd meter 0  ·  you are A ──
    opponent B: Fae Dragon
      in play: (empty)
      hand: 4 cards   deck: 26   discard: 0
    you A: The Bull
      in play: (empty)
      deck: 25   discard: 0
      hand: #27 A Card 27 (Lead/Submission), #10 A Card 10 (Lead/Strike), ...

    decision: turn_action
      1) play #27 (Lead/Submission)
      2) play #10 (Lead/Strike)
      3) pass
    choose [1-3]:

The engine marks the resulting log ``kind: "real"`` (a human took at least one
decision), which is what tells ``review`` — and you — that it is a played match
rather than a simulated one.

.. note::

   Playing against the engine is deliberately **not** coached: your decisions are
   captured unassisted so they are a clean signal of how you actually play. All
   critique happens *after* the match, from the review (todo #42, decision 2).

Reviewing a match
-----------------

``review`` replays the recorded match and reconstructs both views at every
decision. Restrict to your own side with ``--player`` and export the records as
newline-delimited JSON with ``--ndjson``::

    srg-sim review game.jsonl --player A --ndjson review_A.ndjson

::

    review: real log, 385 events — B wins by finish in 41 turns
      26 decision(s) reconstructed (player A)
      ndjson: review_A.ndjson

Each NDJSON line is one decision::

    {
      "turn": 2, "point": "turn_action", "player": "A", "policy": "human",
      "legal":  [ ...the options you chose among... ],
      "chosen": { "kind": "play", "number": 27, ... },
      "player_view": { ...observable(A): opponent hand as a count... },
      "oracle":      { ...full state: opponent's actual hand + deck order... }
    }

Read ``player_view`` to reproduce the decision you faced, then ``oracle`` to score
it against a line only hindsight allows (DESIGN.md §10 M4, *"how a human
differs"*). This is the artifact to hand to a reviewer — human or Claude — for a
post-game debrief, and the same records feed the imitation-learning export
(todo #36).

How it works (and why it needs no schema change)
------------------------------------------------

Because every random step flows through the seeded RNG and every human choice is
recorded as a ``decision`` event, **replaying the recorded decisions reproduces
the match exactly** — byte-for-byte. Two small pieces exploit that:

* :class:`srg_sim.policy.ReplayPolicy` feeds a side's recorded ``chosen`` options
  back in order, so any recorded match (``sim`` *or* ``real``) is deterministically
  replayable — a human game included, which a plain seed-replay cannot do.
* :func:`srg_sim.review.reconstruct` drives the engine with a ``ReplayPolicy`` per
  side and snapshots ``observable(key)`` and ``to_dict()`` at the instant the
  engine consults the policy. That instant is the *only* place the two views line
  up with the recorded choice, so the oracle state is materialized **on demand**
  rather than stored in the log — the "observable-state ref" DESIGN.md §8 promised.

The programmatic entry point mirrors the CLI::

    from srg_sim.gamelog import GameLog
    from srg_sim.loader import CardIndex
    from srg_sim import rules_parser as rp
    from srg_sim.review import reconstruct

    log = GameLog.read("game.jsonl")
    index = CardIndex.from_yaml()          # defaults to the card-DB snapshot
    recon = reconstruct(log, index, rp.load_overrides())

    for rec in recon.for_player("A"):      # your decisions, in order
        print(rec.turn, rec.point, rec.chosen)
        rec.player_view                    # what you saw
        rec.oracle                         # the full truth

``reconstruct`` rebuilds the decks from the log header via the card index; a
lower-level :func:`srg_sim.review.reconstruct_with_decks` takes already-compiled
decks, so the reconstruction is testable without the card DB.
