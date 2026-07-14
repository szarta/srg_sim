"""Smoke tests: the package imports and advertises a version.

These bootstrap the test harness while ``srg_sim`` is still a set of design
stubs. Real engine tests arrive with M1 (see ``DESIGN.md`` §11).
"""

import srg_sim


def test_package_imports() -> None:
    assert srg_sim.__version__ == "0.0.1"
