#!/usr/bin/env python3
"""Draws the four badges Explorer shows on the entries of a root (cairn_protocol::local::Badge): synced,
syncing, warning and error, each a disc in an icon's lower left corner, the rest of the icon clear. Run from
anywhere with `python3 -I crates/explorer/icons/make.py`; needs pycairo and Pillow (Debian: python3-cairo
python3-pil). It writes synced.ico, syncing.ico, warning.ico and error.ico next to this file, which
cairn.exe carries and `cairn explorer on` puts beside the handler's DLL.

Explorer draws an overlay at the size of the icon under it, from the image of that size in the .ico, so each
size is drawn on its own, its strokes on whole pixels where they are thin, rather than one scaled down.
Each image is a PNG, as an .ico may hold since Windows Vista. `--sheet FILE` draws them all over a plain
file icon, at each size, to look at.
"""
import argparse, io, math, struct
from pathlib import Path

import cairo
from PIL import Image

HERE = Path(__file__).resolve().parent
SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]

# name: disc, glyph
BADGES = {
    'synced': ('#2E9D4F', '#FFFFFF'),
    'syncing': ('#1F78D1', '#FFFFFF'),
    'warning': ('#E8A200', '#FFFFFF'),
    'error': ('#D3302F', '#FFFFFF'),
}


def rgb(hex_):
    return tuple(int(hex_[i:i + 2], 16) / 255 for i in (1, 3, 5))


def badge_box(size):
    """The disc's diameter and its top left corner: about half the icon, at least 10 px, against the
    lower left corner."""
    d = max(10, round(size * 0.5))
    return d, 0, size - d


def draw(name, size):
    disc, glyph = BADGES[name]
    surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, size, size)
    ctx = cairo.Context(surface)
    d, x, y = badge_box(size)
    cx, cy, r = x + d / 2, y + d / 2, d / 2
    # A white ring, so the badge stands off whatever icon is under it.
    ctx.arc(cx, cy, r, 0, 2 * math.pi)
    ctx.set_source_rgb(1, 1, 1)
    ctx.fill()
    ring = max(1.0, d * 0.08)
    ctx.arc(cx, cy, r - ring, 0, 2 * math.pi)
    ctx.set_source_rgb(*rgb(disc))
    ctx.fill()

    ctx.set_source_rgb(*rgb(glyph))
    ctx.set_line_cap(cairo.LINE_CAP_ROUND)
    ctx.set_line_join(cairo.LINE_JOIN_ROUND)
    w = max(1.4, d * 0.14)
    ctx.set_line_width(w)
    # The glyph in a box of side g about the centre.
    g = (d - 2 * ring) * 0.62

    def at(u, v):
        return cx + (u - 0.5) * g, cy + (v - 0.5) * g

    if name == 'synced':
        ctx.move_to(*at(0.08, 0.55))
        ctx.line_to(*at(0.38, 0.84))
        ctx.line_to(*at(0.92, 0.22))
        ctx.stroke()
    elif name == 'error':
        for (a, b) in (((0.15, 0.15), (0.85, 0.85)), ((0.85, 0.15), (0.15, 0.85))):
            ctx.move_to(*at(*a))
            ctx.line_to(*at(*b))
            ctx.stroke()
    elif name == 'warning':
        ctx.move_to(*at(0.5, 0.06))
        ctx.line_to(*at(0.5, 0.58))
        ctx.stroke()
        ctx.arc(*at(0.5, 0.9), w * 0.62, 0, 2 * math.pi)
        ctx.fill()
    elif name == 'syncing':
        # One open arc round the centre ending in a head, the way a refresh is drawn: two arcs crowd
        # each other at 16 px.
        rr = g * 0.4
        ctx.set_line_width(w * 0.9)
        start = math.radians(-60)
        end = start + math.radians(270)
        ctx.new_path()
        ctx.arc(cx, cy, rr, start, end)
        ctx.stroke()
        hx, hy = cx + rr * math.cos(end), cy + rr * math.sin(end)
        # The head points along the arc, onward, its base across it.
        tx, ty = -math.sin(end), math.cos(end)
        nx, ny = math.cos(end), math.sin(end)
        h = max(2.6, d * 0.3)
        ctx.move_to(hx + tx * h * 0.7, hy + ty * h * 0.7)
        ctx.line_to(hx + nx * h * 0.62 - tx * h * 0.15, hy + ny * h * 0.62 - ty * h * 0.15)
        ctx.line_to(hx - nx * h * 0.62 - tx * h * 0.15, hy - ny * h * 0.62 - ty * h * 0.15)
        ctx.close_path()
        ctx.fill()
    surface.flush()
    return to_pil(surface)


def to_pil(surface):
    w, h = surface.get_width(), surface.get_height()
    data = bytes(surface.get_data())
    # Cairo's ARGB32 is premultiplied BGRA in memory on little-endian machines.
    img = Image.frombuffer('RGBA', (w, h), data, 'raw', 'BGRa', 0, 1)
    return img.copy()


def ico(images):
    """An .ico of PNG images, smallest first."""
    pngs = []
    for img in images:
        buf = io.BytesIO()
        img.save(buf, format='PNG', optimize=True)
        pngs.append((img.width, img.height, buf.getvalue()))
    out = struct.pack('<HHH', 0, 1, len(pngs))
    offset = 6 + 16 * len(pngs)
    for w, h, data in pngs:
        out += struct.pack('<BBBBHHII', w % 256, h % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    for _, _, data in pngs:
        out += data
    return out


def sheet(path):
    """Each badge at each size over a plain page icon, on white and on a dark ground."""
    pad = 12
    cols = SIZES
    width = sum(s + pad for s in cols) + pad
    rows = len(BADGES) * 2
    height = rows * (max(cols) + pad) + pad
    out = Image.new('RGBA', (width, height), (255, 255, 255, 255))
    y = pad
    for ground in ((255, 255, 255, 255), (32, 32, 32, 255)):
        for name in BADGES:
            x = pad
            row = Image.new('RGBA', (width, max(cols) + pad), ground)
            for s in cols:
                page = Image.new('RGBA', (s, s), (0, 0, 0, 0))
                body = Image.new('RGBA', (int(s * 0.7), int(s * 0.9)), (240, 240, 245, 255))
                page.paste(body, (int(s * 0.15), int(s * 0.05)))
                page.alpha_composite(draw(name, s))
                row.alpha_composite(page, (x, 0))
                x += s + pad
            out.alpha_composite(row, (0, y))
            y += max(cols) + pad
    out.save(path)


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--sheet')
    args = p.parse_args()
    if args.sheet:
        sheet(args.sheet)
        return
    for name in BADGES:
        data = ico([draw(name, s) for s in SIZES])
        (HERE / f'{name}.ico').write_bytes(data)
        print(f'{name}.ico: {len(data)} bytes')


if __name__ == '__main__':
    main()
