//! Detects services in the Docker Compose file and turns known images into chest services.
//!
//! Only standard software (mariadb, postgres, redis/valkey, mailpit/mailhog); everything else is reported, not guessed.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_yaml_ng::Value;

use crate::chest::{Service, sh_quote};
use crate::detect::pick;
use crate::versions::Available;

/// File names to look for; the first one that exists wins.
const FILES: [&str; 4] =
    ["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];

/// The part of the Compose file that is read.
#[derive(Deserialize)]
struct ComposeFile {
    /// Compose services by name
    #[serde(default)]
    services: BTreeMap<String, ComposeService>,
}

/// A service from the Compose file.
#[derive(Deserialize)]
struct ComposeService {
    /// Image, e.g. `mariadb:10.11`; missing for a custom build
    image: Option<String>,
    /// `environment:` as a map or list, not yet parsed
    #[serde(default)]
    environment: Value,
}

/// What was taken over from the Compose file.
#[derive(Debug, Default)]
pub struct Detected {
    /// Compose file that was read
    pub file: PathBuf,
    /// Compose service → description, e.g. `database` → `mariadb 10.11`
    pub found: BTreeMap<String, String>,
    /// Skipped Compose services with the reason
    pub skipped: Vec<String>,
    /// Notes, e.g. a different version
    pub notes: Vec<String>,
    /// Chest services, including the one-shot `-init` and `-setup` services
    pub services: BTreeMap<String, Service>,
    /// nixpkgs packages for the services, e.g. `mariadb_1011`
    pub packages: Vec<String>,
    /// Names of the taken-over Compose services; in the chest they point to 127.0.0.1
    pub hosts: Vec<String>,
}

/// Reads the Compose file in the project; `None` if there is none.
/// `available` is only called when a database version must be chosen.
pub fn detect(
    project: &Path,
    available: &mut dyn FnMut() -> Result<Available>,
) -> Result<Option<Detected>> {
    let Some(file) = FILES.iter().map(|f| project.join(f)).find(|f| f.exists()) else {
        return Ok(None);
    };
    let text = fs::read_to_string(&file)?;
    let compose: ComposeFile =
        serde_yaml_ng::from_str(&text).with_context(|| format!("{} is invalid", file.display()))?;
    let dotenv = read_dotenv(&project.join(".env"));
    let mut d = Detected { file, ..Detected::default() };
    for (name, svc) in &compose.services {
        let Some(image) = svc.image.as_deref().map(|i| interpolate(i, &dotenv)) else {
            d.skipped.push(format!("{name} (custom build)"));
            continue;
        };
        let env = environment(&svc.environment, &dotenv);
        let (repo, tag) = split_image(&image);
        match repo {
            "mariadb" => mariadb(&mut d, name, tag, &env, available)?,
            "postgres" => postgres(&mut d, name, tag, &env, available)?,
            "redis" | "valkey" => redis(&mut d, name, repo),
            "mailpit" | "mailhog" => mailpit(&mut d, name, repo),
            _ => {
                d.skipped.push(format!("{name} ({image})"));
                continue;
            }
        }
        d.hosts.push(name.clone());
    }
    Ok(Some(d))
}

/// Reads a simple `.env` file: `KEY=value`, with quotes removed.
fn read_dotenv(path: &Path) -> BTreeMap<String, String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, v) = l.trim().split_once('=')?;
            if k.starts_with('#') {
                return None;
            }
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_owned(), v.to_owned()))
        })
        .collect()
}

/// Replaces `${VAR}`, `${VAR:-default}` and `${VAR-default}` like Compose (values from `.env`).
fn interpolate(s: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out += &rest[..start];
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        let expr = &rest[start + 2..start + end];
        let (var, default) =
            expr.split_once(":-").or_else(|| expr.split_once('-')).unwrap_or((expr, ""));
        match vars.get(var).filter(|v| !v.is_empty()) {
            Some(v) => out += v,
            None => out += default,
        }
        rest = &rest[start + end + 1..];
    }
    out + rest
}

/// Reads `environment:` as a map or as a list of `KEY=value`.
fn environment(value: &Value, vars: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let scalar = |v: &Value| match v {
        Value::String(s) => Some(interpolate(s, vars)),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    match value {
        Value::Mapping(m) => {
            m.iter().filter_map(|(k, v)| Some((k.as_str()?.to_owned(), scalar(v)?))).collect()
        }
        Value::Sequence(list) => list
            .iter()
            .filter_map(|e| {
                let (k, v) = e.as_str()?.split_once('=')?;
                Some((k.to_owned(), interpolate(v, vars)))
            })
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// `docker.io/library/mariadb:10.11` → `("mariadb", "10.11")`
fn split_image(image: &str) -> (&str, &str) {
    let last = image.rsplit('/').next().unwrap_or(image);
    last.split_once(':').unwrap_or((last, "latest"))
}

/// Leading version number of the tag: `10.11` → (10, Some(11)), `17-alpine` → (17, None).
fn tag_version(tag: &str) -> Option<(u32, Option<u32>)> {
    let head = tag.split(['-', '_']).next()?;
    let mut parts = head.split('.');
    let major = parts.next()?.parse().ok()?;
    Some((major, parts.next().and_then(|p| p.parse().ok())))
}

/// First non-empty value for one of the `keys`.
fn first<'a>(env: &'a BTreeMap<String, String>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| env.get(*k)).map(String::as_str).filter(|v| !v.is_empty())
}

/// SQL string in single quotes; a `'` in the text is doubled.
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// MariaDB identifier in backticks; backticks in the name are doubled.
fn sql_ident(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

/// Service that runs once and then exits (for init and setup).
fn once(command: String, depends_on: &[String]) -> Service {
    Service { command, depends_on: depends_on.to_vec(), once: true, ..Service::default() }
}

/// Takes over a MariaDB service: creates the data directory, starts the server, creates database and user from `environment`.
fn mariadb(
    d: &mut Detected,
    name: &str,
    tag: &str,
    env: &BTreeMap<String, String>,
    available: &mut dyn FnMut() -> Result<Available>,
) -> Result<()> {
    let attr = if let Some((major, minor)) = tag_version(tag) {
        let wanted = (major, minor.unwrap_or(0));
        let versions = available()?.mariadb;
        let (a, b) = pick(&versions, wanted).context("nixpkgs has no MariaDB")?;
        if (a, b) != wanted {
            d.notes.push(format!(
                "{name}: MariaDB {major}.{} is not in nixpkgs, using {a}.{b}",
                minor.unwrap_or(0)
            ));
        }
        d.found.insert(name.into(), format!("mariadb {a}.{b}"));
        format!("mariadb_{a}{b}")
    } else {
        d.found.insert(name.into(), "mariadb".into());
        "mariadb".into()
    };
    d.packages.push(attr);
    let data = format!("/data/{name}");
    let sock = format!("/run/luggage/{name}.sock");
    let init = format!("{name}-init");
    d.services.insert(
        init.clone(),
        once(
            format!(
                "test -d {data}/mysql || mariadb-install-db --no-defaults --auth-root-authentication-method=normal --skip-test-db --datadir={data}"
            ),
            &[],
        ),
    );
    d.services.insert(
        name.into(),
        Service {
            command: format!(
                "exec mariadbd --no-defaults --datadir={data} --socket={sock} --port=3306 --bind-address=127.0.0.1"
            ),
            depends_on: vec![init],
            ready: Some(format!("mariadb-admin --no-defaults -u root -S {sock} ping")),
            ..Service::default()
        },
    );
    let db = first(env, &["MARIADB_DATABASE", "MYSQL_DATABASE"]);
    let user = first(env, &["MARIADB_USER", "MYSQL_USER"]);
    let password = first(env, &["MARIADB_PASSWORD", "MYSQL_PASSWORD"]).unwrap_or("");
    let mut sql = Vec::new();
    if let Some(db) = db {
        sql.push(format!("CREATE DATABASE IF NOT EXISTS {}", sql_ident(db)));
    }
    if let Some(user) = user {
        let u = format!("{}@'%'", sql_str(user));
        sql.push(format!("CREATE USER IF NOT EXISTS {u} IDENTIFIED BY {}", sql_str(password)));
        let on = db.map_or_else(|| "*.*".into(), |db| format!("{}.*", sql_ident(db)));
        sql.push(format!("GRANT ALL ON {on} TO {u}"));
    }
    if first(env, &["MARIADB_ROOT_PASSWORD", "MYSQL_ROOT_PASSWORD"]).is_some() {
        d.notes.push(format!(
            "{name}: root password skipped, root works only over the socket without a password"
        ));
    }
    if !sql.is_empty() {
        d.services.insert(
            format!("{name}-setup"),
            once(
                format!("mariadb --no-defaults -u root -S {sock} -e {}", sh_quote(&sql.join("; "))),
                &[name.to_owned()],
            ),
        );
    }
    Ok(())
}

/// Takes over a PostgreSQL service: runs `initdb`, starts the server, creates the database from `POSTGRES_DB`.
fn postgres(
    d: &mut Detected,
    name: &str,
    tag: &str,
    env: &BTreeMap<String, String>,
    available: &mut dyn FnMut() -> Result<Available>,
) -> Result<()> {
    let attr = if let Some((major, _)) = tag_version(tag) {
        let versions = available()?.postgresql;
        let v = pick(&versions, major).context("nixpkgs has no PostgreSQL")?;
        if v != major {
            d.notes.push(format!("{name}: PostgreSQL {major} is not in nixpkgs, using {v}"));
        }
        d.found.insert(name.into(), format!("postgres {v}"));
        format!("postgresql_{v}")
    } else {
        d.found.insert(name.into(), "postgres".into());
        "postgresql".into()
    };
    d.packages.push(attr);
    let data = format!("/data/{name}");
    let user = first(env, &["POSTGRES_USER"]).unwrap_or("postgres");
    let db = first(env, &["POSTGRES_DB"]).unwrap_or(user);
    let init = format!("{name}-init");
    // trust: any password works, the chest is the boundary
    d.services.insert(
        init.clone(),
        once(
            format!(
                "test -f {data}/PG_VERSION || initdb --no-instructions --auth=trust --encoding=UTF8 -U {} -D {data}",
                sh_quote(user)
            ),
            &[],
        ),
    );
    d.services.insert(
        name.into(),
        Service {
            command: format!("exec postgres -D {data} -k /run/luggage -h 127.0.0.1 -p 5432"),
            depends_on: vec![init],
            ready: Some(format!("pg_isready -h 127.0.0.1 -p 5432 -U {}", sh_quote(user))),
            ..Service::default()
        },
    );
    if db != "postgres" {
        let check = format!("SELECT 1 FROM pg_database WHERE datname = {}", sql_str(db));
        d.services.insert(
            format!("{name}-setup"),
            once(
                format!(
                    "psql -h 127.0.0.1 -U {u} -d postgres -tAc {} | grep -q 1 || createdb -h 127.0.0.1 -U {u} {}",
                    sh_quote(&check),
                    sh_quote(db),
                    u = sh_quote(user)
                ),
                &[name.to_owned()],
            ),
        );
    }
    Ok(())
}

/// Takes over a Redis or Valkey service on port 6379.
fn redis(d: &mut Detected, name: &str, repo: &str) {
    let data = format!("/data/{name}");
    d.found.insert(name.into(), repo.into());
    d.packages.push(repo.into());
    d.services.insert(
        name.into(),
        Service {
            command: format!(
                "mkdir -p {data} && exec {repo}-server --bind 127.0.0.1 --port 6379 --dir {data}"
            ),
            ready: Some(format!("{repo}-cli -p 6379 ping")),
            ..Service::default()
        },
    );
}

/// Takes over Mailpit (SMTP 1025, web 8025); Mailhog is replaced by Mailpit.
fn mailpit(d: &mut Detected, name: &str, repo: &str) {
    let data = format!("/data/{name}");
    if repo == "mailhog" {
        d.notes.push(format!("{name}: mailhog is replaced by mailpit (same ports 1025/8025)"));
    }
    d.found.insert(name.into(), "mailpit".into());
    d.packages.push("mailpit".into());
    d.services.insert(
        name.into(),
        Service {
            command: format!(
                "mkdir -p {data} && exec mailpit --smtp 127.0.0.1:1025 --listen 127.0.0.1:8025 --database {data}/mailpit.db"
            ),
            ..Service::default()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::versions::fixture;

    const SPARENCON: &str = r"
services:
  php:
    build: ./docker/php
  nginx:
    image: nginx:1.27-alpine
  database:
    image: mariadb:10.11
    environment:
      MARIADB_DATABASE: sparrencon
      MARIADB_USER: sparrencon
      MARIADB_PASSWORD: sparrencon
      MARIADB_ROOT_PASSWORD: root
  mailer:
    image: axllent/mailpit:latest
";

    fn run(compose: &str, dotenv: &str) -> Detected {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("compose.yaml"), compose).unwrap();
        fs::write(dir.path().join(".env"), dotenv).unwrap();
        detect(dir.path(), &mut || Ok(fixture())).unwrap().unwrap()
    }

    #[test]
    fn no_compose_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect(dir.path(), &mut || Ok(fixture())).unwrap().is_none());
    }

    #[test]
    fn detects_sparencon() {
        let d = run(SPARENCON, "");
        assert_eq!(d.found["database"], "mariadb 10.11");
        assert_eq!(d.found["mailer"], "mailpit");
        assert_eq!(d.skipped, ["nginx (nginx:1.27-alpine)", "php (custom build)"]);
        assert_eq!(d.hosts, ["database", "mailer"]);
        assert_eq!(d.packages, ["mariadb_1011", "mailpit"]);
        let setup = &d.services["database-setup"];
        assert!(setup.once);
        assert_eq!(setup.depends_on, ["database"]);
        assert!(setup.command.contains(
            r#"CREATE USER IF NOT EXISTS '"'"'sparrencon'"'"'@'"'"'%'"'"' IDENTIFIED BY '"'"'sparrencon'"'"'"#
        ));
        assert_eq!(d.services["database"].depends_on, ["database-init"]);
        assert!(d.notes.iter().any(|n| n.contains("root password")));
    }

    #[test]
    fn postgres_with_interpolated_version_and_list_env() {
        let d = run(
            "services:\n  db:\n    image: postgres:${POSTGRES_VERSION:-17}-alpine\n    environment:\n      - POSTGRES_USER=app\n      - POSTGRES_DB=${DB_NAME}\n",
            "DB_NAME=\"shop\"\n",
        );
        assert_eq!(d.found["db"], "postgres 17");
        assert_eq!(d.packages, ["postgresql_17"]);
        assert!(d.services["db-init"].command.contains("-U 'app'"));
        assert!(d.services["db-setup"].command.ends_with("createdb -h 127.0.0.1 -U 'app' 'shop'"));
    }

    #[test]
    fn picks_next_version_and_notes_it() {
        let d = run(
            "services:\n  db:\n    image: postgres:13\n  cache:\n    image: valkey/valkey:9.1.2-alpine\n  mail:\n    image: mailhog/mailhog\n",
            "",
        );
        assert_eq!(d.found["db"], "postgres 14");
        assert!(!d.services.contains_key("db-setup"));
        assert_eq!(d.found["cache"], "valkey");
        assert!(d.services["cache"].command.contains("valkey-server"));
        assert_eq!(d.found["mail"], "mailpit");
        assert_eq!(d.notes.len(), 2);
    }

    #[test]
    fn interpolation() {
        let vars = BTreeMap::from([("A".to_owned(), "1".to_owned())]);
        assert_eq!(interpolate("x${A}y${B:-2}z${C-3}", &vars), "x1y2z3");
        assert_eq!(interpolate("${B}", &vars), "");
    }

    #[test]
    fn image_parts() {
        assert_eq!(split_image("docker.io/library/mariadb:10.11"), ("mariadb", "10.11"));
        assert_eq!(split_image("localhost:5000/pg"), ("pg", "latest"));
        assert_eq!(tag_version("17-alpine"), Some((17, None)));
        assert_eq!(tag_version("latest"), None);
    }
}
