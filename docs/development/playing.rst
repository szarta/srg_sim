Playing a match and reviewing it
================================

|project| can be driven by a human at the terminal, not only by two policies.
You play one side seeing **only what a player at the table would know** — the
same redacted view the web frontend renders over the decision protocol; the
engine plays the other side. The full oracle state (the opponent's hand, deck
order, and RNG) can be surfaced alongside it for post-game critique, without
having influenced the live choice.

This is the loop behind todo #42: *learn how a human plays, and critique the
decisions after the fact.* Nothing here changes the game-log schema — an
interactive game is replayed and archived through the same **match record**
consumers use for engine-run and real-life-transcribed matches (see
:file:`schemas/v1/match_record.md`, :file:`DESIGN.md` §8.1).

The two information views
-------------------------

Every position has two projections of one shared engine state:

.. list-table::
   :header-rows: 1
   :widths: 18 82

   * - View
     - What it shows
   * - **Player view**
     - The per-seat observable frame the decision protocol sends you: your own
       hand in full; the opponent's hand and **both** decks as *counts only*
       (deck order is hidden from everyone, owner included). This is all ``srg
       repl`` shows you, so you can never accidentally act on hidden state.
   * - **Oracle view**
     - The loss-less full position: both hands, deck order, crowd meter, and the
       RNG state. What the engine sees; surfaced with ``repl --debug`` and
       recoverable by replaying a recorded match's frames.

Playing a match
---------------

Use ``repl``: name the two decks, pick which seat you play with ``--human`` and
the AI policy for the other seat with ``--opponent`` (``smart`` is the strongest
built-in). Write a JSONL observer transcript with ``--transcript`` so you (or
Claude) can review it later; add ``--debug`` to fold the full oracle state into
each decision::

    srg repl decks/bull.yaml decks/fae.yaml \
        --human A --opponent smart --seed 5 \
        --transcript game.jsonl

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

.. note::

   Playing against the engine is deliberately **not** coached: your decisions are
   captured unassisted so they are a clean signal of how you actually play. All
   critique happens *after* the match, from the transcript (todo #42, decision 2).

Reviewing a match
-----------------

The ``--transcript`` file is a JSONL feed of the raw wire traffic (each decision
point's observable frame, legal set, and the chosen action, with card names
resolved); ``--debug`` additionally stamps the loss-less full state at each
decision. Read the observable frame to reproduce the decision you faced, then the
debug oracle to score it against a line only hindsight allows (DESIGN.md §10 M4,
*"how a human differs"*). This is the artifact to hand to a reviewer — human or
Claude — for a post-game debrief.

For a portable, hand-authorable archive, record the match instead of (or in
addition to) transcribing it::

    srg record decks/bull.yaml decks/fae.yaml --out game.json --seed 5
    srg validate-record game.json --cards cards.yaml

A **match record** is the versioned interchange format consumers store and
replay: a sequence of observable frames plus a replay seed, so a viewer walks the
same frame sequence for an engine-run game and for a match transcribed from real
life. ``validate-record`` gates an imported or hand-authored archive (with
``--cards`` it also resolves every card uuid). See
:file:`schemas/v1/match_record.md`.

How it works (and why it needs no schema change)
------------------------------------------------

Because every random step flows through the seeded RNG and every human choice is
recorded as a decision, **replaying the recorded decisions reproduces the match
exactly**. A match record therefore stores only the observable frames and the
replay seed; the full oracle state is materialized **on demand** by re-running
the engine to any decision point — the "observable-state ref" DESIGN.md §8
promised — rather than baked into the archive, which would leak hidden state and
could not be hand-authored.

.. note::

   **Superseded.** The retired Python engine exposed this as a ``srg-sim review``
   command and a ``srg_sim.review.reconstruct`` API that snapshotted both views at
   each decision. In the Rust engine the same capability lives in the match-record
   format (``srg record`` / ``validate-record``) and the ``repl --transcript`` /
   ``--debug`` feed; there is no separate ``review`` subcommand.
