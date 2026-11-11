#!/usr/bin/env python3
"""Measures chud's CPU under heavy output. Usage: tools/load.py [chud binary] [seconds]
Twelve sessions, four on screen: six stream ~1500 coloured lines a second, six repaint a 25-line
live region 30 times a second the way an agent's spinner does."""
import os, sys, time
from harness import Chud, Home, REPO

binary = sys.argv[1] if len(sys.argv) > 1 else REPO + "/target/release/chud"
seconds = float(sys.argv[2]) if len(sys.argv) > 2 else 15

STREAM = r'''
import sys, time, random
words = "the quick brown fox jumps over lazy dog fn let mut struct impl return match".split()
n = 0
while True:
    sys.stdout.write(f"\x1b[3{n%7+1}m{n:6d}\x1b[0m " + " ".join(random.choice(words) for _ in range(18)) + "\r\n"); n += 1
    if n % 50 == 0: sys.stdout.flush(); time.sleep(0.033)
'''
SPIN = r'''
import sys, time, random
words = "the quick brown fox jumps over lazy dog fn let mut struct impl return match".split()
body = [" ".join(random.choice(words) for _ in range(14)) for _ in range(20)]; i = 0
sys.stdout.write("\x1b[2J")
while True:
    body[i % 20] = " ".join(random.choice(words) for _ in range(14))
    out = ["\x1b[?2026h"] + [f"\x1b[{k+1};1H\x1b[37m●\x1b[0m {l}\x1b[K" for k, l in enumerate(body)]
    out.append(f"\x1b[22;1H\x1b[31m{'⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'[i%10]}\x1b[0m Thinking… ({i//30}s)\x1b[K\x1b[24;1H> \x1b[K\x1b[?2026l")
    sys.stdout.write("".join(out)); sys.stdout.flush(); i += 1; time.sleep(0.033)
'''

home = Home()
for name, code in (("stream", STREAM), ("spin", SPIN)):
    open(f"{home.dir}/{name}.py", "w").write(code)
session = lambda i: {"argv": ["python3", f"{home.dir}/{'stream' if i % 2 == 0 else 'spin'}.py"], "cwd": home.dir,
                     "name": f"load {i}", "group": 0, "sid": "", "worked": 0, "resume": None}
split = lambda a, b, d: {"a": a, "b": b, "dir": d, "ratio": 0.5}
home.state({"side": 34, "groups": [{"id": 0, "name": "load", "collapsed": False}],
            "sessions": [session(i) for i in range(12)], "layout": split(split(0, 1, "across"), split(2, 3, "across"), "down")})

frames = [0]
c = Chud(binary, home, cols=160, rows=48, feed=lambda data: frames.__setitem__(0, frames[0] + data.count(b"\x1b[?2026l")))
c.pump(6)  # the sessions off screen start a moment after the ones on it
cpu, drawn, shown, start = c.cpu(), c.drawn, frames[0], time.time()
c.pump(seconds)
took = time.time() - start
print(f"chud {100 * (c.cpu() - cpu) / took:.0f}% of a core · {(c.drawn - drawn) / took / 1e3:.0f} KB/s drawn · {(frames[0] - shown) / took:.0f} frames/s")
c.kill()
