//! chud.app: a window with a built-in terminal (iced_term, on Alacritty's engine) that runs
//! the chud TUI. Its fonts are bundled, so every icon renders; no other terminal needed.
use iced::advanced::text::{Alignment, LineHeight, Paragraph as _, Shaping, Wrapping};
use iced::advanced::Text;
use iced::keyboard::{self, Key};
use iced::{alignment, event, mouse, window, Element, Event, Font, Pixels, Point, Size, Subscription, Task};
use iced_term::actions::Action;
use iced_term::bindings::{Binding, BindingAction, InputKind};
use iced_term::settings::{BackendSettings, FontSettings, Settings};
use iced_term::{BackendCommand, ColorPalette, Command, TermMode, TerminalView};
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

const FONT_SIZE: f32 = 13.0;

/// The terminal's colours on a light desktop. Not the dark palette inverted: every ANSI colour
/// is re-picked to read on near-white, since programs print yellow and white text expecting a
/// dark background.
fn light_palette() -> ColorPalette {
    let c = String::from;
    ColorPalette {
        foreground: c("#383a42"),
        background: c("#fafafa"),
        black: c("#383a42"),
        red: c("#d7443a"),
        green: c("#3f8f3e"),
        yellow: c("#a86b00"),
        blue: c("#3769d4"),
        magenta: c("#9b2a99"),
        cyan: c("#0a7fa6"),
        white: c("#8e9098"),
        bright_black: c("#6a6d78"),
        bright_red: c("#e0564b"),
        bright_green: c("#4ea64d"),
        bright_yellow: c("#c28000"),
        bright_blue: c("#4d80ea"),
        bright_magenta: c("#b23cb0"),
        bright_cyan: c("#1994bd"),
        bright_white: c("#5c5f68"),
        bright_foreground: None,
        dim_foreground: c("#6f727c"),
        dim_black: c("#5a5d66"),
        dim_red: c("#a3342c"),
        dim_green: c("#316e30"),
        dim_yellow: c("#7f5100"),
        dim_blue: c("#2a51a4"),
        dim_magenta: c("#762074"),
        dim_cyan: c("#08617f"),
        dim_white: c("#a9abb2"),
    }
}

fn font(size: f32) -> FontSettings {
    // line height = Fira Code's full-block height (2400/1950 em), so ▀▄█ tile with no gaps
    FontSettings { size, scale_factor: 1.231, font_type: Font::with_name("FiraCode Nerd Font") }
}

/// One terminal cell in window pixels, measured the way iced_term does (its font.rs), which
/// then rounds down to whole pixels.
fn cell_size(font_size: f32) -> Size {
    let f = font(font_size);
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
    /// bigger / smaller text; 0.0 back to the default
    FontSize(f32),
    /// a ⌘ shortcut, as the keys chud would have seen
    Send(Vec<u8>),
    /// the system's light or dark appearance, at start-up and whenever it changes
    SystemTheme(iced::theme::Mode),
}

struct App {
    term: iced_term::Terminal,
    cursor: Point,
    scroll_px: f32, // trackpad scrolling not yet worth a whole line
    font_size: f32,
    size: Size, // the window's, so a font change can re-flow the terminal to it
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
            font: font(FONT_SIZE),
            backend: BackendSettings {
                program: shell,
                args,
                env: HashMap::from(env),
                working_directory: std::env::var_os("HOME").map(Into::into), // Dock apps start in /
            },
            ..Default::default()
        };
        let mut term = iced_term::Terminal::new(0, settings).expect("could not start chud's terminal");

        // The ⌘ shortcuts below are all handled in update(); here they're bound to "write
        // nothing" so iced_term swallows them. Without a binding it types the bare character
        // instead (Ignore falls through to the key's text), which is the old Cmd+V "v" bug.
        // COMMAND is Cmd on macOS and Ctrl on Windows/Linux; the Shift variants are covered too.
        let keys: Vec<String> = "cdv=+-_0123456789tw".chars().map(String::from).collect();
        let swallow: Vec<_> = keys
            .iter()
            .flat_map(|c| {
                [keyboard::Modifiers::COMMAND, keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT].map(
                    |modifiers| {
                        let key = Binding {
                            target: InputKind::Char(c.clone()),
                            modifiers,
                            terminal_mode_include: TermMode::empty(),
                            terminal_mode_exclude: TermMode::empty(),
                        };
                        (key, BindingAction::Esc(String::new()))
                    },
                )
            })
            .collect();
        term.handle(Command::AddBindings(swallow));

        // Apple's symbol font covers ⎿ ⎯ ⧉; it can't be bundled, so load it from the system.
        let symbols = match std::fs::read("/System/Library/Fonts/Apple Symbols.ttf") {
            Ok(bytes) => iced::font::load(bytes).map(|_| Message::FontLoaded),
            Err(_) => Task::none(),
        };
        let focus = TerminalView::focus(term.widget_id().clone());
        let system_theme = iced::system::theme().map(Message::SystemTheme);
        let app = Self { term, cursor: Point::ORIGIN, scroll_px: 0.0, font_size: FONT_SIZE, size: Size::ZERO };
        (app, Task::batch([focus, symbols, system_theme]))
    }

    /// Re-measure at the current font size and re-flow the terminal to the window.
    fn relayout(&mut self) {
        self.term.handle(Command::ChangeFont(font(self.font_size)));
        self.term.handle(Command::ProxyToBackend(BackendCommand::Resize(Some(self.size), None)));
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
                self.term.handle(Command::ChangeFont(font(self.font_size))); // redraw with the fallback
            }
            Message::FontSize(step) => {
                self.font_size = if step == 0.0 { FONT_SIZE } else { (self.font_size + step).clamp(8.0, 32.0) };
                self.relayout();
            }
            Message::Send(bytes) => {
                self.term.handle(write(bytes));
            }
            // repaint the terminal in the matching palette, and tell chud (F13 light, F14 dark)
            // so its own bars and badges follow; chud ignores it if you pinned a theme
            Message::SystemTheme(mode) => {
                let light = mode == iced::theme::Mode::Light;
                let palette = if light { light_palette() } else { ColorPalette::default() };
                self.term.handle(Command::ChangeTheme(Box::new(palette)));
                self.term.handle(write(if light { b"\x1b[25~".to_vec() } else { b"\x1b[26~".to_vec() }));
            }
            // iced_term only re-measures when an input event reaches it, so a window that settles
            // its size after start-up would keep a tiny terminal until you touched it. Push the
            // size ourselves: the font first (sets the cell size), then the layout.
            Message::Resized(size) => {
                self.size = size;
                self.relayout();
            }
            Message::Cursor(position) => self.cursor = position,
            // iced_term passes on only the left button; send right-clicks to chud ourselves,
            // as a press and release in SGR mouse encoding at the cell under the pointer
            Message::RightClick => {
                let (col, row) = self.cell_at(cell_size(self.font_size));
                self.term.handle(write(format!("\x1b[<2;{col};{row}M\x1b[<2;{col};{row}m").into_bytes()));
            }
            // iced_term never reports the wheel (it scrolls, or types arrow keys in full-screen
            // apps); send chud real wheel reports instead, counted in lines like iced_term does
            Message::Wheel(delta) => {
                let cell = cell_size(self.font_size);
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
            // ⌘ shortcuts, the ones a terminal app is expected to have. chud's own commands all
            // start with Ctrl-a, so a shortcut is just those keystrokes sent on your behalf.
            Event::Keyboard(keyboard::Event::KeyPressed { key: Key::Character(c), modifiers, .. })
                if modifiers.command() =>
            {
                let prefixed = |k: u8| Some(Message::Send(vec![0x01, k]));
                match c.as_str() {
                    "v" | "V" => Some(Message::Paste),
                    // F15: chud copies what you dragged over (iced_term would copy its own
                    // selection here, which is always empty, wiping the clipboard)
                    "c" | "C" => Some(Message::Send(b"\x1b[32~".to_vec())),
                    "=" | "+" => Some(Message::FontSize(1.0)),
                    "-" | "_" => Some(Message::FontSize(-1.0)),
                    "0" => Some(Message::FontSize(0.0)),
                    "t" | "T" => prefixed(b'n'), // new terminal session
                    "d" => prefixed(b'|'),       // split side by side, as in iTerm
                    "D" => prefixed(b'-'),       // ⌘⇧D: split stacked
                    "w" | "W" => prefixed(b'x'), // kill this one (chud asks first)
                    d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => prefixed(d.as_bytes()[0]),
                    _ => None,
                }
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
        Subscription::batch([
            self.term.subscription().map(Message::Terminal),
            window_events,
            iced::system::theme_changes().map(Message::SystemTheme),
        ])
    }
}
