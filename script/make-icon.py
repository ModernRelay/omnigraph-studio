#!/usr/bin/env python3
"""Draws Studio's app icon: the Modern Relay mark in white on a black tile, shaped like a macOS
app icon (Apple's template: an 824 px body on a 1024 px canvas, a continuous-corner square and a
soft shadow).

    python3 script/make-icon.py        # needs Pillow; on macOS also writes the .icns

Writes:
    resources/icon/icon-1024.png        the master image
    resources/macos/AppIcon.icns        every size macOS asks for (macOS only: iconutil)
    resources/linux/omnigraph-studio.png  512 px, for the .desktop entry

The mark is the one in ModernRelay/brand (packages/brand/mark.tsx): a 5x5 square of dots with
the corners dropped, dot centres at 12, 31, 50, 69 and 88 in a 100 unit box, radius 5.8.
"""

import math
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

ROOT = Path(__file__).resolve().parent.parent
SUPER = 4  # drawn at 4x, then reduced, for clean edges
SIZE = 1024 * SUPER
BODY = 824 * SUPER  # Apple's template: the icon body inside a 1024 canvas
EXPONENT = 5.0  # a superellipse this round matches macOS's continuous corners

DOTS = [12, 31, 50, 69, 88]
DOT_RADIUS = 5.8
MARK_SHARE = 0.58  # the mark's width as a share of the body

TOP, BOTTOM = (30, 30, 34), (9, 9, 11)  # near-black, slightly lighter at the top


def superellipse(cx, cy, half, n, steps=2048):
    points = []
    for i in range(steps):
        t = 2 * math.pi * i / steps
        c, s = math.cos(t), math.sin(t)
        x = half * math.copysign(abs(c) ** (2 / n), c)
        y = half * math.copysign(abs(s) ** (2 / n), s)
        points.append((cx + x, cy + y))
    return points


def draw() -> Image.Image:
    centre = SIZE / 2
    shape = superellipse(centre, centre, BODY / 2, EXPONENT)

    canvas = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))

    # The shadow Apple's template puts under the body.
    shadow = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    offset = 12 * SUPER
    ImageDraw.Draw(shadow).polygon([(x, y + offset) for x, y in shape], fill=(0, 0, 0, 90))
    canvas = Image.alpha_composite(canvas, shadow.filter(ImageFilter.GaussianBlur(14 * SUPER)))

    # The body: a vertical wash from TOP to BOTTOM, cut to the shape.
    wash = Image.new("RGBA", (SIZE, SIZE))
    wd = ImageDraw.Draw(wash)
    for y in range(SIZE):
        t = y / (SIZE - 1)
        wd.line([(0, y), (SIZE, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM)) + (255,))
    mask = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(mask).polygon(shape, fill=255)
    body = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    body.paste(wash, (0, 0), mask)
    canvas = Image.alpha_composite(canvas, body)

    # A faint light edge, so the tile holds its shape on a dark Dock.
    edge = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    ImageDraw.Draw(edge).line(shape + [shape[0]], fill=(255, 255, 255, 28), width=2 * SUPER)
    edge_mask = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(edge_mask).polygon(shape, fill=255)
    clipped = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    clipped.paste(edge, (0, 0), edge_mask)
    canvas = Image.alpha_composite(canvas, clipped)

    # The mark, centred.
    span = (DOTS[-1] + DOT_RADIUS) - (DOTS[0] - DOT_RADIUS)
    unit = BODY * MARK_SHARE / span
    origin = centre - unit * 50
    md = ImageDraw.Draw(canvas)
    for i, x in enumerate(DOTS):
        for j, y in enumerate(DOTS):
            if i in (0, 4) and j in (0, 4):
                continue
            cx, cy, r = origin + unit * x, origin + unit * y, unit * DOT_RADIUS
            md.ellipse((cx - r, cy - r, cx + r, cy + r), fill=(255, 255, 255, 255))

    return canvas.resize((1024, 1024), Image.LANCZOS)


def main() -> None:
    master = draw()
    icon_dir = ROOT / "resources/icon"
    icon_dir.mkdir(parents=True, exist_ok=True)
    master.save(icon_dir / "icon-1024.png")
    master.resize((512, 512), Image.LANCZOS).save(ROOT / "resources/linux/omnigraph-studio.png")

    iconutil = shutil.which("iconutil")
    if not iconutil:
        print("iconutil not found (macOS only): skipped AppIcon.icns", file=sys.stderr)
        return
    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "AppIcon.iconset"
        iconset.mkdir()
        for points in (16, 32, 128, 256, 512):
            for scale in (1, 2):
                px = points * scale
                name = f"icon_{points}x{points}{'@2x' if scale == 2 else ''}.png"
                master.resize((px, px), Image.LANCZOS).save(iconset / name)
        subprocess.run(
            [iconutil, "-c", "icns", str(iconset), "-o", str(ROOT / "resources/macos/AppIcon.icns")],
            check=True,
        )
    print("wrote resources/icon/icon-1024.png, resources/macos/AppIcon.icns, resources/linux/omnigraph-studio.png")


if __name__ == "__main__":
    main()
