use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) -> io::Result<Output> {
    Command::new("git").arg("-C").arg(dir).args(args).output()
}

fn text(o: io::Result<Output>) -> Result<String, String> {
    match o {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => Err(e.to_string()),
    }
}

pub fn root(cwd: &Path) -> Result<PathBuf, String> {
    text(git(cwd, &["rev-parse", "--show-toplevel"])).map(|s| PathBuf::from(s.trim()))
}

/// (porcelain status code, path relative to root) for every changed or untracked file.
pub fn changes(root: &Path) -> Vec<(String, String)> {
    let out = text(git(root, &["status", "--porcelain", "-z", "-uall"])).unwrap_or_default();
    let mut entries = out.split('\0').filter(|e| !e.is_empty());
    let mut files = vec![];
    while let Some(e) = entries.next() {
        let (Some(code), Some(path)) = (e.get(..2), e.get(3..)) else { continue };
        if code.contains(['R', 'C']) {
            entries.next(); // -z puts the rename source in its own entry
        }
        files.push((code.to_string(), path.to_string()));
    }
    files
}

pub fn diff(root: &Path, code: &str, path: &str) -> String {
    if code == "??" {
        // --no-index exits 1 when the files differ, so take stdout regardless
        return git(root, &["diff", "--no-index", "--", "/dev/null", path])
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_else(|e| e.to_string());
    }
    text(git(root, &["diff", "HEAD", "--", path])).unwrap_or_else(|e| e)
}

pub fn commit_all(root: &Path, msg: &str) -> Result<String, String> {
    text(git(root, &["add", "-A"]))?;
    text(git(root, &["commit", "-q", "-m", msg]))?;
    text(git(root, &["log", "-1", "--format=%h %s"])).map(|s| format!("committed {}", s.trim()))
}

pub fn discard(root: &Path, code: &str, path: &str) -> Result<(), String> {
    match code {
        "??" => std::fs::remove_file(root.join(path)).map_err(|e| e.to_string()),
        c if c.starts_with('A') => text(git(root, &["rm", "-qf", "--", path])).map(drop),
        _ => text(git(root, &["restore", "--source=HEAD", "--staged", "--worktree", "--", path]))
            .map(drop),
    }
}
