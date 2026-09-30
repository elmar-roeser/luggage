//! User config (`~/.config/luggage/config.toml`) with built-in defaults.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::home;

/// The nixpkgs channel when nothing else is set.
pub const DEFAULT_NIXPKGS: &str = "github:NixOS/nixpkgs/nixos-26.05";
/// Where `luggage new` clones or creates projects when nothing is set.
const DEFAULT_PROJECTS_DIR: &str = "~/projects";
/// The PHP `memory_limit` when nothing is set.
const DEFAULT_MEMORY_LIMIT: &str = "512M";
/// The visibility of new remote projects when nothing is set.
const DEFAULT_VISIBILITY: &str = "private";

/// The raw form of the file; `None` means not set, so the default applies.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where `luggage new` clones or creates projects
    pub projects_dir: Option<String>,
    /// Section `[nix]`
    pub nix: NixConfig,
    /// Section `[gitlab]`
    pub gitlab: GitlabConfig,
    /// Section `[github]`
    pub github: GithubConfig,
    /// Section `[php]`
    pub php: PhpConfig,
    /// Section `[node]`
    pub node: NodeConfig,
    /// Section `[python]`
    pub python: PythonConfig,
}

/// Section `[nix]` of the config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NixConfig {
    /// Channel for new flakes
    pub nixpkgs: Option<String>,
    /// Packages that go into every environment
    pub packages: Option<Vec<String>>,
}

/// Section `[gitlab]` of the config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitlabConfig {
    /// GitLab host; if unset, the default host of glab
    pub host: Option<String>,
    /// Group for `--gitlab` without a group
    pub group: Option<String>,
    /// Visibility of new GitLab projects
    pub visibility: Option<String>,
}

/// Section `[github]` of the config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubConfig {
    /// Owner of new repos; if unset, the logged-in gh account
    pub owner: Option<String>,
    /// Visibility of new GitHub repos
    pub visibility: Option<String>,
}

/// Section `[php]` of the config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PhpConfig {
    /// PHP version if `composer.json` does not set one
    pub default: Option<String>,
    /// PHP `memory_limit`
    pub memory_limit: Option<String>,
    /// Extensions in addition to the `ext-*` from `composer.json`
    pub extensions: Option<Vec<String>>,
}

/// Section `[node]` of the config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeConfig {
    /// Node major version if `package.json`/`.nvmrc` do not set one
    pub default: Option<u32>,
}

/// Section `[python]` of the config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PythonConfig {
    /// Python version if `.python-version`/`pyproject.toml` do not set one
    pub default: Option<String>,
}

/// Path of the config file; respects `XDG_CONFIG_HOME`.
pub fn path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| home().join(".config"), PathBuf::from)
        .join("luggage/config.toml")
}

/// Reads the config; a missing file is not an error.
pub fn load() -> Result<Config> {
    let path = path();
    match fs::read_to_string(&path) {
        Ok(text) => parse(&text).with_context(|| format!("config {} is invalid", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("config {} is not readable", path.display())),
    }
}

/// Parses the TOML text and checks the visibilities.
fn parse(text: &str) -> Result<Config> {
    let config: Config = toml::from_str(text)?;
    for v in [&config.gitlab.visibility, &config.github.visibility].into_iter().flatten() {
        check_visibility(v)?;
    }
    Ok(config)
}

/// Checks that `v` is private, internal or public.
pub fn check_visibility(v: &str) -> Result<()> {
    if !matches!(v, "private" | "internal" | "public") {
        bail!("visibility \"{v}\" is invalid (private | internal | public)");
    }
    Ok(())
}

impl Config {
    /// Projects directory from the config, else the default.
    pub fn projects_dir(&self) -> String {
        self.projects_dir.clone().unwrap_or_else(|| DEFAULT_PROJECTS_DIR.into())
    }

    /// The nixpkgs channel from the config, else the default.
    pub fn nixpkgs(&self) -> String {
        self.nix.nixpkgs.clone().unwrap_or_else(|| DEFAULT_NIXPKGS.into())
    }

    /// The PHP `memory_limit` from the config, else the default.
    pub fn memory_limit(&self) -> String {
        self.php.memory_limit.clone().unwrap_or_else(|| DEFAULT_MEMORY_LIMIT.into())
    }

    /// GitLab visibility from the config, else the default.
    pub fn gitlab_visibility(&self) -> String {
        self.gitlab.visibility.clone().unwrap_or_else(|| DEFAULT_VISIBILITY.into())
    }

    /// GitHub visibility from the config, else the default.
    pub fn github_visibility(&self) -> String {
        self.github.visibility.clone().unwrap_or_else(|| DEFAULT_VISIBILITY.into())
    }

    /// Effective values with their source, for `luggage config`.
    pub fn describe(&self) -> Vec<(&'static str, String, &'static str)> {
        fn row<T: std::fmt::Debug>(
            key: &'static str,
            set: Option<&T>,
            fallback: &str,
        ) -> (&'static str, String, &'static str) {
            match set {
                Some(v) => (key, format!("{v:?}"), "config"),
                None => (key, fallback.into(), "default"),
            }
        }
        vec![
            row("projects_dir", self.projects_dir.as_ref(), &format!("{DEFAULT_PROJECTS_DIR:?}")),
            row("nix.nixpkgs", self.nix.nixpkgs.as_ref(), &format!("{DEFAULT_NIXPKGS:?}")),
            row("nix.packages", self.nix.packages.as_ref(), "[]"),
            row("gitlab.host", self.gitlab.host.as_ref(), "(default host of glab)"),
            row("gitlab.group", self.gitlab.group.as_ref(), "(own namespace)"),
            row(
                "gitlab.visibility",
                self.gitlab.visibility.as_ref(),
                &format!("{DEFAULT_VISIBILITY:?}"),
            ),
            row("github.owner", self.github.owner.as_ref(), "(logged-in gh account)"),
            row(
                "github.visibility",
                self.github.visibility.as_ref(),
                &format!("{DEFAULT_VISIBILITY:?}"),
            ),
            row("php.default", self.php.default.as_ref(), "(default PHP of the channel)"),
            row(
                "php.memory_limit",
                self.php.memory_limit.as_ref(),
                &format!("{DEFAULT_MEMORY_LIMIT:?}"),
            ),
            row("php.extensions", self.php.extensions.as_ref(), "[]"),
            row("node.default", self.node.default.as_ref(), "(default Node of the channel)"),
            row("python.default", self.python.default.as_ref(), "(default Python of the channel)"),
        ]
    }
}

/// Template that [`init`] writes.
/// It must parse; a test checks this.
pub const TEMPLATE: &str = r#"# luggage — user config. Commented-out lines show the built-in default.
# Order: command line beats this file, this file beats the default.

# Where `luggage new` clones or creates projects
# projects_dir = "~/projects"

[nix]
# Channel for new flakes; also decides which versions are available
# nixpkgs = "github:NixOS/nixpkgs/nixos-26.05"
# Packages that go into every environment, e.g. ["just", "jq"]
# packages = []

[gitlab]
# host = "gitlab.example.com"   # if unset: default host of glab
# group = "my-group"            # default for --gitlab without a group
# visibility = "private"        # private | internal | public

[github]
# owner = "my-org"              # if unset: logged-in gh account
# visibility = "private"        # private | internal | public

[php]
# default = "8.4"               # if composer.json does not set one
# memory_limit = "512M"
# extensions = ["xdebug"]       # in addition to the ext-* from composer.json

[node]
# default = 24                  # if package.json/.nvmrc do not set one

[python]
# default = "3.13"              # if .python-version/pyproject.toml do not set one
"#;

/// Writes the template; an existing file stays untouched.
pub fn init() -> Result<PathBuf> {
    let path = path();
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, TEMPLATE)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_defaults() {
        let c = parse(TEMPLATE).unwrap();
        assert!(c.projects_dir.is_none() && c.php.default.is_none() && c.github.owner.is_none());
        assert_eq!(c.nixpkgs(), DEFAULT_NIXPKGS);
    }

    #[test]
    fn reads_values_and_rejects_typos() {
        let c = parse("projects_dir = \"~/src\"\n[php]\nextensions = [\"xdebug\"]\n[github]\nvisibility = \"public\"\n").unwrap();
        assert_eq!(c.projects_dir(), "~/src");
        assert_eq!(c.php.extensions.as_deref().unwrap(), ["xdebug"]);
        assert_eq!(c.github_visibility(), "public");
        assert!(parse("[php]\ndefualt = \"8.3\"\n").is_err());
        assert!(parse("[gitlab]\nvisibility = \"secret\"\n").is_err());
    }
}
