Workflow
========

Everyday commands
-----------------

Development tasks are driven by `invoke <https://www.pyinvoke.org/>`_
(:file:`tasks.py`). Each task shells out to the interpreter running invoke — the
shared venv — so it works whether or not that venv is on ``PATH``. First install
the package::

    ~/data/stars/venv/bin/pip install -e ".[dev]"

.. list-table::
   :header-rows: 1
   :widths: 26 74

   * - Command
     - What it does
   * - ``invoke check``
     - pre-commit hooks (ruff + knots + hygiene) + ``mypy`` + ``pytest`` — the
       same gate CI runs.
   * - ``invoke test``
     - Run the test suite (``pytest``).
   * - ``invoke build``
     - Build the sdist and wheel into ``dist/``.
   * - ``invoke docs``
     - Build these docs to ``docs/_build/html`` (``--open-browser`` to view).
   * - ``invoke bump-version``
     - Bump the version across all files; dry-runs with no ``--new-version``.
   * - ``invoke clean``
     - Remove build and test artifacts.

Run ``invoke --list`` to see every task.

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

``.github/workflows/ci.yml`` runs the same checks ``invoke check`` runs (ruff
lint + format check, mypy, pytest) on Python 3.11 and 3.12, and builds the docs
with warnings treated as errors.
