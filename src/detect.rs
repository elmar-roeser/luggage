//! Erkennt PHP, Node, Python und Rust aus den Projektdateien.

use std::fs;
use std::path::Path;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::config::Config;
use crate::versions::Available;

/// Fest einkompiliert, kein eigenes nixpkgs-Attribut.
const PHP_BUILTIN_EXTS: &[&str] =
    &["core", "date", "hash", "json", "pcre", "random", "reflection", "spl", "standard", "libxml"];

/// Erkannte PHP-Umgebung.
#[derive(Debug, PartialEq, Eq)]
pub struct Php {
    /// `(major, minor)`, z.B. `(8, 3)`
    pub version: (u32, u32),
    /// Woher die Version kommt, z.B. `config.platform.php`
    pub source: &'static str,
    /// Extensions ohne `ext-`, sortiert; fest einkompilierte fehlen
    pub exts: Vec<String>,
}

/// Erkannte Node-Umgebung.
#[derive(Debug, PartialEq, Eq)]
pub struct Node {
    /// `None` heißt: Standard-Node des Channels.
    pub version: Option<u32>,
    /// Woher die Version kommt, z.B. `.nvmrc`
    pub source: &'static str,
    /// `pnpm` oder `yarn`, erkannt an der Lock-Datei
    pub tool: Option<&'static str>,
}

/// Erkannte Python-Umgebung.
#[derive(Debug, PartialEq, Eq)]
pub struct Python {
    /// `None` heißt: Standard-Python des Channels.
    pub version: Option<(u32, u32)>,
    /// Woher die Version kommt, z.B. `.python-version`
    pub source: &'static str,
    /// `uv` oder `poetry`; mit pyproject.toml, aber ohne Lock-Datei: `uv`
    pub tool: Option<&'static str>,
}

/// Erkanntes Rust-Projekt (Cargo.toml vorhanden).
#[derive(Debug, PartialEq, Eq)]
pub struct Rust {
    /// Hinweise, z.B. ignorierte rust-toolchain.toml
    pub warnings: Vec<String>,
}

/// Alles, was die Erkennung braucht.
pub struct Ctx<'a> {
    /// Projektverzeichnis
    pub root: &'a Path,
    /// Versionen, die der Channel anbietet
    pub available: &'a Available,
    /// Geladene Config
    pub config: &'a Config,
}

/// Liest eine JSON-Datei; `None`, wenn sie fehlt oder kaputt ist.
fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Liest eine TOML-Datei; `None`, wenn sie fehlt oder kaputt ist.
fn read_toml(path: &Path) -> Option<toml::Table> {
    fs::read_to_string(path).ok()?.parse().ok()
}

/// Alle `major.minor`-Paare, z.B. `"^8.4 || ^9.0"` → `[(8, 4), (9, 0)]`.
fn versions_in(constraint: &str) -> Vec<(u32, u32)> {
    constraint
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .filter_map(|token| {
            let mut parts = token.split('.');
            Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
        })
        .collect()
}

/// Erste Zahl im Text, z.B. `v22.1` → 22.
fn first_number(s: &str) -> Option<u32> {
    s.split(|c: char| !c.is_ascii_digit()).find(|t| !t.is_empty())?.parse().ok()
}

/// Kleinste verfügbare Version, die mindestens `wanted` ist; sonst die größte.
pub fn pick<T: Ord + Copy>(available: &[T], wanted: T) -> Option<T> {
    available.iter().copied().find(|v| *v >= wanted).or_else(|| available.last().copied())
}

/// Versionen für Meldungen, z.B. `8.3, 8.4`.
fn list(versions: &[(u32, u32)]) -> String {
    versions.iter().map(|(a, b)| format!("{a}.{b}")).collect::<Vec<_>>().join(", ")
}

/// Genau die verlangte Version (für `--php` usw.); Fehler, wenn der Channel sie nicht hat.
fn exact(available: &[(u32, u32)], wanted: &str, what: &str) -> Result<(u32, u32)> {
    match versions_in(wanted).first() {
        Some(v) if available.contains(v) => Ok(*v),
        _ => bail!("{what} {wanted} nicht verfügbar ({})", list(available)),
    }
}

/// `--php` gewinnt, dann `config.platform.php`, Untergrenze von `require.php`, Config, Channel-Standard.
pub fn php(ctx: &Ctx, override_version: Option<&str>) -> Result<Option<Php>> {
    let available = &ctx.available.php;
    let composer = read_json(&ctx.root.join("composer.json"));
    let (version, source) = if let Some(o) = override_version {
        (exact(available, o, "PHP")?, "--php")
    } else {
        let Some(c) = &composer else { return Ok(None) };
        let platform = c["config"]["platform"]["php"].as_str().map(versions_in).unwrap_or_default();
        let require = c["require"]["php"].as_str().map(versions_in).unwrap_or_default();
        let config = ctx.config.php.default.as_deref().map(versions_in).unwrap_or_default();
        let found = [
            (platform.iter().min().copied(), "config.platform.php"),
            (require.iter().min().copied(), "require.php (Untergrenze)"),
            (config.first().copied(), "Config php.default"),
            (ctx.available.php_default, "Standard des Channels"),
        ]
        .into_iter()
        .find_map(|(v, s)| Some((pick(available, v?)?, s)));
        match found {
            Some(f) => f,
            None => bail!("der Channel bietet kein PHP an"),
        }
    };

    let from_composer = ["require", "require-dev"]
        .iter()
        .filter_map(|section| composer.as_ref()?[*section].as_object())
        .flat_map(|deps| deps.keys())
        .filter_map(|key| key.strip_prefix("ext-"));
    let from_config = ctx.config.php.extensions.iter().flatten().map(String::as_str);
    let mut exts: Vec<String> = from_composer
        .chain(from_config)
        .map(|name| name.to_lowercase().replace('-', "_"))
        .filter(|name| !PHP_BUILTIN_EXTS.contains(&name.as_str()))
        .collect();
    exts.sort();
    exts.dedup();
    Ok(Some(Php { version, source, exts }))
}

/// `--node` gewinnt, dann `.nvmrc`/`.node-version`, `engines.node`, Config; ohne Node-Dateien kein Node.
pub fn node(ctx: &Ctx, override_version: Option<&str>) -> Result<Option<Node>> {
    let available = &ctx.available.node;
    let package = read_json(&ctx.root.join("package.json"));
    let (version, source) = if let Some(o) = override_version {
        match first_number(o) {
            Some(v) if available.contains(&v) => (Some(v), "--node"),
            _ => bail!("Node {o} nicht verfügbar ({available:?})"),
        }
    } else if let Some((v, f)) = [".nvmrc", ".node-version"]
        .into_iter()
        .find_map(|f| Some((first_number(&fs::read_to_string(ctx.root.join(f)).ok()?)?, f)))
    {
        (pick(available, v), f)
    } else if let Some(p) = &package {
        if let Some(v) = p["engines"]["node"].as_str().and_then(first_number) {
            (pick(available, v), "engines.node")
        } else if let Some(v) = ctx.config.node.default {
            (pick(available, v), "Config node.default")
        } else {
            (None, "Standard des Channels")
        }
    } else {
        return Ok(None);
    };

    let tool = if ctx.root.join("pnpm-lock.yaml").exists() {
        Some("pnpm")
    } else if ctx.root.join("yarn.lock").exists() {
        Some("yarn")
    } else {
        None
    };
    Ok(Some(Node { version, source, tool }))
}

/// `--python` gewinnt, dann `.python-version`, Untergrenze von `requires-python`, Config, Channel-Standard.
pub fn python(ctx: &Ctx, override_version: Option<&str>) -> Result<Option<Python>> {
    let root = ctx.root;
    let available = &ctx.available.python;
    let pyproject = read_toml(&root.join("pyproject.toml"));
    let markers =
        ["pyproject.toml", ".python-version", "uv.lock", "poetry.lock", "requirements.txt"];
    if override_version.is_none() && !markers.iter().any(|f| root.join(f).exists()) {
        return Ok(None);
    }

    let (version, source) = if let Some(o) = override_version {
        (Some(exact(available, o, "Python")?), "--python")
    } else {
        let pinned = fs::read_to_string(root.join(".python-version"))
            .ok()
            .map(|s| versions_in(&s))
            .unwrap_or_default();
        let requires = pyproject
            .as_ref()
            .and_then(|t| {
                t.get("project")
                    .and_then(|p| p.get("requires-python"))
                    .or_else(|| t.get("tool")?.get("poetry")?.get("dependencies")?.get("python"))
            })
            .and_then(toml::Value::as_str)
            .map(versions_in)
            .unwrap_or_default();
        let config = ctx.config.python.default.as_deref().map(versions_in).unwrap_or_default();
        [
            (pinned.first().copied(), ".python-version"),
            (requires.iter().min().copied(), "requires-python (Untergrenze)"),
            (config.first().copied(), "Config python.default"),
        ]
        .into_iter()
        .find_map(|(v, s)| Some((Some(pick(available, v?)?), s)))
        .unwrap_or((None, "Standard des Channels"))
    };

    let poetry_project = pyproject.as_ref().and_then(|t| t.get("tool")?.get("poetry")).is_some();
    let tool = if root.join("uv.lock").exists() {
        Some("uv")
    } else if root.join("poetry.lock").exists() || poetry_project {
        Some("poetry")
    } else if pyproject.is_some() {
        Some("uv")
    } else {
        None
    };
    Ok(Some(Python { version, source, tool }))
}

/// Rust kommt aus nixpkgs; Toolchain-Dateien und zu neue `rust-version` werden nur gemeldet.
pub fn rust(ctx: &Ctx) -> Option<Rust> {
    let cargo = read_toml(&ctx.root.join("Cargo.toml"))?;
    let mut warnings = Vec::new();
    for f in ["rust-toolchain.toml", "rust-toolchain"] {
        if ctx.root.join(f).exists() {
            warnings.push(format!("{f} wird ignoriert — Rust kommt aus nixpkgs"));
        }
    }
    let wanted = cargo
        .get("package")
        .and_then(|p| p.get("rust-version"))
        .or_else(|| cargo.get("workspace")?.get("package")?.get("rust-version"))
        .and_then(toml::Value::as_str)
        .and_then(|v| versions_in(v).first().copied());
    if let (Some(w), Some(have)) = (wanted, ctx.available.rustc)
        && w > have
    {
        warnings.push(format!(
            "rust-version {}.{} verlangt, nixpkgs hat {}.{}",
            w.0, w.1, have.0, have.1
        ));
    }
    Some(Rust { warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::versions::fixture;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            fs::write(dir.path().join(name), content).unwrap();
        }
        dir
    }

    fn with<T>(files: &[(&str, &str)], config: &str, f: impl FnOnce(&Ctx) -> T) -> T {
        let dir = project(files);
        let available = fixture();
        let config: Config = toml::from_str(config).unwrap();
        f(&Ctx { root: dir.path(), available: &available, config: &config })
    }

    #[test]
    fn picks_smallest_available_at_or_above() {
        let a = fixture().php;
        assert_eq!(pick(&a, (8, 1)), Some((8, 2)));
        assert_eq!(pick(&a, (8, 3)), Some((8, 3)));
        assert_eq!(pick(&a, (9, 0)), Some((8, 5)));
        assert_eq!(pick::<u32>(&[], 1), None);
    }

    #[test]
    fn php_platform_beats_require() {
        let php = with(
            &[(
                "composer.json",
                r#"{"require":{"php":">=8.2"},"config":{"platform":{"php":"8.3.0"}}}"#,
            )],
            "",
            |c| php(c, None).unwrap().unwrap(),
        );
        assert_eq!((php.version, php.source), ((8, 3), "config.platform.php"));
    }

    #[test]
    fn php_extensions_from_composer_and_config() {
        let php = with(
            &[(
                "composer.json",
                r#"{"require":{"php":"^8.4","ext-redis":"*","ext-json":"*"},"require-dev":{"ext-Pdo-Pgsql":"*"}}"#,
            )],
            "[php]\nextensions = [\"xdebug\", \"redis\"]\n",
            |c| php(c, None).unwrap().unwrap(),
        );
        assert_eq!(php.version, (8, 4));
        assert_eq!(php.exts, ["pdo_pgsql", "redis", "xdebug"]);
    }

    #[test]
    fn php_falls_back_to_config_then_channel() {
        let cfg = with(&[("composer.json", "{}")], "[php]\ndefault = \"8.3\"\n", |c| {
            php(c, None).unwrap().unwrap()
        });
        assert_eq!((cfg.version, cfg.source), ((8, 3), "Config php.default"));
        let chan = with(&[("composer.json", "{}")], "", |c| php(c, None).unwrap().unwrap());
        assert_eq!((chan.version, chan.source), ((8, 4), "Standard des Channels"));
        assert!(with(&[], "", |c| php(c, None).unwrap()).is_none());
        assert!(with(&[], "", |c| php(c, Some("7.4"))).is_err());
    }

    #[test]
    fn node_nvmrc_beats_engines_and_detects_pnpm() {
        let node = with(
            &[
                ("package.json", r#"{"engines":{"node":">=21"}}"#),
                (".nvmrc", "v23.1\n"),
                ("pnpm-lock.yaml", ""),
            ],
            "",
            |c| node(c, None).unwrap().unwrap(),
        );
        assert_eq!(node, Node { version: Some(24), source: ".nvmrc", tool: Some("pnpm") });
    }

    #[test]
    fn node_config_default_and_absence() {
        let n = with(&[("package.json", "{}")], "[node]\ndefault = 22\n", |c| {
            node(c, None).unwrap().unwrap()
        });
        assert_eq!((n.version, n.source), (Some(22), "Config node.default"));
        let std = with(&[("package.json", "{}")], "", |c| node(c, None).unwrap().unwrap());
        assert_eq!(std.version, None);
        assert!(with(&[], "", |c| node(c, None).unwrap()).is_none());
        assert!(with(&[], "", |c| node(c, Some("19"))).is_err());
    }

    #[test]
    fn python_versions_and_tools() {
        let uv = with(
            &[("pyproject.toml", "[project]\nrequires-python = \">=3.10\"\n"), ("uv.lock", "")],
            "",
            |c| python(c, None).unwrap().unwrap(),
        );
        assert_eq!(
            uv,
            Python {
                version: Some((3, 11)),
                source: "requires-python (Untergrenze)",
                tool: Some("uv")
            }
        );

        let poetry = with(
            &[
                ("pyproject.toml", "[tool.poetry.dependencies]\npython = \"^3.12\"\n"),
                (".python-version", "3.14.1\n"),
            ],
            "",
            |c| python(c, None).unwrap().unwrap(),
        );
        assert_eq!(
            poetry,
            Python { version: Some((3, 14)), source: ".python-version", tool: Some("poetry") }
        );

        let plain =
            with(&[("requirements.txt", "requests\n")], "", |c| python(c, None).unwrap().unwrap());
        assert_eq!(plain, Python { version: None, source: "Standard des Channels", tool: None });
        assert!(with(&[], "", |c| python(c, None).unwrap()).is_none());
    }

    #[test]
    fn rust_warnings() {
        let r = with(
            &[
                ("Cargo.toml", "[package]\nname = \"x\"\nrust-version = \"1.96\"\n"),
                ("rust-toolchain.toml", ""),
            ],
            "",
            rust,
        )
        .unwrap();
        assert_eq!(r.warnings.len(), 2);
        let ok =
            with(&[("Cargo.toml", "[package]\nname = \"x\"\nrust-version = \"1.89\"\n")], "", rust)
                .unwrap();
        assert!(ok.warnings.is_empty());
        assert!(with(&[], "", rust).is_none());
    }
}
