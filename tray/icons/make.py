#!/usr/bin/env python3
"""Draws the app icon, a cairn of three stones, into icon.png, icon.ico and icon.icns. Run with
`python3 -I icons/make.py` from tray/; needs Pillow. The tray icon itself is drawn at run time by
src/icon.rs, in the same shapes."""

import io
import struct
import sys
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve().parent
BIG = 1024


def draw(size: int) -> Image.Image:
    # Drawn four times larger, then reduced: smooth edges without an antialiasing library.
    s = size * 4
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    u = s / 64
    d.rounded_rectangle([2 * u, 2 * u, 62 * u, 62 * u], radius=13 * u, fill=(0x24, 0x32, 0x3d, 255))
    stone = (0xdd, 0xd6, 0xc8, 255)
    for cx, cy, rx, ry in [(32, 47, 20, 7.5), (32.5, 33, 14, 6.5), (31.5, 20.5, 8.5, 5.5)]:
        d.ellipse([(cx - rx) * u, (cy - ry) * u, (cx + rx) * u, (cy + ry) * u], fill=stone)
    return img.resize((size, size), Image.LANCZOS)


def png(img: Image.Image) -> bytes:
    out = io.BytesIO()
    img.save(out, "PNG")
    return out.getvalue()


def ico(sizes) -> bytes:
    # Each entry a PNG, as Windows has read since Vista.
    images = [png(draw(n)) for n in sizes]
    head = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries, data = b"", b""
    for n, blob in zip(sizes, images):
        entries += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(blob), offset + len(data))
        data += blob
    return head + entries + data


def icns() -> bytes:
    # PNG entries: ic07 128, ic08 256, ic09 512, ic10 1024 (512 at 2x).
    body = b""
    for kind, n in [(b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024)]:
        blob = png(draw(n))
        body += kind + struct.pack(">I", 8 + len(blob)) + blob
    return b"icns" + struct.pack(">I", 8 + len(body)) + body


def main() -> int:
    (HERE / "icon.png").write_bytes(png(draw(512)))
    (HERE / "icon.ico").write_bytes(ico([16, 24, 32, 48, 64, 256]))
    (HERE / "icon.icns").write_bytes(icns())
    return 0


if __name__ == "__main__":
    sys.exit(main())
