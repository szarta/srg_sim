"""Card-image resolver + WebP->PNG cache for the report.

The card DB stores art as sharded WebP: ``images/{size}/{uuid[:2]}/{uuid}.webp``.
WebP renders fine in HTML but xelatex needs PNG, so :func:`ensure_png` transcodes
on demand (ImageMagick ``convert``) into the report's ``_images/`` dir, skipping
work when the PNG is already newer than its source.
"""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path

# The card-DB image root (not vendored; every dev has the card-search repo).
IMAGES_ROOT = Path("/home/brandon/data/srg_card_search_website/backend/app/images").expanduser()

_SIZES = ("fullsize", "mobile", "thumbnails")


def source_webp(uuid: str, size: str = "fullsize", root: Path = IMAGES_ROOT) -> Path:
    """The source WebP path for ``uuid`` (sharded by its first two hex chars)."""
    if size not in _SIZES:
        raise ValueError(f"unknown image size {size!r}; choose from {_SIZES}")
    return root / size / uuid[:2] / f"{uuid}.webp"


def converter_available() -> bool:
    """Whether ImageMagick ``convert`` is on PATH (WebP->PNG transcode)."""
    return shutil.which("convert") is not None


def ensure_png(
    uuid: str, dest_dir: Path, size: str = "fullsize", root: Path = IMAGES_ROOT
) -> Path | None:
    """Transcode ``uuid``'s WebP art to ``dest_dir/<uuid>.png`` and return the path.

    Returns ``None`` when the source art or the ``convert`` tool is missing (the
    renderer then omits the image rather than failing the whole report). The PNG is
    reused when it is already newer than the source (mtime-gated, like the DB cache).
    """
    src = source_webp(uuid, size, root)
    if not src.exists() or not converter_available():
        return None
    dest_dir.mkdir(parents=True, exist_ok=True)
    dest = dest_dir / f"{uuid}.png"
    if dest.exists() and dest.stat().st_mtime >= src.stat().st_mtime:
        return dest
    subprocess.run(["convert", str(src), str(dest)], check=True)
    return dest
