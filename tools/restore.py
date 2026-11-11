#!/usr/bin/env python3
"""Checks that chud brings sessions back after being killed. Usage: tools/restore.py [chud binary]
Runs chud with stand-in agents in a throwaway HOME; exits non-zero if a check fails."""
import sys
from harness import Chud, Home, REPO

binary = sys.argv[1] if len(sys.argv) > 1 else REPO + "/target/release/chud"
failed = []


def check(name, ok, detail=""):
    print(("ok   " if ok else "FAIL ") + name + (f"\n     {detail}" if not ok else ""))
    if not ok:
        failed.append(name)


def shells(chud):
    return [cmd for cmd, _ in chud.running()]


# 1. an agent typed into a terminal comes back, again and again
home = Home()
home.chat("chat-one", home.dir)
c = Chud(binary, home)
c.pump(3)
c.type("claude --resume chat-one\r", 6)
c.kill()
saved = home.state()["sessions"][0]["resume"]
check("a chat opened by typing `claude --resume` is saved", bool(saved) and saved["sid"] == "chat-one", saved)
for attempt in (1, 2):
    c = Chud(binary, home)
    c.pump(5)
    check(f"restart {attempt} reopens it", shells(c) == ["zsh -ic claude --resume chat-one; exec zsh"], shells(c))
    c.kill()

# 2. a second chud leaves the first one's sessions and saved state alone
first = Chud(binary, home)
first.pump(5)
before = home.state()
second = Chud(binary, home)
second.pump(5)
check("a second chud does not resume the same chat", shells(second) == ["zsh"], shells(second))
second.type("\x01n", 2)  # a new terminal: something worth saving
check("and does not save over the first", home.state() == before)
second.kill()
first.kill()

# 3. two saved sessions on one chat: only the first resumes it
home = Home()
home.chat("chat-two", home.dir)
entry = lambda: {"argv": ["zsh"], "cwd": home.dir, "group": 0, "sid": "", "worked": 0,
                 "resume": {"agent": "claude", "sid": "chat-two", "cwd": home.dir}}
home.state({"side": 34, "groups": [{"id": 0, "name": "all", "collapsed": False}], "sessions": [entry(), entry()], "layout": 0})
c = Chud(binary, home)
c.pump(6)
check("a chat saved twice is resumed once", sorted(shells(c)) == ["zsh", "zsh -ic claude --resume chat-two; exec zsh"], shells(c))
c.kill()

# 4. quitting the agent means it stays quit
home = Home()
home.chat("chat-three", home.dir)
c = Chud(binary, home)
c.pump(3)
c.type("claude --resume chat-three\r", 6)
c.type("\x03", 4)  # the stand-in dies on Ctrl-C, back to the prompt
c.kill()
check("an agent you quit is not reopened", home.state()["sessions"][0]["resume"] is None, home.state()["sessions"][0])

sys.exit(1 if failed else 0)
