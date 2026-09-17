//! Agents beyond Claude and Copilot: Codex, Gemini CLI, aider, your own harness. chud knows a
//! few by name, and config.json's "agents" adds more or changes these. Each gets an icon, a
//! colour, a chud and a status — the agent's own signals when it sends any, otherwise inferred
//! from its activity (see `Activity`). Usage and context stay Claude- and Copilot-only: their
//! logs have known formats, a custom harness's does not.
use crate::session::Status;
use ratatui::style::Color;
use serde_json::Value;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub name: String,
    /// program names that mean this agent, matched against the executable and its command line
    pub matches: Vec<String>,
    pub icon: String,
    pub color: Color,
    /// set for sessions chud starts with this agent
    pub env: Vec<(String, String)>,
    /// "status": "auto" (the default) infers working/done from activity; "none" never guesses
    pub infer_status: bool,
}

fn builtin(name: &str, icon: &str, color: Color) -> Profile {
    let matches = vec![name.to_string()];
    Profile { name: name.into(), matches, icon: icon.into(), color, env: vec![], infer_status: true }
}

pub fn builtins() -> Vec<Profile> {
    vec![
        builtin("codex", "◈", Color::Rgb(0x10, 0xa3, 0x7f)),
        builtin("gemini", "✦", Color::Rgb(0x42, 0x85, 0xf4)),
        builtin("aider", "◇", Color::Rgb(0xe5, 0xc0, 0x7b)),
        builtin("opencode", "◎", Color::Rgb(0x7a, 0xa2, 0xf7)),
        builtin("amp", "✶", Color::Rgb(0xf9, 0x73, 0x16)),
    ]
}

/// The built-ins, then config.json's "agents": an entry named like a built-in replaces it, any
/// other name is added. A field that is missing or malformed falls back to a default rather
/// than dropping the agent — a typo in a colour should not make your harness disappear.
pub fn from_config(cfg: &Value) -> Vec<Profile> {
    let mut all = builtins();
    for entry in cfg["agents"].as_array().into_iter().flatten() {
        let Some(name) = entry["name"].as_str().filter(|n| !n.is_empty()) else { continue };
        let strings = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect());
        let profile = Profile {
            name: name.into(),
            matches: strings(&entry["match"]).filter(|m: &Vec<String>| !m.is_empty()).unwrap_or_else(|| vec![name.into()]),
            icon: entry["icon"].as_str().unwrap_or("●").into(),
            color: entry["color"].as_str().and_then(hex).unwrap_or(Color::Gray),
            env: entry["env"]
                .as_object()
                .map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
                .unwrap_or_default(),
            infer_status: entry["status"].as_str() != Some("none"),
        };
        match all.iter_mut().find(|p| p.name == profile.name) {
            Some(existing) => *existing = profile,
            None => all.push(profile),
        }
    }
    all
}

fn hex(s: &str) -> Option<Color> {
    let s = s.strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(s.get(i..i + 2)?, 16).ok();
    (s.len() == 6).then_some(())?;
    Some(Color::Rgb(byte(0)?, byte(2)?, byte(4)?))
}

static PROFILES: OnceLock<Vec<Profile>> = OnceLock::new();

/// Called once at start-up, before any session is classified.
pub fn init(cfg: &Value) {
    let _ = PROFILES.set(from_config(cfg));
}

pub fn all() -> &'static [Profile] {
    PROFILES.get_or_init(builtins)
}

/// A program that runs agents written in its language (Gemini CLI and amp are Node scripts,
/// aider is Python): the process in front is the interpreter, so the agent's name is in the
/// command line rather than the executable path.
pub fn is_interpreter(name: &str) -> bool {
    matches!(name, "node" | "bun" | "deno" | "ruby") || name.starts_with("python")
}

/// Output this soon after Enter is the key's own echo, not the agent working.
const ECHO: Duration = Duration::from_millis(300);
/// This long without output after working means the agent is done.
const QUIET: Duration = Duration::from_secs(3);

/// Status for an agent that reports none of its own. Output that keeps coming after you press
/// Enter means working; going quiet after that means done. Anchoring on your Enter is what
/// keeps a spinner or a prompt redraw at start-up from reading as work.
#[derive(Default)]
pub struct Activity {
    submitted: Option<Instant>,
    last_output: Option<Instant>,
    guessing: bool,
}

impl Activity {
    pub fn submit(&mut self, now: Instant) {
        self.submitted = Some(now);
    }

    /// Output arrived; returns the status it implies when that differs from `current`.
    pub fn output(&mut self, now: Instant, current: Status) -> Option<Status> {
        self.last_output = Some(now);
        let after_echo = self.submitted.is_some_and(|t| now.duration_since(t) >= ECHO);
        let idle = matches!(current, Status::Idle | Status::Done | Status::NeedsInput);
        (after_echo && idle).then(|| {
            self.guessing = true;
            Status::Working
        })
    }

    /// Checked every tick; returns Done once work this struct guessed has gone quiet. Work the
    /// agent reported itself, or a question it asked, is left alone.
    pub fn quiet(&mut self, now: Instant, current: Status) -> Option<Status> {
        let quiet = self.last_output.is_some_and(|t| now.duration_since(t) >= QUIET);
        (self.guessing && current == Status::Working && quiet).then(|| {
            (self.guessing, self.submitted) = (false, None);
            Status::Done
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_adds_and_overrides() {
        let cfg = json!({ "agents": [
            { "name": "codex", "icon": "C", "color": "#112233" },
            { "name": "mine", "match": ["my-harness", "mh"], "status": "none", "env": { "FOO": "bar" } },
            { "name": "sloppy", "color": "not a colour", "match": [] },
            { "icon": "no name, skipped" },
        ]});
        let all = from_config(&cfg);
        let get = |n: &str| all.iter().find(|p| p.name == n).cloned().unwrap();
        assert_eq!(all.iter().filter(|p| p.name == "codex").count(), 1, "replaced, not duplicated");
        assert_eq!((get("codex").icon.as_str(), get("codex").color), ("C", Color::Rgb(0x11, 0x22, 0x33)));
        assert_eq!(get("mine").matches, ["my-harness", "mh"]);
        assert!(!get("mine").infer_status);
        assert_eq!(get("mine").env, [("FOO".to_string(), "bar".to_string())]);
        assert_eq!((get("sloppy").color, get("sloppy").matches.clone()), (Color::Gray, vec!["sloppy".to_string()]));
        assert_eq!(all.len(), builtins().len() + 2);
        assert_eq!(from_config(&json!({})), builtins());
    }

    #[test]
    fn activity_reads_work_from_output_after_enter() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut a = Activity::default();
        assert_eq!(a.output(at(0), Status::Idle), None, "start-up chatter before any Enter is not work");
        a.submit(at(1000));
        assert_eq!(a.output(at(1010), Status::Idle), None, "the Enter's own echo");
        assert_eq!(a.output(at(1500), Status::Idle), Some(Status::Working));
        assert_eq!(a.quiet(at(3000), Status::Working), None, "still inside the quiet window");
        assert_eq!(a.quiet(at(4600), Status::Working), Some(Status::Done));
        assert_eq!(a.output(at(5000), Status::Done), None, "a redraw after done is not new work");

        a.submit(at(6000));
        a.output(at(6500), Status::Done);
        assert_eq!(a.quiet(at(10_000), Status::NeedsInput), None, "a question stays a question");
        a.submit(at(11_000));
        assert_eq!(a.output(at(11_500), Status::NeedsInput), Some(Status::Working), "answered, back to work");
    }
}
