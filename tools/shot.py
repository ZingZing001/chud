#!/usr/bin/env python3
"""Renders what chud draws to a PNG, with stand-in agents replaying fixtures/*.raw.
Usage: tools/shot.py out.png [--chud BIN] [--light] [--size COLSxROWS] [actions...]
Actions, in order: k:<keys, \\x01 escapes ok>  m:<col>,<row> (click)  w:<seconds>  r:<cols>,<rows> (resize)
Needs: pip install pyte pillow fonttools"""
import os, re, shutil, sys
import pyte
from fontTools.ttLib import TTFont
from PIL import Image, ImageDraw, ImageFont
from harness import Chud, Home, REPO

args = sys.argv[1:]
take = lambda flag, default: args.pop(args.index(flag) + 1) if flag in args and not args.remove(flag) else default
binary = take("--chud", REPO + "/target/release/chud")
cols, rows = map(int, take("--size", "150x42").split("x"))
light = "--light" in args and not args.remove("--light")
out, actions = args[0], args[1:]

home = Home()
for d in ("proj/chud", "proj/api"):
    os.makedirs(f"{home.dir}/{d}")
session = lambda argv, cwd, group, name=None, worked=0: {"argv": argv, "cwd": f"{home.dir}/proj/{cwd}", "name": name,
                                                         "group": group, "sid": "", "worked": worked, "resume": None}
home.state({"side": 34, "groups": [{"id": 0, "name": "Self", "collapsed": False}, {"id": 1, "name": "Work", "collapsed": False}],
            "sessions": [session(["claude"], "chud", 0, "chud", 5400), session(["copilot"], "api", 1, None, 1200),
                         session(["claude"], "api", 1, "api review", 300), session(["zsh"], "chud", 0)], "layout": 0})
# the recordings end with the agent quitting; stop each while its screen is still up
for name, cut in (("claude", b"waiting for your input"), ("copilot", b"\x1b[?1049l")):
    raw = open(f"{REPO}/fixtures/{name}.raw", "rb").read()
    open(f"{home.dir}/{name}.raw", "wb").write(raw[:raw.rfind(cut)])
# one replay file per program name: the stub reads CHUD_STUB_REPLAY, so point it by wrapper dir
os.rename(home.bin + "/claude", home.bin + "/stub")
os.remove(home.bin + "/copilot")
for name in ("claude", "copilot"):
    os.makedirs(f"{home.dir}/as/{name}")
    shutil.copy(home.bin + "/stub", f"{home.dir}/as/{name}/{name}")  # a copy: a hard link keeps the name "stub"
    open(f"{home.bin}/{name}", "w").write(f"#!/bin/sh\nCHUD_STUB_REPLAY={home.dir}/{name}.raw exec {home.dir}/as/{name}/{name} \"$@\"\n")
    os.chmod(f"{home.bin}/{name}", 0o755)

screen = pyte.Screen(cols, rows)
stream = pyte.ByteStream(screen)
c = Chud(binary, home, cols, rows, feed=lambda data: stream.feed(re.sub(rb"\x1b\[\?[0-9;]*n", b"", data)),
         CHUD_THEME="light" if light else "dark", TERM_PROGRAM="chud-app")
c.pump(5)
for a in actions:
    kind, _, value = a.partition(":")
    if kind == "k":
        os.write(c.fd, value.encode().decode("unicode_escape").encode("latin-1")); c.pump(0.8)
    elif kind == "m":
        x, y = map(int, value.split(","))
        os.write(c.fd, f"\x1b[<0;{x+1};{y+1}M".encode()); c.pump(0.1)
        os.write(c.fd, f"\x1b[<0;{x+1};{y+1}m".encode()); c.pump(0.8)
    elif kind == "w":
        c.pump(float(value))
    elif kind == "r":
        cols, rows = map(int, value.split(",")); screen.resize(rows, cols); c.resize(cols, rows); c.pump(1.5)
c.kill()

fonts, size = REPO + "/app/fonts/", 26
regular, bold = (ImageFont.truetype(fonts + f"FiraCodeNerdFont-{w}.ttf", size) for w in ("Regular", "Bold"))
symbols = ImageFont.truetype(fonts + "NotoSansSymbols2-Regular.ttf", size)
known = TTFont(fonts + "FiraCodeNerdFont-Regular.ttf").getBestCmap()
cw, ch = int(regular.getlength("M")), int(size * 1.25)
ground, ink = ("fafafa", "383a42") if light else ("181818", "d8d8d8")
named = {"black": "181818", "red": "ac4242", "green": "90a959", "brown": "f4bf75", "blue": "6a9fb5", "magenta": "aa759f",
         "cyan": "75b5aa", "white": "d8d8d8", "brightblack": "6b6b6b"}
colour = lambda c, default: default if c == "default" else c if re.fullmatch(r"[0-9a-fA-F]{6}", c) else named.get(c, default)
image = Image.new("RGB", (cols * cw, rows * ch), "#" + ground)
draw = ImageDraw.Draw(image)
for y in range(rows):
    for x in range(cols):
        cell = screen.buffer[y][x]
        fg, bg = colour(cell.fg, ink), colour(cell.bg, ground)
        if cell.reverse:
            fg, bg = bg, fg
        if bg != ground:
            draw.rectangle([x * cw, y * ch, (x + 1) * cw - 1, (y + 1) * ch - 1], fill="#" + bg)
        if cell.data.strip():
            font = symbols if ord(cell.data[0]) not in known else bold if cell.bold else regular
            draw.text((x * cw, y * ch + 2), cell.data, font=font, fill="#" + fg)
image.save(out)
print("saved", out)
