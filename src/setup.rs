//! The first-start walkthrough's outside-world half: Claude Code's status line (the one file
//! outside chud it edits, only when you say so), and read-only checks for the warp plugin, gh
//! and which agents are installed. The overlay itself lives in main.rs and ui.rs.
use crate::config;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What `~/.claude/settings.json` says about its status line.
#[derive(Clone, Debug, PartialEq)]
pub enum StatusLine {
    /// already runs chud: nothing to do
    Ours,
    /// runs something else, shown so you can decide
    Foreign(String),
    Absent,
}

pub fn settings_path(home: &Path) -> PathBuf {
    home.join(".claude/settings.json")
}

pub fn status_line(settings: &Value) -> StatusLine {
    match settings["statusLine"]["command"].as_str() {
        None => StatusLine::Absent,
        // any chud counts, wherever it is installed: "~/Applications/…/chud --statusline"
        Some(cmd) if is_chud(cmd) => StatusLine::Ours,
        Some(cmd) => StatusLine::Foreign(cmd.to_string()),
    }
}

fn is_chud(cmd: &str) -> bool {
    let mut words = cmd.split_whitespace();
    let program = words.next().unwrap_or("");
    program.rsplit(['/', '\\']).next() == Some("chud") && words.any(|w| w == "--statusline")
}

/// Points Claude Code's status line at `exe`. Reads the settings as JSON and changes only
/// `statusLine`, so every other key survives; copies the original to `settings.json.bak` before
/// touching it; and never replaces someone else's status line unless `replace` says so.
/// Returns what happened, in words for the walkthrough.
pub fn enable_status_line(home: &Path, exe: &str, replace: bool) -> Result<String, String> {
    let path = settings_path(home);
    let original = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    let mut settings: Value = match &original {
        Some(text) => serde_json::from_str(text)
            .map_err(|e| format!("{} is not valid JSON ({e}), so chud left it alone", path.display()))?,
        None => json!({}),
    };
    if !settings.is_object() {
        return Err(format!("{} is not a JSON object, so chud left it alone", path.display()));
    }
    match status_line(&settings) {
        StatusLine::Ours => return Ok("Already on: Claude Code reports to chud.".into()),
        StatusLine::Foreign(cmd) if !replace => return Err(format!("kept your status line ({cmd})")),
        _ => {}
    }
    if let Some(text) = &original {
        let backup = path.with_extension("json.bak");
        std::fs::write(&backup, text).map_err(|e| format!("could not back up to {}: {e}", backup.display()))?;
    }
    settings["statusLine"] = json!({ "type": "command", "command": format!("{exe} --statusline") });
    let text = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())? + "\n";
    config::write_atomic(&path, &text);
    Ok(match original {
        Some(_) => "Turned on. The old settings are in settings.json.bak.".into(),
        None => "Turned on (created ~/.claude/settings.json).".into(),
    })
}

/// A check the walkthrough runs in the background and shows when it lands.
#[derive(Clone, Debug, PartialEq)]
pub enum Check {
    Running,
    Ok(String),
    Missing(String),
}

/// `claude plugin list` output mentions the warp plugin, enabled.
pub fn warp_installed(plugin_list: &str) -> bool {
    let Some(at) = plugin_list.find("warp@claude-code-warp") else { return false };
    let entry = plugin_list[at..].lines().take(5).collect::<Vec<_>>().join("\n");
    entry.contains("enabled") && !entry.contains("disabled")
}

pub fn check_warp() -> Check {
    match Command::new("claude").args(["plugin", "list"]).output() {
        Err(_) => Check::Missing("Claude Code isn't installed, so there is nothing to check.".into()),
        Ok(out) if warp_installed(&String::from_utf8_lossy(&out.stdout)) => {
            Check::Ok("claude-code-warp is installed: chud can see Claude working.".into())
        }
        Ok(_) => Check::Missing("claude-code-warp isn't installed.".into()),
    }
}

pub fn check_gh() -> Check {
    match Command::new("gh").args(["auth", "status"]).output() {
        Err(_) => Check::Missing("gh isn't installed: Copilot's quota won't show (everything else works).".into()),
        Ok(out) if out.status.success() => Check::Ok("gh is logged in: Copilot's monthly quota will show.".into()),
        Ok(_) => Check::Missing("gh isn't logged in: run `gh auth login` to see Copilot's quota.".into()),
    }
}

/// Which of these programs are on your PATH.
pub fn installed(names: &[&str]) -> Vec<(String, bool)> {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    names.iter().map(|n| (n.to_string(), dirs.iter().any(|d| d.join(n).is_file()))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("chud-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        home
    }

    /// Shaped like a real settings file: hooks, plugins, a permission block, a theme.
    const SETTINGS: &str = r#"{
  "permissions": { "defaultMode": "auto" },
  "model": "opus",
  "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "rtk hook claude" } ] } ] },
  "enabledPlugins": { "warp@claude-code-warp": true },
  "theme": "dark-ansi"
}"#;

    #[test]
    fn recognises_whose_status_line_it_is() {
        let with = |cmd: &str| json!({ "statusLine": { "type": "command", "command": cmd } });
        assert_eq!(status_line(&with("~/Applications/chud.app/Contents/MacOS/chud --statusline")), StatusLine::Ours);
        assert_eq!(status_line(&with("/Users/me/.cargo/bin/chud --statusline")), StatusLine::Ours);
        assert_eq!(status_line(&with("ccstatusline")), StatusLine::Foreign("ccstatusline".into()));
        assert_eq!(status_line(&with("chudder --statusline")), StatusLine::Foreign("chudder --statusline".into()));
        assert_eq!(status_line(&json!({ "model": "opus" })), StatusLine::Absent);
    }

    #[test]
    fn turning_it_on_keeps_everything_else() {
        let home = temp_home("keep");
        let path = settings_path(&home);
        std::fs::write(&path, SETTINGS).unwrap();

        let said = enable_status_line(&home, "/opt/chud", false).unwrap();
        assert!(said.contains("settings.json.bak"), "{said}");
        let before: Value = serde_json::from_str(SETTINGS).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for (key, value) in before.as_object().unwrap() {
            assert_eq!(&after[key], value, "{key} survived untouched");
        }
        assert_eq!(after["statusLine"]["command"], "/opt/chud --statusline");
        assert_eq!(std::fs::read_to_string(path.with_extension("json.bak")).unwrap(), SETTINGS, "backup is the original");

        let again = enable_status_line(&home, "/somewhere/else/chud", false).unwrap();
        assert!(again.starts_with("Already on"), "a second run changes nothing: {again}");
        assert_eq!(std::fs::read_to_string(path.with_extension("json.bak")).unwrap(), SETTINGS, "and takes no second backup");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn never_replaces_a_foreign_status_line_uninvited() {
        let home = temp_home("foreign");
        let path = settings_path(&home);
        let theirs = r#"{ "statusLine": { "type": "command", "command": "ccstatusline" }, "model": "opus" }"#;
        std::fs::write(&path, theirs).unwrap();

        assert!(enable_status_line(&home, "/opt/chud", false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs, "untouched without permission");
        assert!(!path.with_extension("json.bak").exists());

        enable_status_line(&home, "/opt/chud", true).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((after["statusLine"]["command"].as_str(), after["model"].as_str()), (Some("/opt/chud --statusline"), Some("opus")));
        assert_eq!(std::fs::read_to_string(path.with_extension("json.bak")).unwrap(), theirs);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn leaves_broken_or_missing_files_sensibly() {
        let home = temp_home("broken");
        let path = settings_path(&home);
        std::fs::write(&path, "{ not json").unwrap();
        assert!(enable_status_line(&home, "/opt/chud", true).unwrap_err().contains("not valid JSON"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json", "a file chud can't parse is left alone");

        std::fs::remove_file(&path).unwrap();
        assert!(enable_status_line(&home, "/opt/chud", false).unwrap().contains("created"));
        let made: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(made, json!({ "statusLine": { "type": "command", "command": "/opt/chud --statusline" } }));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn reads_the_plugin_list() {
        let listed = "Installed plugins:\n\n  ❯ ponytail@ponytail\n    Version: 4.9.0\n    Status: ✔ enabled\n\n  ❯ warp@claude-code-warp\n    Version: 2.2.0\n    Scope: user\n    Status: ✔ enabled\n";
        assert!(warp_installed(listed));
        assert!(!warp_installed(&listed.replace("warp@claude-code-warp\n    Version: 2.2.0\n    Scope: user\n    Status: ✔ enabled", "warp@claude-code-warp\n    Status: ✘ disabled")));
        assert!(!warp_installed("Installed plugins:\n\n  ❯ ponytail@ponytail\n    Status: ✔ enabled\n"));
    }
}
