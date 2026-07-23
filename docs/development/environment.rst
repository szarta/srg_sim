Environment
===========

Rust toolchain
--------------

|project| is a Rust crate. The toolchain is pinned by
:file:`rust-toolchain.toml` (``stable`` + ``clippy`` and ``rustfmt``); ``cargo``
picks it up automatically, so a plain ``cargo build`` / ``cargo test`` is all
that is required to compile and run the engine.

Shared virtualenv (tooling only)
--------------------------------

The developer *tooling* — ``invoke`` (the :file:`tasks.py` wrapper over
``cargo``) and ``pre-commit`` (which also builds these docs with
``sphinx-build``) — runs through the **shared virtualenv** at
``~/data/stars/venv``. Do **not** create a new one — it wastes disk. Invoke it
by path::

    ~/data/stars/venv/bin/invoke check

The crate itself is **not** a Python package — there is nothing to
``pip install``. The venv exists only so ``invoke`` / ``pre-commit`` are
available; every task they run shells out to ``cargo``. See :doc:`workflow`.

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
