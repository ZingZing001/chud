//! Keeps chud current with the repo it was built from. It checks whether the branch it was
//! built from has moved on GitHub, and if it has, pulls and rebuilds in the background, then
//! says so — the new build is picked up when chud restarts (About has a button for that).
//! It looks every half hour, and whenever you open About.
//!
//! The repo path and commit are baked in by bundle.sh; a build without them can't update.
use crate::session::Event;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use std::time::Duration;

/// How often to look; GitHub is not going anywhere.
const EVERY: Duration = Duration::from_secs(30 * 60);

/// How long after starting (and after each wait) before looking: chud's own start-up, with
/// every restored agent launching, is the worst moment to add a fetch and a build.
const SETTLE: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    /// asking GitHub
    Checking,
    /// what is running is the newest there is
    Current,
    /// pulling and building this commit: a few minutes of a full optimised build
    Installing(String),
    /// pulled, rebuilt and installed: this commit is waiting for a restart
    Ready(String),
    Failed(String),
    /// this build can't update itself, and why
    Unavailable(String),
}

/// A check is running: a second one waits for it rather than building the same thing twice.
static BUSY: AtomicBool = AtomicBool::new(false);
/// The commit installed since this chud started, if any: it is not built again.
static INSTALLED: Mutex<Option<String>> = Mutex::new(None);

/// The commit this chud was built from, short, if bundle.sh said.
pub fn built() -> Option<&'static str> {
    option_env!("CHUD_COMMIT").map(|c| &c[..7.min(c.len())])
}

/// The built commit's date and subject, for About: "2026-11-19 · Keep scrolling while…".
pub fn describe() -> Option<String> {
    let (repo, commit) = (repo()?, option_env!("CHUD_COMMIT")?);
    git(&repo, &["log", "-1", "--format=%cd · %s", "--date=short", commit])
}

/// Where the repo it updates from is, for About.
pub fn source() -> Option<PathBuf> {
    repo()
}

fn short(commit: &str) -> String {
    commit[..7.min(commit.len())].to_string()
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The repo this build came from, if it is still there.
pub fn repo() -> Option<PathBuf> {
    let path = PathBuf::from(option_env!("CHUD_REPO")?);
    path.join(".git").exists().then_some(path)
}

/// What the branch points at on the remote now, if that is not `built`. An error when GitHub
/// can't be asked: no network, or the repo has no branch checked out.
pub fn pending(repo: &Path, built: &str) -> Result<Option<String>, String> {
    let branch = git(repo, &["symbolic-ref", "--short", "HEAD"]).ok_or("the repo has no branch checked out")?;
    git(repo, &["fetch", "--quiet", "origin", &branch]).ok_or("couldn't reach GitHub")?;
    let head = git(repo, &["rev-parse", &format!("origin/{branch}")]).ok_or("couldn't read the remote branch")?;
    Ok((head != built && !head.is_empty()).then_some(head))
}

/// Pull it and rebuild. Only fast-forwards: local work in the repo is never touched.
pub fn install(repo: &Path) -> Result<(), String> {
    git(repo, &["pull", "--ff-only", "--quiet"]).ok_or("could not fast-forward the repo")?;
    // at the lowest priority: it is a full optimised build, minutes of every core, and it
    // starts by itself while you are working
    let built = Command::new("nice").args(["-n", "19", "sh", "bundle.sh"]).current_dir(repo).output().map_err(|e| e.to_string())?;
    match built.status.success() {
        true => Ok(()),
        false => Err(String::from_utf8_lossy(&built.stderr).lines().last().unwrap_or("build failed").into()),
    }
}

/// Why this chud can't update itself, if it can't.
fn unable() -> Option<String> {
    if cfg!(windows) {
        return Some("chud updates itself on macOS only for now: pull the repo and rebuild.".into());
    }
    match (option_env!("CHUD_REPO"), option_env!("CHUD_COMMIT"), repo()) {
        (Some(_), Some(_), Some(_)) => None,
        (Some(path), Some(_), None) => Some(format!("it was built from {path}, which isn't there any more.")),
        _ => Some("it wasn't built by bundle.sh, so it doesn't know where it came from.".into()),
    }
}

/// Looks for a newer chud and installs it, saying how it is going at each step. What the
/// watcher does every half hour, and opening About does at once. `quiet` leaves out the steps
/// that are no news (checking, nothing new, no network) for the watcher. False once nobody is
/// listening.
pub fn check(tx: &Sender<Event>, quiet: bool) -> bool {
    let say = |u: Update| tx.send(Event::Updated(u)).is_ok();
    if let Some(why) = unable() {
        return quiet || say(Update::Unavailable(why));
    }
    let (Some(repo), Some(built)) = (repo(), option_env!("CHUD_COMMIT")) else { return true };
    if BUSY.swap(true, Ordering::AcqRel) {
        return true; // the check already running says how it went
    }
    let alive = (quiet || say(Update::Checking)) && {
        // what is installed now: this build, or one put in since it started
        let have = INSTALLED.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or(built.to_string());
        match pending(&repo, &have) {
            Err(_) if quiet => true,
            Err(why) => say(Update::Failed(why)),
            Ok(None) if have != built => say(Update::Ready(short(&have))),
            Ok(None) => quiet || say(Update::Current),
            Ok(Some(head)) => {
                say(Update::Installing(short(&head)))
                    && say(match install(&repo) {
                        Ok(()) => {
                            *INSTALLED.lock().unwrap_or_else(|e| e.into_inner()) = Some(head.clone());
                            Update::Ready(short(&head))
                        }
                        Err(e) => Update::Failed(e),
                    })
            }
        }
    };
    BUSY.store(false, Ordering::Release);
    alive
}

/// Checks in the background every half hour until chud exits, starting a while after chud
/// does. Silent when there is nothing to do, and when there is no network: an update that
/// cannot be fetched is not news.
pub fn watch(tx: Sender<Event>) {
    if unable().is_some() || std::env::var_os("CHUD_NO_UPDATE").is_some() {
        return;
    }
    std::thread::spawn(move || loop {
        std::thread::sleep(SETTLE);
        if !check(&tx, true) {
            return;
        }
        std::thread::sleep(EVERY - SETTLE);
    });
}

/// Checks now, in the background (opening About).
pub fn check_now(tx: &Sender<Event>) {
    let tx = tx.clone();
    std::thread::spawn(move || check(&tx, false));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote that moves ahead is picked up; one that has not is not.
    #[test]
    fn spots_new_commits() {
        let tmp = std::env::temp_dir().join(format!("chud-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (origin, clone) = (tmp.join("origin"), tmp.join("clone"));
        std::fs::create_dir_all(&origin).unwrap();
        let sh = |dir: &Path, cmd: &str| {
            Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output().unwrap();
        };
        sh(&origin, "git init -q -b main && git config user.email t@t && git config user.name t");
        sh(&origin, "echo one > f && git add f && git commit -qm one");
        sh(&tmp, &format!("git clone -q {} {}", origin.display(), clone.display()));
        let built = git(&clone, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(pending(&clone, &built), Ok(None), "nothing pushed yet");

        sh(&origin, "echo two > f && git commit -qam two");
        let head = git(&origin, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(pending(&clone, &built), Ok(Some(head)), "the branch moved on the remote");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
