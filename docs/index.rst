srg_sim — Developer Documentation
=================================

|project| is a headless, deterministic Python simulator that plays two 30-card
**SRG Supershow** decks against each other, emits a fully-serialized, replayable
game log, and serves as an analysis bench for finding strengths and weaknesses
in a matchup or a deck build.

This site holds **longer-term design notes and developer/agent helpers**. The
authoritative, pinned architecture — the Effect IR and game-log schema that are
expensive to change — lives in :file:`DESIGN.md` at the repository root, which
is the review gate before the engine is implemented.

.. toctree::
   :maxdepth: 2
   :caption: Development

   development/index

.. toctree::
   :maxdepth: 2
   :caption: Design Notes

   design/index

Indices
-------

* :ref:`genindex`
* :ref:`search`
