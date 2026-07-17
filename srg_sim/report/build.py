"""Build a matchup report: data -> RST + images -> Sphinx HTML (and xelatex PDF).

Each report is a self-contained mini-Sphinx project under ``out_root/<slug>/`` (the
fae_comp per-pod pattern), so it builds independently of the developer docs. Card
art is transcoded WebP->PNG into ``_images/`` for xelatex compatibility.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

from srg_sim.loader import DEFAULT_CARDS_YAML
from srg_sim.report import images as img
from srg_sim.report.carddb import ReportCardDB
from srg_sim.report.glance import render_glance, render_glance_book
from srg_sim.report.model import MatchupData, build_matchup
from srg_sim.report.render import render_report


def slugify(text: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


def build_report(
    name_a: str,
    name_b: str,
    *,
    cards_path: str | Path = DEFAULT_CARDS_YAML,
    cms: tuple[int, ...] = (0, 1, 2, 3, 4, 5),
    mc_games: int = 50_000,
    seed: int = 11,
    out_root: str | Path = Path("docs/reports"),
    html: bool = True,
    pdf: bool = False,
) -> Path:
    """Compute + render a matchup report; return its output directory."""
    db = ReportCardDB.from_yaml(cards_path)
    data = build_matchup(db, name_a, name_b, cms=cms, mc_games=mc_games, seed=seed)
    out_dir = Path(out_root) / slugify(data.title)
    out_dir.mkdir(parents=True, exist_ok=True)
    images = _convert_images(db, data, out_dir / "_images")
    (out_dir / "index.rst").write_text(render_report(data, images))
    (out_dir / "conf.py").write_text(_conf_py(data.title))
    if html:
        _sphinx(out_dir, "-b", "html", out_dir / "_build" / "html")
    if pdf:
        _sphinx(out_dir, "-M", "latexpdf", out_dir / "_build")
    return out_dir


def build_glance(
    name_a: str,
    name_b: str,
    *,
    cards_path: str | Path = DEFAULT_CARDS_YAML,
    cms: tuple[int, ...] = (0, 1, 2, 3, 4, 5),
    mc_games: int = 50_000,
    seed: int = 11,
    out_root: str | Path = Path("docs/reports"),
    html: bool = True,
    pdf: bool = True,
) -> Path:
    """Compute + render a one-page scouting card; return its output directory.

    A self-contained mini-Sphinx project under ``out_root/<slug>-glance/`` (separate
    from the full report), so it produces its own single-page PDF."""
    db = ReportCardDB.from_yaml(cards_path)
    data = build_matchup(db, name_a, name_b, cms=cms, mc_games=mc_games, seed=seed)
    out_dir = Path(out_root) / (slugify(data.title) + "-glance")
    out_dir.mkdir(parents=True, exist_ok=True)
    images = _convert_comp_images(data, out_dir / "_images")
    (out_dir / "index.rst").write_text(render_glance(data, images))
    (out_dir / "conf.py").write_text(_conf_py(f"{data.title} — Scouting Card", toc=False))
    if html:
        _sphinx(out_dir, "-b", "html", out_dir / "_build" / "html")
    if pdf:
        _sphinx(out_dir, "-M", "latexpdf", out_dir / "_build")
    return out_dir


def build_glance_book(
    matchups: list[tuple[str, str]],
    *,
    cards_path: str | Path = DEFAULT_CARDS_YAML,
    cms: tuple[int, ...] = (0, 1, 2, 3, 4, 5),
    mc_games: int = 50_000,
    seed: int = 11,
    out_root: str | Path = Path("docs/reports"),
    out_name: str = "team-scouting-report",
    title: str = "Team Scouting Report",
    html: bool = True,
    pdf: bool = True,
) -> Path:
    """Combine many matchups' scouting cards into one multi-page report; return its dir.

    A single self-contained Sphinx project under ``out_root/<out_name>/`` — a card per
    matchup (each on its own page), with a table of contents listing every matchup."""
    db = ReportCardDB.from_yaml(cards_path)
    datas = [build_matchup(db, a, b, cms=cms, mc_games=mc_games, seed=seed) for a, b in matchups]
    out_dir = Path(out_root) / out_name
    out_dir.mkdir(parents=True, exist_ok=True)
    images: dict[str, str] = {}
    for data in datas:  # dedup across matchups: a competitor reused keeps one PNG
        images.update(_convert_comp_images(data, out_dir / "_images"))
    (out_dir / "index.rst").write_text(render_glance_book(datas, images, title=title))
    (out_dir / "conf.py").write_text(_conf_py(title, toc=True))
    if html:
        _sphinx(out_dir, "-b", "html", out_dir / "_build" / "html")
    if pdf:
        _sphinx(out_dir, "-M", "latexpdf", out_dir / "_build")
    return out_dir


def _convert_comp_images(data: MatchupData, dest: Path) -> dict[str, str]:
    """Transcode only the two competitor portraits to PNG (the scouting card's art)."""
    out: dict[str, str] = {}
    for side in (data.a, data.b):
        _add_image(out, side.comp.db_uuid, dest, "fullsize")
    return out


def _convert_images(db: ReportCardDB, data: MatchupData, dest: Path) -> dict[str, str]:
    """Transcode the competitor + signature-finish art to PNG; map uuid -> rel path."""
    out: dict[str, str] = {}
    for side in (data.a, data.b):
        _add_image(out, side.comp.db_uuid, dest, "fullsize")
        for opt in side.signature_finishes:
            _add_image(out, opt.finish.db_uuid, dest, "mobile")
    return out


def _add_image(out: dict[str, str], uuid: str, dest: Path, size: str) -> None:
    png = img.ensure_png(uuid, dest, size)
    if png is not None:
        out[uuid] = f"_images/{png.name}"


def _sphinx(src: Path, mode: str, target: str, out: Path) -> None:
    subprocess.run(
        [sys.executable, "-m", "sphinx", mode, target, str(src), str(out)],
        check=True,
    )


# Per-report conf.py: xelatex + the \DUrole* color macros the verdict roles use
# (adapted from fae_comp/pdf/conf.py).
def _conf_py(title: str, toc: bool = True) -> str:
    # The one-pager suppresses the local table of contents (empty "Contents" heading).
    toc_line = "" if toc else '    "tableofcontents": "",\n'
    return f'''# Auto-generated by srg_sim.report.build — do not edit by hand.
project = {title!r}
extensions = []
exclude_patterns = ["_build"]
html_theme = "alabaster"

latex_engine = "xelatex"
latex_documents = [("index", "matchup.tex", {title!r}, "SRG Supershow Report", "howto")]
latex_show_urls = "no"
latex_elements = {{
{toc_line}    "papersize": "letterpaper",
    "pointsize": "10pt",
    "sphinxsetup": "hmargin=0.6in,vmargin=0.7in",
    "preamble": r"""
\\usepackage[table]{{xcolor}}
\\definecolor{{favcol}}{{HTML}}{{1F7A43}}
\\definecolor{{leancol}}{{HTML}}{{4E8A2E}}
\\definecolor{{evencol}}{{HTML}}{{9A6A12}}
\\definecolor{{unfavcol}}{{HTML}}{{A5362B}}
\\newcommand{{\\DUrolefav}}[1]{{\\textbf{{\\textcolor{{favcol}}{{#1}}}}}}
\\newcommand{{\\DUrolelean}}[1]{{\\textbf{{\\textcolor{{leancol}}{{#1}}}}}}
\\newcommand{{\\DUroleeven}}[1]{{\\textcolor{{evencol}}{{#1}}}}
\\newcommand{{\\DUroleunfav}}[1]{{\\textbf{{\\textcolor{{unfavcol}}{{#1}}}}}}
""",
}}
'''
