Environment
===========

Virtualenv
----------

|project| uses the **shared virtualenv** at ``~/data/stars/venv``. Do **not**
create a new one — it wastes disk. All tooling (``ruff``, ``pre-commit``,
``sphinx-build``) is expected to be available there.

Install |project| and its dev dependencies (``pytest``, ``mypy``, ``ruff``)
into that venv in editable mode::

    make dev
    # equivalent to: ~/data/stars/venv/bin/pip install -e ".[dev]"

The repo-root :file:`Makefile` points every target at that venv. Override the
location for a one-off with ``make VENV=/path/to/venv <target>``.

Card database
-------------

The **source of authority for card data** is the PostgreSQL database that backs
the SRG card-search website and mobile app:

    ``~/data/srg_card_search_website/backend/app``

That database (connection ``postgresql://…@localhost/srg_cards``, see
``backend/app/database.py``) is updated often as cards are added and corrected.
A YAML export (``backend/app/cards.yaml``) is regenerated from it and is the
convenient read-only snapshot the loader consumes.

.. note::

   The working assumption is that **anyone using this tool also has a checkout
   of the** ``srg_card_search_website`` **repo and access to that database.**
   |project| does not vendor a copy of the card data.

Task tracking
-------------

Tasks are tracked with `todo-sqlite-cli
<https://crates.io/crates/todo-sqlite-cli>`_, backed by ``todo-sqlite-cli.db``
in the repo root (resolved via the ``.todo-sqlite-cli`` marker). Common commands::

    todo-sqlite-cli list        # active work
    todo-sqlite-cli next        # the single next task
    todo-sqlite-cli add "..."   # add a task
    make todo                   # shortcut for `list`
