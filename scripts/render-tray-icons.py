#!/usr/bin/env python3
"""Renders the tray icons (rustinput-gtk/resources/tray/*.png) from the logo.

The tray icons are embedded into the binary as PNG, so the tray does not need an
SVG renderer at runtime. Run this from the repository root after changing the
logo. Requires PyGObject with GdkPixbuf and its SVG loader (librsvg).
"""
import os

import gi

gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf  # noqa: E402

LOGO = "rustinput-gtk/resources/io.github.felipemarques.RustInput.svg"
OUT = "rustinput-gtk/resources/tray"
SIZES = (16, 22, 24, 32, 48, 64)

os.makedirs(OUT, exist_ok=True)
for size in SIZES:
    pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_scale(LOGO, size, size, True)
    if not pixbuf.get_has_alpha():
        pixbuf = pixbuf.add_alpha(False, 0, 0, 0)
    path = os.path.join(OUT, f"rustinput-{size}.png")
    pixbuf.savev(path, "png", [], [])
    print(f"{path}: {pixbuf.get_width()}x{pixbuf.get_height()}")
