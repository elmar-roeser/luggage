//! Benutzer-Config (`~/.config/luggage/config.toml`) mit eingebauten Standardwerten.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::home;

/// nixpkgs-Channel, wenn nichts anderes gesetzt ist.
pub const DEFAULT_NIXPKGS: &str = "github:NixOS/nixpkgs/nixos-26.05";
/// Wohin `luggage new` klont bzw. anlegt, wenn nichts gesetzt ist.
const DEFAULT_PROJECTS_DIR: &str = "~/projects";
/// PHP-`memory_limit`, wenn nichts gesetzt ist.
const DEFAULT_MEMORY_LIMIT: &str = "512M";
/// Sichtbarkeit neuer Remote-Projekte, wenn nichts gesetzt ist.
const DEFAULT_VISIBILITY: &str = "private";

/// Rohform der Datei; `None` heißt: nicht gesetzt, Standard gilt.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Wohin `luggage new` klont bzw. anlegt
    pub projects_dir: Option<String>,
    /// Abschnitt `[nix]`
    pub nix: NixConfig,
    /// Abschnitt `[gitlab]`
    pub gitlab: GitlabConfig,
    /// Abschnitt `[github]`
    pub github: GithubConfig,
    /// Abschnitt `[php]`
    pub php: PhpConfig,
    /// Abschnitt `[node]`
    pub node: NodeConfig,
    /// Abschnitt `[python]`
    pub python: PythonConfig,
}

/// Abschnitt `[nix]` der Config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NixConfig {
    /// Channel für neue Flakes
    pub nixpkgs: Option<String>,
    /// Pakete, die in jede Umgebung kommen
    pub packages: Option<Vec<String>>,
}

/// Abschnitt `[gitlab]` der Config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitlabConfig {
    /// GitLab-Host; ohne Angabe der Standard-Host von glab
    pub host: Option<String>,
    /// Gruppe für `--gitlab` ohne Gruppe
    pub group: Option<String>,
    /// Sichtbarkeit neuer GitLab-Projekte
    pub visibility: Option<String>,
}

/// Abschnitt `[github]` der Config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubConfig {
    /// Besitzer neuer Repos; ohne Angabe der angemeldete gh-Account
    pub owner: Option<String>,
    /// Sichtbarkeit neuer GitHub-Repos
    pub visibility: Option<String>,
}

/// Abschnitt `[php]` der Config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PhpConfig {
    /// PHP-Version, wenn composer.json nichts vorgibt
    pub default: Option<String>,
    /// PHP-`memory_limit`
    pub memory_limit: Option<String>,
    /// Extensions zusätzlich zu den `ext-*` aus composer.json
    pub extensions: Option<Vec<String>>,
}

/// Abschnitt `[node]` der Config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeConfig {
    /// Node-Hauptversion, wenn package.json/.nvmrc nichts vorgeben
    pub default: Option<u32>,
}

/// Abschnitt `[python]` der Config.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PythonConfig {
    /// Python-Version, wenn .python-version/pyproject.toml nichts vorgeben
    pub default: Option<String>,
}

/// Pfad der Config-Datei; beachtet `XDG_CONFIG_HOME`.
pub fn path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| home().join(".config"), PathBuf::from)
        .join("luggage/config.toml")
}

/// Liest die Config; eine fehlende Datei ist kein Fehler.
pub fn load() -> Result<Config> {
    let path = path();
    match fs::read_to_string(&path) {
        Ok(text) => parse(&text).with_context(|| format!("Config {} fehlerhaft", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("Config {} nicht lesbar", path.display())),
    }
}

/// Parst den TOML-Text und prüft die Sichtbarkeiten.
fn parse(text: &str) -> Result<Config> {
    let config: Config = toml::from_str(text)?;
    for v in [&config.gitlab.visibility, &config.github.visibility].into_iter().flatten() {
        check_visibility(v)?;
    }
    Ok(config)
}

/// Prüft, dass `v` private, internal oder public ist.
pub fn check_visibility(v: &str) -> Result<()> {
    if !matches!(v, "private" | "internal" | "public") {
        bail!("visibility \"{v}\" ungültig (private | internal | public)");
    }
    Ok(())
}

impl Config {
    /// Projektverzeichnis aus der Config, sonst Standard.
    pub fn projects_dir(&self) -> String {
        self.projects_dir.clone().unwrap_or_else(|| DEFAULT_PROJECTS_DIR.into())
    }

    /// nixpkgs-Channel aus der Config, sonst Standard.
    pub fn nixpkgs(&self) -> String {
        self.nix.nixpkgs.clone().unwrap_or_else(|| DEFAULT_NIXPKGS.into())
    }

    /// PHP-`memory_limit` aus der Config, sonst Standard.
    pub fn memory_limit(&self) -> String {
        self.php.memory_limit.clone().unwrap_or_else(|| DEFAULT_MEMORY_LIMIT.into())
    }

    /// GitLab-Sichtbarkeit aus der Config, sonst Standard.
    pub fn gitlab_visibility(&self) -> String {
        self.gitlab.visibility.clone().unwrap_or_else(|| DEFAULT_VISIBILITY.into())
    }

    /// GitHub-Sichtbarkeit aus der Config, sonst Standard.
    pub fn github_visibility(&self) -> String {
        self.github.visibility.clone().unwrap_or_else(|| DEFAULT_VISIBILITY.into())
    }

    /// Wirksame Werte mit Herkunft, für `luggage config`.
    pub fn describe(&self) -> Vec<(&'static str, String, &'static str)> {
        fn row<T: std::fmt::Debug>(
            key: &'static str,
            set: Option<&T>,
            fallback: &str,
        ) -> (&'static str, String, &'static str) {
            match set {
                Some(v) => (key, format!("{v:?}"), "Config"),
                None => (key, fallback.into(), "Standard"),
            }
        }
        vec![
            row("projects_dir", self.projects_dir.as_ref(), &format!("{DEFAULT_PROJECTS_DIR:?}")),
            row("nix.nixpkgs", self.nix.nixpkgs.as_ref(), &format!("{DEFAULT_NIXPKGS:?}")),
            row("nix.packages", self.nix.packages.as_ref(), "[]"),
            row("gitlab.host", self.gitlab.host.as_ref(), "(Standard-Host von glab)"),
            row("gitlab.group", self.gitlab.group.as_ref(), "(eigener Namespace)"),
            row(
                "gitlab.visibility",
                self.gitlab.visibility.as_ref(),
                &format!("{DEFAULT_VISIBILITY:?}"),
            ),
            row("github.owner", self.github.owner.as_ref(), "(angemeldeter gh-Account)"),
            row(
                "github.visibility",
                self.github.visibility.as_ref(),
                &format!("{DEFAULT_VISIBILITY:?}"),
            ),
            row("php.default", self.php.default.as_ref(), "(Standard-PHP des Channels)"),
            row(
                "php.memory_limit",
                self.php.memory_limit.as_ref(),
                &format!("{DEFAULT_MEMORY_LIMIT:?}"),
            ),
            row("php.extensions", self.php.extensions.as_ref(), "[]"),
            row("node.default", self.node.default.as_ref(), "(Standard-Node des Channels)"),
            row("python.default", self.python.default.as_ref(), "(Standard-Python des Channels)"),
        ]
    }
}

/// Vorlage, die [`init`] schreibt.
/// Muss gültig parsen; ein Test prüft das.
pub const TEMPLATE: &str = r#"# luggage — Benutzer-Config. Auskommentierte Zeilen zeigen den eingebauten Standard.
# Reihenfolge: Kommandozeile vor dieser Datei vor Standard.

# Wohin `luggage new` klont bzw. anlegt
# projects_dir = "~/projects"

[nix]
# Channel für neue Flakes; bestimmt auch, welche Versionen verfügbar sind
# nixpkgs = "github:NixOS/nixpkgs/nixos-26.05"
# Pakete, die in jede Umgebung kommen, z.B. ["just", "jq"]
# packages = []

[gitlab]
# host = "gitlab.example.com"   # ohne Angabe: Standard-Host von glab
# group = "meine-gruppe"        # Standard für --gitlab ohne Gruppe
# visibility = "private"        # private | internal | public

[github]
# owner = "meine-org"           # ohne Angabe: angemeldeter gh-Account
# visibility = "private"        # private | internal | public

[php]
# default = "8.4"               # wenn composer.json nichts vorgibt
# memory_limit = "512M"
# extensions = ["xdebug"]       # zusätzlich zu den ext-* aus composer.json

[node]
# default = 24                  # wenn package.json/.nvmrc nichts vorgeben

[python]
# default = "3.13"              # wenn .python-version/pyproject.toml nichts vorgeben
"#;

/// Schreibt die Vorlage; eine vorhandene Datei bleibt unangetastet.
pub fn init() -> Result<PathBuf> {
    let path = path();
    if path.exists() {
        bail!("{} existiert schon", path.display());
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
