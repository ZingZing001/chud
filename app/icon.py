#!/usr/bin/env python3
"""Draws chud's app icon (a happy chud with a cookie, on a rounded tile) and builds app/chud.icns.

Pure Python for the drawing; macOS's sips and iconutil for the icon set. Run from anywhere:
    python3 app/icon.py
"""
import os, struct, subprocess, tempfile, zlib

# The big "happy, fat 2" chud from src/chud.rs, with a cookie to munch on.
ART = [
    "    ######           ",
    "  ##oooooo##         ",
    " #oooooooooo#        ",
    "#oo@*oooo*@oo#       ",
    "#oo@@oooo@@oo#   kkk ",
    "#oo+.@..@.+oo#  kcckk",
    " #o...@@...o#   kkkck",
    "  ##......##    kckkk",
    "    ######       kkk ",
    "   ##    ##          ",
]
COLORS = {
    "o": (0xFF, 0xC2, 0x7A), "#": (0xE8, 0x94, 0x4F), ".": (0xFF, 0xE6, 0xC2), "@": (0x2B, 0x1D, 0x16),
    "*": (0xFF, 0xFF, 0xFF), "+": (0xFF, 0x8F, 0xA8), "k": (0xD9, 0xA0, 0x5B), "c": (0x5A, 0x3A, 0x22),
}
SIZE, INSET, RADIUS = 1024, 100, 185            # macOS icon grid: an 824 px tile on a 1024 canvas
TOP, BOTTOM = (0x3A, 0x2F, 0x5B), (0x22, 0x1B, 0x36)


def draw() -> bytearray:
    px = bytearray(SIZE * SIZE * 4)

    def blend(x, y, rgb, a):
        i = (y * SIZE + x) * 4
        old_a = px[i + 3] / 255
        out_a = a + old_a * (1 - a)
        for k in range(3):
            px[i + k] = round((rgb[k] * a + px[i + k] * old_a * (1 - a)) / out_a) if out_a else 0
        px[i + 3] = round(out_a * 255)

    # tile: rounded square, vertical gradient, anti-aliased corners
    lo, hi = INSET, SIZE - INSET
    for y in range(lo, hi):
        t = (y - lo) / (hi - lo)
        rgb = tuple(round(TOP[k] + (BOTTOM[k] - TOP[k]) * t) for k in range(3))
        for x in range(lo, hi):
            cx = min(max(x + 0.5, lo + RADIUS), hi - RADIUS)
            cy = min(max(y + 0.5, lo + RADIUS), hi - RADIUS)
            d = ((x + 0.5 - cx) ** 2 + (y + 0.5 - cy) ** 2) ** 0.5
            blend(x, y, rgb, min(max(RADIUS - d + 0.5, 0), 1))

    scale = 30
    w, h = len(ART[0]) * scale, len(ART) * scale
    x0, y0 = (SIZE - w) // 2, (SIZE - h) // 2 - 20
    # soft shadow under the chud's feet
    sx, sy, rx, ry = x0 + 7 * scale, y0 + h + 6, 6.5 * scale, 0.9 * scale
    for y in range(int(sy - ry), int(sy + ry) + 1):
        for x in range(int(sx - rx), int(sx + rx) + 1):
            d = ((x - sx) / rx) ** 2 + ((y - sy) / ry) ** 2
            if d < 1:
                blend(x, y, (0x12, 0x0E, 0x1E), 0.55 * (1 - d))
    for r, row in enumerate(ART):
        for c, ch in enumerate(row):
            if ch in COLORS:
                for y in range(y0 + r * scale, y0 + (r + 1) * scale):
                    for x in range(x0 + c * scale, x0 + (c + 1) * scale):
                        blend(x, y, COLORS[ch], 1.0)
    return px


def write_png(path: str, w: int, h: int, rgba: bytearray) -> None:
    raw = b"".join(b"\x00" + bytes(rgba[y * w * 4:(y + 1) * w * 4]) for y in range(h))
    chunk = lambda tag, data: struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def main() -> None:
    here = os.path.dirname(os.path.abspath(__file__))
    png = os.path.join(here, "chud-icon.png")
    write_png(png, SIZE, SIZE, draw())
    with tempfile.TemporaryDirectory() as tmp:
        iconset = os.path.join(tmp, "chud.iconset")
        os.mkdir(iconset)
        for size in (16, 32, 128, 256, 512):
            for mult in (1, 2):
                name = f"icon_{size}x{size}{'@2x' if mult == 2 else ''}.png"
                px = str(size * mult)
                subprocess.run(["sips", "-z", px, px, png, "--out", os.path.join(iconset, name)],
                               check=True, capture_output=True)
        subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(here, "chud.icns")], check=True)
    print("wrote", png, "and", os.path.join(here, "chud.icns"))


if __name__ == "__main__":
    main()
