Open questions
==============

Tracked here so they are not silently guessed at. Mirrors :file:`DESIGN.md`
§12; confirm and fold answers back into the design as we hit each one.

Resolved
--------

* **Win/loss conditions** — finish, count-out (a *win* on exhausting deck + hand
  on a won turn), disqualification, pinfall.
* **"Hit a card"** — a card resolving into play: an unstopped played card, or a
  stop entering play (the stop is itself hit).
* **Buff duration** — ``WHILE_IN_PLAY`` for card sources, ``WHILE_GIMMICK_ACTIVE``
  for gimmicks; buffs apply to the unified derived-stats view.
* **Incremental-value cards** — covered by parsing the full card DB during
  build-up; anything unparsed flags ``Unsupported``.

Still open
----------

.. todo::

   Exact interaction of some gimmicks with multi-roll breakouts and with the
   ordering stack (buffs that change mid-breakout; effects that add attempts).

.. todo::

   Ordering-stack edge cases — how many same-stage cards may stack, and stop
   timing against a stacked chain.

.. todo::

   Simultaneity / priority when both players have triggered effects on the same
   event.
