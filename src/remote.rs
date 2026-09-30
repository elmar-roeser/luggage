//! Legt das Remote-Projekt auf GitLab (glab) oder GitHub (gh) an.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::{info, run};

/// Wo das Remote-Projekt angelegt wird.
pub enum Forge {
    /// GitLab über `glab`.
    Gitlab {
        /// Gruppe; ohne Angabe der eigene Namespace
        group: Option<String>,
        /// GitLab-Host; ohne Angabe der Standard-Host von glab
        host: Option<String>,
    },
    /// GitHub über `gh`.
    Github {
        /// Besitzer; ohne Angabe der angemeldete gh-Account
        owner: Option<String>,
    },
}

/// Login-Name des angemeldeten gh-Accounts.
fn gh_login() -> Result<String> {
    let out = Command::new("gh")
        .args(["api", "user", "--jq", ".login"])
        .output()
        .context("gh nicht startbar")?;
    let login = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || login.is_empty() {
        bail!("gh nicht angemeldet — erst `gh auth login`");
    }
    Ok(login)
}

/// Legt `name` an und setzt `origin`; `visibility` ist private, internal oder public.
pub fn create(root: &Path, name: &str, forge: &Forge, visibility: &str) -> Result<()> {
    let flag = format!("--{visibility}");
    match forge {
        Forge::Gitlab { group, host } => {
            let path = group.as_deref().map_or_else(|| name.to_owned(), |g| format!("{g}/{name}"));
            let on = host.as_deref().map(|h| format!(" auf {h}")).unwrap_or_default();
            info(&format!("lege {visibility} GitLab-Projekt {path}{on} an"));
            let env: Vec<(&str, &str)> =
                host.as_deref().map(|h| ("GITLAB_HOST", h)).into_iter().collect();
            let args = ["repo", "create", &path, &flag, "--defaultBranch", "main"];
            if !run(root, "glab", &args, &env, false)? {
                bail!("glab repo create fehlgeschlagen");
            }
            if !run(root, "git", &["remote", "get-url", "origin"], &[], true)? {
                info("WARNUNG: glab hat kein Remote 'origin' gesetzt — bitte manuell hinzufügen");
            }
        }
        Forge::Github { owner } => {
            let owner = match owner {
                Some(o) => o.clone(),
                None => gh_login()?,
            };
            let full = format!("{owner}/{name}");
            info(&format!("lege {visibility} GitHub-Repo {full} an"));
            if !run(root, "gh", &["repo", "create", &full, &flag], &[], false)? {
                bail!("gh repo create fehlgeschlagen");
            }
            // SSH statt HTTPS: Workflow-Dateien pushen sonst nur mit workflow-Scope im gh-Token
            let url = format!("git@github.com:{full}.git");
            if !run(root, "git", &["remote", "add", "origin", &url], &[], false)? {
                bail!("git remote add origin {url} fehlgeschlagen");
            }
        }
    }
    Ok(())
}
