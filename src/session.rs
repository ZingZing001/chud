use crate::agents::{self, Activity};
use crate::usage::{self, Usage};
use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc::Sender, Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Status {
    #[default]
    Idle,
    Working,
    NeedsInput,
    Done,
    Exited,
}

pub enum Event {
    Input(crossterm::event::Event),
    Output(usize),
    Exited(usize),
    /// Copilot's monthly premium-request quota, fetched from GitHub in the background
    Copilot(Option<(usage::Window, u64)>),
    /// a newer chud was pulled and built, or the attempt failed
    Updated(crate::update::Update),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Agent {
    Claude,
    Copilot,
    /// one of `agents::all()`: a built-in like Codex, or one from config.json
    Profile(usize),
    Shell,
    Other(String),
}

/// Classifies the program at `path` (a session's foreground process, or a command name).
pub fn agent_of(path: &str) -> Agent {
    agent_in(path, agents::all())
}

fn agent_in(path: &str, profiles: &[agents::Profile]) -> Agent {
    let name = path.rsplit('/').next().unwrap_or(path).trim_start_matches('-'); // login shells: "-zsh"
    match name {
        "claude" => Agent::Claude,
        _ if path.contains("/claude/versions/") => Agent::Claude, // native install: .../claude/versions/2.1.270
        "copilot" => Agent::Copilot,
        "zsh" | "bash" | "fish" | "sh" | "dash" | "nu" => Agent::Shell,
        _ => match profiles.iter().position(|p| p.matches.iter().any(|m| m == name)) {
            Some(i) => Agent::Profile(i),
            None => Agent::Other(name.to_string()),
        },
    }
}

/// The agent a command line runs, for when the process in front is an interpreter:
/// `node /opt/homebrew/bin/gemini --yolo` is Gemini, `python3 -m aider` is aider.
fn agent_in_cmdline(args: &str, profiles: &[agents::Profile]) -> Option<Agent> {
    args.split_whitespace()
        .skip(1)
        .map(|word| agent_in(word, profiles))
        .find(|a| matches!(a, Agent::Claude | Agent::Copilot | Agent::Profile(_)))
}

/// A chat title from the terminal title: drops Claude's spinner glyphs, Copilot's suffix and
/// the placeholder titles both show before the first prompt.
pub fn chat_name(title: &str) -> Option<String> {
    let t = title.trim_start_matches(|c: char| !c.is_alphanumeric()).trim();
    let t = t.strip_suffix(" - GitHub Copilot").unwrap_or(t).trim();
    (!matches!(t, "" | "Claude Code" | "GitHub Copilot")).then(|| t.to_string())
}

/// vt100 callbacks: turns the agent's escape sequences into status, chat name and ids.
#[derive(Default)]
pub struct Signals {
    pub status: Status,
    /// the program reports progress itself (OSC 9;4), so its status is never guessed
    pub progress_seen: bool,
    pub title: String,
    pub chat: String,
    pub summary: String,
    pub sid: String,
    pub agent_cwd: String,
    reply: Vec<u8>,
}

impl vt100::Callbacks for Signals {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.set(Status::NeedsInput, None);
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
        // keep the last real name: claude clears its title on exit
        if let Some(chat) = chat_name(&self.title) {
            self.chat = chat;
        }
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, p: &[&[u8]]) {
        let rest = |i: usize| String::from_utf8_lossy(&p[i..].join(&b';')).into_owned();
        match p {
            // progress (Copilot): 0 = cleared, anything else = busy
            [b"9", b"4", state, ..] => {
                self.progress_seen = true;
                self.set(if *state == b"0" { Status::Done } else { Status::Working }, None)
            }
            [b"9", ..] => self.set(Status::NeedsInput, Some(rest(1))),
            [b"777", b"notify", b"warp://cli-agent", ..] => self.warp(&rest(3)),
            [b"777", b"notify", _, ..] => self.set(Status::NeedsInput, Some(rest(3))),
            _ => {}
        }
    }

    // Answer the two queries TUIs block on at startup (cursor position, device attributes).
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, p0) {
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.reply
                    .extend(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            (None, 'c', 0) => self.reply.extend(b"\x1b[?62;22c"),
            _ => {}
        }
    }
}

impl Signals {
    fn set(&mut self, status: Status, summary: Option<String>) {
        // A bell/notify only means "needs you" mid-turn; after the turn it's claude's
        // 60s "still waiting" reminder, which must not flip Done back.
        if status == Status::NeedsInput && self.status != Status::Working {
            return;
        }
        self.status = status;
        if let Some(s) = summary {
            self.summary = s;
        }
    }

    // Payloads from the installed claude-code-warp plugin (scripts/build-payload.sh).
    fn warp(&mut self, json: &str) {
        let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
        let field = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(String::from);
        if let Some(s) = field("session_id") {
            self.sid = s;
        }
        if let Some(s) = field("cwd") {
            self.agent_cwd = s;
        }
        // vte keeps only 16 OSC params, so a payload with many ';' arrives truncated.
        // "event" sits near the front, so fall back to reading just that.
        let event = v["event"].as_str().or_else(|| json.split("\"event\":\"").nth(1)?.split('"').next());
        let status = match event {
            Some("prompt_submit" | "tool_complete") => Status::Working,
            Some("permission_request") => Status::NeedsInput,
            Some("stop" | "stop_failure") => Status::Done,
            _ => return,
        };
        self.set(status, field("summary").or(field("response")).or(field("query")));
    }
}

pub struct Session {
    pub id: usize,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub parser: Arc<Mutex<vt100::Parser<Signals>>>,
    pub name: Option<String>,
    pub group: usize,
    pub agent: Agent,
    pub sid: String,
    pub seen: Status,
    pub unread: bool,
    pub working_since: Option<Instant>,
    /// total time the agent spent working in earlier stretches (the chud's diet)
    pub worked: Duration,
    pub usage: Usage,
    pub exit: Option<u32>,
    probed: Instant,
    /// the process in front (the agent, when one runs), and the Copilot session found for it
    fg_pid: Option<i32>,
    found: Option<String>,
    /// the agent a script or interpreter in front is running, read from its command line
    fg_script: Option<Agent>,
    activity: Activity,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl Session {
    pub fn spawn(
        id: usize,
        argv: Vec<String>,
        cwd: PathBuf,
        (rows, cols): (u16, u16),
        tx: Sender<Event>,
    ) -> Result<Self> {
        let agent = agent_of(&argv[0]);
        // Pick the agent's session id ourselves so we can find its log and resume it later.
        let mut run = argv.clone();
        let mut sid = resumed_id(&argv).unwrap_or_default();
        if matches!(agent, Agent::Claude | Agent::Copilot) && !continues(&argv) {
            sid = uuid();
            run.extend(["--session-id".to_string(), sid.clone()]);
        }

        let pair = native_pty_system().openpty(size(rows, cols))?;
        let mut cmd = CommandBuilder::new(&run[0]);
        cmd.args(&run[1..]);
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("TERM_PROGRAM", "ghostty"); // copilot only emits OSC 9;4 progress for terminals it knows
        // If chud was started from inside a Claude Code session, drop that session's identity:
        // claude refuses to start "inside" it (CLAUDECODE), and an interactive claude that
        // inherits the rest never writes its transcript (no usage numbers, nothing to resume).
        // User settings such as CLAUDE_CODE_USE_BEDROCK pass through.
        for var in [
            "CLAUDECODE",
            "CLAUDE_PID",
            "CLAUDE_EFFORT",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_BRIDGE_SESSION_ID",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_SESSION_ATTENDED",
        ] {
            cmd.env_remove(var);
        }
        // ponytail: poses as Warp so the installed claude-code-warp plugin reports status;
        // ship our own hooks.json plugin if Warp changes that protocol.
        cmd.env("WARP_CLI_AGENT_PROTOCOL_VERSION", "1");
        cmd.env("WARP_CLIENT_VERSION", "v0.2099.01.01.00.00.stable_00");
        if let Agent::Profile(i) = agent {
            for (k, v) in &agents::all()[i].env {
                cmd.env(k, v);
            }
        }
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            5000,
            Signals::default(),
        )));
        let (p, w) = (parser.clone(), writer.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                let reply = {
                    let mut p = p.lock().unwrap();
                    p.process(&buf[..n]);
                    std::mem::take(&mut p.callbacks_mut().reply)
                };
                if !reply.is_empty() {
                    let _ = w.lock().unwrap().write_all(&reply);
                }
                if tx.send(Event::Output(id)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Event::Exited(id));
        });

        Ok(Self {
            id,
            argv,
            cwd,
            parser,
            name: None,
            group: 0,
            agent,
            sid,
            seen: Status::Idle,
            unread: false,
            working_since: None,
            worked: Duration::ZERO,
            usage: Usage::default(),
            exit: None,
            probed: Instant::now(),
            fg_pid: None,
            found: None,
            fg_script: None,
            activity: Activity::default(),
            writer,
            master: pair.master,
            child,
        })
    }

    /// You answered what the agent was waiting on. Agents only report again when the *next*
    /// step ends — Claude's comes when the tool you approved finishes, which can be minutes — so
    /// the answer itself is taken as the sign it is back at work. Asking again sets it back.
    pub fn answered(&mut self) -> bool {
        let parser = self.parser.clone();
        let mut p = parser.lock().unwrap();
        let signals = p.callbacks_mut();
        if self.exit.is_some() || signals.status != Status::NeedsInput {
            return false;
        }
        signals.status = Status::Working;
        true
    }

    /// Enter was sent to this session: the start of a possible stretch of work.
    pub fn submit(&mut self) {
        if self.infers_status() {
            self.activity.submit(Instant::now());
        }
    }

    /// The program printed something. For an agent that reports nothing itself, that may mean
    /// it started working.
    pub fn on_output(&mut self) {
        self.guess(|a, now, st| a.output(now, st));
    }

    /// Once a tick: the agent may have gone quiet, which means done. True when the status changed.
    pub fn on_tick(&mut self) -> bool {
        self.guess(|a, now, st| a.quiet(now, st))
    }

    fn guess(&mut self, step: impl FnOnce(&mut Activity, Instant, Status) -> Option<Status>) -> bool {
        if !self.infers_status() {
            return false;
        }
        let parser = self.parser.clone();
        let mut p = parser.lock().unwrap();
        let signals = p.callbacks_mut();
        if signals.progress_seen {
            return false;
        }
        match step(&mut self.activity, Instant::now(), signals.status) {
            Some(st) => {
                signals.status = st;
                true
            }
            None => false,
        }
    }

    fn infers_status(&self) -> bool {
        self.exit.is_none() && matches!(self.agent, Agent::Profile(i) if agents::all().get(i).is_some_and(|p| p.infer_status))
    }

    pub fn write(&self, bytes: &[u8]) {
        let _ = self.writer.lock().unwrap().write_all(bytes);
    }

    pub fn paste(&self, text: &str) {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if self.parser.lock().unwrap().screen().bracketed_paste() {
            self.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.write(text.as_bytes());
        }
    }

    pub fn resize(&self, (rows, cols): (u16, u16)) {
        let _ = self.master.resize(size(rows, cols));
        self.parser.lock().unwrap().screen_mut().set_size(rows, cols);
    }

    pub fn status(&self) -> Status {
        match self.exit {
            Some(_) => Status::Exited,
            None => self.parser.lock().unwrap().callbacks().status,
        }
    }

    /// Sidebar name: your rename, else the chat's title, else the folder.
    pub fn label(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        let chat = self.parser.lock().unwrap().callbacks().chat.clone();
        if chat.is_empty() { self.folder() } else { chat }
    }

    /// All the time the agent has spent working, including the current stretch.
    pub fn worked(&self) -> Duration {
        self.worked + self.working_since.map_or(Duration::ZERO, |t| t.elapsed())
    }

    pub fn folder(&self) -> String {
        base(&self.cwd)
    }

    /// Last prompt / response / permission request, on one line.
    pub fn detail(&self) -> String {
        let p = self.parser.lock().unwrap();
        p.callbacks().summary.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The agent's own session id: as reported by the plugin, else the one we assigned.
    pub fn agent_sid(&self) -> String {
        let p = self.parser.lock().unwrap();
        let reported = &p.callbacks().sid;
        if reported.is_empty() { self.sid.clone() } else { reported.clone() }
    }

    /// Re-checks which program is in front, at most once a second. True if it changed.
    pub fn probe(&mut self) -> bool {
        if self.exit.is_some() || self.probed.elapsed() < Duration::from_secs(1) {
            return false;
        }
        self.probed = Instant::now();
        let Some(pid) = self.master.process_group_leader() else { return false };
        let Some(mut agent) = proc_path(pid).map(|p| agent_of(&p)) else { return false };
        if self.fg_pid != Some(pid) {
            (self.fg_pid, self.found) = (Some(pid), None);
            // A shell or an interpreter may be running an agent: a custom harness is often a
            // script, and Gemini CLI is Node. The command line says which; it is read once per
            // process, not on every burst of output.
            let scripted = match &agent {
                Agent::Shell => true,
                Agent::Other(name) => agents::is_interpreter(name),
                _ => false,
            };
            self.fg_script = if scripted { proc_args(pid).and_then(|a| agent_in_cmdline(&a, agents::all())) } else { None };
        }
        if let Some(script) = &self.fg_script {
            agent = script.clone();
        }
        if agent == self.agent {
            return false;
        }
        self.agent = agent;
        let mut p = self.parser.lock().unwrap();
        let s = p.callbacks_mut();
        s.chat.clear(); // a different program: the old chat name and summary no longer apply
        s.summary.clear();
        true
    }

    pub fn refresh_usage(&mut self) {
        let copilot = match self.agent {
            Agent::Copilot => true,
            Agent::Claude => false,
            _ => return,
        };
        // the running agent's own session, found by its pid: this also covers an agent
        // started by hand in a shell, where chud didn't choose the session id
        let home = std::env::var("HOME").map(PathBuf::from);
        let by_pid = match (self.fg_pid, &home) {
            (Some(pid), Ok(home)) if copilot && self.found.is_none() => {
                self.found = find_sid(home, &self.agent, pid).map(|(sid, _)| sid);
                None
            }
            (Some(pid), Ok(home)) if !copilot => find_sid(home, &self.agent, pid),
            _ => None,
        };
        let (sid, found_cwd) = match (by_pid, &self.found) {
            (Some((sid, cwd)), _) => (sid, cwd),
            (None, Some(sid)) if copilot => (sid.clone(), None),
            _ => (self.agent_sid(), None),
        };
        let agent_cwd = self.parser.lock().unwrap().callbacks().agent_cwd.clone();
        let cwd = found_cwd
            .or_else(|| Some(PathBuf::from(&agent_cwd)).filter(|_| !agent_cwd.is_empty()))
            .unwrap_or_else(|| self.cwd.clone());
        if let Some(path) = usage::log_path(copilot, &sid, &cwd) {
            self.usage.refresh(&path, copilot);
        }
        // Claude Code itself reports what its context holds and how big the window is, which
        // beats adding up the transcript against a guessed window (see usage::claude_context).
        if let Some((used, window)) = (!copilot).then(|| usage::claude_context(&sid)).flatten() {
            self.usage.context = used;
            self.usage.window = window;
        }
    }

    pub fn reap(&mut self) {
        self.exit = Some(self.child.wait().map(|s| s.exit_code()).unwrap_or(1));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// The agent's own session for the process `pid`: Claude keeps ~/.claude/sessions/<pid>.json
/// (session id and folder), Copilot marks its session folder with inuse.<pid>.lock.
fn find_sid(home: &Path, agent: &Agent, pid: i32) -> Option<(String, Option<PathBuf>)> {
    match agent {
        Agent::Claude => {
            let text = std::fs::read_to_string(home.join(format!(".claude/sessions/{pid}.json"))).ok()?;
            let v: serde_json::Value = serde_json::from_str(&text).ok()?;
            Some((v["sessionId"].as_str()?.to_string(), v["cwd"].as_str().map(PathBuf::from)))
        }
        Agent::Copilot => std::fs::read_dir(home.join(".copilot/session-state"))
            .ok()?
            .flatten()
            .find(|e| e.path().join(format!("inuse.{pid}.lock")).exists())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), None)),
        _ => None,
    }
}

/// Session id named on the command line (`--resume <id>`, `--session-id <id>`, `--resume=<id>`).
fn resumed_id(argv: &[String]) -> Option<String> {
    argv.iter().enumerate().find_map(|(i, a)| match a.as_str() {
        "--session-id" | "--resume" | "-r" => argv.get(i + 1).filter(|v| !v.starts_with('-')).cloned(),
        _ => a.strip_prefix("--resume=").or(a.strip_prefix("--session-id=")).map(String::from),
    })
}

/// Claude rejects --session-id together with these.
fn continues(argv: &[String]) -> bool {
    argv.iter().any(|a| {
        matches!(a.as_str(), "-c" | "--continue" | "-r" | "--resume" | "--session-id")
            || a.starts_with("--resume=")
            || a.starts_with("--session-id=")
    })
}

fn uuid() -> String {
    let mut b = [0u8; 16];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    b[6] = b[6] & 0x0f | 0x40; // version 4
    b[8] = b[8] & 0x3f | 0x80; // RFC 4122 variant
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// The full command line of `pid`, for interpreters whose path alone does not say what they run.
fn proc_args(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps").args(["-o", "args=", "-p", &pid.to_string()]).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|a| !a.is_empty())
}

#[cfg(target_os = "macos")]
fn proc_path(pid: i32) -> Option<String> {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is valid for its full length, which is what we pass.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn proc_path(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| p.to_string_lossy().into_owned())
}

fn size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

fn base(p: &Path) -> String {
    p.file_name().map_or("/".into(), |n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(bytes: &[u8]) -> Signals {
        let mut p = vt100::Parser::new_with_callbacks(40, 120, 0, Signals::default());
        p.process(bytes);
        std::mem::take(p.callbacks_mut())
    }

    #[test]
    fn progress_and_notify() {
        assert_eq!(feed(b"\x1b]9;4;3;0\x07").status, Status::Working);
        assert_eq!(feed(b"\x1b]9;4;3;0\x07\x1b]9;4;0;0\x07").status, Status::Done);
        let s = feed(b"\x1b]9;4;3;0\x07\x07");
        assert_eq!(s.status, Status::NeedsInput, "bell while working");
        let s = feed(b"\x1b]9;4;3;0\x07\x1b]777;notify;Claude;Needs your permission\x07");
        assert_eq!((s.status, s.summary.as_str()), (Status::NeedsInput, "Needs your permission"));
        let s = feed(b"\x1b]9;4;3;0\x07\x1b]9;4;0;0\x07\x1b]9;still waiting\x07\x07");
        assert_eq!(s.status, Status::Done, "reminders after the turn don't flip Done");
    }

    #[test]
    fn warp_plugin_payloads() {
        let osc = |json: &str| format!("\x1b]777;notify;warp://cli-agent;{json}\x07");
        let s = feed(osc(r#"{"v":1,"agent":"claude","event":"prompt_submit","query":"fix it; now"}"#).as_bytes());
        assert_eq!((s.status, s.summary.as_str()), (Status::Working, "fix it; now"));
        let working = osc(r#"{"event":"prompt_submit"}"#);
        let perm = osc(r#"{"event":"permission_request","summary":"Wants to run Bash: rm -rf x"}"#);
        let s = feed(format!("{working}{perm}").as_bytes());
        assert_eq!((s.status, s.summary.as_str()), (Status::NeedsInput, "Wants to run Bash: rm -rf x"));
        let s = feed(osc(r#"{"event":"stop","query":"q","response":"All done"}"#).as_bytes());
        assert_eq!((s.status, s.summary.as_str()), (Status::Done, "All done"));
        let s = feed(osc(r#"{"event":"session_start","session_id":"abc","cwd":"/x"}"#).as_bytes());
        assert_eq!((s.status, s.sid.as_str(), s.agent_cwd.as_str()), (Status::Idle, "abc", "/x"));
        let semis = ";".repeat(30);
        let perm = osc(&format!(r#"{{"v":1,"event":"permission_request","summary":"x{semis}y"}}"#));
        let s = feed(format!("{working}{perm}").as_bytes());
        assert_eq!(s.status, Status::NeedsInput, "truncated payload still yields the event");
    }

    #[test]
    fn answers_startup_queries() {
        assert_eq!(feed(b"\x1b[6n").reply, b"\x1b[1;1R");
        assert_eq!(feed(b"\x1b[c").reply, b"\x1b[?62;22c");
    }

    #[test]
    fn chat_names() {
        assert_eq!(chat_name("✳ Reply with hi").as_deref(), Some("Reply with hi"));
        assert_eq!(chat_name("◐ Fix auth token refresh").as_deref(), Some("Fix auth token refresh"));
        let copilot = chat_name("Calculate Simple Addition - GitHub Copilot");
        assert_eq!(copilot.as_deref(), Some("Calculate Simple Addition"));
        for t in ["✳ Claude Code", "GitHub Copilot", "", "✳ "] {
            assert_eq!(chat_name(t), None, "{t:?}");
        }
    }

    #[test]
    fn agents() {
        assert_eq!(agent_of("/Users/me/.local/share/claude/versions/2.1.270"), Agent::Claude);
        assert_eq!(agent_of("claude"), Agent::Claude);
        assert_eq!(agent_of("/Users/me/.local/bin/copilot"), Agent::Copilot);
        assert_eq!(agent_of("/bin/zsh"), Agent::Shell);
        assert_eq!(agent_of("-zsh"), Agent::Shell);
        assert_eq!(agent_of("/usr/bin/vim"), Agent::Other("vim".into()));

        let profiles = agents::from_config(&serde_json::json!({ "agents": [{ "name": "mine", "match": ["mh"] }] }));
        let index = |n: &str| Agent::Profile(profiles.iter().position(|p| p.name == n).unwrap());
        assert_eq!(agent_in("/opt/homebrew/bin/codex", &profiles), index("codex"));
        assert_eq!(agent_in("/usr/local/bin/mh", &profiles), index("mine"), "a harness from config.json");
        assert_eq!(agent_in("/Users/me/.local/share/claude/versions/2.1.270", &profiles), Agent::Claude);
        assert_eq!(agent_in("/opt/homebrew/bin/node", &profiles), Agent::Other("node".into()));
        let cmdline = |a: &str| agent_in_cmdline(a, &profiles);
        assert_eq!(cmdline("node /opt/homebrew/bin/gemini --yolo"), Some(index("gemini")), "a Node agent");
        assert_eq!(cmdline("python3 -m aider --model x"), Some(index("aider")), "a Python agent");
        assert_eq!(cmdline("node /usr/lib/node_modules/vite/bin/vite.js"), None, "node running something else");
    }

    #[test]
    fn session_ids() {
        let u = uuid();
        assert_eq!((u.len(), &u[14..15], &u[8..9]), (36, "4", "-"));
        assert_ne!(u, uuid());
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(resumed_id(&v("claude --resume abc")).as_deref(), Some("abc"));
        assert_eq!(resumed_id(&v("copilot --resume=abc")).as_deref(), Some("abc"));
        assert_eq!(resumed_id(&v("claude --model opus")), None);
        assert!(continues(&v("claude -c")) && !continues(&v("claude --model opus")));
    }

    #[test]
    fn finds_agent_sessions_by_pid() {
        let home = std::env::temp_dir().join(format!("chud-find-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".claude/sessions")).unwrap();
        std::fs::create_dir_all(home.join(".copilot/session-state/cop-1")).unwrap();
        std::fs::write(home.join(".claude/sessions/4242.json"), r#"{"pid":4242,"sessionId":"abc","cwd":"/x"}"#).unwrap();
        std::fs::write(home.join(".copilot/session-state/cop-1/inuse.77.lock"), "77").unwrap();
        let found = |agent, pid| find_sid(&home, &agent, pid);
        assert_eq!(found(Agent::Claude, 4242), Some(("abc".into(), Some(PathBuf::from("/x")))));
        assert_eq!(found(Agent::Copilot, 77), Some(("cop-1".into(), None)));
        assert_eq!(found(Agent::Claude, 1), None);
        assert_eq!(found(Agent::Shell, 4242), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    // Raw PTY output recorded from a real `copilot` run: prompt -> work -> finish.
    #[test]
    fn copilot_recording() {
        let s = feed(include_bytes!("../fixtures/copilot.raw"));
        assert_eq!(s.status, Status::Done);
        assert!(s.title.contains("Copilot"), "title: {}", s.title);
        assert_eq!(s.chat, "Run Shell Command Date");
    }

    // Raw PTY output recorded from a real `claude` run: trust dialog -> prompt -> stop ->
    // 60s idle reminder (OSC 9 + BEL) -> Ctrl-C (which clears the title).
    #[test]
    fn claude_recording() {
        let s = feed(include_bytes!("../fixtures/claude.raw"));
        assert_eq!(s.status, Status::Done);
        assert_eq!(s.summary, "Reply with only the word hi.");
        assert_eq!(s.chat, "Reply with hi");
        assert_eq!(s.sid, "2b1e6bc2-4b99-4f56-ab36-2d31aeadc6fd");
    }
}
