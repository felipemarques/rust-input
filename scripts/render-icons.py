#!/usr/bin/env python3
"""Renders the raster icons from the logo.

- rustinput-gtk/resources/tray/*.png: the tray icons, embedded into the
  binary as PNG so the tray does not need an SVG renderer at runtime;
- build-aux/windows/rustinput.ico: the icon of the Windows executable, used by
  Explorer, the taskbar and Alt+Tab.

Run this from the repository root after changing the logo. Requires PyGObject
with GdkPixbuf and its SVG loader (librsvg).
"""
import os
import struct

import gi

gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf  # noqa: E402

LOGO = "rustinput-gtk/resources/io.github.felipemarques.RustInput.svg"
TRAY_DIR = "rustinput-gtk/resources/tray"
TRAY_SIZES = (16, 22, 24, 32, 48, 64)
ICO = "build-aux/windows/rustinput.ico"
# the sizes Windows asks for at 100 % to 250 % display scaling
ICO_SIZES = (16, 20, 24, 32, 40, 48, 64, 96, 128, 256)


def render(size):
    pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_scale(LOGO, size, size, True)
    if not pixbuf.get_has_alpha():
        pixbuf = pixbuf.add_alpha(False, 0, 0, 0)
    return pixbuf


def png(pixbuf):
    ok, data = pixbuf.save_to_bufferv("png", [], [])
    assert ok
    return data


os.makedirs(TRAY_DIR, exist_ok=True)
for size in TRAY_SIZES:
    path = os.path.join(TRAY_DIR, f"rustinput-{size}.png")
    render(size).savev(path, "png", [], [])
    print(f"{path}: {size}x{size}")

# ICO with PNG compressed entries, supported since Windows Vista
images = [png(render(size)) for size in ICO_SIZES]
offset = 6 + 16 * len(images)
header = struct.pack("<HHH", 0, 1, len(images))
entries = b""
for size, image in zip(ICO_SIZES, images):
    dimension = 0 if size == 256 else size  # 0 means 256
    entries += struct.pack("<BBBBHHII", dimension, dimension, 0, 0, 1, 32, len(image), offset)
    offset += len(image)
os.makedirs(os.path.dirname(ICO), exist_ok=True)
with open(ICO, "wb") as f:
    f.write(header + entries + b"".join(images))
print(f"{ICO}: {', '.join(f'{s}x{s}' for s in ICO_SIZES)}")
