#!/usr/bin/env python3
"""Pack wire-app/assets/new-icon.png into multi-res icon.ico and icon.png.

Requires Pillow:  python -m pip install Pillow

Usage (from repo root):
    python scripts/pack_icon.py
    python scripts/pack_icon.py --rm-bg
    python scripts/pack_icon.py --rm-bg --tolerance 20
    python scripts/pack_icon.py --rm-bg --color "#22B14C"
    python scripts/pack_icon.py --rm-bg --preview
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

try:
    from PIL import Image
except ImportError:
    sys.exit("Pillow is required. Install with:  python -m pip install Pillow")

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "wire-app" / "assets" / "new-icon.png"
ICO_OUT = ROOT / "wire-app" / "assets" / "icon.ico"
PNG_OUT = ROOT / "wire-app" / "assets" / "icon.png"
# Preview composites only (not used by build)
PREVIEW_OUT = ROOT / "wire-app" / "assets" / "icon-preview.png"

# Standard Windows multi-res ICO sizes (16..256, 7 entries)
SIZES = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]

# MS Paint default green — fill-bucket marker for "make this transparent"
DEFAULT_CHROMA = (0x22, 0xB1, 0x4C)

# Taskbar-like dark background used in the size strip preview
TASKBAR_BG = (0x1C, 0x1C, 0x1C, 255)


def parse_color(value: str) -> tuple[int, int, int]:
    """Parse #RGB, #RRGGBB, or R,G,B into an (r, g, b) tuple."""
    s = value.strip()
    if s.startswith("#"):
        h = s[1:]
        if len(h) == 3:
            h = "".join(c * 2 for c in h)
        if not re.fullmatch(r"[0-9A-Fa-f]{6}", h):
            raise argparse.ArgumentTypeError(f"invalid hex color: {value!r}")
        return (int(h[0:2], 16), int(h[2:4], 16), int(h[4:6], 16))
    parts = [p.strip() for p in s.split(",")]
    if len(parts) != 3:
        raise argparse.ArgumentTypeError(
            f"expected #RRGGBB or R,G,B color, got {value!r}"
        )
    try:
        rgb = tuple(int(p) for p in parts)
    except ValueError as e:
        raise argparse.ArgumentTypeError(f"invalid R,G,B color: {value!r}") from e
    if any(c < 0 or c > 255 for c in rgb):
        raise argparse.ArgumentTypeError(f"RGB components must be 0-255: {value!r}")
    return rgb  # type: ignore[return-value]


def remove_chroma(
    img: Image.Image,
    color: tuple[int, int, int],
    tolerance: int,
) -> tuple[Image.Image, int]:
    """Set pixels near `color` to fully transparent (0,0,0,0).

    RGB is zeroed (not left as the chroma color). Leaving chroma in the RGB of
    transparent pixels causes a coloured fringe when Windows scales the icon
    with independent R/G/B/A filtering (taskbar / shell).
    """
    px = img.load()
    assert px is not None
    w, h = img.size
    cr, cg, cb = color
    tol2 = tolerance * tolerance
    removed = 0
    for y in range(h):
        for x in range(w):
            r, g, b, a = px[x, y]
            dr, dg, db = r - cr, g - cg, b - cb
            if dr * dr + dg * dg + db * db <= tol2:
                px[x, y] = (0, 0, 0, 0)
                removed += 1
    return img, removed


def scrub_transparent_rgb(img: Image.Image) -> int:
    """Force every a==0 pixel to (0,0,0,0). Returns how many pixels were scrubbed."""
    px = img.load()
    assert px is not None
    w, h = img.size
    scrubbed = 0
    for y in range(h):
        for x in range(w):
            r, g, b, a = px[x, y]
            if a == 0 and (r, g, b) != (0, 0, 0):
                px[x, y] = (0, 0, 0, 0)
                scrubbed += 1
    return scrubbed


def resize_premultiplied(img: Image.Image, size: tuple[int, int]) -> Image.Image:
    """Resize RGBA with premultiplied alpha to avoid fringe colours on downscale."""
    src = img.convert("RGBA")
    # Premultiply
    r, g, b, a = src.split()
    r = Image.composite(r, Image.new("L", src.size, 0), a)
    g = Image.composite(g, Image.new("L", src.size, 0), a)
    b = Image.composite(b, Image.new("L", src.size, 0), a)
    pre = Image.merge("RGBA", (r, g, b, a))
    small = pre.resize(size, Image.Resampling.LANCZOS)
    # Unpremultiply
    sr, sg, sb, sa = small.split()
    out = Image.new("RGBA", size)
    op = out.load()
    assert op is not None
    sp = small.load()
    assert sp is not None
    for y in range(size[1]):
        for x in range(size[0]):
            pr, pg, pb, pa = sp[x, y]
            if pa == 0:
                op[x, y] = (0, 0, 0, 0)
            elif pa == 255:
                op[x, y] = (pr, pg, pb, pa)
            else:
                op[x, y] = (
                    min(255, (pr * 255) // pa),
                    min(255, (pg * 255) // pa),
                    min(255, (pb * 255) // pa),
                    pa,
                )
    return out


def save_ico(img: Image.Image, dest: Path) -> None:
    """Write a multi-size ICO from the (already scrubbed) full-res image.

    Pillow's ``sizes=`` path generates each mip by resizing the base image.
    Transparent pixels must already be (0,0,0,0) — see scrub_transparent_rgb —
    or Windows' independent-channel scaler paints a chroma fringe on the taskbar.
    """
    img.save(dest, format="ICO", sizes=SIZES)


def checkerboard_bg(size: tuple[int, int], tile: int = 16) -> Image.Image:
    """Opaque grey checkerboard behind transparent regions."""
    w, h = size
    light = (0xCC, 0xCC, 0xCC, 255)
    dark = (0x99, 0x99, 0x99, 255)
    cell = Image.new("RGBA", (tile * 2, tile * 2), light)
    dark_tile = Image.new("RGBA", (tile, tile), dark)
    cell.paste(dark_tile, (0, tile))
    cell.paste(dark_tile, (tile, 0))
    bg = Image.new("RGBA", (w, h))
    for y in range(0, h, cell.height):
        for x in range(0, w, cell.width):
            bg.paste(cell, (x, y))
    return bg


def solid_bg(size: tuple[int, int], color: tuple[int, int, int, int]) -> Image.Image:
    return Image.new("RGBA", size, color)


def write_preview(img: Image.Image, dest: Path) -> None:
    """Checkerboard full-res + taskbar-dark size strip so fringes are obvious."""
    full = Image.alpha_composite(checkerboard_bg(img.size), img).convert("RGB")

    # Size strip on taskbar-coloured background (matches the real failure mode)
    pad = 12
    label_h = 18
    strip_h = max(s[1] for s in SIZES) + pad * 2 + label_h
    strip_w = sum(s[0] for s in SIZES) + pad * (len(SIZES) + 1)
    strip = Image.new("RGB", (strip_w, strip_h), TASKBAR_BG[:3])
    x = pad
    for s in SIZES:
        sized = resize_premultiplied(img, s)
        cell = Image.alpha_composite(solid_bg(s, TASKBAR_BG), sized).convert("RGB")
        strip.paste(cell, (x, pad + label_h))
        x += s[0] + pad

    # Stack full (scaled down if huge) above the strip
    max_full_w = max(strip_w, 512)
    fw, fh = full.size
    if fw > max_full_w:
        nh = max(1, fh * max_full_w // fw)
        full = full.resize((max_full_w, nh), Image.Resampling.LANCZOS)
        fw, fh = full.size

    canvas = Image.new("RGB", (max(fw, strip_w), fh + strip_h + pad), (30, 30, 30))
    canvas.paste(full, ((canvas.width - fw) // 2, 0))
    canvas.paste(strip, ((canvas.width - strip_w) // 2, fh + pad // 2))
    canvas.save(dest, format="PNG", optimize=True)


def open_in_viewer(path: Path) -> None:
    """Open a file with the OS default application."""
    path = path.resolve()
    if sys.platform == "win32":
        os.startfile(path)  # type: ignore[attr-defined]
    elif sys.platform == "darwin":
        subprocess.run(["open", str(path)], check=False)
    else:
        subprocess.run(["xdg-open", str(path)], check=False)


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Pack new-icon.png into icon.ico + icon.png"
    )
    parser.add_argument(
        "--rm-bg",
        action="store_true",
        help=(
            "make the chroma color transparent "
            f"(default #{DEFAULT_CHROMA[0]:02X}{DEFAULT_CHROMA[1]:02X}{DEFAULT_CHROMA[2]:02X}, "
            "MS Paint green fill)"
        ),
    )
    parser.add_argument(
        "--color",
        type=parse_color,
        default=DEFAULT_CHROMA,
        metavar="HEX",
        help="chroma key color as #RRGGBB (default: #22B14C)",
    )
    parser.add_argument(
        "--tolerance",
        type=int,
        default=0,
        metavar="N",
        help="RGB distance tolerance for --rm-bg (default: 0 = exact match only)",
    )
    parser.add_argument(
        "--preview",
        action="store_true",
        help=(
            "write icon-preview.png (full checkerboard + dark taskbar size strip) "
            "and open it in the default image viewer"
        ),
    )
    args = parser.parse_args()
    if args.tolerance < 0:
        parser.error("--tolerance must be >= 0")

    if not SRC.is_file():
        sys.exit(f"Missing source icon: {SRC}")

    img = Image.open(SRC).convert("RGBA")
    print(f"source: {SRC} size={img.size} mode={img.mode}")

    if args.rm_bg:
        hex_color = f"#{args.color[0]:02X}{args.color[1]:02X}{args.color[2]:02X}"
        img, removed = remove_chroma(img, args.color, args.tolerance)
        total = img.size[0] * img.size[1]
        print(
            f"chroma: removed {removed}/{total} pixels matching {hex_color} "
            f"(tolerance={args.tolerance})"
        )

    # Always scrub: leftover chroma RGB under a==0 is what causes the taskbar
    # green ring when the shell scales the runtime window icon (icon.png).
    scrubbed = scrub_transparent_rgb(img)
    if scrubbed:
        print(f"scrub: zeroed RGB on {scrubbed} transparent pixels")

    save_ico(img, ICO_OUT)
    print(f"wrote {ICO_OUT} ({ICO_OUT.stat().st_size} bytes)")

    # High-res PNG for the runtime window icon / bundle metadata
    img.save(PNG_OUT, format="PNG", optimize=True)
    print(f"wrote {PNG_OUT} ({PNG_OUT.stat().st_size} bytes)")

    if args.preview:
        write_preview(img, PREVIEW_OUT)
        print(f"wrote {PREVIEW_OUT} ({PREVIEW_OUT.stat().st_size} bytes)")
        open_in_viewer(PREVIEW_OUT)
        print(f"opened preview: {PREVIEW_OUT}")


if __name__ == "__main__":
    main()
