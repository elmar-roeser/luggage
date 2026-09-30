//! Fragt den nixpkgs-Channel, welche PHP-/Node-/Python-Versionen es gibt (Cache: 1 Tag).

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::home;

/// Wie lange eine zwischengespeicherte Antwort gilt.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Nix-Ausdruck, der die Versionen als JSON liefert.
/// `@NIXPKGS@` wird vor dem Aufruf durch den Channel ersetzt.
const QUERY: &str = r#"
let
  p = (builtins.getFlake "@NIXPKGS@").legacyPackages.x86_64-linux;
  ver = n: let r = builtins.tryEval (p.${n}.version or null); in if r.success then r.value else null;
  pick = re: builtins.filter (n: builtins.match re n != null && ver n != null) (builtins.attrNames p);
  entries = re: map (n: { attr = n; version = ver n; }) (pick re);
in {
  php = entries "php8[0-9]";
  node = entries "nodejs_[0-9]+";
  python = entries "python3[0-9]+";
  php_default = ver "php";
  python_default = ver "python3";
  rustc = ver "rustc";
}
"#;

/// Ein nixpkgs-Attribut mit seiner Version.
#[derive(Debug, Deserialize)]
struct Entry {
    /// Attributname, z.B. `nodejs_22`
    attr: String,
    /// Version, wie nix sie meldet
    version: String,
}

/// Rohe JSON-Antwort von `nix eval` auf [`QUERY`].
#[derive(Debug, Deserialize)]
struct Raw {
    /// Attribute `php8x`
    php: Vec<Entry>,
    /// Attribute `nodejs_N`
    node: Vec<Entry>,
    /// Attribute `python3N`
    python: Vec<Entry>,
    /// Version von `php`
    php_default: Option<String>,
    /// Version von `python3`
    python_default: Option<String>,
    /// Version von `rustc`
    rustc: Option<String>,
}

/// Verfügbare Versionen eines Channels, jeweils aufsteigend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    /// PHP als (Major, Minor)
    pub php: Vec<(u32, u32)>,
    /// Node-Hauptversionen
    pub node: Vec<u32>,
    /// Python als (Major, Minor)
    pub python: Vec<(u32, u32)>,
    /// Standard-PHP des Channels
    pub php_default: Option<(u32, u32)>,
    /// Standard-Python des Channels
    pub python_default: Option<(u32, u32)>,
    /// Rust-Version des Channels
    pub rustc: Option<(u32, u32)>,
}

/// `"8.3.35"` → `(8, 3)`; Vorabversionen wie `"3.15.0rc2"` → `None`.
pub fn major_minor(version: &str) -> Option<(u32, u32)> {
    if !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let mut parts = version.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Wandelt die Antwort von `nix eval` in [`Available`]; Vorabversionen fallen weg.
fn parse(json: &str) -> Result<Available> {
    let raw: Raw = serde_json::from_str(json)?;
    let stable = |entries: &[Entry]| {
        let mut v: Vec<(u32, u32)> =
            entries.iter().filter_map(|e| major_minor(&e.version)).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let mut node: Vec<u32> = raw
        .node
        .iter()
        .filter(|e| major_minor(&e.version).is_some())
        .filter_map(|e| e.attr.strip_prefix("nodejs_")?.parse().ok())
        .collect();
    node.sort_unstable();
    Ok(Available {
        php: stable(&raw.php),
        node,
        python: stable(&raw.python),
        php_default: raw.php_default.as_deref().and_then(major_minor),
        python_default: raw.python_default.as_deref().and_then(major_minor),
        rustc: raw.rustc.as_deref().and_then(major_minor),
    })
}

/// Cache-Datei für den Channel unter `XDG_CACHE_HOME`.
fn cache_file(nixpkgs: &str) -> PathBuf {
    let name: String =
        nixpkgs.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| home().join(".cache"), PathBuf::from)
        .join("luggage")
        .join(format!("{name}.json"))
}

/// Ob die Datei jünger als [`CACHE_TTL`] ist.
fn fresh(path: &PathBuf) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < CACHE_TTL)
}

/// Verfügbare Versionen von `nixpkgs`, aus dem Cache oder per `nix eval`.
pub fn query(nixpkgs: &str) -> Result<Available> {
    let cache = cache_file(nixpkgs);
    if fresh(&cache)
        && let Some(a) = fs::read_to_string(&cache).ok().and_then(|j| parse(&j).ok())
    {
        return Ok(a);
    }
    let out = Command::new("nix")
        .args(["eval", "--json", "--impure", "--expr", &QUERY.replace("@NIXPKGS@", nixpkgs)])
        .output()
        .context("nix nicht startbar")?;
    if !out.status.success() {
        bail!(
            "Versionen von {nixpkgs} nicht abfragbar:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let json = String::from_utf8(out.stdout)?;
    let available = parse(&json).context("unerwartete Antwort von nix eval")?;
    if let Some(dir) = cache.parent() {
        // Cache ist nur Beschleunigung, Schreibfehler sind egal
        let _ = fs::create_dir_all(dir).and_then(|()| fs::write(&cache, &json));
    }
    Ok(available)
}

#[cfg(test)]
pub fn fixture() -> Available {
    Available {
        php: vec![(8, 2), (8, 3), (8, 4), (8, 5)],
        node: vec![20, 22, 24, 26],
        python: vec![(3, 11), (3, 12), (3, 13), (3, 14)],
        php_default: Some((8, 4)),
        python_default: Some((3, 13)),
        rustc: Some((1, 95)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nix_output_and_drops_prereleases() {
        let json = r#"{"php":[{"attr":"php83","version":"8.3.35"},{"attr":"php82","version":"8.2.34"}],
            "node":[{"attr":"nodejs_22","version":"22.23.3"},{"attr":"nodejs_20","version":"20.20.2"}],
            "python":[{"attr":"python313","version":"3.13.15"},{"attr":"python315","version":"3.15.0rc2"}],
            "php_default":"8.4.26","node_default":"24.21.0","python_default":"3.13.15","rustc":"1.95.0"}"#;
        let a = parse(json).unwrap();
        assert_eq!(a.php, [(8, 2), (8, 3)]);
        assert_eq!(a.node, [20, 22]);
        assert_eq!(a.python, [(3, 13)]);
        assert_eq!((a.php_default, a.rustc), (Some((8, 4)), Some((1, 95))));
    }
}
