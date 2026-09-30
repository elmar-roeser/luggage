//! Asks the nixpkgs channel which PHP, Node, Python and database versions it has (cached for 1 day).

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::home;

/// How long a cached answer stays valid.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Nix expression that returns the versions as JSON.
/// `@NIXPKGS@` is replaced with the channel before the call.
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
  mariadb = entries "mariadb_[0-9]+";
  postgresql = entries "postgresql_[0-9]+";
  php_default = ver "php";
  python_default = ver "python3";
  rustc = ver "rustc";
}
"#;

/// A nixpkgs attribute with its version.
#[derive(Debug, Deserialize)]
struct Entry {
    /// Attribute name, e.g. `nodejs_22`
    attr: String,
    /// Version as nix reports it
    version: String,
}

/// Raw JSON answer of `nix eval` for [`QUERY`].
#[derive(Debug, Deserialize)]
struct Raw {
    /// Attributes `php8x`
    php: Vec<Entry>,
    /// Attributes `nodejs_N`
    node: Vec<Entry>,
    /// Attributes `python3N`
    python: Vec<Entry>,
    /// Attributes `mariadb_N`
    mariadb: Vec<Entry>,
    /// Attributes `postgresql_N`
    postgresql: Vec<Entry>,
    /// Version of `php`
    php_default: Option<String>,
    /// Version of `python3`
    python_default: Option<String>,
    /// Version of `rustc`
    rustc: Option<String>,
}

/// Available versions of a channel, each in ascending order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Available {
    /// PHP as (major, minor)
    pub php: Vec<(u32, u32)>,
    /// Node major versions
    pub node: Vec<u32>,
    /// Python as (major, minor)
    pub python: Vec<(u32, u32)>,
    /// MariaDB versions as (major, minor)
    pub mariadb: Vec<(u32, u32)>,
    /// PostgreSQL major versions
    pub postgresql: Vec<u32>,
    /// Default PHP of the channel
    pub php_default: Option<(u32, u32)>,
    /// Default Python of the channel
    pub python_default: Option<(u32, u32)>,
    /// Rust version of the channel
    pub rustc: Option<(u32, u32)>,
}

/// `"8.3.35"` → `(8, 3)`; pre-releases like `"3.15.0rc2"` → `None`.
pub fn major_minor(version: &str) -> Option<(u32, u32)> {
    if !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let mut parts = version.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Turns the answer of `nix eval` into [`Available`]; pre-releases are dropped.
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
    let mut postgresql: Vec<u32> = raw
        .postgresql
        .iter()
        .filter_map(|e| major_minor(&e.version).map(|(major, _)| major))
        .collect();
    postgresql.sort_unstable();
    postgresql.dedup();
    Ok(Available {
        php: stable(&raw.php),
        node,
        python: stable(&raw.python),
        mariadb: stable(&raw.mariadb),
        postgresql,
        php_default: raw.php_default.as_deref().and_then(major_minor),
        python_default: raw.python_default.as_deref().and_then(major_minor),
        rustc: raw.rustc.as_deref().and_then(major_minor),
    })
}

/// Cache file for the channel under `XDG_CACHE_HOME`.
fn cache_file(nixpkgs: &str) -> PathBuf {
    let name: String =
        nixpkgs.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| home().join(".cache"), PathBuf::from)
        .join("luggage")
        .join(format!("{name}.json"))
}

/// Whether the file is younger than [`CACHE_TTL`].
fn fresh(path: &PathBuf) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < CACHE_TTL)
}

/// Available versions of `nixpkgs`, from the cache or via `nix eval`.
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
        .context("cannot start nix")?;
    if !out.status.success() {
        bail!(
            "cannot query versions of {nixpkgs}:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let json = String::from_utf8(out.stdout)?;
    let available = parse(&json).context("unexpected answer from nix eval")?;
    if let Some(dir) = cache.parent() {
        // The cache only speeds things up, so write errors do not matter
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
        mariadb: vec![(10, 6), (10, 11), (11, 4), (11, 8)],
        postgresql: vec![14, 15, 16, 17, 18],
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
            "mariadb":[{"attr":"mariadb_114","version":"11.4.12"},{"attr":"mariadb_1011","version":"10.11.17"}],
            "postgresql":[{"attr":"postgresql_17","version":"17.11"},{"attr":"postgresql_16","version":"16.14"}],
            "php_default":"8.4.26","node_default":"24.21.0","python_default":"3.13.15","rustc":"1.95.0"}"#;
        let a = parse(json).unwrap();
        assert_eq!(a.php, [(8, 2), (8, 3)]);
        assert_eq!(a.node, [20, 22]);
        assert_eq!(a.python, [(3, 13)]);
        assert_eq!(a.mariadb, [(10, 11), (11, 4)]);
        assert_eq!(a.postgresql, [16, 17]);
        assert_eq!((a.php_default, a.rustc), (Some((8, 4)), Some((1, 95))));
    }
}
