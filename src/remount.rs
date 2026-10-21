//! Makes the files that the host trusts read-only again after a program on the host replaced one of them.
//!
//! The kernel drops a bind mount in every other namespace when its file is replaced (`git config` does that).
//! luggage looks for that at each command and binds the new file read-only in the namespace again.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use rustix::mount::{MountFlags, mount_bind, mount_remount};
use rustix::process::{chroot, fchdir};
use rustix::thread::{LinkNameSpaceType, move_into_link_name_space};

use crate::chest::PROTECTED;

/// Undoes the octal escapes in `/proc/<pid>/mountinfo`, like `\040` for a space.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = b
            .get(i + 1..i + 4)
            .filter(|d| b[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        if let Some(d) = octal {
            out.push(d.iter().fold(0u8, |n, c| n.wrapping_mul(8).wrapping_add(c - b'0')));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mount points in a mountinfo text, as its process sees them.
fn mount_points(mountinfo: &str) -> Vec<PathBuf> {
    mountinfo
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|m| PathBuf::from(unescape(m)))
        .collect()
}

/// The flags of the newest mount on `point` in a mountinfo text, from its per-mount options.
/// A remount in a user namespace must repeat these: the kernel locks them, also the atime mode and noexec.
fn mount_flags(mountinfo: &str, point: &Path) -> Option<MountFlags> {
    let options = mountinfo
        .lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            if Path::new(&unescape(f.nth(4)?)) != point {
                return None;
            }
            f.next()
        })
        .next_back()?;
    let mut flags = MountFlags::STRICTATIME;
    for o in options.split(',') {
        flags |= match o {
            "ro" => MountFlags::RDONLY,
            "nosuid" => MountFlags::NOSUID,
            "nodev" => MountFlags::NODEV,
            "noexec" => MountFlags::NOEXEC,
            "nodiratime" => MountFlags::NODIRATIME,
            "noatime" => MountFlags::NOATIME,
            "relatime" => MountFlags::RELATIME,
            _ => continue,
        };
    }
    // the kernel refuses strictatime together with another atime mode
    if flags.intersects(MountFlags::NOATIME | MountFlags::RELATIME) {
        flags.remove(MountFlags::STRICTATIME);
    }
    Some(flags)
}

/// The protected paths that exist on the host but are no mount point in the namespace, where the project is at `root`.
/// A symlink is left out: binding it would bind its target.
fn dropped(project: &Path, root: &Path, mounts: &[PathBuf]) -> Vec<PathBuf> {
    PROTECTED
        .iter()
        .filter(|n| project.join(n).symlink_metadata().is_ok_and(|m| !m.is_symlink()))
        .map(|n| root.join(n))
        .filter(|p| !mounts.contains(p))
        .collect()
}

/// Binds the protected paths of `project` read-only again where the namespace of `pid` lost them.
/// `root` is where `pid` sees the project; the binding runs in `luggage rebind`, which enters the namespace.
///
/// # Errors
/// If a path stays writable in the chest.
pub fn protect_again(pid: u32, project: &Path, root: &Path) -> Result<()> {
    let Ok(info) = fs::read_to_string(format!("/proc/{pid}/mountinfo")) else {
        return Ok(());
    };
    let lost = dropped(project, root, &mount_points(&info));
    if lost.is_empty() {
        return Ok(());
    }
    let ok = Command::new(std::env::current_exe()?)
        .arg("rebind")
        .arg(pid.to_string())
        .args(&lost)
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        bail!("{} is writable in the chest", lost[0].display());
    }
    Ok(())
}

/// The parent of the process `pid`, from `/proc/<pid>/stat`.
fn parent(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// The process in the outermost user namespace above `pid` that is not ours. That namespace owns the mount
/// namespace: bwrap puts the chest itself into a nested one, where luggage would have no rights over the mounts.
fn outermost(pid: u32) -> u32 {
    // Ubuntu gives an empty link instead of an error for processes of other users, like PID 1
    let user = |p: u32| {
        fs::read_link(format!("/proc/{p}/ns/user")).ok().filter(|l| !l.as_os_str().is_empty())
    };
    let ours = user(std::process::id());
    let mut found = pid;
    while let Some(p) = parent(found).filter(|&p| user(p).is_some_and(|u| Some(u) != ours)) {
        found = p;
    }
    found
}

/// `luggage rebind`: enters the mount namespace of `pid` and binds each path read-only onto itself.
/// The process must have a single thread, else the kernel refuses to change its user namespace.
///
/// # Errors
/// If the namespace cannot be entered or a path cannot be bound.
pub fn rebind(pid: u32, paths: &[PathBuf]) -> Result<()> {
    let open = |p: String| File::open(&p).with_context(|| format!("cannot open {p}"));
    let root = open(format!("/proc/{pid}/root"))?;
    let user = open(format!("/proc/{}/ns/user", outermost(pid)))?;
    let mnt = open(format!("/proc/{pid}/ns/mnt"))?;
    // opened here: the namespace may have no /proc; it shows the mounts as `pid` sees them
    let mut mountinfo = open(format!("/proc/{pid}/mountinfo"))?;
    move_into_link_name_space(user.as_fd(), Some(LinkNameSpaceType::User))
        .context("cannot enter the user namespace")?;
    move_into_link_name_space(mnt.as_fd(), Some(LinkNameSpaceType::Mount))
        .context("cannot enter the mount namespace")?;
    fchdir(&root)?;
    chroot(".")?;
    for p in paths {
        mount_bind(p, p).with_context(|| format!("cannot bind {}", p.display()))?;
        let mut info = String::new();
        mountinfo.seek(SeekFrom::Start(0))?;
        mountinfo.read_to_string(&mut info)?;
        let flags = mount_flags(&info, p)
            .with_context(|| format!("cannot find the mount of {}", p.display()))?;
        mount_remount(p, flags | MountFlags::BIND | MountFlags::RDONLY, "")
            .with_context(|| format!("cannot make {} read-only", p.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_parent_process() {
        assert_eq!(parent(std::process::id()), Some(std::os::unix::process::parent_id()));
        // luggage's own namespace is no target
        assert_eq!(outermost(std::process::id()), std::process::id());
    }

    #[test]
    fn reads_mount_points() {
        let info = "2183 1785 0:28 /@rootfs/p / rw,nosuid - btrfs /dev/x rw\n\
                    2202 2183 0:28 /@rootfs/p/my\\040dir /my\\040dir ro,nosuid - btrfs /dev/x rw\n";
        assert_eq!(mount_points(info), [PathBuf::from("/"), PathBuf::from("/my dir")]);
        assert_eq!(unescape(r"a\134b\0"), r"a\b\0");
    }

    #[test]
    fn repeats_the_locked_flags_of_the_mount() {
        let info = "2183 1785 0:28 /p / rw,nosuid,nodev,noatime - btrfs /dev/x rw\n\
                    2202 2183 0:28 /p/a /a rw,relatime - btrfs /dev/x rw\n\
                    2203 2183 0:28 /p/a /a rw,nosuid,nodev,noexec,nodiratime - btrfs /dev/x rw\n";
        assert_eq!(
            mount_flags(info, Path::new("/")),
            Some(MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOATIME)
        );
        // the newest mount on a path is the one that counts, and no atime option means strictatime
        let flags = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
        assert_eq!(
            mount_flags(info, Path::new("/a")),
            Some(flags | MountFlags::NODIRATIME | MountFlags::STRICTATIME)
        );
        assert_eq!(mount_flags(info, Path::new("/b")), None);
    }

    #[test]
    fn finds_protected_files_that_lost_their_mount() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        fs::create_dir_all(p.join(".git/hooks")).unwrap();
        for f in ["flake.nix", ".git/config"] {
            fs::write(p.join(f), "").unwrap();
        }
        std::os::unix::fs::symlink("flake.nix", p.join(".envrc")).unwrap();
        // the namespace of virtiofsd sees the project at /
        let root = Path::new("/");
        let mounts: Vec<PathBuf> =
            ["/", "/.git", "/flake.nix", "/.git/hooks"].map(PathBuf::from).into();
        assert_eq!(dropped(p, root, &mounts), [PathBuf::from("/.git/config")]);
        // the bubblewrap chest sees it at its own path
        let mounts: Vec<PathBuf> =
            ["flake.nix", ".git/config", ".git/hooks"].map(|n| p.join(n)).into();
        assert_eq!(dropped(p, p, &mounts), Vec::<PathBuf>::new());
    }
}
