# tools

Checks that run a real chud in a pty. Each makes a throwaway HOME with stand-in `claude` and
`copilot` programs, so nothing here touches your saved sessions, your chats or your quota.
Build first (`cargo build --release`); every script takes another binary to compare against.

- `python3 tools/restore.py` — sessions come back after chud is killed: a chat opened by typing
  `claude --resume`, twice over; a second chud leaving the first one's alone; a chat saved twice
  resumed once; an agent you quit staying quit. Exits non-zero on a failure.
- `python3 tools/load.py [chud] [seconds]` — chud's CPU with twelve sessions printing hard.
  Run it on the build before and after a change to the drawing or the event loop.
- `python3 tools/shot.py out.png [--light] [--size 110x42] [actions]` — a PNG of what chud draws,
  for looking at a UI change. `m:22,0` clicks the Dashboard button, `k:\x01?` opens help.
  Needs `pip install pyte pillow fonttools`.
