mod chud;
mod git;
mod session;
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
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(16);
const TICK: Duration = Duration::from_secs(1);
const SIDE: u16 = 38;
const DEFAULT_CMD: &str = "zsh"; // what a new terminal runs

enum Ask {
    Commit,
    Kill,
    Discard,
    Quit,
    Rename,
    Group,
    GroupRename(usize), // index into App.groups
    NewGroup,
}

struct Prompt {
    ask: Ask,
    input: String,
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
    Pane,
    Backdrop,
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

fn state_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/chud/state.json")
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
    /// mouse reporting off so the terminal can select text (C-a v)
    select: bool,
    /// sidebar width parked here while the terminal is zoomed (C-a f)
    zoom: Option<u16>,
    /// the window changed size: repaint every cell, not just the ones that differ
    resized: bool,
    /// one line of feedback in the status bar, until the next key
    flash: Option<String>,
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
        zoom: None,
        resized: false,
        flash: None,
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    for cmd in &args {
        app.open(cmd);
    }
    if args.is_empty() {
        app.restore();
    }
    if app.sessions.is_empty() {
        app.open(DEFAULT_CMD); // like any terminal app: start with a shell
    }
    app.refresh_plan();

    let mut hits = vec![];
    term.draw(|f| hits = ui::draw(f, &app))?;
    app.hits = hits;
    let mut last = Instant::now();
    loop {
        // Block until something happens (idle = no wakeups); while something is animating or
        // timing, wake once a second. Then drain and draw once.
        let ticking = app.dash
            || app.sessions.iter().any(|s| s.working_since.is_some() || s.status() == Status::NeedsInput);
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
                term.clear()?;
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
                    let status = self.check_status(i);
                    agent || status || (i == self.sel && self.diff.is_none() && !self.dash)
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
                let size = self.pane();
                for s in &self.sessions {
                    s.resize(size);
                }
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
        if let Some(s) = self.sessions.get(self.sel) {
            let mut p = s.parser.lock().unwrap();
            p.screen_mut().set_scrollback(0);
            let bytes = key_bytes(k, p.screen().application_cursor());
            drop(p);
            s.write(&bytes);
        }
    }

    fn command(&mut self, k: KeyEvent, ctrl_a: bool) {
        let has = self.sel < self.sessions.len();
        let ask = |ask: Ask, input: String| Some(Prompt { ask, input });
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
            KeyCode::Char('v') => self.toggle_select(),
            KeyCode::Char('y') if has => self.copy_screen(),
            KeyCode::Char('f') => {
                match self.zoom.take() {
                    Some(width) => self.side = width,
                    None => self.zoom = Some(std::mem::replace(&mut self.side, 0)),
                }
                self.save();
            }
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
    fn toggle_select(&mut self) {
        self.select = !self.select;
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
        self.flash = match pbcopy(&text) {
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
        let yes_no = matches!(p.ask, Ask::Kill | Ask::Discard | Ask::Quit);
        match k.code {
            KeyCode::Esc => {}
            KeyCode::Char('y') if yes_no => self.confirm(p.ask),
            _ if yes_no => {}
            KeyCode::Enter => self.submit(p.ask, p.input.trim()),
            KeyCode::Backspace => {
                p.input.pop();
                self.prompt = Some(p);
            }
            KeyCode::Char(c) => {
                p.input.push(c);
                self.prompt = Some(p);
            }
            _ => self.prompt = Some(p),
        }
    }

    /// The prompt's OK / Yes button.
    fn accept_prompt(&mut self) {
        if let Some(Prompt { ask, input }) = self.prompt.take() {
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
                self.sessions.remove(self.sel); // Drop kills the child
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
                Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default() + rest),
                None => PathBuf::from(last),
            };
            if dir.is_dir() {
                cwd = dir.canonicalize().unwrap_or(dir);
                argv.pop();
            }
        }
        match argv.first().map(String::as_str) {
            None => return,
            Some("shell") => argv[0] = std::env::var("SHELL").unwrap_or("zsh".into()),
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

    fn spawn(&mut self, argv: Vec<String>, cwd: PathBuf, group: usize) -> Option<usize> {
        let mut s = Session::spawn(self.next_id, argv, cwd, self.pane(), self.tx.clone()).ok()?;
        s.group = group;
        self.next_id += 1;
        self.sessions.push(s);
        Some(self.sessions.len() - 1)
    }

    fn rows(&self, expand_all: bool) -> Vec<Row> {
        let members: Vec<usize> = self.sessions.iter().map(|s| s.group).collect();
        layout(&self.groups, &members, expand_all)
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
                json!({ "argv": s.argv, "cwd": s.cwd, "name": s.name, "group": s.group,
                        "sid": s.agent_sid(), "worked": s.worked().as_secs() })
            })
            .collect();
        let path = state_path();
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let state = json!({ "side": self.zoom.unwrap_or(self.side), "groups": groups, "sessions": sessions });
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
        for s in v["sessions"].as_array().into_iter().flatten() {
            let argv: Vec<String> =
                s["argv"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect();
            let (Some(prog), Some(cwd)) = (argv.first(), s["cwd"].as_str()) else { continue };
            let cwd = PathBuf::from(cwd);
            let sid = s["sid"].as_str().unwrap_or_default();
            let copilot = session::agent_of(prog) == Agent::Copilot;
            let saved = usage::log_path(copilot, sid, &cwd).is_some_and(|p| p.exists());
            let run = resume_argv(&argv, sid, saved);
            let group = s["group"].as_u64().unwrap_or(0) as usize;
            let group = if self.groups.iter().any(|g| g.id == group) { group } else { 0 };
            if let Some(i) = self.spawn(run, cwd, group) {
                let restored = &mut self.sessions[i];
                restored.argv = argv;
                restored.name = s["name"].as_str().map(String::from);
                restored.worked = Duration::from_secs(s["worked"].as_u64().unwrap_or(0));
                restored.refresh_usage();
            }
        }
        self.sel = self.visible().first().copied().unwrap_or(0);
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
            DiffAct::Commit if has_files => self.prompt = Some(Prompt { ask: Ask::Commit, input: String::new() }),
            DiffAct::Discard if has_files => self.prompt = Some(Prompt { ask: Ask::Discard, input: String::new() }),
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
        let ask = |ask: Ask, input: String| Some(Prompt { ask, input });
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
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.press(hit, m),
            // right-click opens the same menu as a row's … button
            MouseEventKind::Down(MouseButton::Right) => match hit {
                Some(Hit::Session(i) | Hit::SessionMenu(i)) => self.press(Some(Hit::SessionMenu(i)), m),
                Some(Hit::Group(gi) | Hit::GroupMenu(gi)) => self.press(Some(Hit::GroupMenu(gi)), m),
                Some(Hit::Pane) => self.to_pane(m),
                _ => self.menu = None,
            },
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(hit, m),
            MouseEventKind::Up(MouseButton::Left) => self.release(hit, m),
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                match hit {
                    Some(Hit::Pane) => self.to_pane(m),
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
            _ if hit == Some(Hit::Pane) => self.to_pane(m),
            _ => {}
        }
    }

    fn press(&mut self, hit: Option<Hit>, m: MouseEvent) {
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
                self.menu = at(vec![
                    ("Rename…", Act::Rename(i)),
                    ("Move to group…", Act::Group(i)),
                    ("New terminal here", Act::NewTerminal(gi)),
                    ("Review changes", Act::Diff(i)),
                    ("Kill…", Act::Kill(i)),
                ])
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
            Some(Hit::Pane) => {
                self.drag = Some(Drag { from: Hit::Pane, at: (m.column, m.row), moved: false });
                self.to_pane(m);
            }
            _ => {}
        }
    }

    fn drag_to(&mut self, hit: Option<Hit>, m: MouseEvent) {
        let Some(d) = &mut self.drag else { return };
        d.moved |= (m.column, m.row) != d.at;
        match d.from {
            _ if !d.moved => {}
            Hit::Edge => self.side = (m.column + 1).clamp(24, (self.size.0 / 2).max(24)),
            Hit::Pane => self.to_pane(m),
            _ => self.hover = hit,
        }
    }

    fn release(&mut self, hit: Option<Hit>, m: MouseEvent) {
        let Some(d) = self.drag.take() else { return };
        self.hover = None;
        match d.from {
            Hit::Session(i) if !d.moved || hit == Some(Hit::Session(i)) => self.select(i), // a click
            Hit::Session(i) => self.drop_session(i, hit),
            Hit::Edge => {
                let size = self.pane();
                for s in &self.sessions {
                    s.resize(size);
                }
                self.save();
            }
            Hit::Pane => self.to_pane(m),
            _ => {}
        }
    }

    /// Forwards a mouse event to the agent in the pane, or scrolls our scrollback if it
    /// didn't ask for the mouse.
    fn to_pane(&mut self, m: MouseEvent) {
        let Some(&(r, _)) = self.hits.iter().find(|(_, h)| *h == Hit::Pane) else { return };
        let Some(s) = self.sessions.get(self.sel) else { return };
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
    let path = usage::limits_path();
    let limits = if v["rate_limits"].is_object() {
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let tmp = path.with_extension(format!("{}.tmp", std::process::id())); // sessions run this at once
        if std::fs::write(&tmp, v["rate_limits"].to_string()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
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

fn pbcopy(text: &str) -> std::io::Result<()> {
    let mut child = Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn()?;
    child.stdin.take().expect("piped").write_all(text.as_bytes())?;
    child.wait()?;
    Ok(())
}

fn notify(title: &str, body: &str) {
    let body: String = body.chars().filter(|c| !c.is_control()).take(200).collect();
    let title = title.replace(';', ",");
    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("iTerm.app") => print!("\x1b]9;{title}: {body}\x07"),
        Ok("WarpTerminal" | "ghostty" | "WezTerm") => print!("\x1b]777;notify;{title};{body}\x07"),
        _ => {
            let script = format!("display notification {body:?} with title {title:?}");
            std::thread::spawn(move || Command::new("osascript").args(["-e", &script]).status());
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
