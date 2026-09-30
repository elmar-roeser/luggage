//! Creates the remote project on GitLab (glab) or GitHub (gh).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::{find_in_path, info, run};

/// Where the remote project is created.
pub enum Forge {
    /// GitLab via `glab`.
    Gitlab {
        /// Group; if unset, your own namespace
        group: Option<String>,
        /// GitLab host; if unset, the default host of glab
        host: Option<String>,
    },
    /// GitHub via `gh`.
    Github {
        /// Owner; if unset, the logged-in gh account
        owner: Option<String>,
    },
}

/// Program and leading arguments for `gh`/`glab`: from the system, else via `nix run` from nixpkgs.
fn tool(name: &str) -> (String, Vec<String>) {
    if find_in_path(name).is_some() {
        (name.to_owned(), Vec::new())
    } else {
        ("nix".to_owned(), vec!["run".to_owned(), format!("nixpkgs#{name}"), "--".to_owned()])
    }
}

/// Like `run`, but for `gh`/`glab` (see `tool`).
fn run_tool(root: &Path, name: &str, args: &[&str], env: &[(&str, &str)]) -> Result<bool> {
    let (program, pre) = tool(name);
    let all: Vec<&str> = pre.iter().map(String::as_str).chain(args.iter().copied()).collect();
    run(root, &program, &all, env, false)
}

/// Login name of the logged-in gh account.
fn gh_login() -> Result<String> {
    let (program, pre) = tool("gh");
    let out = Command::new(program)
        .args(pre)
        .args(["api", "user", "--jq", ".login"])
        .output()
        .context("cannot start gh")?;
    let login = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || login.is_empty() {
        bail!("gh is not logged in — run `gh auth login` first");
    }
    Ok(login)
}

/// Creates `name` and sets `origin`; `visibility` is private, internal or public.
pub fn create(root: &Path, name: &str, forge: &Forge, visibility: &str) -> Result<()> {
    let flag = format!("--{visibility}");
    match forge {
        Forge::Gitlab { group, host } => {
            let path = group.as_deref().map_or_else(|| name.to_owned(), |g| format!("{g}/{name}"));
            let on = host.as_deref().map(|h| format!(" on {h}")).unwrap_or_default();
            info(&format!("creating {visibility} GitLab project {path}{on}"));
            let env: Vec<(&str, &str)> =
                host.as_deref().map(|h| ("GITLAB_HOST", h)).into_iter().collect();
            let args = ["repo", "create", &path, &flag, "--defaultBranch", "main"];
            if !run_tool(root, "glab", &args, &env)? {
                bail!("glab repo create failed");
            }
            if !run(root, "git", &["remote", "get-url", "origin"], &[], true)? {
                info("warning: glab did not set the remote 'origin' — please add it by hand");
            }
        }
        Forge::Github { owner } => {
            let owner = match owner {
                Some(o) => o.clone(),
                None => gh_login()?,
            };
            let full = format!("{owner}/{name}");
            info(&format!("creating {visibility} GitHub repo {full}"));
            if !run_tool(root, "gh", &["repo", "create", &full, &flag], &[])? {
                bail!("gh repo create failed");
            }
            // SSH instead of HTTPS: otherwise pushing workflow files needs the workflow scope in the gh token
            let url = format!("git@github.com:{full}.git");
            if !run(root, "git", &["remote", "add", "origin", &url], &[], false)? {
                bail!("git remote add origin {url} failed");
            }
        }
    }
    Ok(())
}
