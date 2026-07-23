srg_sim — Developer Documentation
=================================

|project| is a headless, deterministic **Rust** match engine (crate
``srg-core``, library ``srg_core``, binary ``srg``) that plays two 30-card
**SRG Supershow** decks against each other, emits a fully-serialized, replayable
game log, and serves as an analysis bench for finding strengths and weaknesses
in a matchup or a deck build. It is the authoritative rules core; consumers
(console, MCP, WASM/web, mobile) sit on top.

This site holds **longer-term design notes and developer/agent helpers**. The
authoritative, pinned architecture — the Effect IR and game-log schema that are
expensive to change — lives in :file:`DESIGN.md` at the repository root, which
is the review gate for any change to those cross-language contracts.

.. toctree::
   :maxdepth: 2
   :caption: Development

   development/index
   coverage-tail-audit

.. toctree::
   :maxdepth: 2
   :caption: Design Notes

   design/index

Indices
-------

* :ref:`genindex`
* :ref:`search`
