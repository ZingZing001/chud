"""Shared by the scripts here: runs a chud binary in a pty with a throwaway HOME, and builds
stand-in agents, so nothing touches your real ~/.config/chud, your chats or your quota."""
import fcntl, json, os, pty, select, signal, struct, subprocess, tempfile, termios, time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# A program called `claude` or `copilot` as far as chud can tell (it goes by the executable's
# name). Like the real ones it notes which chat it has open under ~/.claude/sessions/<pid>.json,
# prints a line, and waits.
STUB = r'''
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
int main(int argc, char **argv) {
    const char *id = "", *home = getenv("HOME");
    char path[4096], cwd[4096];
    for (int i = 1; i + 1 < argc; i++)
        if (!strcmp(argv[i], "--resume") || !strcmp(argv[i], "--session-id")) id = argv[i + 1];
    getcwd(cwd, sizeof cwd);
    snprintf(path, sizeof path, "%s/.claude/sessions/%d.json", home, getpid());
    FILE *f = fopen(path, "w");
    if (f) { fprintf(f, "{\"sessionId\":\"%s\",\"cwd\":\"%s\"}", id, cwd); fclose(f); }
    const char *replay = getenv("CHUD_STUB_REPLAY");
    if (replay && (f = fopen(replay, "r"))) { int c; while ((c = fgetc(f)) != EOF) putchar(c); fclose(f); }
    else printf("stand-in agent, chat %s\r\n", id);
    fflush(stdout);
    for (;;) pause();
}
'''


class Home:
    """A throwaway HOME with stand-in `claude` and `copilot` on its PATH."""

    def __init__(self):
        self.dir = os.path.realpath(tempfile.mkdtemp(prefix="chud-test-"))
        self.bin = self.dir + "/bin"
        for d in (".config/chud", ".claude/sessions", ".claude/projects", ".copilot/session-state", "bin"):
            os.makedirs(f"{self.dir}/{d}")
        open(f"{self.dir}/.config/chud/config.json", "w").write('{"theme":"dark"}')  # skips first-run setup
        src = self.dir + "/stub.c"
        open(src, "w").write(STUB)
        subprocess.run(["cc", "-w", "-o", self.bin + "/claude", src], check=True)
        subprocess.run(["cp", self.bin + "/claude", self.bin + "/copilot"], check=True)

    def chat(self, sid, cwd):
        """A Claude transcript for `sid` in `cwd`, so chud believes the chat can be resumed."""
        slug = "".join(c if c.isascii() and c.isalnum() else "-" for c in cwd)
        os.makedirs(f"{self.dir}/.claude/projects/{slug}", exist_ok=True)
        open(f"{self.dir}/.claude/projects/{slug}/{sid}.jsonl", "w").write(json.dumps({"cwd": cwd}) + "\n")

    def state(self, value=None):
        path = self.dir + "/.config/chud/state.json"
        if value is not None:
            open(path, "w").write(json.dumps(value))
        return json.load(open(path)) if os.path.exists(path) else None

    def env(self, **extra):
        env = {k: v for k, v in os.environ.items() if not k.startswith(("CLAUDE", "ANTHROPIC"))}
        env.update(HOME=self.dir, TERM="xterm-256color", CHUD_SETUP="skip", CHUD_NO_UPDATE="1", SHELL="/bin/zsh",
                   PATH=self.bin + ":" + env["PATH"], **extra)
        return env


class Chud:
    """A chud running in a pty. `feed` is called with everything it draws."""

    def __init__(self, binary, home, cols=150, rows=42, feed=None, **env):
        self.feed, self.drawn = feed, 0
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(home.dir)
            os.execve(os.path.abspath(binary), ["chud"], home.env(**env))
        self.resize(cols, rows)

    def resize(self, cols, rows):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)

    def pump(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            if select.select([self.fd], [], [], 0.05)[0]:
                try:
                    data = os.read(self.fd, 1 << 20)
                except OSError:
                    return
                self.drawn += len(data)
                if self.feed:
                    self.feed(data)

    def type(self, text, wait=0.5):
        for ch in text:
            os.write(self.fd, ch.encode())
            time.sleep(0.01)
        self.pump(wait)

    def running(self):
        """The command lines of everything chud started, and what those started."""
        ps = lambda pid: subprocess.run(["ps", "-o", "args=", "-p", pid], capture_output=True, text=True).stdout.strip()
        kids = lambda pid: subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout.split()
        return [(ps(k), [ps(g) for g in kids(k)]) for k in kids(self.pid)]

    def cpu(self):
        t = subprocess.run(["ps", "-o", "time=", "-p", str(self.pid)], capture_output=True, text=True).stdout.strip()
        parts = [float(x) for x in t.split(":")]
        return sum(p * 60 ** i for i, p in enumerate(reversed(parts)))

    def kill(self):
        """The way closing the window does it: no quit, no chance to save."""
        subprocess.run(["pkill", "-9", "-P", str(self.pid)])
        os.kill(self.pid, signal.SIGKILL)
        os.close(self.fd)
        os.waitpid(self.pid, 0)
        time.sleep(0.5)
