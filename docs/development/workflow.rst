Workflow
========

Everyday commands
-----------------

Run from the repository root (they target the shared venv):

.. list-table::
   :header-rows: 1
   :widths: 22 78

   * - Command
     - What it does
   * - ``make dev``
     - Install |project| + dev deps into the venv (editable).
   * - ``make lint``
     - ``ruff check`` over ``srg_sim`` and ``tests``.
   * - ``make fmt``
     - Auto-format and apply ``ruff`` fixes.
   * - ``make typecheck``
     - ``mypy srg_sim`` (strict; see ``pyproject.toml``).
   * - ``make test``
     - ``pytest``.
   * - ``make check``
     - lint + typecheck + test — the same gate CI runs.
   * - ``make docs``
     - Build these docs to ``docs/_build/html``.
   * - ``make precommit``
     - Run all pre-commit hooks against every file.

Pre-commit
----------

Install the git hooks once per clone::

    ~/data/stars/venv/bin/pre-commit install

The configured hooks (see :file:`.pre-commit-config.yaml`):

* standard hygiene — trailing whitespace, EOF fixer, large-file / YAML / TOML /
  merge-conflict checks;
* **ruff** — lint (with ``--fix``) and format;
* **knots** — code-complexity gate (Python only), prebuilt PyPI wheel so the
  first run is fast. The knots source of truth lives at ``~/data/knots``.

Continuous integration
----------------------

``.github/workflows/ci.yml`` runs the ``check`` gate (ruff lint + format check,
mypy, pytest) on Python 3.11 and 3.12, and builds the docs with warnings treated
as errors.
