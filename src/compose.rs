//! Erkennt Dienste aus der Docker-Compose-Datei und übersetzt bekannte Images in Truhen-Dienste.
//!
//! Nur Standard-Software (mariadb, postgres, redis/valkey, mailpit/mailhog); alles andere wird gemeldet, nicht geraten.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_yaml_ng::Value;

use crate::detect::pick;
use crate::truhe::{Service, sh_quote};
use crate::versions::Available;

/// Dateinamen, nach denen gesucht wird; die erste vorhandene gewinnt.
const FILES: [&str; 4] =
    ["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];

/// Der Teil der Compose-Datei, der gelesen wird.
#[derive(Deserialize)]
struct ComposeFile {
    /// Compose-Dienste nach Name
    #[serde(default)]
    services: BTreeMap<String, ComposeService>,
}

/// Ein Dienst aus der Compose-Datei.
#[derive(Deserialize)]
struct ComposeService {
    /// Image, z.B. `mariadb:10.11`; fehlt bei eigenem Build
    image: Option<String>,
    /// `environment:` als Map oder Liste, noch nicht ausgewertet
    #[serde(default)]
    environment: Value,
}

/// Was aus der Compose-Datei übernommen wurde.
#[derive(Debug, Default)]
pub struct Detected {
    /// Gelesene Compose-Datei
    pub file: PathBuf,
    /// Compose-Dienst → Beschreibung, z.B. `database` → `mariadb 10.11`
    pub found: BTreeMap<String, String>,
    /// nicht übernommene Compose-Dienste mit Grund
    pub skipped: Vec<String>,
    /// Hinweise, z.B. abweichende Version
    pub notes: Vec<String>,
    /// Truhen-Dienste, inkl. einmaliger `-init`- und `-setup`-Dienste
    pub services: BTreeMap<String, Service>,
    /// nixpkgs-Pakete für die Dienste, z.B. `mariadb_1011`
    pub packages: Vec<String>,
    /// Namen der übernommenen Compose-Dienste; zeigen in der Truhe auf 127.0.0.1
    pub hosts: Vec<String>,
}

/// Liest die Compose-Datei im Projekt; `None`, wenn es keine gibt.
/// `available` wird nur gefragt, wenn eine Datenbank-Version gewählt werden muss.
pub fn detect(
    project: &Path,
    available: &mut dyn FnMut() -> Result<Available>,
) -> Result<Option<Detected>> {
    let Some(file) = FILES.iter().map(|f| project.join(f)).find(|f| f.exists()) else {
        return Ok(None);
    };
    let text = fs::read_to_string(&file)?;
    let compose: ComposeFile =
        serde_yaml_ng::from_str(&text).with_context(|| format!("{} fehlerhaft", file.display()))?;
    let dotenv = read_dotenv(&project.join(".env"));
    let mut d = Detected { file, ..Detected::default() };
    for (name, svc) in &compose.services {
        let Some(image) = svc.image.as_deref().map(|i| interpolate(i, &dotenv)) else {
            d.skipped.push(format!("{name} (eigenes Build)"));
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

/// Einfache `.env`-Datei: `KEY=wert`, Anführungszeichen werden entfernt.
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

/// Ersetzt `${VAR}`, `${VAR:-standard}` und `${VAR-standard}` wie Compose (Werte aus `.env`).
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

/// `environment:` als Map oder Liste `KEY=wert`.
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

/// Führende Versionsnummer aus dem Tag: `10.11` → (10, Some(11)), `17-alpine` → (17, None).
fn tag_version(tag: &str) -> Option<(u32, Option<u32>)> {
    let head = tag.split(['-', '_']).next()?;
    let mut parts = head.split('.');
    let major = parts.next()?.parse().ok()?;
    Some((major, parts.next().and_then(|p| p.parse().ok())))
}

/// Erster nicht-leerer Wert zu einem der `keys`.
fn first<'a>(env: &'a BTreeMap<String, String>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| env.get(*k)).map(String::as_str).filter(|v| !v.is_empty())
}

/// SQL-String in einfachen Anführungszeichen; `'` im Text wird verdoppelt.
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// MariaDB-Bezeichner in Backticks; Backticks im Namen werden verdoppelt.
fn sql_ident(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

/// Dienst, der einmal läuft und sich dann beendet (für Init und Setup).
fn once(command: String, depends_on: &[String]) -> Service {
    Service { command, depends_on: depends_on.to_vec(), once: true, ..Service::default() }
}

/// Übernimmt einen MariaDB-Dienst: Datenverzeichnis anlegen, Server starten, Datenbank und Nutzer aus `environment` anlegen.
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
        let (a, b) = pick(&versions, wanted).context("nixpkgs hat kein MariaDB")?;
        if (a, b) != wanted {
            d.notes.push(format!(
                "{name}: MariaDB {major}.{} gibt es nicht in nixpkgs, nehme {a}.{b}",
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
            "{name}: root-Passwort nicht übernommen, root geht nur über den Socket ohne Passwort"
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

/// Übernimmt einen PostgreSQL-Dienst: `initdb`, Server starten, Datenbank aus `POSTGRES_DB` anlegen.
fn postgres(
    d: &mut Detected,
    name: &str,
    tag: &str,
    env: &BTreeMap<String, String>,
    available: &mut dyn FnMut() -> Result<Available>,
) -> Result<()> {
    let attr = if let Some((major, _)) = tag_version(tag) {
        let versions = available()?.postgresql;
        let v = pick(&versions, major).context("nixpkgs hat kein PostgreSQL")?;
        if v != major {
            d.notes.push(format!("{name}: PostgreSQL {major} gibt es nicht in nixpkgs, nehme {v}"));
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
    // trust: jedes Passwort passt, die Truhe ist die Grenze
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

/// Übernimmt einen Redis- oder Valkey-Dienst auf Port 6379.
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

/// Übernimmt Mailpit (SMTP 1025, Web 8025); Mailhog wird durch Mailpit ersetzt.
fn mailpit(d: &mut Detected, name: &str, repo: &str) {
    let data = format!("/data/{name}");
    if repo == "mailhog" {
        d.notes
            .push(format!("{name}: mailhog wird durch mailpit ersetzt (gleiche Ports 1025/8025)"));
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
        assert_eq!(d.skipped, ["nginx (nginx:1.27-alpine)", "php (eigenes Build)"]);
        assert_eq!(d.hosts, ["database", "mailer"]);
        assert_eq!(d.packages, ["mariadb_1011", "mailpit"]);
        let setup = &d.services["database-setup"];
        assert!(setup.once);
        assert_eq!(setup.depends_on, ["database"]);
        assert!(setup.command.contains(
            r#"CREATE USER IF NOT EXISTS '"'"'sparrencon'"'"'@'"'"'%'"'"' IDENTIFIED BY '"'"'sparrencon'"'"'"#
        ));
        assert_eq!(d.services["database"].depends_on, ["database-init"]);
        assert!(d.notes.iter().any(|n| n.contains("root-Passwort")));
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
