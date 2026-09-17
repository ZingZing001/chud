//! Keeps chud current with the repo it was built from. It checks whether the branch it was
//! built from has moved on GitHub, and if it has, pulls and rebuilds in the background, then
//! says so — the new build is picked up next time you start chud.
//!
//! The repo path and commit are baked in by bundle.sh; a build without them never checks.
use crate::session::Event;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::time::Duration;

/// How often to look; GitHub is not going anywhere.
const EVERY: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    /// pulled, rebuilt and installed: this commit is waiting for a restart
    Ready(String),
    Failed(String),
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

/// What the branch points at on the remote now, if that is not what we are running.
pub fn pending(repo: &Path, built: &str) -> Option<String> {
    let branch = git(repo, &["symbolic-ref", "--short", "HEAD"])?;
    git(repo, &["fetch", "--quiet", "origin", &branch])?;
    let head = git(repo, &["rev-parse", &format!("origin/{branch}")])?;
    (head != built && !head.is_empty()).then_some(head)
}

/// Pull it and rebuild. Only fast-forwards: local work in the repo is never touched.
pub fn install(repo: &Path) -> Result<(), String> {
    git(repo, &["pull", "--ff-only", "--quiet"]).ok_or("could not fast-forward the repo")?;
    let built = Command::new("sh").arg("bundle.sh").current_dir(repo).output().map_err(|e| e.to_string())?;
    match built.status.success() {
        true => Ok(()),
        false => Err(String::from_utf8_lossy(&built.stderr).lines().last().unwrap_or("build failed").into()),
    }
}

/// Watches for new commits until chud exits. Silent when there is nothing to do, and when
/// there is no network: an update that cannot be fetched is not news.
pub fn watch(tx: Sender<Event>) {
    let (Some(repo), Some(built)) = (repo(), option_env!("CHUD_COMMIT")) else { return };
    if cfg!(windows) {
        return; // bundle.sh builds a macOS app; there is no Windows install to update yet
    }
    if std::env::var_os("CHUD_NO_UPDATE").is_some() {
        return;
    }
    std::thread::spawn(move || loop {
        if let Some(head) = pending(&repo, built) {
            let msg = match install(&repo) {
                Ok(()) => Update::Ready(head[..7.min(head.len())].to_string()),
                Err(e) => Update::Failed(e),
            };
            if tx.send(Event::Updated(msg)).is_err() {
                return;
            }
        }
        std::thread::sleep(EVERY);
    });
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
        assert_eq!(pending(&clone, &built), None, "nothing pushed yet");

        sh(&origin, "echo two > f && git commit -qam two");
        let head = git(&origin, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(pending(&clone, &built), Some(head), "the branch moved on the remote");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
