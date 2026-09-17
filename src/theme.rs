//! chud's own colours, dark and light. Only chud's chrome is painted — the bars, buttons,
//! highlights and badges; what runs inside a session keeps the terminal's own colours.
use ratatui::style::Color;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

pub struct Palette {
    /// highlights, the brand badge, a focused border
    pub accent: Color,
    /// text on a filled accent
    pub on_accent: Color,
    /// secondary text and borders
    pub dim: Color,
    /// the toolbar, status bar and header strips, and the text on them
    pub bar_bg: Color,
    pub bar_fg: Color,
    pub button_bg: Color,
    pub selected_bg: Color,
    pub drop_bg: Color,
    /// the empty part of a progress bar
    pub track: Color,
    /// the activity grid: an hour with no work, the quietest hour with some, the busiest
    pub heat_empty: Color,
    pub heat_low: (u8, u8, u8),
    pub heat_high: (u8, u8, u8),
    pub claude: Color,
    pub copilot: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

pub const DARK: Palette = Palette {
    accent: rgb(0xff, 0xc2, 0x7a),
    on_accent: Color::Black,
    dim: rgb(0x6c, 0x6c, 0x78),
    bar_bg: rgb(0x1e, 0x1e, 0x26),
    bar_fg: rgb(0xe6, 0xe6, 0xea),
    button_bg: rgb(0x2e, 0x2e, 0x38),
    selected_bg: rgb(0x2c, 0x2a, 0x40),
    drop_bg: rgb(0x3b, 0x3b, 0x5c),
    track: rgb(0x33, 0x33, 0x3a),
    heat_empty: rgb(0x26, 0x26, 0x2e),
    heat_low: (0x4a, 0x3a, 0x2a),
    heat_high: (0xff, 0xc2, 0x7a),
    claude: rgb(0xd9, 0x77, 0x57),
    copilot: rgb(0xa3, 0x71, 0xf7),
};

/// Same roles, darker inks: an accent that holds up as a border on white, secondary text that
/// still passes contrast, and an activity grid that deepens rather than lightens with work.
pub const LIGHT: Palette = Palette {
    accent: rgb(0xe8, 0x8a, 0x2e),
    on_accent: Color::Black,
    dim: rgb(0x6a, 0x6a, 0x76),
    bar_bg: rgb(0xec, 0xec, 0xf0),
    bar_fg: rgb(0x2a, 0x2a, 0x33),
    button_bg: rgb(0xdc, 0xdc, 0xe3),
    selected_bg: rgb(0xe6, 0xe1, 0xf5),
    drop_bg: rgb(0xd2, 0xd6, 0xf4),
    track: rgb(0xdd, 0xdd, 0xe3),
    heat_empty: rgb(0xec, 0xec, 0xf0),
    heat_low: (0xf6, 0xdd, 0xc0),
    heat_high: (0xd9, 0x72, 0x1c),
    claude: rgb(0xc1, 0x5f, 0x3c),
    copilot: rgb(0x82, 0x50, 0xd8),
};

// An atomic rather than a OnceLock: the theme can change while chud runs (the system flips to
// dark at sunset, or you are trying one out), and a colour read costs one relaxed load.
static LIGHT_ON: AtomicBool = AtomicBool::new(false);

pub fn set_light(on: bool) {
    LIGHT_ON.store(on, Relaxed);
}

pub fn is_light() -> bool {
    LIGHT_ON.load(Relaxed)
}

pub fn p() -> &'static Palette {
    if is_light() { &LIGHT } else { &DARK }
}

/// Light or dark, asking whoever knows best first: what you chose, then what the terminal says
/// its background is (COLORFGBG, set by iTerm, rxvt and others), then the desktop's appearance.
pub fn choose(chosen: Option<&str>, colorfgbg: Option<&str>) -> bool {
    match chosen {
        Some("light") => true,
        Some("dark") => false,
        _ => colorfgbg.and_then(colorfgbg_light).or_else(system_light).unwrap_or(false),
    }
}

/// "15;0" is light text on a dark background: the last field is the background's colour index.
fn colorfgbg_light(v: &str) -> Option<bool> {
    let bg: u8 = v.rsplit(';').next()?.trim().parse().ok()?;
    Some(matches!(bg, 7 | 15))
}

/// What the desktop's appearance is set to, when the platform can say.
pub fn system_light() -> Option<bool> {
    let run = |cmd: &str, args: &[&str]| {
        let out = std::process::Command::new(cmd).args(args).output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    if cfg!(target_os = "macos") {
        run("defaults", &["read", "-g", "AppleInterfaceStyle"]).map(|out| macos_light(&out))
    } else if cfg!(windows) {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
        windows_light(&run("reg", &["query", key, "/v", "AppsUseLightTheme"])?)
    } else {
        gnome_light(&run("gsettings", &["get", "org.gnome.desktop.interface", "color-scheme"])?)
    }
}

/// The key only exists while dark mode is on, so "does not exist" (no output) means light.
fn macos_light(out: &str) -> bool {
    !out.trim().eq_ignore_ascii_case("dark")
}

fn windows_light(out: &str) -> Option<bool> {
    let line = out.lines().find(|l| l.contains("AppsUseLightTheme"))?;
    match line.split_whitespace().last()? {
        "0x1" => Some(true),
        "0x0" => Some(false),
        _ => None,
    }
}

fn gnome_light(out: &str) -> Option<bool> {
    match out.trim().trim_matches('\'') {
        "prefer-dark" => Some(false),
        "prefer-light" | "default" => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_each_platform() {
        assert!(!macos_light("Dark\n"));
        assert!(macos_light(""), "the key is absent in light mode");

        let reg = "\r\nHKEY_CURRENT_USER\\Software\\...\\Personalize\r\n    AppsUseLightTheme    REG_DWORD    0x0\r\n";
        assert_eq!(windows_light(reg), Some(false));
        assert_eq!(windows_light(&reg.replace("0x0", "0x1")), Some(true));
        assert_eq!(windows_light("ERROR: The system was unable to find the specified registry key"), None);

        assert_eq!(gnome_light("'prefer-dark'\n"), Some(false));
        assert_eq!(gnome_light("'default'\n"), Some(true));
        assert_eq!(gnome_light(""), None, "not GNOME: no opinion");

        assert_eq!(colorfgbg_light("15;0"), Some(false), "light text on black");
        assert_eq!(colorfgbg_light("0;15"), Some(true));
        assert_eq!(colorfgbg_light("0;default;7"), Some(true), "rxvt's three-field form");
        assert_eq!(colorfgbg_light("nonsense"), None);
    }

    #[test]
    fn a_choice_beats_every_guess() {
        assert!(choose(Some("light"), Some("15;0")), "chosen light wins over a dark terminal");
        assert!(!choose(Some("dark"), Some("0;15")));
        assert!(choose(None, Some("0;15")), "then the terminal's own background");
        assert!(!choose(Some("auto"), Some("15;0")));
    }
}
