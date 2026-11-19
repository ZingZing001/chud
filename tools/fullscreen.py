"""A stand-in for a full-screen agent (Claude Code, Copilot): it draws in the alternate screen,
takes the mouse, and scrolls its own transcript three lines per wheel notch. A header on top, a
spinner and an input box below, and a scrollbar in the last column, as they have. Each frame is
written in two pieces with a pause between, as a real one can arrive.
Usage: python3 fullscreen.py [first line shown]"""
import fcntl, os, re, select, signal, struct, sys, termios, time, tty

LINES = []
for i in range(400):
    LINES.append({2: "", 4: "}"}.get(i % 7, f"line {i:03} of the reply, with words to copy"))
top = int(sys.argv[1]) if len(sys.argv) > 1 else 100
out = sys.stdout


def size():
    rows, cols, *_ = struct.unpack("HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8))
    return rows, cols


def frame(n):
    global top
    rows, cols = size()
    body = rows - 5
    top = max(0, min(top, len(LINES) - body))
    thumb = range(1 + top * body // len(LINES), 1 + (top + body) * body // len(LINES) + 1)
    put = []
    put.append(f"\x1b[1;1H\x1b[2K  stand-in agent · ~/repo")
    for r in range(body):
        bar = "█" if r + 1 in thumb else "│"
        put.append(f"\x1b[{r + 2};1H\x1b[2K{LINES[top + r][: cols - 2]}\x1b[{r + 2};{cols}H{bar}")
    put.append(f"\x1b[{rows - 3};1H\x1b[2K{'✻✶✳✢'[n % 4]} Thinking… ({n // 10}s)")
    put.append(f"\x1b[{rows - 2};1H\x1b[2K╭{'─' * (cols - 4)}╮")
    put.append(f"\x1b[{rows - 1};1H\x1b[2K│ > {' ' * (cols - 7)}│")
    put.append(f"\x1b[{rows};1H\x1b[2K╰{'─' * (cols - 4)}╯")
    text = "".join(put)
    half = len(text) // 2
    out.write("\x1b[?2026h" + text[:half]); out.flush()
    time.sleep(0.005)
    out.write(text[half:] + "\x1b[?2026l"); out.flush()


tty.setraw(0)
out.write("\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h")
signal.signal(signal.SIGWINCH, lambda *_: frame(0))
n = 0
frame(n)
pending = b""
while True:
    if select.select([0], [], [], 0.1)[0]:
        pending += os.read(0, 4096)
        for button in re.findall(rb"\x1b\[<(\d+);\d+;\d+[Mm]", pending):
            if button in (b"64", b"65"):
                top += 3 if button == b"65" else -3
        pending = pending[pending.rfind(b"\x1b") :] if b"\x1b" in pending and not re.search(rb"[Mm]$", pending) else b""
    n += 1
    frame(n)  # the spinner turns whether or not anything scrolled
