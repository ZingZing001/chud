mod agents;
mod chud;
mod config;
mod git;
mod layout;
mod update;
mod session;
mod setup;
mod theme;
mod ui;
mod usage;

use anyhow::Result;
use crossterm::event::{
    self as ct, Event as Input, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::layout::Rect;
use serde_json::{json, Value};
use session::{Agent, Event, Session, Status};
use std::io::{stdout, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(16);
const TICK: Duration = Duration::from_secs(1);
const SIDE: u16 = 38;
const DEFAULT_CMD: &str = if cfg!(windows) { "powershell" } else { "zsh" }; // what a new terminal runs

enum Ask {
    Commit,
    Kill,
    Discard,
    Quit,
    Rename,
    Group,
    GroupRename(usize), // index into App.groups
    NewGroup,
    /// filters the sidebar as you type, rather than answering on Enter
    Find,
}

/// Fuzzy match: every character of the query, in order, anywhere in the text, case-insensitive.
/// The score favours runs of adjacent characters and matches at the start of a word, so "cl"
/// ranks "claude" above "terminal", and returns None when the text doesn't match at all.
/// Matching the first occurrence of each character is greedy but never misses a subsequence.
fn fuzzy(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut qi, mut score, mut run, mut start) = (0, 0, 0, None);
    for (i, &c) in t.iter().enumerate() {
        if qi < q.len() && c == q[qi] {
            run += 1;
            let word_start = i == 0 || !t[i - 1].is_alphanumeric();
            score += run + if word_start { 4 } else { 0 };
            start.get_or_insert(i as i32);
            qi += 1;
        } else {
            run = 0;
        }
    }
    // a match near the front of the name beats the same letters found late in a longer one
    (qi == q.len()).then(|| score - start.unwrap_or(0))
}

struct Prompt {
    ask: Ask,
    input: String,
    /// which button a yes/no prompt has selected, moved with the arrow keys and taken by Enter
    yes: bool,
}

impl Prompt {
    fn new(ask: Ask, input: String) -> Prompt {
        Prompt { ask, input, yes: true }
    }

    /// Yes/no prompts answer a question; the rest take typing.
    fn yes_no(&self) -> bool {
        matches!(self.ask, Ask::Kill | Ask::Discard | Ask::Quit)
    }
}

struct Diff {
    root: PathBuf,
    files: Vec<(String, String)>,
    sel: usize,
    text: String,
    scroll: u16,
}

impl Diff {
    fn load(&mut self) {
        self.scroll = 0;
        self.text = match self.files.get(self.sel) {
            Some((code, path)) => git::diff(&self.root, code, path),
            None if git::root(&self.root).is_err() => "Not a git repository, so there's nothing to review.".into(),
            None => "working tree clean".into(),
        };
    }
}

/// Group 0 ("ungrouped") always exists and always sits at index 0.
struct Group {
    id: usize,
    name: String,
    collapsed: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Row {
    Group(usize),   // index into App.groups
    Session(usize), // index into App.sessions
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tool {
    New,
    Dash,
    Diff,
    Help,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum DiffAct {
    Commit,
    Discard,
    Refresh,
    Close,
}

/// What a click landed on (ui::draw records a rectangle for each).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Tool(Tool),
    Session(usize),
    SessionMenu(usize),
    Group(usize),
    GroupMenu(usize),
    Edge,
    SidebarEmpty,
    MenuItem(usize),
    Ok,
    Cancel,
    DiffFile(usize),
    DiffAct(DiffAct),
    Card(usize),
    Cards,
    /// the terminal pane showing this session
    Pane(usize),
    /// the divider between split panes, by its position in `layout.dividers()`
    Divider(usize),
    /// the ✕ on a pane's header
    ClosePane(usize),
    Backdrop,
    SetupChoice(usize),
    SetupBack,
    SetupNext,
}

/// The first-start walkthrough, and `chud --setup`: one screen per step, nothing written
/// until you finish except the status line, which is changed only on its own step and only
/// when you pick "turn it on".
pub struct Setup {
    pub step: usize,
    /// the highlighted option, on steps that have options
    pub choice: usize,
    /// follow the system / always dark / always light
    pub theme: usize,
    /// smooth block characters / chunky spaces
    pub mascot: usize,
    pub status_line: setup::StatusLine,
    pub status_result: Option<Result<String, String>>,
    pub warp: setup::Check,
    pub gh: setup::Check,
    pub agents: Vec<(String, bool)>,
}

pub const SETUP_STEPS: usize = 7;
const THEMES: [&str; 3] = ["auto", "dark", "light"];
const MASCOTS: [&str; 2] = ["blocks", "safe"];

impl Setup {
    fn new(tx: &mpsc::Sender<Event>, cfg: &Value) -> Setup {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Event::SetupChecks(setup::check_warp(), setup::check_gh()));
        });
        let home = config::home();
        let settings = std::fs::read_to_string(setup::settings_path(&home)).ok();
        let settings: Value = settings.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let at = |list: &[&str], key: &str| list.iter().position(|v| cfg[key].as_str() == Some(v));
        Setup {
            step: 0,
            choice: 0,
            theme: at(&THEMES, "theme").unwrap_or(0),
            mascot: at(&MASCOTS, "mascot").unwrap_or(chud::safe() as usize),
            status_line: setup::status_line(&settings),
            status_result: None,
            warp: setup::Check::Running,
            gh: setup::Check::Running,
            agents: setup::installed(&["claude", "copilot", "codex", "gemini", "aider", "opencode", "amp"]),
        }
    }

    /// The options on the current step, if it has any.
    pub fn choices(&self) -> Vec<String> {
        let system = if theme::system_light().unwrap_or(false) { "light" } else { "dark" };
        match self.step {
            1 => vec![format!("Follow the system (it's {system} right now)"), "Always dark".into(), "Always light".into()],
            2 => vec!["A · smooth".into(), "B · chunky, works with any font".into()],
            3 if self.status_result.is_some() => vec![],
            3 => match &self.status_line {
                setup::StatusLine::Ours => vec![],
                setup::StatusLine::Absent => vec!["Turn it on (settings.json is backed up first)".into(), "Not now".into()],
                setup::StatusLine::Foreign(_) => vec!["Replace it with chud's (backed up first)".into(), "Keep mine".into()],
            },
            _ => vec![],
        }
    }
}

/// Menu actions; the numbers are indexes into App.sessions, or App.groups for group actions.
#[derive(Clone, Copy, Debug)]
enum Act {
    Rename(usize),
    Group(usize),
    NewTerminal(usize),
    NewGroup,
    Diff(usize),
    Kill(usize),
    GroupRename(usize),
    Fold(usize),
    DeleteGroup(usize),
    /// show session i beside (or below) the focused pane
    Split(usize, layout::Dir),
    /// split session i's pane with a new terminal
    SplitNew(usize, layout::Dir),
    /// take session i off screen; it keeps running in the sidebar
    ClosePane(usize),
}

struct Menu {
    x: u16,
    y: u16,
    items: Vec<(&'static str, Act)>,
}

/// A mouse press being held: a session being dragged, the sidebar edge, or the agent's pane.
/// It only counts as moved once the pointer reaches another cell: chud.app reports a drag for
/// every pixel of jitter while the button is down.
#[derive(Clone, Copy)]
struct Drag {
    from: Hit,
    at: (u16, u16),
    moved: bool,
}

/// Sidebar order: named groups, then "ungrouped"; each group's sessions in Vec order.
/// `members[i]` is session i's group id. Headers only appear once a named group exists.
fn layout(groups: &[Group], members: &[usize], expand_all: bool) -> Vec<Row> {
    let headers = groups.len() > 1;
    let mut rows = vec![];
    for gi in (1..groups.len()).chain([0]) {
        let g = &groups[gi];
        let mine: Vec<usize> = (0..members.len()).filter(|&i| members[i] == g.id).collect();
        if headers && (gi != 0 || !mine.is_empty()) {
            rows.push(Row::Group(gi));
        }
        if !headers || expand_all || !g.collapsed {
            rows.extend(mine.into_iter().map(Row::Session));
        }
    }
    rows
}

/// Moves v[from] to just before v[before] (or to the end); returns its new index.
fn move_item<T>(v: &mut Vec<T>, from: usize, before: Option<usize>) -> usize {
    let item = v.remove(from);
    let at = match before {
        Some(b) if b > from => b - 1,
        Some(b) => b,
        None => v.len(),
    };
    v.insert(at, item);
    at
}

/// How a saved session reopens: agents resume their conversation if it reached disk.
fn resume_argv(argv: &[String], sid: &str, saved: bool) -> Vec<String> {
    let agent = matches!(session::agent_of(&argv[0]), Agent::Claude | Agent::Copilot);
    if agent && saved && !sid.is_empty() {
        vec![argv[0].clone(), "--resume".into(), sid.into()]
    } else {
        argv.to_vec()
    }
}

/// `zsh -ic "claude --resume <id>; exec zsh"`: the shell runs the agent's chat and becomes a
/// plain shell again once you quit it. None unless `argv` is a shell and the id is only letters,
/// digits and dashes, since it lands in a command line.
fn resume_in_shell(argv: &[String], agent: &str, sid: &str) -> Option<Vec<String>> {
    let shell = argv.first().filter(|p| session::agent_of(p) == Agent::Shell && !cfg!(windows))?;
    let safe = !sid.is_empty() && sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let agent = ["claude", "copilot"].into_iter().find(|a| *a == agent)?;
    safe.then(|| vec![shell.clone(), "-ic".into(), format!("{agent} --resume {sid}; exec {shell}")])
}

fn state_path() -> PathBuf {
    config::home().join(".config/chud/state.json")
}

struct App {
    sessions: Vec<Session>,
    groups: Vec<Group>,
    sel: usize,
    prefix: bool,
    prompt: Option<Prompt>,
    diff: Option<Diff>,
    dash: bool,
    dash_scroll: u16,
    help: bool,
    menu: Option<Menu>,
    focused: bool,
    quit: bool,
    next_id: usize,
    next_gid: usize,
    size: (u16, u16),
    side: u16,
    hits: Vec<(Rect, Hit)>,
    drag: Option<Drag>,
    hover: Option<Hit>,
    /// mouse reporting off so the terminal can select text (C-a v, or holding ⌥)
    select: bool,
    /// dragging over the terminal selects its text: (anchor, cursor), both screen cells
    picked: Option<((u16, u16), (u16, u16))>,
    /// sidebar width parked here while the terminal is zoomed (C-a f)
    zoom: Option<u16>,
    /// the window changed size: repaint every cell, not just the ones that differ
    resized: bool,
    /// one line of feedback in the status bar, until the next key
    flash: Option<String>,
    /// a rebuilt chud is installed and waiting for a restart
    update: Option<update::Update>,
    /// no theme was chosen, so chud.app may switch it when the system does
    theme_follows: bool,
    setup: Option<Setup>,
    /// which sessions are on screen, and how the main area is split between them
    layout: layout::Node,
    /// where a session dragged out of the sidebar would land: (pane's session, zone)
    drop: Option<(usize, layout::Zone)>,
    last_usage: Instant,
    plan: usage::Plan,
    tx: mpsc::Sender<Event>,
}

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--statusline") {
        return statusline();
    }
    let mut term = ratatui::init();
    // Also switch off "alternate scroll" (mode 1007): with it, some terminals (chud.app's among
    // them) turn the wheel into Up/Down arrow keys in full-screen apps, which reached the agent.
    use crossterm::style::Print;
    let modes = || {
        execute!(stdout(), ct::DisableMouseCapture, ct::DisableBracketedPaste, ct::DisableFocusChange, Print("\x1b[?1007h"))
    };
    execute!(stdout(), ct::EnableMouseCapture, ct::EnableBracketedPaste, ct::EnableFocusChange, Print("\x1b[?1007l"))?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = modes();
        hook(info);
    }));
    let res = run(&mut term);
    let _ = modes();
    ratatui::restore();
    res
}

fn run(term: &mut ratatui::DefaultTerminal) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        while let Ok(e) = ct::read() {
            if input_tx.send(Event::Input(e)).is_err() {
                break;
            }
        }
    });
    let size = term.size()?;
    let mut app = App {
        sessions: vec![],
        groups: vec![Group { id: 0, name: "ungrouped".into(), collapsed: false }],
        sel: 0,
        prefix: false,
        prompt: None,
        diff: None,
        dash: false,
        dash_scroll: 0,
        help: false,
        menu: None,
        focused: true,
        quit: false,
        next_id: 0,
        next_gid: 0,
        size: (size.width, size.height),
        side: SIDE,
        hits: vec![],
        drag: None,
        hover: None,
        select: false,
        picked: None,
        zoom: None,
        resized: false,
        flash: None,
        update: None,
        theme_follows: true,
        setup: None,
        layout: layout::Node::Leaf(0),
        drop: None,
        last_usage: Instant::now(),
        plan: usage::Plan::default(),
        tx,
    };
    // chud.app starts us before its window has a size; wait briefly for the real one so the
    // first sessions don't start (and wrap their first output) in a tiny terminal
    let deadline = Instant::now() + Duration::from_millis(1500);
    while app.size.1 < 10 {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else { break };
        match rx.recv_timeout(left) {
            Ok(Event::Input(Input::Resize(w, h))) => app.size = (w, h),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let cfg = config::load();
    agents::init(&cfg); // before any session is classified
    let var = |k| std::env::var(k).ok();
    let mascot = config::setting(&cfg, "mascot", "CHUD_MASCOT");
    chud::set_safe(!ui::block_glyphs(var("TERM_PROGRAM").as_deref(), mascot.as_deref()));
    let chosen = config::setting(&cfg, "theme", "CHUD_THEME");
    app.theme_follows = !matches!(chosen.as_deref(), Some("light" | "dark"));
    theme::set_light(theme::choose(chosen.as_deref(), var("COLORFGBG").as_deref()));

    let (flags, args): (Vec<String>, Vec<String>) = std::env::args().skip(1).partition(|a| a == "--setup");
    for cmd in &args {
        app.open(cmd);
    }
    if args.is_empty() {
        app.restore();
    }
    if app.sessions.is_empty() {
        app.open(DEFAULT_CMD); // like any terminal app: start with a shell
    }
    app.apply_layout();
    // first start (no config yet), or asked for; CHUD_SETUP=skip is for scripts and tests
    let skip = std::env::var("CHUD_SETUP").as_deref() == Ok("skip");
    if !flags.is_empty() || (!config::exists() && !skip) {
        app.setup = Some(Setup::new(&app.tx, &cfg));
    }
    app.refresh_plan();
    update::watch(app.tx.clone());

    let mut hits = vec![];
    term.draw(|f| hits = ui::draw(f, &app))?;
    app.hits = hits;
    let mut last = Instant::now();
    loop {
        // Block until something happens (idle = no wakeups); while something is animating or
        // timing, wake once a second. Then drain and draw once.
        let ticking = app.dash
            || app.sessions.iter().any(|s| {
                s.working_since.is_some() || s.status() == Status::NeedsInput || s.compacting()
            });
        let first = if ticking { rx.recv_timeout(TICK).ok() } else { Some(rx.recv()?) };
        let mut dirty = first.is_some_and(|e| app.handle(e));
        while let Ok(e) = rx.try_recv() {
            dirty |= app.handle(e);
        }
        if app.quit {
            app.save();
            return Ok(());
        }
        if ticking && last.elapsed() >= TICK {
            app.tick();
            dirty = true;
        }
        if dirty {
            if std::mem::take(&mut app.resized) {
                // Not Terminal::clear(): that asks the terminal where the cursor is, and the
                // reply goes to the thread reading input, so it times out and takes chud down
                // with it. Wipe the screen, then blank what ratatui believes is on it with one
                // throwaway frame, so the real frame after it draws every cell.
                execute!(stdout(), crossterm::terminal::Clear(crossterm::terminal::ClearType::All))?;
                term.draw(|f| f.render_widget(ratatui::widgets::Clear, f.area()))?;
            }
            let wait = FRAME.saturating_sub(last.elapsed());
            if !wait.is_zero() {
                std::thread::sleep(wait);
                while let Ok(e) = rx.try_recv() {
                    app.handle(e);
                }
            }
            let mut hits = vec![];
            term.draw(|f| hits = ui::draw(f, &app))?;
            app.hits = hits;
            last = Instant::now();
        }
    }
}

impl App {
    /// Returns true when the screen needs a redraw.
    fn handle(&mut self, ev: Event) -> bool {
        match ev {
            Event::Output(id) => match self.sessions.iter().position(|s| s.id == id) {
                Some(i) => {
                    let agent = self.sessions[i].probe();
                    if agent {
                        self.sessions[i].refresh_usage(); // a new agent in front: follow its usage
                    }
                    self.sessions[i].on_output();
                    let status = self.check_status(i);
                    let folding = self.sessions[i].check_compacting();
                    agent || status || folding || (i == self.sel && self.diff.is_none() && !self.dash)
                }
                None => false,
            },
            Event::Exited(id) => {
                if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
                    self.sessions[i].reap();
                    self.check_status(i);
                }
                true
            }
            Event::Input(e) => {
                self.input(e);
                true
            }
            Event::Copilot(quota) => {
                self.plan.copilot = quota;
                true
            }
            Event::SetupChecks(warp, gh) => {
                if let Some(s) = &mut self.setup {
                    (s.warp, s.gh) = (warp, gh);
                }
                true
            }
            Event::Updated(u) => {
                if let update::Update::Ready(commit) = &u {
                    notify("chud updated", &format!("{commit} is installed · quit and start chud again"));
                }
                self.update = Some(u);
                true
            }
        }
    }

    fn check_status(&mut self, i: usize) -> bool {
        let attention = i != self.sel || !self.focused;
        let s = &mut self.sessions[i];
        let st = s.status();
        if st == s.seen {
            return false;
        }
        s.seen = st;
        if st == Status::Working {
            s.working_since = s.working_since.or(Some(Instant::now()));
        } else if let Some(t) = s.working_since.take() {
            s.worked += t.elapsed(); // the chud keeps what it ate
        }
        if matches!(st, Status::NeedsInput | Status::Done) {
            s.refresh_usage();
            if attention {
                s.unread = true;
                notify(&format!("{} · {}", s.label(), ui::label(st)), &s.detail());
            }
            if st == Status::Done && i == self.sel {
                self.refresh_diff();
            }
            self.refresh_plan();
        }
        true
    }

    /// Once a second while something works or the summary is open: keep usage numbers fresh.
    fn tick(&mut self) {
        // agents that report nothing themselves: gone quiet after working means done
        for i in 0..self.sessions.len() {
            if self.sessions[i].on_tick() {
                self.check_status(i);
            }
            // the output that takes the compaction hint off the screen can be the last one for
            // a while, and may fall inside the check's own quiet window, so look again here
            self.sessions[i].check_compacting();
        }
        let every = if self.dash { 2 } else { 3 };
        if self.last_usage.elapsed() >= every * TICK {
            let all = self.dash;
            for s in self.sessions.iter_mut().filter(|s| all || s.working_since.is_some()) {
                s.refresh_usage();
            }
            self.refresh_plan();
            self.last_usage = Instant::now();
        }
    }

    fn refresh_usage(&mut self) {
        for s in &mut self.sessions {
            s.refresh_usage();
        }
        self.last_usage = Instant::now();
    }

    /// Plan limits for the session header. Claude's come from its status line (`chud
    /// --statusline` saves them); Copilot's from GitHub, fetched in the background at most every
    /// 5 minutes and only while a Copilot session exists.
    fn refresh_plan(&mut self) {
        let saved = std::fs::read_to_string(usage::limits_path()).ok();
        if let Some(v) = saved.and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            [self.plan.five_hour, self.plan.seven_day] = usage::claude_windows(&v, usage::now_secs());
        }
        let copilot = self.sessions.iter().any(|s| s.agent == Agent::Copilot);
        if copilot && self.plan.copilot_checked.is_none_or(|t| t.elapsed() > Duration::from_secs(300)) {
            self.plan.copilot_checked = Some(Instant::now());
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let out = Command::new("gh").args(["api", "/copilot_internal/user"]).output();
                let v = out.ok().and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok());
                let _ = tx.send(Event::Copilot(v.as_ref().and_then(usage::copilot_quota)));
            });
        }
    }

    fn pane(&self) -> (u16, u16) {
        ui::pane(self.size.0, self.size.1, self.side)
    }

    fn input(&mut self, e: Input) {
        match e {
            Input::Resize(w, h) => {
                // ratatui only clears the screen when the width shrinks, so after growing (going
                // full screen, say) it still believes the old frame is on display and repaints
                // only the cells that differ from it. Whatever the terminal did with the old
                // frame then shows through. Start from a clean screen instead.
                self.resized = true;
                self.size = (w, h);
                self.apply_layout();
            }
            Input::FocusGained => {
                self.focused = true;
                if let Some(s) = self.sessions.get_mut(self.sel) {
                    s.unread = false;
                }
            }
            Input::FocusLost => self.focused = false,
            Input::Paste(t) => match (&mut self.prompt, self.sessions.get(self.sel)) {
                (Some(p), _) => p.input.push_str(t.trim()),
                (None, Some(s)) if self.diff.is_none() && !self.dash => s.paste(&t),
                _ => {}
            },
            Input::Mouse(m) => self.mouse(m),
            Input::Key(k) if k.kind != KeyEventKind::Release => self.key(k),
            _ => {}
        }
    }

    fn key(&mut self, k: KeyEvent) {
        // Keys no keyboard sends, used by chud.app: F15 is ⌘C (copy what the mouse selected);
        // F13 / F14 mean the system just switched to light / dark.
        match k.code {
            KeyCode::F(15) => return self.copy_picked(),
            KeyCode::F(13 | 14) if self.theme_follows => return theme::set_light(k.code == KeyCode::F(13)),
            KeyCode::F(13 | 14) => return,
            _ => {}
        }
        if self.setup.is_some() {
            return self.setup_key(k);
        }
        if self.help || self.menu.is_some() {
            (self.help, self.menu) = (false, None);
            return;
        }
        if self.prompt.is_some() {
            return self.prompt_key(k);
        }
        self.flash = None; // it says what the last key did; this one gets to speak for itself
        let ctrl_a = k.code == KeyCode::Char('a') && k.modifiers.contains(KeyModifiers::CONTROL);
        if self.prefix {
            self.prefix = false;
            return self.command(k, ctrl_a);
        }
        if ctrl_a {
            self.prefix = true;
            return;
        }
        if self.diff.is_some() {
            return self.diff_key(k);
        }
        if self.dash {
            if matches!(k.code, KeyCode::Esc | KeyCode::Char('q' | 's')) {
                self.dash = false;
            }
            return;
        }
        let mut answered = false;
        if let Some(s) = self.sessions.get_mut(self.sel) {
            let mut p = s.parser.lock().unwrap();
            p.screen_mut().set_scrollback(0);
            let bytes = key_bytes(k, p.screen().application_cursor());
            drop(p);
            s.write(&bytes);
            if k.code == KeyCode::Enter {
                s.submit();
            }
            // Enter or Esc, or a number (Claude's prompts take 1/2/3), answers a waiting agent;
            // arrow keys only move the highlight, so they do not
            answered = matches!(k.code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char('1'..='9')) && s.answered();
        }
        if answered {
            self.check_status(self.sel); // the badge and the working timer update now, not later
        }
    }

    fn command(&mut self, k: KeyEvent, ctrl_a: bool) {
        let has = self.sel < self.sessions.len();
        let ask = |ask: Ask, input: String| Some(Prompt::new(ask, input));
        match k.code {
            _ if ctrl_a => {
                if let Some(s) = self.sessions.get(self.sel) {
                    s.write(&[1]);
                }
            }
            KeyCode::Char('n') => self.new_terminal(self.cur_group()),
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Char(c @ '1'..='9') => {
                if let Some(&i) = self.visible().get(c as usize - '1' as usize) {
                    self.select(i);
                }
            }
            KeyCode::Tab => self.next_waiting(),
            KeyCode::Char('v') => self.set_select(!self.select),
            KeyCode::Char('y') if has => self.copy_screen(),
            KeyCode::Char('f') => {
                match self.zoom.take() {
                    Some(width) => self.side = width,
                    None => self.zoom = Some(std::mem::replace(&mut self.side, 0)),
                }
                self.apply_layout(); // the panes just got wider or narrower
                self.save();
            }
            KeyCode::Char('|') => self.split(layout::Dir::Across),
            KeyCode::Char('-') => self.split(layout::Dir::Down),
            KeyCode::Char('o') => self.next_pane(),
            KeyCode::Char('w') => self.close_pane(self.sel),
            KeyCode::Char('/') if has => self.prompt = ask(Ask::Find, String::new()),
            KeyCode::Char('d') => self.tool(Tool::Diff),
            KeyCode::Char('s') => self.tool(Tool::Dash),
            KeyCode::Char('?') => self.tool(Tool::Help),
            KeyCode::Char('r') if has => self.prompt = ask(Ask::Rename, self.sessions[self.sel].label()),
            KeyCode::Char('g') if has => self.prompt = ask(Ask::Group, String::new()),
            KeyCode::Char('G') if has => self.run_act(Act::GroupRename(self.cur_group())),
            KeyCode::Char('z') if has => {
                let gi = self.cur_group();
                self.groups[gi].collapsed ^= true;
                self.save();
            }
            KeyCode::Char('J') if has => self.shift(true),
            KeyCode::Char('K') if has => self.shift(false),
            KeyCode::Char('x') if has => self.prompt = ask(Ask::Kill, String::new()),
            KeyCode::Char('q') if self.sessions.iter().any(|s| s.exit.is_none()) => {
                self.prompt = ask(Ask::Quit, String::new())
            }
            KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    /// Selecting text with the mouse is the terminal's job, but it only gets the chance while
    /// nothing is reporting the mouse: until then every drag comes here instead. So hand the
    /// mouse back for as long as you're selecting, then take it again.
    fn set_select(&mut self, on: bool) {
        if self.select == on {
            return;
        }
        self.select = on;
        self.drag = None;
        let _ = if self.select {
            execute!(stdout(), ct::DisableMouseCapture)
        } else {
            execute!(stdout(), ct::EnableMouseCapture)
        };
    }

    /// Copy what the session shows to the clipboard, for when reaching for the mouse (C-a v)
    /// is the slower way to get at it.
    fn copy_screen(&mut self) {
        let Some(s) = self.sessions.get(self.sel) else { return };
        let text = s.parser.lock().unwrap().screen().contents();
        let lines = text.lines().count();
        self.flash = match copy_to_clipboard(&text) {
            Ok(()) => Some(format!(" copied {lines} lines to the clipboard ")),
            Err(e) => Some(format!(" could not copy: {e} ")),
        };
    }

    fn tool(&mut self, t: Tool) {
        match t {
            // a drop-down under the + New button
            Tool::New => {
                let items = vec![("New terminal session", Act::NewTerminal(self.cur_group())), ("New group…", Act::NewGroup)];
                self.menu = Some(Menu { x: 7, y: 1, items });
            }
            Tool::Dash => {
                self.dash = !self.dash;
                self.diff = None;
                if self.dash {
                    self.refresh_usage();
                }
            }
            Tool::Diff if self.diff.is_some() => self.diff = None,
            Tool::Diff => self.open_diff(),
            Tool::Help => self.help = true,
        }
    }

    fn prompt_key(&mut self, k: KeyEvent) {
        let Some(mut p) = self.prompt.take() else { return };
        let yes_no = p.yes_no();
        match k.code {
            KeyCode::Esc => {}
            KeyCode::Char('y') if yes_no => self.confirm(p.ask),
            KeyCode::Char('n') if yes_no => {}
            // the buttons are a two-item row: arrows (or Tab) walk it, Enter takes the one lit
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab if yes_no => {
                p.yes = !p.yes;
                self.prompt = Some(p);
            }
            KeyCode::Enter if yes_no => {
                if p.yes {
                    self.confirm(p.ask)
                }
            }
            _ if yes_no => self.prompt = Some(p),
            KeyCode::Enter => self.submit(p.ask, p.input.trim()),
            KeyCode::Backspace | KeyCode::Char(_) => {
                match k.code {
                    KeyCode::Char(c) => p.input.push(c),
                    _ => {
                        p.input.pop();
                    }
                }
                // the find box filters and follows as you type; the rest answer on Enter
                if matches!(p.ask, Ask::Find) {
                    let q = p.input.clone();
                    self.jump_to_match(&q);
                }
                self.prompt = Some(p);
            }
            _ => self.prompt = Some(p),
        }
    }

    /// The prompt's OK / Yes button.
    fn accept_prompt(&mut self) {
        if let Some(Prompt { ask, input, .. }) = self.prompt.take() {
            match ask {
                Ask::Kill | Ask::Discard | Ask::Quit => self.confirm(ask),
                _ => self.submit(ask, input.trim()),
            }
        }
    }

    fn submit(&mut self, ask: Ask, input: &str) {
        match ask {
            Ask::Commit if !input.is_empty() => {
                let failed = self.diff.as_ref().and_then(|d| git::commit_all(&d.root, input).err());
                self.refresh_diff();
                if let (Some(e), Some(d)) = (failed, &mut self.diff) {
                    d.text = e; // shown in the diff view, where you committed
                }
            }
            Ask::Rename => {
                if let Some(s) = self.sessions.get_mut(self.sel) {
                    // Enter on the untouched auto name keeps it automatic
                    if !(s.name.is_none() && input == s.label()) {
                        s.name = Some(input.to_string()).filter(|n| !n.is_empty());
                    }
                }
                self.save();
            }
            Ask::Group if self.sel < self.sessions.len() => self.move_to_group(input),
            // names stay unique: C-a g moves sessions by group name
            Ask::GroupRename(gi) if !input.is_empty() && !self.groups.iter().any(|g| g.name == input) => {
                if let Some(g) = self.groups.get_mut(gi) {
                    g.name = input.to_string();
                }
                self.save();
            }
            Ask::NewGroup if !input.is_empty() => {
                self.group_id(input);
                self.save();
            }
            _ => {}
        }
    }

    fn confirm(&mut self, ask: Ask) {
        match ask {
            Ask::Kill if self.sel < self.sessions.len() => {
                let id = self.sessions.remove(self.sel).id; // Drop kills the child
                self.layout.remove(id);
                self.select(self.sel.min(self.sessions.len().saturating_sub(1)));
                self.save();
            }
            Ask::Discard => {
                let file = self.diff.as_ref().and_then(|d| Some((d.root.clone(), d.files.get(d.sel)?.clone())));
                let failed = file.and_then(|(root, (code, path))| git::discard(&root, &code, &path).err());
                self.refresh_diff();
                if let (Some(e), Some(d)) = (failed, &mut self.diff) {
                    d.text = e;
                }
            }
            Ask::Quit => self.quit = true,
            _ => {}
        }
    }

    /// `line` is "<command> [args] [dir]"; a trailing existing directory becomes the cwd.
    fn open(&mut self, line: &str) {
        let mut argv: Vec<String> = line.split_whitespace().map(String::from).collect();
        let mut cwd = std::env::current_dir().unwrap_or_default();
        if argv.len() > 1 {
            let last = argv.last().unwrap();
            let dir = match last.strip_prefix('~') {
                Some(rest) => PathBuf::from(format!("{}{rest}", config::home().display())),
                None => PathBuf::from(last),
            };
            if dir.is_dir() {
                cwd = dir.canonicalize().unwrap_or(dir);
                argv.pop();
            }
        }
        match argv.first().map(String::as_str) {
            None => return,
            Some("shell") => argv[0] = std::env::var("SHELL").unwrap_or(DEFAULT_CMD.into()),
            _ => {}
        }
        let group = self.sessions.get(self.sel).map_or(0, |s| s.group);
        if let Some(i) = self.spawn(argv, cwd, group) {
            self.select(i);
            self.save();
        }
    }

    /// A new zsh in group `gi`, in the selected session's folder (like a terminal's new tab).
    fn new_terminal(&mut self, gi: usize) {
        let cwd = self.sessions.get(self.sel).map(|s| s.cwd.clone());
        let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let group = self.groups.get(gi).map_or(0, |g| g.id);
        if let Some(i) = self.spawn(vec![DEFAULT_CMD.into()], cwd, group) {
            self.select(i);
            self.save();
        }
    }

    /// The main area: right of the sidebar, between the toolbar and the status bar.
    fn main_area(&self) -> Rect {
        let (w, h) = self.size;
        Rect::new(self.side.min(w), 1, w.saturating_sub(self.side), h.saturating_sub(2))
    }

    /// Drops panes whose session is gone, and makes sure something is on screen.
    fn fix_layout(&mut self) {
        for id in self.layout.leaves() {
            if !self.sessions.iter().any(|s| s.id == id) {
                self.layout.remove(id);
            }
        }
        let shown = self.layout.leaves().iter().any(|id| self.sessions.iter().any(|s| s.id == *id));
        if !shown && let Some(s) = self.sessions.get(self.sel).or(self.sessions.first()) {
            self.layout = layout::Node::Leaf(s.id);
        }
    }

    /// Sizes each session's terminal to its pane — sessions not on screen to the whole area — so
    /// every agent redraws for the space it really has.
    fn apply_layout(&mut self) {
        self.fix_layout();
        let panes = self.layout.rects(self.main_area());
        let whole = self.pane();
        for s in &self.sessions {
            let size = panes
                .iter()
                .find(|(id, _)| *id == s.id)
                // the header above each pane takes one row, or two for an agent
                .map_or(whole, |(_, r)| (r.height.saturating_sub(ui::header_rows(s)).max(4), r.width.max(20)));
            s.resize(size);
        }
    }

    /// Splits the focused pane (C-a | and C-a -): the new half shows the next session in the
    /// sidebar that isn't on screen yet, so splitting puts your agents side by side. Only when
    /// every session is already showing does it start a new terminal there.
    fn split(&mut self, dir: layout::Dir) {
        if self.focused_pane_fits(dir).is_none() {
            return;
        }
        let order: Vec<usize> =
            self.rows(true).into_iter().filter_map(|r| if let Row::Session(i) = r { Some(i) } else { None }).collect();
        let after = order.iter().position(|&i| i == self.sel).map_or(0, |p| p + 1);
        let hidden = (0..order.len()).map(|k| order[(after + k) % order.len()]).find(|&i| !self.layout.contains(self.sessions[i].id));
        match hidden {
            Some(i) => self.split_with(i, dir),
            None => self.split_new(dir),
        }
    }

    /// Splits the focused pane with a new terminal in the new half, as tmux does.
    fn split_new(&mut self, dir: layout::Dir) {
        let Some(at) = self.focused_pane_fits(dir) else { return };
        let (cwd, group) = (self.sessions[self.sel].cwd.clone(), self.sessions[self.sel].group);
        if let Some(i) = self.spawn(vec![DEFAULT_CMD.into()], cwd, group) {
            self.layout.split(at, dir, self.sessions[i].id);
            self.select(i); // already on screen, so this only moves the focus
            self.apply_layout();
            self.save();
        }
    }

    /// The focused pane's session id, when that pane has room to split this way; otherwise
    /// says why not.
    fn focused_pane_fits(&mut self, dir: layout::Dir) -> Option<usize> {
        let at = self.sessions.get(self.sel).map(|s| s.id)?;
        let room = self.layout.rects(self.main_area()).into_iter().find(|(id, _)| *id == at);
        if room.is_some_and(|(_, r)| layout::fits(r, dir)) {
            return Some(at);
        }
        self.flash = Some(" not enough room to split this pane · make the window bigger or zoom with C-a f ".into());
        None
    }

    /// Shows session i beside (or below) the pane you are working in: how you get two agents
    /// next to each other. A session already on screen just gets the focus.
    fn split_with(&mut self, i: usize, dir: layout::Dir) {
        let Some(id) = self.sessions.get(i).map(|s| s.id) else { return };
        if self.layout.contains(id) {
            return self.select(i);
        }
        let focused = self.sessions.get(self.sel).map(|s| s.id).filter(|f| self.layout.contains(*f));
        let Some(at) = focused.or(self.layout.leaves().first().copied()) else { return };
        let room = self.layout.rects(self.main_area()).into_iter().find(|(p, _)| *p == at);
        if !room.is_some_and(|(_, r)| layout::fits(r, dir)) {
            self.flash = Some(" not enough room beside this pane · make the window bigger or zoom with C-a f ".into());
            return;
        }
        self.layout.split(at, dir, id);
        self.select(i);
        self.apply_layout();
        self.save();
    }

    /// Takes a session off screen without stopping it; its neighbour takes the space.
    fn close_pane(&mut self, i: usize) {
        let Some(id) = self.sessions.get(i).map(|s| s.id) else { return };
        if self.layout.leaves().len() < 2 || !self.layout.remove(id) {
            return;
        }
        if self.sel == i {
            let first = self.layout.leaves()[0];
            self.sel = self.sessions.iter().position(|s| s.id == first).unwrap_or(0);
        }
        self.apply_layout();
        self.save();
    }

    fn next_pane(&mut self) {
        let leaves = self.layout.leaves();
        let at = self.sessions.get(self.sel).and_then(|s| leaves.iter().position(|id| *id == s.id));
        let next = leaves[at.map_or(0, |p| (p + 1) % leaves.len())];
        if let Some(i) = self.sessions.iter().position(|s| s.id == next) {
            self.select(i);
        }
    }

    fn spawn(&mut self, argv: Vec<String>, cwd: PathBuf, group: usize) -> Option<usize> {
        let mut s = Session::spawn(self.next_id, argv, cwd, self.pane(), self.tx.clone()).ok()?;
        s.group = group;
        self.next_id += 1;
        self.sessions.push(s);
        Some(self.sessions.len() - 1)
    }

    fn rows(&self, expand_all: bool) -> Vec<Row> {
        let members: Vec<usize> = self.sessions.iter().map(|s| s.group).collect();
        let rows = layout(&self.groups, &members, expand_all);
        // While you are searching the sidebar shows the matches, flat: groups would only hide
        // what you are looking for. expand_all callers want every session, so they keep it.
        match self.finding() {
            Some(q) if !expand_all && !q.is_empty() => {
                rows.into_iter().filter(|r| matches!(r, Row::Session(i) if self.matches(*i, q).is_some())).collect()
            }
            _ => rows,
        }
    }

    /// The query typed into the find box, while it is open.
    fn finding(&self) -> Option<&str> {
        self.prompt.as_ref().filter(|p| matches!(p.ask, Ask::Find)).map(|p| p.input.as_str())
    }

    fn matches(&self, i: usize, query: &str) -> Option<i32> {
        let s = &self.sessions[i];
        // the name you see, plus the folder behind an auto name and the agent you started
        fuzzy(query, &format!("{} {} {}", s.label(), s.folder(), s.argv.first().map_or("", |a| a.as_str())))
    }

    /// Each keystroke in the find box moves to the best match, so the pane follows the search.
    fn jump_to_match(&mut self, query: &str) {
        if query.is_empty() {
            return; // an empty box matches everything: stay where you are
        }
        let best = (0..self.sessions.len()).filter_map(|i| Some((self.matches(i, query)?, i))).max();
        if let Some((_, i)) = best {
            self.select(i);
        }
    }

    /// Sessions in sidebar order, skipping folded groups.
    fn visible(&self) -> Vec<usize> {
        let rows = self.rows(false).into_iter();
        rows.filter_map(|r| if let Row::Session(i) = r { Some(i) } else { None }).collect()
    }

    fn cur_group(&self) -> usize {
        let id = self.sessions.get(self.sel).map_or(0, |s| s.group);
        self.groups.iter().position(|g| g.id == id).unwrap_or(0)
    }

    fn select(&mut self, i: usize) {
        self.picked = None; // a selection belongs to the session it was made in
        let before = self.sessions.get(self.sel).map(|s| s.id);
        if let Some(id) = self.sessions.get(i).map(|s| s.id).filter(|id| !self.layout.contains(*id)) {
            // not on screen: it takes the focused pane (or the first, if focus was elsewhere)
            let target = before.filter(|b| self.layout.contains(*b)).or(self.layout.leaves().first().copied());
            match target {
                Some(old) => {
                    self.layout.replace(old, id);
                }
                None => self.layout = layout::Node::Leaf(id),
            }
            self.sel = i;
            self.apply_layout();
        }
        self.sel = i;
        self.diff = None;
        self.dash = false;
        if let Some(s) = self.sessions.get_mut(i) {
            s.unread = false;
            s.refresh_usage(); // for the usage bar above its terminal
            let id = s.group;
            if let Some(g) = self.groups.iter_mut().find(|g| g.id == id) {
                g.collapsed = false;
            }
        }
        self.refresh_plan();
    }

    fn step(&mut self, by: isize) {
        let v = self.visible();
        if v.is_empty() {
            return;
        }
        let at = v.iter().position(|&i| i == self.sel);
        let next = at.map_or(0, |p| (p as isize + by).rem_euclid(v.len() as isize) as usize);
        self.select(v[next]);
    }

    fn next_waiting(&mut self) {
        let order: Vec<usize> =
            self.rows(true).into_iter().filter_map(|r| if let Row::Session(i) = r { Some(i) } else { None }).collect();
        let n = order.len();
        let at = order.iter().position(|&i| i == self.sel).unwrap_or(0);
        for want in [Status::NeedsInput, Status::Done] {
            let found = (1..=n).map(|k| order[(at + k) % n]).find(|&i| self.sessions[i].status() == want);
            if let Some(i) = found {
                return self.select(i);
            }
        }
    }

    /// Moves the session within its group, swapping with its neighbour.
    fn shift(&mut self, down: bool) {
        let g = self.sessions[self.sel].group;
        let same = |j: &usize| self.sessions[*j].group == g;
        let other = if down {
            (self.sel + 1..self.sessions.len()).find(same)
        } else {
            (0..self.sel).rev().find(same)
        };
        if let Some(j) = other {
            self.sessions.swap(self.sel, j);
            self.sel = j;
            self.save();
        }
    }

    /// The id of the group with this name, creating it if needed.
    fn group_id(&mut self, name: &str) -> usize {
        if let Some(g) = self.groups.iter().find(|g| g.name == name) {
            return g.id;
        }
        self.next_gid += 1;
        self.groups.push(Group { id: self.next_gid, name: name.into(), collapsed: false });
        self.next_gid
    }

    /// Existing name moves there, a new name creates the group, empty means ungrouped.
    fn move_to_group(&mut self, name: &str) {
        let id = if name.is_empty() { 0 } else { self.group_id(name) };
        self.sessions[self.sel].group = id;
        self.save();
    }

    /// Deletes a named group; its sessions become ungrouped.
    fn delete_group(&mut self, gi: usize) {
        if gi == 0 || gi >= self.groups.len() {
            return;
        }
        let id = self.groups.remove(gi).id;
        for s in self.sessions.iter_mut().filter(|s| s.group == id) {
            s.group = 0;
        }
        self.save();
    }

    /// Drag and drop: onto a session (goes just above it, in its group), onto a group header
    /// (joins that group), or onto empty sidebar space (ungrouped).
    fn drop_session(&mut self, i: usize, target: Option<Hit>) {
        let selected = self.sessions.get(self.sel).map(|s| s.id);
        match target {
            Some(Hit::Session(j)) if j != i => {
                self.sessions[i].group = self.sessions[j].group;
                move_item(&mut self.sessions, i, Some(j));
            }
            Some(Hit::Group(gi)) => {
                self.sessions[i].group = self.groups[gi].id;
                move_item(&mut self.sessions, i, None);
            }
            Some(Hit::SidebarEmpty) => {
                self.sessions[i].group = 0;
                move_item(&mut self.sessions, i, None);
            }
            _ => return,
        }
        self.sel = selected.and_then(|id| self.sessions.iter().position(|s| s.id == id)).unwrap_or(0);
        self.save();
    }

    fn save(&self) {
        let groups: Vec<Value> = self
            .groups
            .iter()
            .map(|g| json!({ "id": g.id, "name": g.name, "collapsed": g.collapsed }))
            .collect();
        let sessions: Vec<Value> = self
            .sessions
            .iter()
            .map(|s| {
                // an agent typed into a shell: the chat it has open, so a restart reopens it
                let resume = s.resume.as_ref().filter(|(agent, ..)| *agent == s.agent).map(|(agent, sid, cwd)| {
                    json!({ "agent": if *agent == Agent::Copilot { "copilot" } else { "claude" }, "sid": sid, "cwd": cwd })
                });
                json!({ "argv": s.argv, "cwd": s.cwd, "name": s.name, "group": s.group,
                        "sid": s.agent_sid(), "worked": s.worked().as_secs(), "resume": resume })
            })
            .collect();
        let path = state_path();
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let layout = self.layout.to_json(&|id| self.sessions.iter().position(|s| s.id == id));
        let state = json!({ "side": self.zoom.unwrap_or(self.side), "groups": groups, "sessions": sessions, "layout": layout });
        let _ = std::fs::write(&path, state.to_string());
    }

    /// Reopens the layout saved at the last quit.
    fn restore(&mut self) {
        let Some(v) = std::fs::read_to_string(state_path()).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            return;
        };
        self.side = v["side"].as_u64().map_or(SIDE, |n| n as u16).clamp(24, 120);
        for g in v["groups"].as_array().into_iter().flatten() {
            let id = g["id"].as_u64().unwrap_or(0) as usize;
            let name = g["name"].as_str().unwrap_or("group").to_string();
            let collapsed = g["collapsed"].as_bool().unwrap_or(false);
            if id == 0 {
                (self.groups[0].name, self.groups[0].collapsed) = (name, collapsed);
            } else {
                self.groups.push(Group { id, name, collapsed });
                self.next_gid = self.next_gid.max(id);
            }
        }
        let mut restored_ids: Vec<Option<usize>> = vec![]; // by position in the saved list
        for s in v["sessions"].as_array().into_iter().flatten() {
            restored_ids.push(None);
            let argv: Vec<String> =
                s["argv"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect();
            let (Some(prog), Some(cwd)) = (argv.first(), s["cwd"].as_str()) else { continue };
            let cwd = PathBuf::from(cwd);
            let sid = s["sid"].as_str().unwrap_or_default();
            let copilot = session::agent_of(prog) == Agent::Copilot;
            let saved = usage::log_path(copilot, sid, &cwd).is_some_and(|p| p.exists());
            let mut run = resume_argv(&argv, sid, saved);
            let mut cwd = cwd;
            // a terminal you ran claude or copilot in comes back running that chat again, in
            // the folder it was in, and leaves you at the prompt when you quit it
            // (state from before chud saved "resume" has only the chat id a warp-reporting claude
            // gave; its folder is read back from the transcript)
            let old = match s.get("resume") {
                None if !sid.is_empty() => usage::claude_chat_cwd(&config::home(), sid)
                    .map(|dir| json!({ "agent": "claude", "sid": sid, "cwd": dir })),
                _ => None,
            };
            let r = old.as_ref().unwrap_or(&s["resume"]);
            if let (Some(agent), Some(rsid), Some(rcwd)) = (r["agent"].as_str(), r["sid"].as_str(), r["cwd"].as_str()) {
                let on_disk = usage::log_path(agent == "copilot", rsid, Path::new(rcwd)).is_some_and(|p| p.exists());
                if let Some(argv) = resume_in_shell(&argv, agent, rsid).filter(|_| on_disk) {
                    (run, cwd) = (argv, PathBuf::from(rcwd));
                }
            }
            let group = s["group"].as_u64().unwrap_or(0) as usize;
            let group = if self.groups.iter().any(|g| g.id == group) { group } else { 0 };
            if let Some(i) = self.spawn(run, cwd, group) {
                *restored_ids.last_mut().unwrap() = Some(self.sessions[i].id);
                let restored = &mut self.sessions[i];
                restored.argv = argv;
                restored.name = s["name"].as_str().map(String::from);
                restored.worked = Duration::from_secs(s["worked"].as_u64().unwrap_or(0));
                restored.refresh_usage();
            }
        }
        self.sel = self.visible().first().copied().unwrap_or(0);
        if let Some(layout) = layout::Node::from_json(&v["layout"], &|k| restored_ids.get(k).copied().flatten()) {
            // focus the first pane, so what is on screen and what is selected agree
            let first = layout.leaves()[0];
            self.layout = layout;
            self.sel = self.sessions.iter().position(|s| s.id == first).unwrap_or(self.sel);
        }
    }

    fn open_diff(&mut self) {
        let Some(s) = self.sessions.get(self.sel) else { return };
        // outside a repo the view opens anyway and says so (Diff::load)
        let root = git::root(&s.cwd).unwrap_or_else(|_| s.cwd.clone());
        self.dash = false;
        self.diff = Some(Diff { root, files: vec![], sel: 0, text: String::new(), scroll: 0 });
        self.refresh_diff();
    }

    fn refresh_diff(&mut self) {
        if let Some(d) = &mut self.diff {
            d.files = git::changes(&d.root);
            d.sel = d.sel.min(d.files.len().saturating_sub(1));
            d.load();
        }
    }

    fn diff_act(&mut self, act: DiffAct) {
        let has_files = self.diff.as_ref().is_some_and(|d| !d.files.is_empty());
        match act {
            DiffAct::Commit if has_files => self.prompt = Some(Prompt::new(Ask::Commit, String::new())),
            DiffAct::Discard if has_files => self.prompt = Some(Prompt::new(Ask::Discard, String::new())),
            DiffAct::Refresh => self.refresh_diff(),
            DiffAct::Close => self.diff = None,
            _ => {}
        }
    }

    fn diff_key(&mut self, k: KeyEvent) {
        let Some(d) = &mut self.diff else { return };
        match k.code {
            KeyCode::Esc | KeyCode::Char('d') | KeyCode::Char('q') => self.diff = None,
            KeyCode::Char('j') | KeyCode::Down if d.sel + 1 < d.files.len() => {
                d.sel += 1;
                d.load();
            }
            KeyCode::Char('k') | KeyCode::Up if d.sel > 0 => {
                d.sel -= 1;
                d.load();
            }
            KeyCode::Char('J') | KeyCode::PageDown | KeyCode::Char(' ') => d.scroll = d.scroll.saturating_add(20),
            KeyCode::Char('K') | KeyCode::PageUp => d.scroll = d.scroll.saturating_sub(20),
            KeyCode::Char('c') => self.diff_act(DiffAct::Commit),
            KeyCode::Char('r') => self.diff_act(DiffAct::Discard),
            KeyCode::Char('R') => self.diff_act(DiffAct::Refresh),
            _ => {}
        }
    }

    fn run_act(&mut self, act: Act) {
        let ask = |ask: Ask, input: String| Some(Prompt::new(ask, input));
        match act {
            Act::Rename(i) => {
                self.select(i);
                self.prompt = ask(Ask::Rename, self.sessions[i].label());
            }
            Act::Group(i) => {
                self.select(i);
                self.prompt = ask(Ask::Group, String::new());
            }
            Act::NewTerminal(gi) => self.new_terminal(gi),
            Act::NewGroup => self.prompt = ask(Ask::NewGroup, String::new()),
            Act::DeleteGroup(gi) => self.delete_group(gi),
            Act::Diff(i) => {
                self.select(i);
                self.open_diff();
            }
            Act::Kill(i) => {
                self.select(i);
                self.prompt = ask(Ask::Kill, String::new());
            }
            Act::Split(i, dir) => self.split_with(i, dir),
            Act::SplitNew(i, dir) => {
                self.select(i);
                self.split_new(dir);
            }
            Act::ClosePane(i) => self.close_pane(i),
            Act::GroupRename(gi) => {
                if let Some(g) = self.groups.get(gi) {
                    self.prompt = ask(Ask::GroupRename(gi), g.name.clone());
                }
            }
            Act::Fold(gi) => {
                self.groups[gi].collapsed ^= true;
                self.save();
            }
        }
    }

    fn hit_at(&self, col: u16, row: u16) -> Option<Hit> {
        let p = ratatui::layout::Position::new(col, row);
        self.hits.iter().rev().find(|(r, _)| r.contains(p)).map(|&(_, h)| h)
    }

    fn mouse(&mut self, m: MouseEvent) {
        let hit = self.hit_at(m.column, m.row);
        if self.setup.is_some() && m.kind != MouseEventKind::Down(MouseButton::Left) {
            return; // the walkthrough is modal: no scrolling or dragging what is behind it
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.press(hit, m),
            // right-click opens the same menu as a row's … button
            MouseEventKind::Down(MouseButton::Right) => match hit {
                Some(Hit::Session(i) | Hit::SessionMenu(i)) => self.press(Some(Hit::SessionMenu(i)), m),
                Some(Hit::Group(gi) | Hit::GroupMenu(gi)) => self.press(Some(Hit::GroupMenu(gi)), m),
                Some(Hit::Pane(i)) => self.to_pane(m, i),
                _ => self.menu = None,
            },
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(hit, m),
            MouseEventKind::Up(MouseButton::Left) => self.release(hit, m),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                match hit {
                    Some(Hit::Pane(i)) => self.to_pane(m, i), // scrolls that pane, focused or not
                    Some(Hit::Cards | Hit::Card(_)) => {
                        self.dash_scroll = if up { self.dash_scroll.saturating_sub(1) } else { self.dash_scroll + 1 }
                    }
                    _ => {
                        if let Some(d) = &mut self.diff {
                            d.scroll = if up { d.scroll.saturating_sub(3) } else { d.scroll.saturating_add(3) };
                        }
                    }
                }
            }
            _ => {
                if let Some(Hit::Pane(i)) = hit {
                    self.to_pane(m, i);
                }
            }
        }
    }

    fn setup_key(&mut self, k: KeyEvent) {
        let Some(s) = &self.setup else { return };
        let (n, at) = (s.choices().len(), s.choice);
        match k.code {
            KeyCode::Up | KeyCode::Char('k') if n > 0 => self.setup_choose((at + n - 1) % n),
            KeyCode::Down | KeyCode::Char('j') if n > 0 => self.setup_choose((at + 1) % n),
            KeyCode::Enter | KeyCode::Right => self.setup_next(),
            KeyCode::Left | KeyCode::Backspace => self.setup_back(),
            KeyCode::Esc => self.setup_finish(), // skip the rest, keeping what you picked so far
            _ => {}
        }
    }

    /// Highlight an option; the theme and mascot steps preview it on the spot.
    fn setup_choose(&mut self, k: usize) {
        let Some(s) = &mut self.setup else { return };
        s.choice = k.min(s.choices().len().saturating_sub(1));
        match s.step {
            1 => theme::set_light(theme::choose(Some(THEMES[s.choice]), std::env::var("COLORFGBG").ok().as_deref())),
            2 => chud::set_safe(s.choice == 1),
            _ => {}
        }
    }

    fn setup_next(&mut self) {
        let Some(s) = &mut self.setup else { return };
        match s.step {
            1 => s.theme = s.choice,
            2 => s.mascot = s.choice,
            // the one step that changes a file outside chud: act on the choice, then stay to
            // show what happened; the next Enter moves on
            3 if !s.choices().is_empty() => {
                s.status_result = Some(match s.choice {
                    0 => {
                        let home = config::home();
                        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or("chud".into());
                        let replace = matches!(s.status_line, setup::StatusLine::Foreign(_));
                        setup::enable_status_line(&home, &exe, replace)
                    }
                    _ => Err("Left as it is. Context bars and usage limits stay off until you run chud --setup.".into()),
                });
                s.choice = 0;
                return;
            }
            _ if s.step + 1 >= SETUP_STEPS => return self.setup_finish(),
            _ => {}
        }
        s.step += 1;
        s.choice = match s.step {
            1 => s.theme,
            2 => s.mascot,
            _ => 0,
        };
        let k = s.choice;
        self.setup_choose(k);
    }

    fn setup_back(&mut self) {
        let Some(s) = &mut self.setup else { return };
        if s.step == 0 {
            return;
        }
        s.step -= 1;
        let k = match s.step {
            1 => s.theme,
            2 => s.mascot,
            _ => 0,
        };
        self.setup_choose(k);
    }

    /// Saves your choices into config.json, keeping anything else in it (your agents), and
    /// applies them to this run.
    fn setup_finish(&mut self) {
        let Some(s) = self.setup.take() else { return };
        let mut cfg = config::load();
        cfg["version"] = json!(1);
        cfg["theme"] = json!(THEMES[s.theme]);
        cfg["mascot"] = json!(MASCOTS[s.mascot]);
        config::save(&cfg);
        self.theme_follows = s.theme == 0;
        theme::set_light(theme::choose(Some(THEMES[s.theme]), std::env::var("COLORFGBG").ok().as_deref()));
        chud::set_safe(s.mascot == 1);
        self.flash = Some(format!(" setup saved to {} · run chud --setup to change it ", config::path().display()));
    }

    fn press(&mut self, hit: Option<Hit>, m: MouseEvent) {
        if self.setup.is_some() {
            match hit {
                Some(Hit::SetupChoice(k)) => self.setup_choose(k),
                Some(Hit::SetupNext) => self.setup_next(),
                Some(Hit::SetupBack) => self.setup_back(),
                _ => {}
            }
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if let Some(menu) = self.menu.take() {
            if let Some(Hit::MenuItem(k)) = hit {
                if let Some(&(_, act)) = menu.items.get(k) {
                    self.run_act(act);
                }
            }
            return; // a click anywhere else just closes the menu
        }
        if self.prompt.is_some() {
            match hit {
                Some(Hit::Ok) => self.accept_prompt(),
                Some(Hit::Cancel) => self.prompt = None,
                _ => {}
            }
            return;
        }
        let at = |items| Some(Menu { x: m.column, y: m.row + 1, items });
        match hit {
            Some(Hit::Tool(t)) => self.tool(t),
            Some(h @ (Hit::Session(_) | Hit::Edge)) => {
                self.drag = Some(Drag { from: h, at: (m.column, m.row), moved: false })
            }
            Some(Hit::SessionMenu(i)) => {
                let gi = self.groups.iter().position(|g| g.id == self.sessions[i].group).unwrap_or(0);
                let mut items = vec![("Rename…", Act::Rename(i)), ("Move to group…", Act::Group(i))];
                if self.layout.contains(self.sessions[i].id) {
                    items.push(("New terminal beside", Act::SplitNew(i, layout::Dir::Across)));
                    items.push(("New terminal below", Act::SplitNew(i, layout::Dir::Down)));
                    if self.layout.leaves().len() > 1 {
                        items.push(("Close pane (keeps running)", Act::ClosePane(i)));
                    }
                } else {
                    items.push(("Open beside current pane", Act::Split(i, layout::Dir::Across)));
                    items.push(("Open below current pane", Act::Split(i, layout::Dir::Down)));
                }
                items.extend([
                    ("New terminal here", Act::NewTerminal(gi)),
                    ("Review changes", Act::Diff(i)),
                    ("Kill…", Act::Kill(i)),
                ]);
                self.menu = at(items)
            }
            Some(Hit::Group(gi)) => {
                self.groups[gi].collapsed ^= true;
                self.save();
            }
            Some(Hit::GroupMenu(gi)) => {
                let fold = if self.groups[gi].collapsed { "Unfold" } else { "Fold" };
                let mut items = vec![
                    ("New terminal here", Act::NewTerminal(gi)),
                    ("Rename group…", Act::GroupRename(gi)),
                    (fold, Act::Fold(gi)),
                ];
                if gi != 0 {
                    items.push(("Delete group", Act::DeleteGroup(gi)));
                }
                self.menu = at(items);
            }
            Some(Hit::DiffFile(k)) => {
                if let Some(d) = &mut self.diff {
                    d.sel = k;
                    d.load();
                }
            }
            Some(Hit::DiffAct(a)) => self.diff_act(a),
            Some(Hit::Card(i)) => self.select(i),
            Some(Hit::Pane(i)) => {
                if i != self.sel {
                    self.select(i); // clicking a pane focuses it
                }
                // where a selection would start; a press that never moves is just a click,
                // and the agent gets it either way
                self.picked = Some(((m.column, m.row), (m.column, m.row)));
                self.drag = Some(Drag { from: Hit::Pane(i), at: (m.column, m.row), moved: false });
                self.to_pane(m, i);
            }
            Some(Hit::ClosePane(i)) => self.close_pane(i),
            Some(h @ Hit::Divider(_)) => self.drag = Some(Drag { from: h, at: (m.column, m.row), moved: false }),
            _ => {}
        }
    }

    fn drag_to(&mut self, hit: Option<Hit>, m: MouseEvent) {
        let Some(d) = &mut self.drag else { return };
        d.moved |= (m.column, m.row) != d.at;
        match d.from {
            _ if !d.moved => {}
            Hit::Edge => self.side = (m.column + 1).clamp(24, (self.size.0 / 2).max(24)),
            // dragging over the terminal selects its text rather than reaching the agent
            Hit::Pane(i) => match &mut self.picked {
                Some((_, to)) => *to = (m.column, m.row),
                None => self.to_pane(m, i),
            },
            // the panes follow the divider as you drag; terminals are resized once you let go
            Hit::Divider(k) => {
                if let Some(div) = self.layout.dividers(self.main_area()).get(k) {
                    let ratio = layout::ratio_at(div, m.column, m.row);
                    let path = div.path.clone();
                    self.layout.set_ratio(&path, ratio);
                }
            }
            Hit::Session(_) => {
                self.hover = hit;
                // out of the sidebar and over a pane: outline where it would open
                self.drop = self.pane_at(m.column, m.row).map(|(p, r)| (p, layout::Zone::at(r, m.column, m.row)));
            }
            _ => self.hover = hit,
        }
    }

    /// The pane under a point, header included: its session index and area.
    fn pane_at(&self, col: u16, row: u16) -> Option<(usize, Rect)> {
        let at = ratatui::layout::Position::new(col, row);
        let (id, r) = self.layout.rects(self.main_area()).into_iter().find(|(_, r)| r.contains(at))?;
        Some((self.sessions.iter().position(|s| s.id == id)?, r))
    }

    /// A session dragged onto a pane: near an edge it opens on that side, in the middle it takes
    /// the pane's place. One already on screen moves rather than appearing twice.
    fn drop_on_pane(&mut self, i: usize, p: usize, zone: layout::Zone) {
        let (Some(id), Some(target)) = (self.sessions.get(i).map(|s| s.id), self.sessions.get(p).map(|s| s.id)) else {
            return;
        };
        if id == target {
            return;
        }
        match zone.split() {
            None => {
                self.layout.remove(id);
                self.layout.replace(target, id);
            }
            Some((dir, first)) => {
                let room = self.layout.rects(self.main_area()).into_iter().find(|(t, _)| *t == target);
                if !room.is_some_and(|(_, r)| layout::fits(r, dir)) {
                    self.flash = Some(" not enough room to open it there · make the window bigger or zoom with C-a f ".into());
                    return;
                }
                self.layout.remove(id);
                self.layout.split_placed(target, dir, id, first);
            }
        }
        self.select(i);
        self.apply_layout();
        self.save();
    }

    fn release(&mut self, hit: Option<Hit>, m: MouseEvent) {
        let Some(d) = self.drag.take() else { return };
        self.hover = None;
        let dropped = self.drop.take();
        match d.from {
            Hit::Session(i) if !d.moved || hit == Some(Hit::Session(i)) => self.select(i), // a click
            Hit::Session(i) if dropped.is_some() => {
                let (p, zone) = dropped.unwrap();
                self.drop_on_pane(i, p, zone);
            }
            Hit::Session(i) => self.drop_session(i, hit),
            Hit::Edge => {
                self.apply_layout();
                self.save();
            }
            Hit::Pane(_) if d.moved => self.copy_picked(),
            Hit::Pane(i) => {
                self.picked = None;
                self.to_pane(m, i);
            }
            Hit::Divider(_) => {
                self.apply_layout();
                self.save();
            }
            _ => {}
        }
    }

    /// What the drag covered, in the agent's own screen coordinates, ordered from the earlier
    /// cell to the later one.
    fn picked_cells(&self) -> Option<((u16, u16), (u16, u16))> {
        let (anchor, to) = self.picked?;
        let pane = self.hits.iter().find(|(_, h)| *h == Hit::Pane(self.sel))?.0; // the focused pane: drags start there
        let cell = |(x, y): (u16, u16)| {
            (y.clamp(pane.y, pane.bottom() - 1) - pane.y, x.clamp(pane.x, pane.right() - 1) - pane.x)
        };
        let (a, b) = (cell(anchor), cell(to));
        (a != b).then(|| if a < b { (a, b) } else { (b, a) }) // a press that never moved is a click
    }

    /// Copies the dragged-over text. The selection stays lit so you can see what you got.
    fn copy_picked(&mut self) {
        let (Some(((r1, c1), (r2, c2))), Some(s)) = (self.picked_cells(), self.sessions.get(self.sel)) else {
            self.flash = Some(" nothing selected · drag over the terminal first ".into());
            return;
        };
        let text = s.parser.lock().unwrap().screen().contents_between(r1, c1, r2, c2 + 1);
        self.flash = match (text.is_empty(), copy_to_clipboard(&text)) {
            (true, _) => None,
            (_, Ok(())) => Some(format!(" copied {} characters to the clipboard ", text.chars().count())),
            (_, Err(e)) => Some(format!(" could not copy: {e} ")),
        };
    }

    /// Forwards a mouse event to the agent in the pane, or scrolls our scrollback if it
    /// didn't ask for the mouse.
    fn to_pane(&mut self, m: MouseEvent, i: usize) {
        let Some(&(r, _)) = self.hits.iter().find(|(_, h)| *h == Hit::Pane(i)) else { return };
        let Some(s) = self.sessions.get(i) else { return };
        let (col, row) = (m.column.saturating_sub(r.x).min(r.width - 1), m.row.saturating_sub(r.y).min(r.height - 1));
        let mut p = s.parser.lock().unwrap();
        let screen = p.screen();
        if let Some(bytes) = mouse_bytes(m, col, row, screen.mouse_protocol_mode(), screen.mouse_protocol_encoding()) {
            drop(p);
            return s.write(&bytes);
        }
        let back = p.screen().scrollback();
        match m.kind {
            MouseEventKind::ScrollUp => p.screen_mut().set_scrollback(back + 3),
            MouseEventKind::ScrollDown => p.screen_mut().set_scrollback(back.saturating_sub(3)),
            _ => {}
        }
    }
}

/// `chud --statusline` is Claude Code's status-line command (set in ~/.claude/settings.json).
/// After each reply Claude pipes in its session JSON; chud keeps the plan limits for its session
/// header and prints them for Claude's own footer.
fn statusline() -> Result<()> {
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let v: Value = serde_json::from_str(&input).unwrap_or_default();
    // this session's context window, for the bar chud draws beside the session
    if let (Some(sid), true) = (v["session_id"].as_str(), v["context_window"].is_object()) {
        config::write_atomic(&usage::context_path(sid), &v["context_window"].to_string());
    }
    let path = usage::limits_path();
    let limits = if v["rate_limits"].is_object() {
        config::write_atomic(&path, &v["rate_limits"].to_string());
        v["rate_limits"].clone()
    } else {
        // not sent yet this session (it comes with the first reply): show the last known
        std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    };
    let pct = |w: Option<usage::Window>| w.map(|w| format!("{:.0}%", w.used));
    match usage::claude_windows(&limits, usage::now_secs()).map(pct) {
        [Some(h), Some(w)] => println!("5h {h} · week {w}"),
        [Some(h), None] => println!("5h {h}"),
        _ => {}
    }
    Ok(())
}

/// Puts text on the system clipboard with whatever this platform provides: pbcopy on macOS,
/// PowerShell on Windows (reading UTF-8, so non-ASCII survives), wl-copy or xclip on Linux.
fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    let windows = "[Console]::InputEncoding = [Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())";
    let tools: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(windows) {
        &[("powershell", &["-NoProfile", "-Command", windows])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    };
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no clipboard tool found");
    for (cmd, args) in tools {
        match Command::new(cmd).args(*args).stdin(std::process::Stdio::piped()).spawn() {
            Ok(mut child) => {
                child.stdin.take().expect("piped").write_all(text.as_bytes())?;
                child.wait()?;
                return Ok(());
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn notify(title: &str, body: &str) {
    let body: String = body.chars().filter(|c| !c.is_control()).take(200).collect();
    let title = title.replace(';', ",");
    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("iTerm.app") => print!("\x1b]9;{title}: {body}\x07"),
        Ok("WarpTerminal" | "ghostty" | "WezTerm") => print!("\x1b]777;notify;{title};{body}\x07"),
        // otherwise the desktop's own notifications
        _ if cfg!(target_os = "macos") => {
            let script = format!("display notification {body:?} with title {title:?}");
            std::thread::spawn(move || Command::new("osascript").args(["-e", &script]).status());
        }
        _ if cfg!(windows) => {
            // a toast through PowerShell's own app id, which Windows lets post without registering
            let quote = |s: &str| s.replace('\'', "''");
            let script = format!(
                "$x = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]::GetTemplateContent(1); \
                 $t = $x.GetElementsByTagName('text'); $t[0].AppendChild($x.CreateTextNode('{}')) > $null; $t[1].AppendChild($x.CreateTextNode('{}')) > $null; \
                 [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}}\\WindowsPowerShell\\v1.0\\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($x))",
                quote(&title),
                quote(&body)
            );
            std::thread::spawn(move || Command::new("powershell").args(["-NoProfile", "-Command", &script]).status());
        }
        _ => {
            std::thread::spawn(move || Command::new("notify-send").args([title.as_str(), body.as_str()]).status());
        }
    }
    let _ = stdout().flush();
}

/// Encodes a key press the way xterm would send it to the program.
fn key_bytes(k: KeyEvent, app_cursor: bool) -> Vec<u8> {
    let m = k.modifiers;
    let (shift, alt, ctrl) =
        (m.contains(KeyModifiers::SHIFT), m.contains(KeyModifiers::ALT), m.contains(KeyModifiers::CONTROL));
    let modn = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let cursor = |c: char| match (modn, app_cursor) {
        (1, true) => format!("\x1bO{c}"),
        (1, false) => format!("\x1b[{c}"),
        _ => format!("\x1b[1;{modn}{c}"),
    };
    let tilde = |n: u8| match modn {
        1 => format!("\x1b[{n}~"),
        _ => format!("\x1b[{n};{modn}~"),
    };
    let mut out = match k.code {
        KeyCode::Char(c) if ctrl => vec![match c {
            'a'..='z' | 'A'..='Z' => c.to_ascii_lowercase() as u8 & 0x1f,
            ' ' | '@' => 0,
            '[' => 27,
            '\\' => 28,
            ']' => 29,
            '^' => 30,
            '_' => 31,
            '?' => 127,
            _ => return c.to_string().into_bytes(),
        }],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => return b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![if ctrl { 0x08 } else { 0x7f }],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => return cursor('A').into_bytes(),
        KeyCode::Down => return cursor('B').into_bytes(),
        KeyCode::Right => return cursor('C').into_bytes(),
        KeyCode::Left => return cursor('D').into_bytes(),
        KeyCode::Home => return cursor('H').into_bytes(),
        KeyCode::End => return cursor('F').into_bytes(),
        KeyCode::Insert => return tilde(2).into_bytes(),
        KeyCode::Delete => return tilde(3).into_bytes(),
        KeyCode::PageUp => return tilde(5).into_bytes(),
        KeyCode::PageDown => return tilde(6).into_bytes(),
        KeyCode::F(n @ 1..=4) => {
            let c = b"PQRS"[n as usize - 1] as char;
            return match modn {
                1 => format!("\x1bO{c}"),
                _ => format!("\x1b[1;{modn}{c}"),
            }
            .into_bytes();
        }
        KeyCode::F(n @ 5..=12) => return tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]).into_bytes(),
        _ => return vec![],
    };
    if alt {
        out.insert(0, 0x1b);
    }
    out
}

/// Encodes a mouse event for a program that enabled mouse reporting; None if it didn't ask.
fn mouse_bytes(
    m: MouseEvent,
    col: u16,
    row: u16,
    mode: vt100::MouseProtocolMode,
    enc: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use vt100::MouseProtocolMode as M;
    use MouseEventKind as K;
    let btn = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (code, release) = match (m.kind, mode) {
        (_, M::None) => return None,
        (K::ScrollUp, _) => (64, false),
        (K::ScrollDown, _) => (65, false),
        (K::Down(b), _) => (btn(b), false),
        (K::Up(b), M::PressRelease | M::ButtonMotion | M::AnyMotion) => (btn(b), true),
        (K::Drag(b), M::ButtonMotion | M::AnyMotion) => (btn(b) + 32, false),
        (K::Moved, M::AnyMotion) => (35, false),
        _ => return None,
    };
    let code = code
        + 4 * m.modifiers.contains(KeyModifiers::SHIFT) as u16
        + 8 * m.modifiers.contains(KeyModifiers::ALT) as u16
        + 16 * m.modifiers.contains(KeyModifiers::CONTROL) as u16;
    Some(match enc {
        vt100::MouseProtocolEncoding::Sgr => {
            format!("\x1b[<{code};{};{}{}", col + 1, row + 1, if release { 'm' } else { 'M' }).into_bytes()
        }
        _ => {
            let b = if release { 3 } else { code };
            let clamp = |v: u16| (32 + (v + 1).min(223)) as u8;
            vec![0x1b, b'[', b'M', 32 + b as u8, clamp(col), clamp(row)]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn a_shell_comes_back_running_its_chat() {
        let zsh = vec!["zsh".to_string()];
        let id = "b8d81aaf-7db2-43f9-9cae-3d2211082fb9";
        assert_eq!(
            resume_in_shell(&zsh, "claude", id),
            Some(vec!["zsh".into(), "-ic".into(), format!("claude --resume {id}; exec zsh")])
        );
        assert!(resume_in_shell(&zsh, "copilot", id).is_some());
        assert_eq!(resume_in_shell(&["claude".to_string()], "claude", id), None, "agents resume their own way");
        assert_eq!(resume_in_shell(&zsh, "claude", "x; rm -rf ~"), None, "the id lands in a command line");
        assert_eq!(resume_in_shell(&zsh, "claude", ""), None);
        assert_eq!(resume_in_shell(&zsh, "sh -c evil", id), None, "only the two agents we know");
    }

    #[test]
    fn fuzzy_finds_and_ranks() {
        assert!(fuzzy("", "anything").is_some(), "an empty search matches everything");
        assert!(fuzzy("cld", "claude · chud").is_some(), "gaps are allowed");
        assert!(fuzzy("xyz", "claude").is_none());
        assert!(fuzzy("CHUD", "chud").is_some(), "case is ignored both ways");
        // the whole word beats the same letters scattered, and a word start beats mid-word
        let best = |q: &str, a: &str, b: &str| {
            let (sa, sb) = (fuzzy(q, a).unwrap(), fuzzy(q, b).unwrap());
            assert!(sa > sb, "{q:?}: {a:?} ({sa}) should rank above {b:?} ({sb})");
        };
        best("auth", "auth tokens", "a quick thing");
        best("cl", "claude code", "terminal client");
        best("api", "api server", "rapid");
    }

    #[test]
    fn keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(key_bytes(key(KeyCode::Char('a'), KeyModifiers::CONTROL), false), [1]);
        assert_eq!(key_bytes(key(KeyCode::Char('b'), KeyModifiers::ALT), false), b"\x1bb");
        assert_eq!(key_bytes(key(KeyCode::Enter, none), false), b"\r");
        assert_eq!(key_bytes(key(KeyCode::Enter, KeyModifiers::ALT), false), b"\x1b\r");
        assert_eq!(key_bytes(key(KeyCode::Up, none), false), b"\x1b[A");
        assert_eq!(key_bytes(key(KeyCode::Up, none), true), b"\x1bOA");
        assert_eq!(key_bytes(key(KeyCode::Left, KeyModifiers::CONTROL), true), b"\x1b[1;5D");
        assert_eq!(key_bytes(key(KeyCode::Delete, none), false), b"\x1b[3~");
        assert_eq!(key_bytes(key(KeyCode::F(5), none), false), b"\x1b[15~");
        assert_eq!(key_bytes(key(KeyCode::Char('é'), none), false), "é".as_bytes());
    }

    #[test]
    fn mouse() {
        use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
        let ev = |kind| MouseEvent { kind, column: 0, row: 0, modifiers: KeyModifiers::NONE };
        let down = ev(MouseEventKind::Down(MouseButton::Left));
        assert_eq!(mouse_bytes(down, 0, 0, M::None, E::Sgr), None);
        assert_eq!(mouse_bytes(down, 4, 2, M::PressRelease, E::Sgr).unwrap(), b"\x1b[<0;5;3M");
        let up = ev(MouseEventKind::Up(MouseButton::Left));
        assert_eq!(mouse_bytes(up, 4, 2, M::PressRelease, E::Sgr).unwrap(), b"\x1b[<0;5;3m");
        assert_eq!(mouse_bytes(up, 0, 0, M::Press, E::Sgr), None);
        let wheel = ev(MouseEventKind::ScrollUp);
        assert_eq!(mouse_bytes(wheel, 0, 0, M::Press, E::Default).unwrap(), [0x1b, b'[', b'M', 96, 33, 33]);
    }

    #[test]
    fn sidebar_layout() {
        let g = |id, name: &str, collapsed| Group { id, name: name.into(), collapsed };
        // only "ungrouped": no headers, like v1
        let solo = [g(0, "ungrouped", false)];
        assert_eq!(layout(&solo, &[0, 0], false), [Row::Session(0), Row::Session(1)]);
        // sessions: 0 ungrouped, 1 backend, 2 scratch (folded), 3 backend
        let groups = [g(0, "ungrouped", false), g(1, "backend", false), g(2, "scratch", true)];
        let rows = layout(&groups, &[0, 1, 2, 1], false);
        let want = [Row::Group(1), Row::Session(1), Row::Session(3), Row::Group(2), Row::Group(0), Row::Session(0)];
        assert_eq!(rows, want);
        assert!(layout(&groups, &[0, 1, 2, 1], true).contains(&Row::Session(2)), "expand_all shows folded");
        // an empty "ungrouped" gets no header
        assert_eq!(layout(&groups[..2], &[1], false), [Row::Group(1), Row::Session(0)]);
    }

    #[test]
    fn drag_reorder() {
        let mut v = vec!['a', 'b', 'c', 'd'];
        assert_eq!(move_item(&mut v, 0, Some(2)), 1);
        assert_eq!(v, ['b', 'a', 'c', 'd'], "a lands just above c");
        assert_eq!(move_item(&mut v, 3, Some(0)), 0);
        assert_eq!(v, ['d', 'b', 'a', 'c']);
        assert_eq!(move_item(&mut v, 0, None), 3);
        assert_eq!(v, ['b', 'a', 'c', 'd']);
    }

    #[test]
    fn resume() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(resume_argv(&v("claude"), "abc", true), v("claude --resume abc"));
        assert_eq!(resume_argv(&v("copilot"), "abc", true), v("copilot --resume abc"));
        assert_eq!(resume_argv(&v("claude"), "abc", false), v("claude"), "nothing on disk yet: start fresh");
        assert_eq!(resume_argv(&v("/bin/zsh"), "abc", true), v("/bin/zsh"));
    }
}
