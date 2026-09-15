//! chud.app: a window with a built-in terminal (iced_term, on Alacritty's engine) that runs
//! the chud TUI. Its fonts are bundled, so every icon renders; no other terminal needed.
use iced::advanced::text::{Alignment, LineHeight, Paragraph as _, Shaping, Wrapping};
use iced::advanced::Text;
use iced::keyboard::{self, Key};
use iced::{alignment, event, mouse, window, Element, Event, Font, Pixels, Point, Size, Subscription, Task};
use iced_term::actions::Action;
use iced_term::bindings::{Binding, BindingAction, InputKind};
use iced_term::settings::{BackendSettings, FontSettings, Settings};
use iced_term::{BackendCommand, Command, TermMode, TerminalView};
use std::collections::HashMap;

fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title(|_: &App| String::from("chud"))
        .subscription(App::subscription)
        // Fira Code, in the "Nerd Font" variant rather than "Mono": its icons are ~1.6 cells wide
        // and spill into the space chud leaves after each one, so agent icons read at a glance
        .font(include_bytes!("../fonts/FiraCodeNerdFont-Regular.ttf").as_slice())
        .font(include_bytes!("../fonts/FiraCodeNerdFont-Bold.ttf").as_slice())
        // fallback for the symbols Claude and Copilot draw that Fira Code lacks (⏺ ✻ ✢ ⏵ ...)
        .font(include_bytes!("../fonts/NotoSansSymbols2-Regular.ttf").as_slice())
        .window(window::Settings {
            size: Size::new(1280.0, 800.0),
            min_size: Some(Size::new(700.0, 400.0)),
            ..Default::default()
        })
        .run()
}

fn font() -> FontSettings {
    // line height = Fira Code's full-block height (2400/1950 em), so ▀▄█ tile with no gaps
    FontSettings { size: 13.0, scale_factor: 1.231, font_type: Font::with_name("FiraCode Nerd Font") }
}

/// One terminal cell in window pixels, measured the way iced_term does (its font.rs), which
/// then rounds down to whole pixels.
fn cell_size() -> Size {
    let f = font();
    let m = iced_graphics::text::paragraph::Paragraph::with_text(Text {
        content: "m",
        font: f.font_type,
        size: Pixels(f.size),
        align_y: alignment::Vertical::Center,
        align_x: Alignment::Center,
        shaping: Shaping::Advanced,
        line_height: LineHeight::Relative(f.scale_factor),
        bounds: Size::INFINITE,
        wrapping: Wrapping::Glyph,
    })
    .min_bounds();
    Size::new(m.width.floor().max(1.0), m.height.floor().max(1.0))
}

#[derive(Debug, Clone)]
enum Message {
    Terminal(iced_term::Event),
    Paste,
    Pasted(Option<String>),
    Focus(bool),
    FontLoaded,
    Resized(Size),
    Cursor(Point),
    RightClick,
    Wheel(mouse::ScrollDelta),
}

struct App {
    term: iced_term::Terminal,
    cursor: Point,
    scroll_px: f32, // trackpad scrolling not yet worth a whole line
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let chud = std::env::current_exe().map(|p| p.with_file_name("chud")).unwrap_or_else(|_| "chud".into());
        // A login + interactive shell gives chud (and the agents it starts) your real PATH;
        // apps launched from the Dock otherwise get only /usr/bin:/bin.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut args: Vec<String> = ["-l", "-i", "-c", r#"exec "$0" "$@""#].map(String::from).into();
        args.push(chud.to_string_lossy().into_owned());
        args.extend(std::env::args().skip(1)); // `open -a chud --args claude` starts a claude session
        let env = [("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TERM_PROGRAM", "chud-app")]
            .map(|(k, v)| (k.to_string(), v.to_string()));
        let settings = Settings {
            font: font(),
            backend: BackendSettings {
                program: shell,
                args,
                env: HashMap::from(env),
                working_directory: std::env::var_os("HOME").map(Into::into), // Dock apps start in /
            },
            ..Default::default()
        };
        let mut term = iced_term::Terminal::new(0, settings).expect("could not start chud's terminal");

        // Paste is handled in update(): the built-in paste sends the text raw, which would
        // submit a multi-line paste to the agent line by line. The keys are bound to "write
        // nothing" so they're swallowed: with no binding (Ignore) iced_term types the "v".
        // COMMAND is Cmd on macOS and Ctrl on Windows/Linux; the Shift variant is covered too.
        let paste_keys = [keyboard::Modifiers::COMMAND, keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT]
            .map(|modifiers| {
                let key = Binding {
                    target: InputKind::Char("v".into()),
                    modifiers,
                    terminal_mode_include: TermMode::empty(),
                    terminal_mode_exclude: TermMode::empty(),
                };
                (key, BindingAction::Esc(String::new()))
            });
        term.handle(Command::AddBindings(paste_keys.into()));

        // Apple's symbol font covers ⎿ ⎯ ⧉; it can't be bundled, so load it from the system.
        let symbols = match std::fs::read("/System/Library/Fonts/Apple Symbols.ttf") {
            Ok(bytes) => iced::font::load(bytes).map(|_| Message::FontLoaded),
            Err(_) => Task::none(),
        };
        let focus = TerminalView::focus(term.widget_id().clone());
        (Self { term, cursor: Point::ORIGIN, scroll_px: 0.0 }, Task::batch([focus, symbols]))
    }

    /// The 1-based terminal cell under the pointer.
    fn cell_at(&self, cell: Size) -> (u32, u32) {
        ((self.cursor.x / cell.width) as u32 + 1, (self.cursor.y / cell.height) as u32 + 1)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let write = |bytes: Vec<u8>| Command::ProxyToBackend(BackendCommand::Write(bytes));
        match message {
            Message::Terminal(iced_term::Event::BackendCall(_, cmd)) => {
                if matches!(self.term.handle(Command::ProxyToBackend(cmd)), Action::Shutdown) {
                    return iced::exit(); // chud quit
                }
            }
            Message::Paste => return iced::clipboard::read().map(Message::Pasted),
            Message::Pasted(Some(text)) => {
                // chud turns on bracketed paste; the markers make the whole paste one input
                self.term.handle(write([b"\x1b[200~", text.as_bytes(), b"\x1b[201~"].concat()));
            }
            Message::Pasted(None) => {}
            // chud asks for focus reports; iced_term doesn't send them, so the window does
            Message::Focus(focused) => {
                self.term.handle(write(if focused { b"\x1b[I" } else { b"\x1b[O" }.to_vec()));
            }
            Message::FontLoaded => {
                self.term.handle(Command::ChangeFont(font())); // redraw with the new fallback
            }
            // iced_term only re-measures when an input event reaches it, so a window that settles
            // its size after start-up would keep a tiny terminal until you touched it. Push the
            // size ourselves: the font first (sets the cell size), then the layout.
            Message::Resized(size) => {
                self.term.handle(Command::ChangeFont(font()));
                self.term.handle(Command::ProxyToBackend(BackendCommand::Resize(Some(size), None)));
            }
            Message::Cursor(position) => self.cursor = position,
            // iced_term passes on only the left button; send right-clicks to chud ourselves,
            // as a press and release in SGR mouse encoding at the cell under the pointer
            Message::RightClick => {
                let (col, row) = self.cell_at(cell_size());
                self.term.handle(write(format!("\x1b[<2;{col};{row}M\x1b[<2;{col};{row}m").into_bytes()));
            }
            // iced_term never reports the wheel (it scrolls, or types arrow keys in full-screen
            // apps); send chud real wheel reports instead, counted in lines like iced_term does
            Message::Wheel(delta) => {
                let cell = cell_size();
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => y.round() as i32,
                    mouse::ScrollDelta::Pixels { y, .. } => {
                        self.scroll_px -= y;
                        let n = (self.scroll_px / cell.height).trunc();
                        self.scroll_px -= n * cell.height;
                        n as i32
                    }
                };
                if lines != 0 {
                    let (col, row) = self.cell_at(cell);
                    let button = if lines > 0 { 64 } else { 65 }; // wheel up / down
                    let report = format!("\x1b[<{button};{col};{row}M");
                    self.term.handle(write(report.repeat(lines.unsigned_abs().min(10) as usize).into_bytes()));
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        TerminalView::show(&self.term).map(Message::Terminal)
    }

    fn subscription(&self) -> Subscription<Message> {
        let window_events = event::listen_with(|event, _status, _window| match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key: Key::Character(c), modifiers, .. })
                if modifiers.command() && c.eq_ignore_ascii_case("v") =>
            {
                Some(Message::Paste)
            }
            Event::Window(window::Event::Opened { size, .. } | window::Event::Resized(size)) => {
                Some(Message::Resized(size))
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => Some(Message::Cursor(position)),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => Some(Message::RightClick),
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => Some(Message::Wheel(delta)),
            Event::Window(window::Event::Focused) => Some(Message::Focus(true)),
            Event::Window(window::Event::Unfocused) => Some(Message::Focus(false)),
            _ => None,
        });
        Subscription::batch([self.term.subscription().map(Message::Terminal), window_events])
    }
}
