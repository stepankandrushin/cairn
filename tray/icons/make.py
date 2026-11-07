#!/usr/bin/env python3
"""Draws Cairn's two app icons: the app (Cairn.app, the tray) and the daemon (Cairn Sync.app), a cairn of
three stones, the daemon's inside two sync arrows. Run from tray/ with `python3 -I icons/make.py`; needs
pycairo and Pillow (Debian: python3-cairo python3-pil). It writes, next to this file:

- Cairn.xcassets and Sync.xcassets, the asset catalogs macos/bundle.sh compiles with Xcode's actool;
- icon.icns and sync.icns, the plain icons a bundle falls back on without actool;
- icon.png and icon.ico, the app's, which Tauri builds into the tray on Windows;
- sync.ico, the daemon's, which crates/cli/build.rs builds into cairn.exe on Windows.

The body follows macOS's app icon grid, an 824 px continuous-corner square on a 1024 px canvas with room for
its shadow. macOS 26 and later draw an icon they do not take for one of theirs smaller, on a gray tile: a plain
.icns at 32 and 16 px whatever is drawn, an asset catalog only at 16 px unless the square fills 15 of the 16
pixels, without margin or shadow, as here.

macOS 26 and 27 also redraw some icons at 64 px and below, whatever the format: the sync arrows in #5CC48A or
#34C759 came out gray there, or gone, while #43B86F, #30D158, #2E9D4F, blue, red and white kept their colour
(macOS 27.0.1, 2026-11-07). Look at a new colour at those sizes on a Mac, through NSWorkspace's icon for the
bundle, before using it.

Three styles were drawn; slate is the one in use. `--style dusk` or `--style day` draws another, and
`--sheet FILE` draws all three side by side, at each size and in a mock Login Items list, to choose by.
"""
import argparse, io, json, math, struct, sys
from pathlib import Path

import cairo
from PIL import Image, ImageDraw, ImageFilter, ImageFont

HERE = Path(__file__).resolve().parent
S = 1024

# name: tile top, tile bottom, stone top, stone bottom, ground shadow, arrows
STYLES = {
    'slate': ('#33475A', '#141D26', '#ECE5D6', '#A99D88', 0.45, '#43B86F'),
    'dusk': ('#3D7EA6', '#123049', '#F4EEE2', '#B8AB95', 0.35, '#FFFFFF'),
    'day': ('#BFE6FB', '#4F9BE3', '#6B7680', '#2E373F', 0.25, '#FFFFFF'),
}

# A continuous ("squircle") corner, iOS 7's coefficients: (along the incoming edge, along the outgoing) * r.
CORNER = [
    ('l', (1.52866471, 0)),
    ('c', (1.08849323, 0), (0.86840689, 0), (0.66993427, 0.06549600)),
    ('l', (0.63149399, 0.07491100)),
    ('c', (0.37282392, 0.16905899), (0.16906013, 0.37282401), (0.07491176, 0.63149399)),
    ('l', (0.06549569, 0.66993493)),
    ('c', (0, 0.86840701), (0, 1.08849299), (0, 1.52866483)),
]


def squircle(ctx, x, y, w, h, r):
    r = min(r, min(w, h) / 2 / 1.52866483)
    corners = [((x + w, y), (1, 0), (0, 1)), ((x + w, y + h), (0, 1), (-1, 0)),
               ((x, y + h), (-1, 0), (0, -1)), ((x, y), (0, -1), (1, 0))]
    ctx.new_path()
    first = True
    for (cx, cy), din, dout in corners:
        def p(xy):
            a, b = xy
            return (cx - a * r * din[0] + b * r * dout[0], cy - a * r * din[1] + b * r * dout[1])
        for seg in CORNER:
            if seg[0] == 'l':
                if first:
                    ctx.move_to(*p(seg[1]))
                    first = False
                else:
                    ctx.line_to(*p(seg[1]))
            else:
                ctx.curve_to(*p(seg[1]), *p(seg[2]), *p(seg[3]))
    ctx.close_path()


def rgb(h, a=1.0):
    h = h.lstrip('#')
    return (int(h[0:2], 16) / 255, int(h[2:4], 16) / 255, int(h[4:6], 16) / 255, a)


def vgrad(y0, y1, c0, c1):
    g = cairo.LinearGradient(0, y0, 0, y1)
    g.add_color_stop_rgba(0, *rgb(c0))
    g.add_color_stop_rgba(1, *rgb(c1))
    return g


def layer():
    surf = cairo.ImageSurface(cairo.FORMAT_ARGB32, S, S)
    return surf, cairo.Context(surf)


def to_pil(surf):
    surf.flush()
    return Image.frombuffer('RGBA', (S, S), bytes(surf.get_data()), 'raw', 'BGRa', surf.get_stride(), 1).copy()


def shadow_of(img, blur, dy, alpha):
    a = img.getchannel('A').point(lambda v: int(v * alpha))
    sh = Image.new('RGBA', img.size, (0, 0, 0, 0))
    sh.paste(Image.new('RGBA', img.size, (0, 0, 0, 255)), (0, 0), a)
    sh = sh.filter(ImageFilter.GaussianBlur(blur))
    out = Image.new('RGBA', img.size, (0, 0, 0, 0))
    out.paste(sh, (0, int(dy)))
    return out


def compose(layers):
    out = Image.new('RGBA', (S, S), (0, 0, 0, 0))
    for item in layers:
        img, shadow = item if isinstance(item, tuple) else (item, None)
        if shadow:
            out = Image.alpha_composite(out, shadow_of(img, *shadow))
        out = Image.alpha_composite(out, img)
    return out


def tile(c0, c1):
    surf, ctx = layer()
    squircle(ctx, 100, 100, 824, 824, 185.4)
    ctx.set_source(vgrad(100, 924, c0, c1))
    ctx.fill_preserve()
    g = cairo.RadialGradient(512, 60, 0, 512, 60, 760)
    g.add_color_stop_rgba(0, 1, 1, 1, 0.20)
    g.add_color_stop_rgba(1, 1, 1, 1, 0)
    ctx.set_source(g)
    ctx.fill()
    return to_pil(surf)


def stone(cx, cy, a, b, tilt, n, flat, wobble, top, bottom):
    """A pebble: a superellipse with a flatter underside, its outline made uneven by a few low harmonics
    (`wobble`: (k, amplitude, phase)), tilted by `tilt` radians, lit from the upper left."""
    surf, ctx = layer()
    ctx.new_path()
    for i in range(361):
        t = 2 * math.pi * i / 360
        c, s = math.cos(t), math.sin(t)
        x = a * math.copysign(abs(c) ** (2 / n), c)
        y = b * math.copysign(abs(s) ** (2 / n), s)
        if y > 0:
            y *= 1 - flat
        m = 1 + sum(amp * math.cos(k * t + ph) for k, amp, ph in wobble)
        x, y = x * m, y * m
        X = cx + x * math.cos(tilt) - y * math.sin(tilt)
        Y = cy + x * math.sin(tilt) + y * math.cos(tilt)
        (ctx.move_to if i == 0 else ctx.line_to)(X, Y)
    ctx.close_path()
    ctx.set_source(vgrad(cy - b, cy + b, top, bottom))
    ctx.fill_preserve()
    ctx.save()
    ctx.clip()
    g = cairo.RadialGradient(cx - a * 0.35, cy - b * 0.75, 0, cx - a * 0.35, cy - b * 0.75, a * 0.95)
    g.add_color_stop_rgba(0, 1, 1, 1, 0.38)
    g.add_color_stop_rgba(1, 1, 1, 1, 0)
    ctx.set_source(g)
    ctx.paint()
    g = cairo.LinearGradient(0, cy + b * 0.2, 0, cy + b)
    g.add_color_stop_rgba(0, 0, 0, 0, 0)
    g.add_color_stop_rgba(1, 0, 0, 0, 0.22)
    ctx.set_source(g)
    ctx.paint()
    ctx.restore()
    return to_pil(surf)


def ground(cx, cy, rx, ry, alpha):
    surf, ctx = layer()
    ctx.translate(cx, cy)
    ctx.scale(rx, ry)
    g = cairo.RadialGradient(0, 0, 0, 0, 0, 1)
    g.add_color_stop_rgba(0, 0, 0, 0, alpha)
    g.add_color_stop_rgba(1, 0, 0, 0, 0)
    ctx.set_source(g)
    ctx.arc(0, 0, 1, 0, 2 * math.pi)
    ctx.fill()
    return to_pil(surf)


def arrows(cx, cy, r, w, colour):
    """Two arrows chasing round a circle: the sync mark."""
    surf, ctx = layer()
    ctx.set_source_rgba(*rgb(colour, 0.95))
    ctx.set_line_width(w)
    gap = 0.62
    for start in (-math.pi * 0.95, math.pi * 0.05):
        a1 = start + math.pi - gap
        ctx.new_path()
        ctx.arc(cx, cy, r, start + 0.12, a1)
        ctx.stroke()
        hx, hy = cx + r * math.cos(a1), cy + r * math.sin(a1)
        tx, ty = -math.sin(a1), math.cos(a1)
        nx, ny = math.cos(a1), math.sin(a1)
        length, half = w * 1.55, w * 1.15
        ctx.new_path()
        ctx.move_to(hx + tx * length, hy + ty * length)
        ctx.line_to(hx + nx * half, hy + ny * half)
        ctx.line_to(hx - nx * half, hy - ny * half)
        ctx.close_path()
        ctx.fill()
    return to_pil(surf)


# (dx, height of the centre above the base, a, b, tilt, n, flat, wobble), bottom stone first
STONES = [
    (-4, 90, 272, 92, -0.03, 2.6, 0.22, ((2, 0.025, 0.4), (3, 0.018, 1.9), (5, 0.008, 0.7))),
    (12, 256, 192, 86, 0.06, 2.3, 0.15, ((2, 0.030, 2.2), (3, 0.020, 0.3), (4, 0.010, 1.2))),
    (-10, 402, 124, 74, -0.09, 2.1, 0.08, ((2, 0.035, 1.0), (3, 0.022, 2.8))),
]


def cairn(style, k=1.0, base=742, cx=512):
    _, _, top, bottom, shade, _ = STYLES[style]
    layers = [ground(cx, base + 6 * k, 330 * k, 42 * k, shade)]
    for dx, h, a, b, tilt, n, flat, wob in STONES:
        img = stone(cx + dx * k, base - h * k, a * k, b * k, tilt, n, flat, wob, top, bottom)
        layers.append((img, (14 * k, 16 * k, 0.38)))
    return layers


def icon(style, sync=False, shadow=True):
    c0, c1, *_, mark = STYLES[style]
    items = [(tile(c0, c1), (12, 10, 0.35) if shadow else None)]
    if sync:
        items.append(arrows(512, 512, 318, 46, mark))
        items += cairn(style, k=0.62, base=677)
    else:
        items += cairn(style)
    return compose(items)


def filled(img, fill, px):
    """`img` at `px`, its 824 px body filling that fraction of it."""
    m = round((824 / fill - 824) / 2)
    pad = Image.new('RGBA', (S + 2 * m, S + 2 * m), (0, 0, 0, 0))
    pad.paste(img, (m, m))
    return pad.crop((100, 100, 100 + 824 + 2 * m, 100 + 824 + 2 * m)).resize((px, px), Image.LANCZOS)


def at(big, flat, px):
    if px == 16:
        return filled(flat, 15 / 16, 16)
    return big if px == S else big.resize((px, px), Image.LANCZOS)


def png(img):
    out = io.BytesIO()
    img.save(out, 'PNG', optimize=True)
    return out.getvalue()


def catalog(path, big, flat):
    info = {'info': {'version': 1, 'author': 'xcode'}}
    sets = path / 'AppIcon.appiconset'
    sets.mkdir(parents=True, exist_ok=True)
    (path / 'Contents.json').write_text(json.dumps(info, indent=2) + '\n')
    images = []
    for size in (16, 32, 128, 256, 512):
        for scale in (1, 2):
            name = f'icon_{size}x{size}' + ('@2x' if scale == 2 else '') + '.png'
            (sets / name).write_bytes(png(at(big, flat, size * scale)))
            images.append({'idiom': 'mac', 'size': f'{size}x{size}', 'scale': f'{scale}x', 'filename': name})
    (sets / 'Contents.json').write_text(json.dumps({'images': images, **info}, indent=2) + '\n')


def icns(big, flat):
    body = b''
    for kind, px in [(b'icp4', 16), (b'icp5', 32), (b'icp6', 64), (b'ic07', 128), (b'ic08', 256),
                     (b'ic09', 512), (b'ic10', 1024), (b'ic11', 32), (b'ic12', 64), (b'ic13', 256),
                     (b'ic14', 512)]:
        blob = png(at(big, flat, px))
        body += kind + struct.pack('>I', 8 + len(blob)) + blob
    return b'icns' + struct.pack('>I', 8 + len(body)) + body


def ico(big, flat, sizes=(16, 24, 32, 48, 64, 256)):
    # Each entry a PNG, as Windows has read since Vista.
    blobs = [png(at(big, flat, n)) for n in sizes]
    head = struct.pack('<HHH', 0, 1, len(blobs))
    offset = 6 + 16 * len(blobs)
    entries, data = b'', b''
    for n, blob in zip(sizes, blobs):
        entries += struct.pack('<BBBBHHII', n % 256, n % 256, 0, 0, 1, 32, len(blob), offset + len(data))
        data += blob
    return head + entries + data


def sheet(path):
    """All three styles, each at 320 to 16 px and in a mock Login Items list, dark and light, at 2x."""
    font = lambda n: ImageFont.truetype('/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf', n)
    names = list(STYLES)
    rowh = 600
    out = Image.new('RGBA', (2200, 120 + rowh * len(names)), (246, 246, 248, 255))
    d = ImageDraw.Draw(out)
    d.text((40, 36), 'Cairn icons by style. Left: the app (tray). Right: the daemon (Cairn Sync).',
           font=font(40), fill=(20, 20, 20))

    def row(x, y, w, img, name, dark):
        out.alpha_composite(img.resize((64, 64), Image.LANCZOS), (x + 16, y + 18))
        d.text((x + 100, y + 16), name, font=font(30), fill=(236, 236, 236) if dark else (30, 30, 30))
        d.ellipse((x + 102, y + 62, x + 118, y + 78), fill=(52, 199, 89))
        d.text((x + 128, y + 56), 'Running in background', font=font(26),
               fill=(160, 160, 160) if dark else (110, 110, 110))
        sx = x + w - 110
        d.rounded_rectangle((sx, y + 32, sx + 76, y + 70), radius=19, fill=(10, 122, 255))
        d.ellipse((sx + 40, y + 35, sx + 73, y + 67), fill=(255, 255, 255))

    for i, name in enumerate(names):
        y0 = 120 + i * rowh
        d.text((40, y0 + 10), f'{i + 1}  {name}', font=font(44), fill=(20, 20, 20))
        x, bigs = 40, []
        for sync in (False, True):
            big, flat = icon(name, sync), icon(name, sync, shadow=False)
            bigs.append(big)
            out.alpha_composite(big.resize((320, 320), Image.LANCZOS), (x, y0 + 80))
            xx = x + 330
            for px in (128, 64, 32, 16):
                out.alpha_composite(at(big, flat, px), (xx, y0 + 80 + (128 - px) // 2 + 100))
                xx += px + 18
            x = xx + 30
        for j, dark in enumerate((True, False)):
            py = y0 + 40 + j * 260
            d.rounded_rectangle((1520, py, 2160, py + 230), radius=22,
                                fill=(40, 40, 42) if dark else (255, 255, 255),
                                outline=None if dark else (200, 200, 200))
            row(1530, py + 10, 620, bigs[0], 'Cairn', dark)
            d.line((1630, py + 115, 2140, py + 115), fill=(70, 70, 72) if dark else (225, 225, 225), width=2)
            row(1530, py + 120, 620, bigs[1], 'Cairn Sync', dark)
    out.convert('RGB').save(path, optimize=True)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('--style', choices=list(STYLES), default='slate')
    ap.add_argument('--sheet', metavar='FILE', help='draw every style into FILE and write nothing else')
    args = ap.parse_args()
    if args.sheet:
        sheet(args.sheet)
        return 0
    app, app_flat = icon(args.style), icon(args.style, shadow=False)
    sync, sync_flat = icon(args.style, True), icon(args.style, True, shadow=False)
    catalog(HERE / 'Cairn.xcassets', app, app_flat)
    catalog(HERE / 'Sync.xcassets', sync, sync_flat)
    (HERE / 'icon.icns').write_bytes(icns(app, app_flat))
    (HERE / 'sync.icns').write_bytes(icns(sync, sync_flat))
    (HERE / 'icon.png').write_bytes(png(app.resize((512, 512), Image.LANCZOS)))
    (HERE / 'icon.ico').write_bytes(ico(app, app_flat))
    (HERE / 'sync.ico').write_bytes(ico(sync, sync_flat))
    return 0


if __name__ == '__main__':
    sys.exit(main())
