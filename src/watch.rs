//! Changes on the host for file watchers in the VM: inotify in the guest does not see host writes over virtiofs.
//!
//! `luggage watch-project <dir>` prints each path the host changes in the project, one per line; `vm::start` pipes
//! them into a guest script that marks each one with a `chmod` to its own mode, so watchers in the guest notice.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

/// Directories at the top of the project that are not watched: history, tools, dependencies and caches.
const SKIP_TOP: [&str; 6] = [".git", ".direnv", ".luggage", ".idea", "vendor", "var"];

/// Changes that matter to a watcher. Attribute changes are left out: the guest's own marks show up as those.
const MASK: WatchMask = WatchMask::CLOSE_WRITE
    .union(WatchMask::CREATE)
    .union(WatchMask::DELETE)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::ONLYDIR)
    .union(WatchMask::DONT_FOLLOW);

/// Is the directory `dir` left out? `node_modules` is left out at any depth, the others only at the top.
pub fn skipped(project: &Path, dir: &Path) -> bool {
    let Ok(rel) = dir.strip_prefix(project) else { return true };
    SKIP_TOP.iter().any(|s| rel == Path::new(s))
        || dir.file_name().is_some_and(|n| n == "node_modules")
}

/// Watches of all directories below the project.
struct Tree<'a> {
    /// Project directory
    project: &'a Path,
    /// The inotify instance
    inotify: Inotify,
    /// Watched directory per watch
    dirs: HashMap<WatchDescriptor, PathBuf>,
    /// Was the watch limit reached?
    full: bool,
}

impl Tree<'_> {
    /// Watches `top` and the directories below it; with `out`, also prints every path in it (it is new).
    fn add(&mut self, top: PathBuf, out: &mut Option<&mut dyn Write>) -> io::Result<()> {
        let mut todo = vec![top];
        while let Some(dir) = todo.pop() {
            match self.inotify.watches().add(&dir, MASK) {
                Ok(wd) => {
                    self.dirs.insert(wd, dir.clone());
                }
                Err(e) if e.kind() == io::ErrorKind::StorageFull => {
                    if !self.full {
                        eprintln!(
                            "inotify watch limit reached (fs.inotify.max_user_watches): changes below {} do not reach the VM",
                            dir.display()
                        );
                    }
                    self.full = true;
                    continue;
                }
                // gone again, or not a directory
                Err(_) => continue,
            }
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                if is_dir && skipped(self.project, &path) {
                    continue;
                }
                if let Some(out) = out.as_deref_mut() {
                    print_path(out, &path)?;
                }
                if is_dir {
                    todo.push(path);
                }
            }
        }
        Ok(())
    }
}

/// Prints `path` on its own line. A path with a line break cannot go through the line protocol and is left out.
fn print_path(out: &mut dyn Write, path: &Path) -> io::Result<()> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&b'\n') {
        return Ok(());
    }
    out.write_all(bytes)?;
    out.write_all(b"\n")
}

/// Prints each path changed below `project` until nobody reads the output any more.
///
/// # Errors
/// If inotify is not available.
#[cfg(not(tarpaulin_include))]
pub fn serve(project: &Path) -> Result<()> {
    let mut tree = Tree {
        project,
        inotify: Inotify::init().context("cannot start inotify")?,
        dirs: HashMap::new(),
        full: false,
    };
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    tree.add(project.to_path_buf(), &mut None)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut changed = Vec::new();
        for e in tree.inotify.read_events_blocking(&mut buf)? {
            if e.mask.contains(EventMask::IGNORED) {
                tree.dirs.remove(&e.wd);
                continue;
            }
            let (Some(dir), Some(name)) = (tree.dirs.get(&e.wd), e.name) else { continue };
            let path = dir.join(name);
            let is_dir = e.mask.contains(EventMask::ISDIR);
            if is_dir && skipped(project, &path) {
                continue;
            }
            let new_dir = is_dir && e.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO);
            changed.push((path, new_dir));
        }
        let mut sent = Ok(());
        for (path, new_dir) in changed {
            sent = sent.and_then(|()| print_path(&mut out, &path));
            // what landed in it before the watch existed is new too
            if new_dir {
                sent = sent.and_then(|()| tree.add(path, &mut Some(&mut out)));
            }
        }
        // the guest is gone when nobody reads any more
        if sent.and_then(|()| out.flush()).is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_tools_and_dependencies() {
        let p = Path::new("/w/app");
        for d in [
            "/w/app/.git",
            "/w/app/vendor",
            "/w/app/var",
            "/w/app/node_modules",
            "/w/app/assets/node_modules",
        ] {
            assert!(skipped(p, Path::new(d)), "{d}");
        }
        for d in ["/w/app/src", "/w/app/src/var", "/w/app/src/vendor", "/w/app/assets"] {
            assert!(!skipped(p, Path::new(d)), "{d}");
        }
        assert!(skipped(p, Path::new("/elsewhere")));
    }

    #[test]
    fn prints_new_trees_and_their_files() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        fs::create_dir_all(project.join("src/sub")).unwrap();
        fs::create_dir_all(project.join("vendor/pkg")).unwrap();
        fs::create_dir_all(project.join("src/node_modules/x")).unwrap();
        fs::write(project.join("src/sub/a.php"), "").unwrap();
        fs::write(project.join("vendor/pkg/b.php"), "").unwrap();
        let mut tree =
            Tree { project, inotify: Inotify::init().unwrap(), dirs: HashMap::new(), full: false };
        let mut out = Vec::new();
        tree.add(project.join("src"), &mut Some(&mut out)).unwrap();
        let mut lines: Vec<_> =
            String::from_utf8(out).unwrap().lines().map(str::to_owned).collect();
        lines.sort();
        let want = |p: &str| project.join(p).display().to_string();
        assert_eq!(lines, [want("src/sub"), want("src/sub/a.php")]);
        // src and src/sub, not node_modules
        assert_eq!(tree.dirs.len(), 2);
    }

    #[test]
    fn leaves_out_paths_with_line_breaks() {
        let mut out = Vec::new();
        print_path(&mut out, Path::new("/w/a\nb")).unwrap();
        print_path(&mut out, Path::new("/w/c d")).unwrap();
        assert_eq!(out, b"/w/c d\n");
    }
}
