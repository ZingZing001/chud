#!/usr/bin/env python3
"""Checks that a selection held past a pane's edge keeps scrolling, and copies everything it
passed over: in a full-screen program that scrolls itself, and in a plain shell's scrollback.
Usage: tools/selection.py [chud binary]. Puts your clipboard back the way it was."""
import os, re, subprocess, sys
from harness import Chud, Home, REPO

binary = sys.argv[1] if len(sys.argv) > 1 else REPO + "/target/release/chud"
here = os.path.dirname(os.path.abspath(__file__))
COLS, ROWS, SIDE = 120, 40, 34
X = SIDE  # the pane's first column
failed = []


def check(name, ok, detail=""):
    print(("ok   " if ok else "FAIL ") + name + (f"\n     {detail}" if not ok else ""))
    if not ok:
        failed.append(name)


def mouse(c, code, x, y, up=False, wait=0.15):
    os.write(c.fd, f"\x1b[<{code};{x + 1};{y + 1}{'m' if up else 'M'}".encode())
    c.pump(wait)


def drag(c, start, path, hold, end):
    """Press at `start`, move through `path`, stay at the last point for `hold` seconds, let go."""
    mouse(c, 0, *start)
    for p in path:
        mouse(c, 32, *p)
    c.pump(hold)
    mouse(c, 0, *end, up=True, wait=0.5)
    return subprocess.run(["pbpaste"], capture_output=True, text=True).stdout


def session(argv, home):
    return {"argv": argv, "cwd": home.dir, "group": 0, "sid": "", "worked": 0, "resume": None, "name": "test"}


def numbers(text):
    return [int(n) for n in re.findall(r"^(?:line )?(\d+)", text, re.M)]


def in_order(ns):
    return all(b > a for a, b in zip(ns, ns[1:]))


saved = subprocess.run(["pbpaste"], capture_output=True).stdout
try:
    # a full-screen program: header row 0, the transcript from pane row 1, so pane row r shows
    # transcript line top + r - 1; the pane starts below the toolbar and the session's header
    home = Home()
    state = lambda argv: {"side": SIDE, "groups": [{"id": 0, "name": "all", "collapsed": False}], "sessions": [session(argv, home)], "layout": 0}
    home.state(state(["python3", f"{here}/fullscreen.py", "100"]))
    c = Chud(binary, home, COLS, ROWS)
    c.pump(3)
    top_y, bottom_y = 2, ROWS - 2  # the pane's first and last rows; ROWS - 1 is the status bar
    text = drag(c, (X, top_y + 5), [(X + 3, top_y + 8), (X + 3, ROWS - 1)], 2.5, (X + 3, ROWS - 1))
    ns = numbers(text)
    check("down: starts where the press was", bool(ns) and ns[0] == 104, text[:200])
    check("down: runs past the screen it started on", bool(ns) and ns[-1] > 104 + (bottom_y - top_y) + 10, f"last line {ns[-1] if ns else None}")
    check("down: every line once, in order", in_order(ns) and len(ns) == len({n for n in range(ns[0], ns[-1] + 1) if n % 7 not in (2, 4)}), ns)
    check("down: no scrollbar, box or spinner", not re.search(r"[│█╭╰╮╯]|Thinking", text), text[-300:])
    c.kill()

    home.state(state(["python3", f"{here}/fullscreen.py", "200"]))
    c = Chud(binary, home, COLS, ROWS)
    c.pump(3)
    text = drag(c, (X + 10, bottom_y - 6), [(X + 10, bottom_y - 9), (X + 10, 1)], 2.5, (X + 10, 1))
    ns = numbers(text)
    last = 200 + (bottom_y - 6 - top_y) - 1
    check("up: ends where the press was", bool(ns) and ns[-1] == last, text[-200:])
    check("up: reaches back past the screen it started on", bool(ns) and ns[0] < 200 - 10, f"first line {ns[0] if ns else None}")
    check("up: every line once, in order", in_order(ns), ns)
    c.kill()

    # a plain shell: chud's own scrollback, scrolled while held above the pane
    home.state(state(["zsh"]))
    c = Chud(binary, home, COLS, ROWS)
    c.pump(2)
    c.type("clear; seq 1 500\r", 2)
    # pressed past the end of a line, so the whole of it is in the selection
    text = drag(c, (X + 20, bottom_y - 2), [(X + 20, bottom_y - 5), (X, 1)], 2.0, (X, 1))
    ns = numbers(text)
    check("shell: reaches back past the screen", len(ns) > 2 * (bottom_y - top_y), f"{len(ns)} lines")
    check("shell: consecutive", ns == list(range(ns[0], ns[0] + len(ns))) if ns else False, ns[:5])
    c.kill()
finally:
    subprocess.run(["pbcopy"], input=saved)

sys.exit(1 if failed else 0)
