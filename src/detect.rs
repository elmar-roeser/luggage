//! Erkennt PHP- und Node-Versionen aus den Projektdateien.

use std::fs;
use std::path::Path;

use anyhow::{Result, bail};
use serde_json::Value;

/// In nixos-26.05 vorhandene PHP-Versionen (major, minor), aufsteigend.
pub const PHP_VERSIONS: &[(u32, u32)] = &[(8, 2), (8, 3), (8, 4), (8, 5)];
/// In nixos-26.05 vorhandene Node-Majors, aufsteigend.
pub const NODE_VERSIONS: &[u32] = &[20, 22, 24, 26];
const DEFAULT_PHP: (u32, u32) = (8, 4);
/// Fest einkompiliert, kein eigenes nixpkgs-Attribut.
const PHP_BUILTIN_EXTS: &[&str] = &[
    "core",
    "date",
    "hash",
    "json",
    "pcre",
    "random",
    "reflection",
    "spl",
    "standard",
    "libxml",
];

#[derive(Debug, PartialEq, Eq)]
pub struct Php {
    pub version: (u32, u32),
    pub source: &'static str,
    pub exts: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Node {
    /// `None` heißt: Standard-Node von nixpkgs.
    pub version: Option<u32>,
    pub source: &'static str,
    pub tool: Option<&'static str>,
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
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

fn first_number(s: &str) -> Option<u32> {
    s.split(|c: char| !c.is_ascii_digit())
        .find(|t| !t.is_empty())?
        .parse()
        .ok()
}

/// Kleinste verfügbare PHP-Version, die mindestens `wanted` ist.
fn pick_php(wanted: (u32, u32)) -> (u32, u32) {
    PHP_VERSIONS
        .iter()
        .copied()
        .find(|v| *v >= wanted)
        .unwrap_or(PHP_VERSIONS[PHP_VERSIONS.len() - 1])
}

/// Kleinster verfügbarer Node-Major, der mindestens `wanted` ist.
fn pick_node(wanted: u32) -> u32 {
    NODE_VERSIONS
        .iter()
        .copied()
        .find(|v| *v >= wanted)
        .unwrap_or(NODE_VERSIONS[NODE_VERSIONS.len() - 1])
}

fn php_list() -> String {
    PHP_VERSIONS
        .iter()
        .map(|(a, b)| format!("{a}.{b}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Liest composer.json; `--php` gewinnt, sonst `config.platform.php`, sonst die Untergrenze von `require.php`.
pub fn detect_php(root: &Path, override_version: Option<&str>) -> Result<Option<Php>> {
    let composer = read_json(&root.join("composer.json"));
    let (version, source) = if let Some(o) = override_version {
        match versions_in(o).first() {
            Some(v) if PHP_VERSIONS.contains(v) => (*v, "--php"),
            _ => bail!("PHP {o} nicht verfügbar ({})", php_list()),
        }
    } else {
        let Some(c) = &composer else { return Ok(None) };
        let platform = c["config"]["platform"]["php"]
            .as_str()
            .map(versions_in)
            .unwrap_or_default();
        let require = c["require"]["php"]
            .as_str()
            .map(versions_in)
            .unwrap_or_default();
        if let Some(min) = platform.iter().min() {
            (pick_php(*min), "config.platform.php")
        } else if let Some(min) = require.iter().min() {
            (pick_php(*min), "require.php (Untergrenze)")
        } else {
            (DEFAULT_PHP, "Standard")
        }
    };

    let mut exts: Vec<String> = ["require", "require-dev"]
        .iter()
        .filter_map(|section| composer.as_ref()?[*section].as_object())
        .flat_map(|deps| deps.keys())
        .filter_map(|key| key.strip_prefix("ext-"))
        .map(|name| name.to_lowercase().replace('-', "_"))
        .filter(|name| !PHP_BUILTIN_EXTS.contains(&name.as_str()))
        .collect();
    exts.sort();
    exts.dedup();
    Ok(Some(Php {
        version,
        source,
        exts,
    }))
}

/// `--node` gewinnt, sonst `.nvmrc`/`.node-version`, sonst `engines.node`; ohne package.json kein Node.
pub fn detect_node(root: &Path, override_version: Option<&str>) -> Result<Option<Node>> {
    let package = read_json(&root.join("package.json"));
    let (version, source) = if let Some(o) = override_version {
        match first_number(o) {
            Some(v) if NODE_VERSIONS.contains(&v) => (Some(v), "--node"),
            _ => bail!("Node {o} nicht verfügbar ({NODE_VERSIONS:?})"),
        }
    } else if let Some(found) = [".nvmrc", ".node-version"]
        .into_iter()
        .find_map(|f| Some((first_number(&fs::read_to_string(root.join(f)).ok()?)?, f)))
    {
        (Some(pick_node(found.0)), found.1)
    } else if let Some(p) = &package {
        match p["engines"]["node"].as_str().and_then(first_number) {
            Some(v) => (Some(pick_node(v)), "engines.node"),
            None => (None, "Standard"),
        }
    } else {
        return Ok(None);
    };

    let tool = if root.join("pnpm-lock.yaml").exists() {
        Some("pnpm")
    } else if root.join("yarn.lock").exists() {
        Some("yarn")
    } else {
        None
    };
    Ok(Some(Node {
        version,
        source,
        tool,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            fs::write(dir.path().join(name), content).unwrap();
        }
        dir
    }

    #[test]
    fn picks_smallest_available_php_at_or_above() {
        assert_eq!(pick_php((8, 1)), (8, 2));
        assert_eq!(pick_php((8, 3)), (8, 3));
        assert_eq!(pick_php((9, 0)), (8, 5));
    }

    #[test]
    fn platform_beats_require() {
        let p = project(&[(
            "composer.json",
            r#"{"require":{"php":">=8.2"},"config":{"platform":{"php":"8.3.0"}}}"#,
        )]);
        let php = detect_php(p.path(), None).unwrap().unwrap();
        assert_eq!((php.version, php.source), ((8, 3), "config.platform.php"));
    }

    #[test]
    fn require_lower_bound_and_extensions() {
        let p = project(&[(
            "composer.json",
            r#"{"require":{"php":"^8.4 || ^9.0","ext-redis":"*","ext-json":"*"},"require-dev":{"ext-Pdo-Pgsql":"*"}}"#,
        )]);
        let php = detect_php(p.path(), None).unwrap().unwrap();
        assert_eq!(php.version, (8, 4));
        assert_eq!(php.exts, ["pdo_pgsql", "redis"]);
    }

    #[test]
    fn php_override_and_absence() {
        let empty = project(&[]);
        assert_eq!(detect_php(empty.path(), None).unwrap(), None);
        assert_eq!(
            detect_php(empty.path(), Some("8.3"))
                .unwrap()
                .unwrap()
                .version,
            (8, 3)
        );
        assert!(detect_php(empty.path(), Some("7.4")).is_err());
    }

    #[test]
    fn nvmrc_beats_engines_and_detects_pnpm() {
        let p = project(&[
            ("package.json", r#"{"engines":{"node":">=21"}}"#),
            (".nvmrc", "v23.1\n"),
            ("pnpm-lock.yaml", ""),
        ]);
        let node = detect_node(p.path(), None).unwrap().unwrap();
        assert_eq!(
            node,
            Node {
                version: Some(24),
                source: ".nvmrc",
                tool: Some("pnpm")
            }
        );
    }

    #[test]
    fn node_default_and_absence() {
        let p = project(&[("package.json", "{}")]);
        assert_eq!(detect_node(p.path(), None).unwrap().unwrap().version, None);
        assert_eq!(detect_node(project(&[]).path(), None).unwrap(), None);
        assert!(detect_node(p.path(), Some("19")).is_err());
    }
}
