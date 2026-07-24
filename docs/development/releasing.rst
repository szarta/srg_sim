Releasing to get-diced
======================

How a change to ``srg-core`` (new rules coverage, a fixed override, a schema
bump) reaches the live site at `get-diced.com <https://get-diced.com>`_.

Two consumers, one engine
-------------------------

The production site runs the *same* ``srg-core`` build two ways, and a release
has to refresh **both** from the **same commit** so their enriched-deck /
``observable_state`` schemas cannot skew:

.. list-table::
   :header-rows: 1
   :widths: 20 34 46

   * - Consumer
     - What it uses
     - How it is built
   * - **Backend** (``srg-backend.service``, gunicorn/uvicorn)
     - The **native** ``srg`` release binary — shelled per request by
       :file:`backend/app/rib_engine.py` (``srg session open`` to enrich stored
       decks). It is resolved as ``SRG_BIN`` → ``srg`` on ``PATH`` → a dev
       checkout's ``target/`` tree.
     - ``cargo install`` on prod to a ``PATH`` location (``/usr/local/bin/srg``).
       The binary is **not** in git — a ``git pull`` never delivers it, so it
       must be rebuilt/reinstalled in place.
   * - **Frontend** ("Run It Back")
     - The **WASM** package (``srg_core_bg.wasm`` + ``srg_core.js``) — imported
       by :file:`frontend/src/runitback/engine.js`.
     - ``invoke wasm`` writes :file:`web/src/pkg` (committed); that copy is
       vendored into the frontend and bundled by ``vite build``.

Because the overrides and grammar are compiled *into* both artifacts,
publishing new coverage is a **rebuild**, not a data sync — there is no
override file the running services read at runtime.

Production topology
-------------------

Host ``prod-1.get-diced.com`` (user ``dondo``); reach it with ``ssh
get-diced``. Relevant paths:

.. list-table::
   :header-rows: 1
   :widths: 40 60

   * - Path
     - Role
   * - ``~/srg_sim``
     - The engine checkout (``main``). Source of the binary and the WASM pkg.
   * - ``/usr/local/bin/srg``
     - The installed native binary the backend shells, found on ``PATH`` (see
       below). ``cargo install`` puts it here.
   * - ``~/srg_sim/web/src/pkg``
     - Freshly built WASM, committed. The frontend's vendored copy is taken
       from here.
   * - ``~/srg_card_search_website/frontend/src/runitback/pkg``
     - The frontend's **vendored copy** of the pkg (a plain copy, not a
       symlink — it must be refreshed on each release).
   * - ``~/srg_card_search_website/frontend/dist``
     - ``vite build`` output. **nginx serves this directly** (``root`` in
       :file:`/etc/nginx/sites-enabled/srg.conf`) — no app server to restart
       for a frontend change.

The backend resolves the binary in this order (``rib_engine._srg_bin``):

1. ``SRG_BIN`` env var, if set — an explicit override.
2. ``srg`` on ``PATH`` (``shutil.which``) — the prod shape. A ``cargo
   install``ed ``/usr/local/bin/srg`` is on the systemd service's default
   ``PATH`` (``/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin``),
   so it resolves with **no env var required**.
3. ``<SRG_SIM_DIR>/target/{release,debug}/srg`` — a dev-checkout fallback only.

So prod needs neither ``SRG_BIN`` nor a specific checkout layout: install ``srg``
to ``/usr/local/bin`` and the ``PATH`` lookup finds it. (``SRG_BIN`` remains a
valid pin if you ever want to run a binary from somewhere off ``PATH``.) Note
that ``~/.cargo/bin`` — where a bare ``cargo install`` lands — is **not** on the
service's ``PATH``, which is why the install target is ``/usr/local/bin``.

First-time prod setup (one-time cutover)
----------------------------------------

Do this once to move prod onto the ``PATH``-installed binary. Until it is done,
an ``SRG_BIN`` pinned into the old build tree **shadows** the ``PATH`` lookup, so
the ``cargo install`` step below would have no effect.

1. Install the binary to ``/usr/local/bin`` (see step 2 of the runbook).
2. Retire the ``SRG_BIN`` override so the ``PATH`` lookup governs. It lives in the
   service's environment (a ``systemctl edit`` drop-in that also holds secrets —
   edit it interactively, do not script it)::

       sudo systemctl edit srg-backend.service   # remove the SRG_BIN=… token
       sudo systemctl daemon-reload
       sudo systemctl restart srg-backend.service

   (Equivalently, repoint it to ``SRG_BIN=/usr/local/bin/srg`` if you prefer an
   explicit pin over the ``PATH`` lookup.)

**Optional — passwordless release commands.** The recurring release touches a few
``sudo`` commands (install the binary, restart the service). To run them without a
password prompt, a helper installs a *scoped* ``/etc/sudoers.d`` drop-in (only
those exact commands; validated with ``visudo``)::

    sudo bash ~/srg-release-sudoers.sh            # grant
    sudo bash ~/srg-release-sudoers.sh --remove   # revoke when done

Release runbook
---------------

Run on the developer machine first, then on prod.

**1. On the dev machine — build, verify, commit.**

.. code-block:: bash

   cd ~/data/srg_sim
   invoke check                 # the full gate must be green
   invoke release-web           # rebuilds target/release/srg AND web/src/pkg from one tree
   git add web/src/pkg overrides.yaml overrides.ir.json fixtures/
   git commit                   # ship the refreshed pkg with the source change
   git push

``invoke release-web`` prints the commit stamp both artifacts carry; the WASM
``version()`` and ``srg info`` will report the same ``commit`` and ``schemas``.

**2. On prod — pull and install the native binary.**

.. code-block:: bash

   ssh get-diced
   cd ~/srg_sim
   git pull
   cargo install --path . --bin srg          # builds release -> ~/.cargo/bin/srg (reuses the build cache)
   sudo install -m 755 ~/.cargo/bin/srg /usr/local/bin/srg   # onto the service PATH

The backend picks this up on its next request (it re-execs the binary each
time); it is found via the ``PATH`` lookup with no ``SRG_BIN`` needed. Building
as ``dondo`` and ``sudo install``-ing the artifact avoids running ``cargo`` as
root.

If you also want the on-prod WASM pkg rebuilt at the pulled commit (rather than
trusting the committed one, which lags one commit by construction), run
``invoke wasm`` here too — otherwise step 3 just copies the committed pkg.

**3. Publish the frontend — copy the pkg and build.**

.. code-block:: bash

   cp ~/srg_sim/web/src/pkg/srg_core_bg.wasm \
      ~/srg_sim/web/src/pkg/srg_core.js \
      ~/srg_card_search_website/frontend/src/runitback/pkg/
   cd ~/srg_card_search_website/frontend
   npm run build                # -> dist/ ; nginx serves it immediately (hashed asset names)

**4. Restart the backend** so the deck-enrichment path uses the new binary.

.. code-block:: bash

   sudo systemctl restart srg-backend.service

The backend shells the binary fresh per request, so this is belt-and-suspenders
rather than strictly required — but restart to be certain no worker is holding a
stale path or a cached response.

Verify the release
------------------

.. code-block:: bash

   # same commit + schema versions on both sides — no skew
   ~/srg_sim/target/release/srg info               # backend binary
   # frontend: DevTools console on the play screen -> the engine logs version()

Both should report the commit you pushed and identical ``schemas`` numbers. For
a coverage change, confirm the fix end-to-end with the deck audit before
pushing::

   srg audit decks/<a>.yaml decks/<b>.yaml --games 30   # 0 unmodeled, 0 no-ops

What each kind of change touches
--------------------------------

.. list-table::
   :header-rows: 1
   :widths: 42 58

   * - Change
     - Rebuild needed
   * - New override / grammar (rules coverage)
     - Both artifacts (steps 1–4). No schema bump if only existing IR nodes are
       used.
   * - Effect-IR / ``observable_state`` schema bump
     - Both artifacts **together** — a skew here breaks the wire contract. Never
       ship one side ahead of the other.
   * - Frontend-only (UI, no engine change)
     - Step 3 only (``npm run build``); no pkg copy, no binary rebuild.
   * - Card DB refresh (new ``cards.yaml``)
     - Backend deck enrichment picks it up via ``SRG_CARDS`` / the bundled
       snapshot; no engine rebuild unless coverage also changed.
