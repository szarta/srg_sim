"""Relocated Effect-IR authoring tooling (the override expander's dependency).

A self-contained copy of the frozen Effect-IR dataclasses (``effects.py``) and the
three card enums it needs (``cards.py``), lifted out of the retired ``srg_sim_python``
oracle so ``scripts/gen_overrides_ir.py`` can validate + default-fill + canonicalize
``overrides.yaml`` without reaching into that archived checkout. The authoritative IR
is ``src/ir.rs``; this Python mirror exists only to expand the hand-authored override
table into the machine-read ``overrides.ir.json`` the engine embeds (schemas/v1 is the
cross-language contract both sides validate against). Keep it in lockstep with
``src/ir.rs`` when a node changes — the same dual edit an IR change already requires.
"""
