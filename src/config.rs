//! User config (`~/.config/luggage/config.toml`) with built-in defaults.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

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
    /// Sections `[skills.<name>]`: where skills for coding agents come from
    pub skills: BTreeMap<String, SkillSource>,
}

/// A source of skills, `[skills.<name>]`: a git repo or a local directory.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillSource {
    /// Git address, e.g. `git@gitlab.example.com:dev/skills.git`
    pub git: Option<String>,
    /// Branch or tag of `git` [default: the default branch]
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    /// Local directory; skills from it change at once, without a commit
    pub path: Option<String>,
    /// Folder in the source that holds the skills [default: its top]
    pub dir: Option<String>,
    /// Only these skills of the source [default: all]
    pub only: Option<Vec<String>>,
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
    for (name, s) in &config.skills {
        check_skill_source(name, s).with_context(|| format!("[skills.{name}] is invalid"))?;
    }
    Ok(config)
}

/// Checks the name and that a skill source has either `git` or `path`.
fn check_skill_source(name: &str, s: &SkillSource) -> Result<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("only letters, digits, - and _ may name a source");
    }
    match (&s.git, &s.path) {
        (Some(_), Some(_)) | (None, None) => bail!("it needs either git or path"),
        (None, Some(_)) if s.git_ref.is_some() => bail!("ref needs git"),
        _ => {}
    }
    // the folder lies in the source, so it may not climb out of it
    if s.dir
        .as_deref()
        .is_some_and(|d| Path::new(d).components().any(|c| !matches!(c, Component::Normal(_))))
    {
        bail!("dir must be a plain path inside the source");
    }
    Ok(())
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
            row(
                "skills",
                Some(&self.skills.keys().collect::<Vec<_>>()).filter(|k| !k.is_empty()),
                "(none)",
            ),
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

# Skills for coding agents; `luggage skills sync` links them into ~/.claude/skills and ~/.agents/skills
# [skills.team]
# git = "git@gitlab.example.com:dev/skills.git"
# ref = "main"                  # if unset: the default branch
# dir = "skills"                # folder with the skills; if unset: the top
# only = ["glab"]               # if unset: all skills of the source
# [skills.own]
# path = "~/skills"             # a local directory instead of git
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

    #[test]
    fn checks_skill_sources() {
        let c = parse("[skills.team]\ngit = \"git@h:a.git\"\nref = \"main\"\ndir = \"skills\"\n")
            .unwrap();
        assert_eq!(c.skills["team"].git_ref.as_deref(), Some("main"));
        assert!(parse("[skills.own]\npath = \"~/s\"\nonly = [\"x\"]\n").is_ok());
        for bad in [
            "[skills.a]\n",
            "[skills.a]\ngit = \"x\"\npath = \"y\"\n",
            "[skills.a]\npath = \"y\"\nref = \"main\"\n",
            "[skills.a]\ngit = \"x\"\ndir = \"../up\"\n",
            "[skills.\"a b\"]\ngit = \"x\"\n",
            "[skills.a]\ngit = \"x\"\nbranch = \"main\"\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
