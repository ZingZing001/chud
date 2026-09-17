//! ~/.config/chud/config.json: your choices (theme, mascot style, extra agents), kept apart
//! from state.json — the layout chud rewrites constantly — so neither can clobber the other.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/chud/config.json")
}

/// What you chose, or `{}` when there is no config yet or it cannot be read: every setting has a
/// default, so a broken file costs you your choices, never a working chud.
pub fn load() -> Value {
    let text = std::fs::read_to_string(path()).ok();
    text.and_then(|t| serde_json::from_str(&t).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}))
}

pub fn exists() -> bool {
    path().exists()
}

pub fn save(v: &Value) {
    write_atomic(&path(), &(serde_json::to_string_pretty(v).unwrap_or_default() + "\n"));
}

/// A setting as text: the environment wins (CHUD_THEME, CHUD_MASCOT — handy for trying things),
/// then the config file.
pub fn setting(cfg: &Value, key: &str, env: &str) -> Option<String> {
    std::env::var(env).ok().or_else(|| cfg[key].as_str().map(String::from))
}

/// Writes a file in one step, a temporary file renamed over it: several Claude sessions run the
/// status line at once, and a half-written config must never be what chud reads.
pub fn write_atomic(path: &Path, text: &str) {
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(path));
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
