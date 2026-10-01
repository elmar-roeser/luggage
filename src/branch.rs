//! Service data per git branch: which branch the project has checked out, safe directory names for branches,
//! and cheap copies of the data (reflinks where the file system has them).
//!
//! The chest can write `.git/HEAD`, so luggage reads it without following links and never runs git for it.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use rustix::fs::{Mode, OFlags};

use crate::info;

/// Longest directory name luggage uses for a branch; longer branch names keep the data in use.
const MAX_NAME: usize = 200;

/// What the project has checked out.
#[derive(Debug, PartialEq, Eq)]
pub enum Head {
    /// a branch, by its short name
    Branch(String),
    /// a commit, or a rebase or bisect under way
    Detached,
    /// no git repo, or one luggage cannot read
    None,
}

/// Reads at most 4 KiB of the file `p`, without following a link.
fn read_nofollow(p: &Path) -> Option<String> {
    let fd =
        rustix::fs::open(p, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
            .ok()?;
    let mut text = String::new();
    File::from(fd).take(4096).read_to_string(&mut text).ok()?;
    Some(text)
}

/// The git dir of `project`: `.git` itself, or the dir a `.git` file names (a worktree).
fn git_dir(project: &Path) -> Option<PathBuf> {
    let git = project.join(".git");
    let meta = git.symlink_metadata().ok()?;
    if meta.is_dir() {
        return Some(git);
    }
    if !meta.is_file() {
        return None;
    }
    let text = read_nofollow(&git)?;
    let dir = text.strip_prefix("gitdir:")?.trim();
    Some(project.join(dir))
}

/// What `project` has checked out, read from its `HEAD` file.
pub fn head(project: &Path) -> Head {
    let Some(dir) = git_dir(project) else {
        return Head::None;
    };
    if ["rebase-merge", "rebase-apply", "BISECT_LOG"].iter().any(|m| dir.join(m).exists()) {
        return Head::Detached;
    }
    match read_nofollow(&dir.join("HEAD")) {
        Some(text) => match text.trim().strip_prefix("ref: refs/heads/") {
            // the chest can write HEAD, and luggage prints the name: no control characters for the terminal
            Some(b) if !b.is_empty() && !b.chars().any(char::is_control) => {
                Head::Branch(b.to_owned())
            }
            _ => Head::Detached,
        },
        None => Head::None,
    }
}

/// A directory name for `branch` that keeps branches apart (`a/b` and `a-b`) and can be turned back;
/// `None` if it gets too long.
pub fn dir_name(branch: &str) -> Option<String> {
    let mut out = String::new();
    for (i, b) in branch.bytes().enumerate() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || (b == b'.' && i > 0) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    (!branch.is_empty() && out.len() <= MAX_NAME).then_some(out)
}

/// The branch whose directory is `name`, the reverse of `dir_name`.
pub fn branch_name(name: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut rest = name.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        if b == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(b);
            rest = tail;
        }
    }
    let branch = String::from_utf8(bytes).ok()?;
    (dir_name(&branch).as_deref() == Some(name)).then_some(branch)
}

/// Disk space that `p` takes, with everything below it; links are not followed.
/// Blocks shared by reflinks count for each copy.
pub fn usage(p: &Path) -> u64 {
    let Ok(meta) = p.symlink_metadata() else {
        return 0;
    };
    let own = meta.blocks() * 512;
    if !meta.is_dir() {
        return own;
    }
    own + fs::read_dir(p)
        .into_iter()
        .flatten()
        .filter_map(|e| Some(usage(&e.ok()?.path())))
        .sum::<u64>()
}

/// `bytes` for people, e.g. `12 MB`.
pub fn human(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024 * 10 && unit < units.len() - 1 {
        value /= 1024;
        unit += 1;
    }
    format!("{value} {}", units[unit])
}

/// Removes the file or directory `p` if it exists.
fn remove(p: &Path) -> Result<()> {
    let Ok(meta) = p.symlink_metadata() else {
        return Ok(());
    };
    if meta.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }
        .with_context(|| format!("cannot remove {}", p.display()))
}

/// Copies `from` to `to` with `cp -a <mode>` and tells whether it worked.
/// The copy appears under its name only once it is complete.
fn cp(from: &Path, to: &Path, mode: &str) -> Result<bool> {
    let mut tmp = to.as_os_str().to_owned();
    tmp.push(".copy");
    let tmp = PathBuf::from(tmp);
    remove(&tmp)?;
    let ok = Command::new("cp")
        .args(["-a", mode, "--"])
        .arg(from)
        .arg(&tmp)
        .stderr(Stdio::null())
        .status()
        .context("cannot start cp")?
        .success();
    if ok {
        fs::rename(&tmp, to).with_context(|| format!("cannot rename {}", tmp.display()))?;
    } else {
        remove(&tmp)?;
    }
    Ok(ok)
}

/// Copies `from` (file or directory) to `to` with reflinks; where that fails, in full after a warning.
/// A file without copy-on-write (microvm.nix creates VM disks so) then gets replaced by a clone of the copy, so the next copy is cheap.
///
/// # Errors
/// If `cp` fails.
pub fn copy(from: &Path, to: &Path) -> Result<()> {
    if cp(from, to, "--reflink=always")? {
        return Ok(());
    }
    info(&format!(
        "warning: cannot clone {} (no reflinks on this file system, or the file has copy-on-write off) — copying {} in full",
        from.display(),
        human(usage(from))
    ));
    if !cp(from, to, "--sparse=always")? {
        bail!("cannot copy {} to {}", from.display(), to.display());
    }
    if from.is_file() && cp(to, from, "--reflink=always")? {
        info(&format!("{} has copy-on-write now, so the next copy is cheap", from.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_become_distinct_safe_dir_names() {
        assert_eq!(dir_name("main").as_deref(), Some("main"));
        assert_eq!(dir_name("feature/x").as_deref(), Some("feature%2Fx"));
        assert_eq!(dir_name("feature-x").as_deref(), Some("feature-x"));
        assert_eq!(dir_name(".hidden").as_deref(), Some("%2Ehidden"));
        assert_eq!(dir_name("ä").as_deref(), Some("%C3%A4"));
        assert_eq!(dir_name(""), None);
        assert_eq!(dir_name(&"x".repeat(MAX_NAME + 1)), None);
        for b in ["main", "feature/x", "a b", ".x", "ä/ö", "v1.2"] {
            assert_eq!(branch_name(&dir_name(b).unwrap()).as_deref(), Some(b));
        }
        assert_eq!(branch_name("%2"), None);
        assert_eq!(branch_name("a/b"), None);
        assert_eq!(branch_name("%2Fx%2"), None);
    }

    #[test]
    fn head_reads_branch_detached_and_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert_eq!(head(p), Head::None);
        fs::create_dir(p.join(".git")).unwrap();
        fs::write(p.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(head(p), Head::Branch("feature/x".into()));
        fs::create_dir(p.join(".git/rebase-merge")).unwrap();
        assert_eq!(head(p), Head::Detached);
        fs::remove_dir(p.join(".git/rebase-merge")).unwrap();
        fs::write(p.join(".git/HEAD"), "ref: refs/heads/a\x1b[2Jb\n").unwrap();
        assert_eq!(head(p), Head::Detached);
        fs::write(p.join(".git/HEAD"), "0123abcd\n").unwrap();
        assert_eq!(head(p), Head::Detached);
        // a link planted in place of HEAD is not followed
        fs::remove_file(p.join(".git/HEAD")).unwrap();
        fs::write(p.join("other"), "ref: refs/heads/evil\n").unwrap();
        std::os::unix::fs::symlink(p.join("other"), p.join(".git/HEAD")).unwrap();
        assert_eq!(head(p), Head::None);
        // a worktree: .git is a file that names the git dir
        let wt = p.join("wt");
        fs::create_dir_all(p.join("gd")).unwrap();
        fs::create_dir(&wt).unwrap();
        fs::write(p.join("gd/HEAD"), "ref: refs/heads/topic\n").unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", p.join("gd").display())).unwrap();
        assert_eq!(head(&wt), Head::Branch("topic".into()));
    }

    #[test]
    fn copy_makes_a_complete_copy() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("data");
        fs::create_dir_all(from.join("db")).unwrap();
        fs::write(from.join("db/table"), "rows").unwrap();
        // a leftover of an interrupted copy goes away
        fs::create_dir_all(dir.path().join("new.copy/junk")).unwrap();
        let to = dir.path().join("new");
        copy(&from, &to).unwrap();
        assert_eq!(fs::read_to_string(to.join("db/table")).unwrap(), "rows");
        assert!(!dir.path().join("new.copy").exists());
        assert!(usage(&to) > 0);
        assert_eq!(human(512), "512 B");
        assert_eq!(human(30 * 1024 * 1024), "30 MB");
    }
}
