//! Coding agents in the chest: Claude Code and Codex, from their own installers into the home of the chest.
//!
//! They update themselves there like on the host, and each chest keeps its own login.

use std::fmt::Write as _;

use anyhow::{Result, bail};

/// The agents that `agents` in `chest.toml` may name.
pub const NAMES: [&str; 2] = ["claude", "codex"];

/// Packages from nixpkgs that the installers need in the chest.
pub const PACKAGES: [&str; 3] = ["curl", "gnutar", "gzip"];

/// Checks an entry of `agents`.
///
/// # Errors
/// If it is not one of [`NAMES`].
pub fn check(name: &str) -> Result<()> {
    if !NAMES.contains(&name) {
        bail!("agents: {name:?} is unknown (claude | codex)");
    }
    Ok(())
}

/// The hosts, with their subdomains, that the agents reach for their login, their API and their updates.
pub fn hosts(agents: &[String]) -> Vec<String> {
    let mut hosts = Vec::new();
    for agent in agents {
        let more: &[&str] = match agent.as_str() {
            "claude" => &["anthropic.com", "claude.ai", "claude.com"],
            // Codex comes from its releases on GitHub
            "codex" => &["openai.com", "chatgpt.com", "github.com", "githubusercontent.com"],
            _ => &[],
        };
        hosts.extend(more.iter().map(|h| (*h).to_owned()));
    }
    hosts
}

/// A bash script for the chest that installs each missing agent into `~/.local/bin`.
/// A failed install only warns: the chest runs anyway, and the next start tries again.
pub fn install_script(agents: &[String]) -> String {
    let mut s = String::from("mkdir -p \"$HOME/.local/bin\"\n");
    for agent in agents {
        let (title, install) = match agent.as_str() {
            "claude" => ("Claude Code", "curl -fsSL https://claude.ai/install.sh | bash >&2"),
            "codex" => (
                "Codex",
                "t=$(mktemp -d) && curl -fsSL https://github.com/openai/codex/releases/latest/download/codex-x86_64-unknown-linux-musl.tar.gz | tar xz -C \"$t\" && mv \"$t/codex-x86_64-unknown-linux-musl\" \"$HOME/.local/bin/codex\" && rmdir \"$t\"",
            ),
            _ => continue,
        };
        let _ = write!(
            s,
            "if [ ! -x \"$HOME/.local/bin/{agent}\" ]; then\n  echo \"luggage: installing {title} in the chest\" >&2\n  ( {install} ) || echo \"luggage: warning: cannot install {title}; the next start tries again\" >&2\nfi\n"
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_two_agents() {
        check("claude").unwrap();
        check("codex").unwrap();
        assert!(check("cursor").is_err());
        let both = ["claude".to_owned(), "codex".to_owned()];
        assert!(hosts(&both[..1]).contains(&"claude.ai".to_owned()));
        assert!(hosts(&both).contains(&"chatgpt.com".to_owned()));
    }

    #[test]
    fn installs_only_what_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        for a in NAMES {
            std::fs::write(bin.join(a), "").unwrap();
            std::fs::set_permissions(
                bin.join(a),
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
        }
        // with both present, the script downloads nothing, so it works without network
        let script = install_script(&NAMES.map(str::to_owned));
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .env("HOME", tmp.path())
            .env("PATH", "/usr/bin:/bin")
            .env("https_proxy", "http://127.0.0.1:9")
            .output()
            .unwrap();
        assert!(
            out.status.success() && out.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_failed_install_only_warns() {
        let tmp = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(install_script(&["codex".to_owned()]))
            .env("HOME", tmp.path())
            // no curl: the download fails
            .env("PATH", "/nonexistent:/usr/bin:/bin")
            .env("https_proxy", "http://127.0.0.1:9")
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{err}");
        assert!(err.contains("cannot install Codex"), "{err}");
    }
}
