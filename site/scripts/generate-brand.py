#!/usr/bin/env python3
"""Derive the site's pixel-art brand assets from the repository's logo.png.

The source logo is a black gorilla drawing on a white field. The site is a
pixel-art page, so every mark here is the drawing reduced to a small grid and
scaled back up with nearest-neighbour sampling: hard edges, no antialiasing.

Run from anywhere:  python3 site/scripts/generate-brand.py
Requires Pillow and numpy.
"""

from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[2]
PUBLIC = ROOT / "site" / "public"
BRAND = PUBLIC / "brand"

BG = (12, 10, 7)
PANEL = (23, 19, 14)
AMBER = (242, 169, 59)
BONE = (243, 234, 217)
COPPER = (140, 106, 47)
INK = (20, 16, 11)
PAPER = (237, 230, 214)

# Pixel grid the mark is reduced to. Fewer columns = chunkier pixels.
MARK_COLUMNS = 44

SANS = "/System/Library/Fonts/SFNS.ttf"
MONO = "/System/Library/Fonts/SFNSMono.ttf"


def load_mask() -> Image.Image:
    """Return the logo as a hard-edged alpha mask on a small pixel grid."""
    grey = np.asarray(Image.open(ROOT / "logo.png").convert("L"), dtype=np.float32)
    alpha = np.clip((255.0 - grey) - 26.0, 0, None) * (255.0 / 200.0)
    mask = Image.fromarray(np.clip(alpha, 0, 255).astype(np.uint8))
    mask = mask.crop(mask.getbbox())

    ratio = MARK_COLUMNS / mask.width
    small = mask.resize(
        (MARK_COLUMNS, max(1, round(mask.height * ratio))), Image.BOX
    )
    # Hard threshold: a pixel is ink or it is not. This is what makes it read
    # as pixel art rather than a blurry thumbnail.
    small = small.point(lambda v: 255 if v > 104 else 0)
    return small


def pixelate(image: Image.Image, factor: int) -> Image.Image:
    return image.resize(
        (image.width * factor, image.height * factor), Image.NEAREST
    )


def tinted(mask: Image.Image, colour: tuple[int, int, int], factor: int = 14) -> Image.Image:
    image = Image.new("RGBA", mask.size, colour + (0,))
    image.putalpha(mask)
    return pixelate(image, factor)


def fixed_mark(mark: Image.Image, colours: dict[str, tuple[int, int, int]], factor: int = 14):
    out = {}
    for name, colour in colours.items():
        out[name] = tinted(mark, colour, factor)
    return out


def save_square(image: Image.Image, path: Path, size: int, pad_ratio: float) -> None:
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    inner = int(size * (1 - 2 * pad_ratio))
    scaled = image.copy()
    scaled.thumbnail((inner, inner), Image.NEAREST)
    canvas.paste(
        scaled,
        ((size - scaled.width) // 2, (size - scaled.height) // 2),
        scaled,
    )
    canvas.save(path)


def font(path: str, size: int, weight: str) -> ImageFont.FreeTypeFont:
    face = ImageFont.truetype(path, size)
    if weight != "Regular":
        face.set_variation_by_name(weight)
    return face


def social_card(mark: Image.Image, path: Path) -> None:
    """1200x630 card: an amber frame, a pixel mark, and a terminal prompt."""
    width, height = 1200, 630
    cell = 18  # border thickness in card pixels
    card = Image.new("RGB", (width, height), BG)
    draw = ImageDraw.Draw(card, "RGBA")

    # Chunked grid, one line per 24px, deliberately visible.
    for x in range(0, width, 24):
        draw.line([(x, 0), (x, height)], fill=(255, 255, 255, 9))
    for y in range(0, height, 24):
        draw.line([(0, y), (width, y)], fill=(255, 255, 255, 9))

    # Amber frame drawn as four solid bars — no rounded corners anywhere.
    draw.rectangle([(0, 0), (width, cell)], fill=AMBER)
    draw.rectangle([(0, 0), (cell, height)], fill=AMBER)
    draw.rectangle([(0, height - cell), (width, height)], fill=COPPER)
    draw.rectangle([(width - cell, 0), (width, height)], fill=COPPER)

    # Pixel mark, right side, chunky.
    big = tinted(mark, BONE, factor=13)
    big.thumbnail((430, 430), Image.NEAREST)
    card.paste(big, (width - big.width - 84, height - big.height - 96), big)

    draw.text((72, 68), "CODAPE", font=font(MONO, 22, "Bold"), fill=AMBER)
    draw.text((72, 100), "/TERMINAL-CODING-AGENT", font=font(MONO, 18, "Medium"), fill=COPPER)

    draw.text((64, 214), "codape", font=font(SANS, 124, "Bold"), fill=BONE)

    draw.text((72, 386), "❯ it reads your repo.", font=font(MONO, 30, "Medium"), fill=BONE)
    draw.text((72, 430), "❯ it does the work.", font=font(MONO, 30, "Medium"), fill=AMBER)

    draw.rectangle([(72, 500), (72 + 260, 500 + 34)], fill=PANEL)
    draw.rectangle([(72, 500), (72 + 260, 500 + 2)], fill=COPPER)
    draw.text((86, 507), "one native binary", font=font(MONO, 20, "Medium"), fill=COPPER)

    card.save(path, optimize=True)


def main() -> None:
    BRAND.mkdir(parents=True, exist_ok=True)
    mask = load_mask()
    print("mask grid:", mask.size, "aspect", round(mask.width / mask.height, 3))

    marks = fixed_mark(mask, {"bone": BONE, "amber": AMBER, "ink": INK, "paper": PAPER})
    marks["bone"].save(BRAND / "mark-light.png", optimize=True)
    marks["amber"].save(BRAND / "mark-amber.png", optimize=True)
    marks["ink"].save(BRAND / "mark-ink.png", optimize=True)

    save_square(marks["amber"], BRAND / "mark-square.png", 512, 0.10)

    # Favicons: amber mark on the page's near-black, square.
    for size, name in ((32, "favicon-32.png"), (192, "favicon-192.png"), (512, "favicon-512.png")):
        tile = Image.new("RGBA", (size, size), BG + (255,))
        inner = marks["amber"].copy()
        inner.thumbnail((int(size * 0.78),) * 2, Image.NEAREST)
        tile.paste(inner, ((size - inner.width) // 2, (size - inner.height) // 2), inner)
        tile.save(PUBLIC / name, optimize=True)

    touch = Image.new("RGBA", (180, 180), BG + (255,))
    inner = marks["amber"].copy()
    inner.thumbnail((140, 140), Image.NEAREST)
    touch.paste(inner, ((180 - inner.width) // 2, (180 - inner.height) // 2), inner)
    touch.convert("RGB").save(PUBLIC / "apple-touch-icon.png", optimize=True)

    social_card(mask, PUBLIC / "og.png")

    base = Image.open(PUBLIC / "favicon-512.png").convert("RGBA")
    base.save(PUBLIC / "favicon.ico", sizes=[(16, 16), (32, 32), (48, 48), (64, 64)])

    print("brand assets written to", BRAND, "and", PUBLIC)


if __name__ == "__main__":
    main()
