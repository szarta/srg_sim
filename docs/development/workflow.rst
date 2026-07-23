Workflow
========

Everyday commands
-----------------

Development tasks are driven by `invoke <https://www.pyinvoke.org/>`_
(:file:`tasks.py`), a thin wrapper over ``cargo``. Each task shells out to
``cargo``, so the wrapper adds convenience (and the fixture-regen steps), not a
second build system. Run them by path so the shared venv's ``invoke`` is used
(see :doc:`environment`)::

    ~/data/stars/venv/bin/invoke check

.. list-table::
   :header-rows: 1
   :widths: 26 74

   * - Command
     - What it does
   * - ``invoke check``
     - The full CI gate: pre-commit hooks (``cargo fmt`` + ``cargo clippy`` +
       knots) followed by ``cargo test``.
   * - ``invoke test``
     - Run the test suite (``cargo test``).
   * - ``invoke build``
     - ``cargo build`` (debug by default; ``--release`` for optimized).
   * - ``invoke overrides``
     - Regenerate the embedded :file:`overrides.ir.json` from
       :file:`overrides.yaml` (the single authoring source).
   * - ``invoke cards-ir``
     - Regenerate the whole-DB parser golden
       :file:`fixtures/parser/cards.ir.json` from the Rust parser.
   * - ``invoke parser-fixture``
     - Refresh the curated parser regression sample
       :file:`fixtures/parser/clauses.json`.
   * - ``invoke wasm``
     - Build the web WASM package (``srg-core`` ``wasm`` feature →
       ``web/src/pkg`` via wasm-bindgen).
   * - ``invoke bump-version``
     - Bump the crate version in :file:`Cargo.toml`; prints the current version
       with no ``--new-version``.

Run ``~/data/stars/venv/bin/invoke --list`` to see every task. When a parser or
IR change lands, the usual sequence is ``invoke overrides`` → ``invoke
cards-ir`` → ``invoke parser-fixture`` → ``invoke check`` (see
:doc:`coverage-grind`).

Pre-commit
----------

Install the git hooks once per clone::

    ~/data/stars/venv/bin/pre-commit install

The configured hooks (see :file:`.pre-commit-config.yaml`):

* standard hygiene — trailing whitespace, end-of-file fixer, large-file /
  YAML / TOML / merge-conflict / line-ending checks;
* **cargo fmt** — ``cargo fmt --all -- --check`` (formatting gate);
* **cargo clippy** — ``cargo clippy --all-targets --all-features -- -D
  warnings`` (lints are errors);
* **knots** — the code-complexity gate; keep functions small. The knots source
  of truth lives at ``~/data/knots``.

Continuous integration
----------------------

:file:`.github/workflows/ci.yml` installs the pinned Rust toolchain (with
``clippy`` and ``rustfmt``) and runs the same checks the gate runs — ``cargo fmt
--all -- --check``, ``cargo clippy --all-targets --all-features -- -D
warnings``, ``cargo build --all-targets --locked``, and ``cargo test --locked``.
