//! Host-side check of the git files that the chest can write but git on the host reads, like `.git/commondir`.
//!
//! luggage notes them when the chest starts and compares before each command; their config could make git on
//! the host run programs (`core.fsmonitor`). It is a check, not a lock: it cannot stop the user's own `git`.
//! What is read-only in the chest, like `.git/modules`, is left out: the host changes it, the chest cannot.

use std::fs::{self, Metadata};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// One line for `rel`: kind, inode and change time, which no one can set back by hand.
/// Directories leave out the time: git changes it whenever it writes a file in them.
fn line(out: &mut Vec<String>, rel: &str, m: &Metadata) {
    let (kind, time) = if m.is_dir() {
        ('d', String::new())
    } else {
        (if m.is_symlink() { 'l' } else { 'f' }, format!("{}.{}", m.ctime(), m.ctime_nsec()))
    };
    out.push(format!("{kind} {} {time} {}", m.ino(), rel.replace('\n', "\\n")));
}

/// Adds a line for `dir/name` if it exists.
fn entry(out: &mut Vec<String>, dir: &Path, rel: &str, name: &str) {
    if let Ok(m) = dir.join(name).symlink_metadata() {
        line(out, &format!("{rel}/{name}"), &m);
    }
}

/// Sorted names of the entries in `dir`.
fn names(dir: &Path) -> Vec<String> {
    let mut n: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
        .collect();
    n.sort();
    n
}

/// Is `dir` a git dir? Git takes a directory with `HEAD` and `objects` as a repo.
fn is_git_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir()
}

/// Lines for the git dir `dir`: the files that point git to another config, and for all but the top one also
/// its config, hooks, worktrees and modules. In the top `.git` those are read-only in the chest.
fn git_dir(out: &mut Vec<String>, dir: &Path, rel: &str, top: bool) {
    let own: &[&str] = if top { &[] } else { &["config", "hooks"] };
    for name in ["commondir", "config.worktree"].iter().chain(own) {
        entry(out, dir, rel, name);
    }
    if top {
        return;
    }
    for hook in names(&dir.join("hooks")) {
        entry(out, &dir.join("hooks"), &format!("{rel}/hooks"), &hook);
    }
    for wt in names(&dir.join("worktrees")) {
        let (d, r) = (dir.join("worktrees").join(&wt), format!("{rel}/worktrees/{wt}"));
        entry(out, &dir.join("worktrees"), &format!("{rel}/worktrees"), &wt);
        for name in ["commondir", "config.worktree", "config"] {
            entry(out, &d, &r, name);
        }
    }
    modules(out, &dir.join("modules"), &format!("{rel}/modules"));
}

/// Lines for the submodule git dirs below `dir`; a submodule name with `/` makes plain directories in between.
fn modules(out: &mut Vec<String>, dir: &Path, rel: &str) {
    for name in names(dir) {
        let (d, r) = (dir.join(&name), format!("{rel}/{name}"));
        if !d.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            continue;
        }
        entry(out, dir, rel, &name);
        if is_git_dir(&d) {
            git_dir(out, &d, &r, false);
        } else {
            modules(out, &d, &r);
        }
    }
}

/// Lines for the repos nested in the project below `dir`: `.git` files and dirs, and bare repos.
/// It reads only directory entries and their types and leaves out `node_modules`, so it stays fast in large trees.
fn nested(out: &mut Vec<String>, dir: &Path, rel: &str) {
    let (mut head, mut objects, mut git, mut dirs) = (false, false, false, Vec::new());
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name();
        match name.as_bytes() {
            b"HEAD" => head = true,
            b"objects" => objects = true,
            b".git" => git = true,
            // the largest tree by far; packages from git in it are rare, and only a warning anyway
            b"node_modules" => {}
            _ => {
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    dirs.push(name);
                }
            }
        }
    }
    if rel.is_empty() {
        dirs.retain(|d| d != ".direnv" && d != ".luggage");
    } else if head && objects {
        // git also takes a directory with HEAD and objects as a bare repo
        if let Ok(m) = dir.symlink_metadata() {
            line(out, rel, &m);
        }
        git_dir(out, dir, rel, false);
        return;
    } else if git && let Ok(m) = dir.join(".git").symlink_metadata() {
        let r = format!("{rel}/.git");
        line(out, &r, &m);
        if m.is_dir() {
            git_dir(out, &dir.join(".git"), &r, false);
        }
    }
    for d in dirs {
        let name = d.to_string_lossy();
        let r = if rel.is_empty() { name.into_owned() } else { format!("{rel}/{name}") };
        nested(out, &dir.join(&d), &r);
    }
}

/// The git files of `project` that the chest can write and git on the host reads, one line each.
/// `deep` also walks the whole project for nested repos, which is too slow for every command.
pub fn snapshot(project: &Path, deep: bool) -> String {
    let mut out = Vec::new();
    let git = project.join(".git");
    if let Ok(m) = git.symlink_metadata() {
        line(&mut out, ".git", &m);
        if m.is_dir() {
            git_dir(&mut out, &git, ".git", true);
        }
    }
    if deep {
        nested(&mut out, project, "");
    }
    out.join("\n")
}

/// Keeps the lines of `snapshot` without `deep`: those of the top `.git`.
pub fn shallow(snapshot: &str) -> String {
    part(snapshot, true)
}

/// Keeps the lines that only `deep` adds: those of the nested repos.
pub fn nested_repos(snapshot: &str) -> String {
    part(snapshot, false)
}

/// The lines of the top `.git` if `top`, else the others. Lines for the read-only `.git/modules` and
/// `.git/worktrees` are dropped: an older luggage noted them, and they would count as gone.
fn part(snapshot: &str, top: bool) -> String {
    let path = |l: &str| l.splitn(4, ' ').nth(3).unwrap_or_default().to_owned();
    snapshot
        .lines()
        .filter(|l| {
            let p = path(l);
            if p.starts_with(".git/modules") || p.starts_with(".git/worktrees") {
                return false;
            }
            (p == ".git" || p.starts_with(".git/")) == top
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first path that differs between two snapshots: new, changed or gone.
pub fn changed(before: &str, now: &str) -> Option<String> {
    let path = |l: &str| l.splitn(4, ' ').nth(3).unwrap_or_default().to_owned();
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = now.lines().collect();
    new.iter()
        .find(|l| !old.contains(l))
        .or_else(|| old.iter().find(|l| !new.contains(l)))
        .map(|l| path(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo with a submodule, as `git init` and `git submodule add` leave it.
    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let git = tmp.path().join(".git");
        for d in ["objects", "hooks", "modules/lib/a/objects", "modules/lib/a/hooks"] {
            fs::create_dir_all(git.join(d)).unwrap();
        }
        for f in ["HEAD", "config", "modules/lib/a/HEAD", "modules/lib/a/config"] {
            fs::write(git.join(f), "").unwrap();
        }
        fs::create_dir_all(tmp.path().join("src/deep")).unwrap();
        tmp
    }

    #[test]
    fn notices_files_that_lead_git_to_another_config() {
        let tmp = repo();
        let p = tmp.path();
        let before = snapshot(p, true);
        assert_eq!(changed(&before, &snapshot(p, true)), None);
        // the reproduced attack: git reads the config of the dir that commondir names
        fs::write(p.join(".git/commondir"), "../evil").unwrap();
        assert_eq!(changed(&before, &snapshot(p, false)).as_deref(), Some(".git/commondir"));
        fs::remove_file(p.join(".git/commondir")).unwrap();
        assert_eq!(changed(&before, &snapshot(p, true)), None);

        fs::write(p.join(".git/config.worktree"), "").unwrap();
        assert_eq!(changed(&before, &snapshot(p, false)).as_deref(), Some(".git/config.worktree"));
        fs::remove_file(p.join(".git/config.worktree")).unwrap();
        // `git worktree add` and `git submodule update --init` on the host: the chest cannot write there
        fs::create_dir_all(p.join(".git/worktrees/x")).unwrap();
        fs::write(p.join(".git/modules/lib/a/hooks/post-checkout"), "").unwrap();
        fs::create_dir_all(p.join(".git/modules/b/objects")).unwrap();
        assert_eq!(changed(&before, &snapshot(p, false)), None);

        // in a nested repo the chest can write all of them
        let git = p.join("vendor/x/.git");
        for d in ["objects", "hooks", "modules/lib/a/objects", "modules/lib/a/hooks"] {
            fs::create_dir_all(git.join(d)).unwrap();
        }
        for f in ["HEAD", "config", "modules/lib/a/HEAD", "modules/lib/a/config"] {
            fs::write(git.join(f), "").unwrap();
        }
        let cases: [(&str, &dyn Fn()); 5] = [
            ("config.worktree", &|| fs::write(git.join("config.worktree"), "").unwrap()),
            ("worktrees/x", &|| fs::create_dir_all(git.join("worktrees/x")).unwrap()),
            ("modules/lib/a/config", &|| {
                fs::write(git.join("modules/lib/a/config"), "[core]").unwrap();
            }),
            ("modules/lib/a/hooks/post-checkout", &|| {
                fs::write(git.join("modules/lib/a/hooks/post-checkout"), "").unwrap();
            }),
            ("modules/b", &|| fs::create_dir_all(git.join("modules/b/objects")).unwrap()),
        ];
        for (path, change) in cases {
            let before = snapshot(p, true);
            change();
            let path = format!("vendor/x/.git/{path}");
            assert_eq!(changed(&before, &snapshot(p, true)).as_deref(), Some(path.as_str()));
        }
    }

    #[test]
    fn notices_nested_repos_only_deep() {
        let tmp = repo();
        let p = tmp.path();
        let before = snapshot(p, true);
        fs::create_dir_all(p.join("src/deep/.git")).unwrap();
        fs::create_dir_all(p.join("src/node_modules/pkg/.git")).unwrap();
        assert_eq!(changed(&shallow(&before), &snapshot(p, false)), None);
        let now = snapshot(p, true);
        assert_eq!(changed(&shallow(&before), &shallow(&now)), None);
        assert_eq!(
            changed(&nested_repos(&before), &nested_repos(&now)).as_deref(),
            Some("src/deep/.git")
        );
        assert_eq!(changed(&before, &snapshot(p, true)).as_deref(), Some("src/deep/.git"));
        fs::remove_dir(p.join("src/deep/.git")).unwrap();
        // git also takes a directory with HEAD and objects as a bare repo
        fs::create_dir_all(p.join("src/objects")).unwrap();
        fs::write(p.join("src/HEAD"), "").unwrap();
        assert_eq!(changed(&before, &snapshot(p, true)).as_deref(), Some("src"));
    }

    #[test]
    fn notes_of_an_older_luggage_about_read_only_dirs_are_dropped() {
        let tmp = repo();
        let p = tmp.path();
        let old = format!(
            "{}\nd 7  .git/modules/lib/a\nf 8 1.0 .git/modules/lib/a/config\nd 9  .git/worktrees/x",
            snapshot(p, true)
        );
        assert_eq!(changed(&shallow(&old), &snapshot(p, false)), None);
        assert_eq!(changed(&nested_repos(&old), &nested_repos(&snapshot(p, true))), None);
    }

    #[test]
    fn a_replaced_file_counts_as_changed() {
        let tmp = repo();
        let p = tmp.path();
        let config = p.join(".git/config.worktree");
        fs::write(&config, "").unwrap();
        let before = snapshot(p, false);
        fs::write(p.join("new"), "").unwrap();
        fs::rename(p.join("new"), &config).unwrap();
        assert_eq!(changed(&before, &snapshot(p, false)).as_deref(), Some(".git/config.worktree"));
    }
}
