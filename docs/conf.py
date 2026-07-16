"""Sphinx configuration for the srg_sim developer docs."""

project = "srg_sim"
author = "Brandon Arrendondo"
release = "0.0.1"
copyright = "2026, Brandon Arrendondo"

extensions = [
    "sphinx.ext.autosectionlabel",
    "sphinx.ext.todo",
]

autosectionlabel_prefix_document = True
todo_include_todos = True

templates_path = ["_templates"]
# ``reports/`` holds generated, self-contained per-matchup Sphinx projects
# (``srg-sim report``); the developer-docs build must not descend into them.
exclude_patterns = ["_build", "Thumbs.db", ".DS_Store", "reports"]

html_theme = "alabaster"
html_static_path = ["_static"]

html_theme_options = {
    "description": "Headless, deterministic Supershow match simulator.",
    "github_user": "szarta",
    "github_repo": "srg_sim",
    "fixed_sidebar": True,
}

rst_prolog = """
.. |project| replace:: srg_sim
"""
