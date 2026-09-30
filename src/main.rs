//! luggage — Projekt anlegen/klonen und eine Nix-Dev-Umgebung (flake + direnv) einrichten.

mod compose;
mod config;
mod detect;
mod flake;
mod remote;
mod truhe;
mod versions;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use config::Config;
use remote::Forge;

/// Projekt anlegen/klonen und eine Nix-Dev-Umgebung (flake + direnv) einrichten.
///
/// Erkennt PHP, Node, Python und Rust aus den Projektdateien. Standards stehen in
/// ~/.config/luggage/config.toml (siehe `luggage config`). Committet wird nichts.
#[derive(Parser)]
#[command(name = "luggage", version)]
struct Cli {
    /// Unterbefehl
    #[command(subcommand)]
    cmd: Cmd,
}

/// Unterbefehle von luggage.
#[derive(Subcommand)]
enum Cmd {
    /// Repo klonen (URL oder lokaler Pfad) oder neues Projekt anlegen
    New {
        /// git-URL, lokaler Repo-Pfad oder Projektname
        target: String,
        /// Zielverzeichnis [Standard: Projektordner aus der Config]
        #[arg(long)]
        dir: Option<PathBuf>,
        /// GitLab-Projekt anlegen, optional in GRUPPE
        #[arg(long, value_name = "GRUPPE", num_args = 0..=1, default_missing_value = "", conflicts_with = "github")]
        gitlab: Option<String>,
        /// GitHub-Repo anlegen, optional unter OWNER (Organisation)
        #[arg(long, value_name = "OWNER", num_args = 0..=1, default_missing_value = "")]
        github: Option<String>,
        /// GitLab-Host [Standard: Config, sonst glab]
        #[arg(long, requires = "gitlab")]
        host: Option<String>,
        /// Sichtbarkeit des neuen Remote-Projekts [Standard: Config, sonst private]
        #[arg(long, value_parser = ["private", "internal", "public"])]
        visibility: Option<String>,
        /// Optionen für die Nix-Umgebung
        #[command(flatten)]
        env: EnvOpts,
    },
    /// Umgebung in einem bestehenden Repo einrichten
    Init {
        /// Verzeichnis des Repos
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Optionen für die Nix-Umgebung
        #[command(flatten)]
        env: EnvOpts,
    },
    /// Wirksame Config anzeigen
    Config {
        /// Kommentierte Vorlage anlegen (überschreibt nie)
        #[arg(long)]
        init: bool,
    },
    /// Befehl in einer frischen Truhe ausführen (ohne Befehl: Shell)
    Run {
        /// Internet in der Truhe erlauben
        #[arg(long)]
        net: bool,
        /// Befehl mit Argumenten
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Truhe mit ihren Diensten im Hintergrund starten
    Up,
    /// Befehl in der laufenden Truhe ausführen
    Exec {
        /// Befehl mit Argumenten
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Shell in der laufenden Truhe öffnen
    Open,
    /// Laufende Truhe beenden
    Down,
    /// Zeigen, ob die Truhe läuft und wie es den Diensten geht
    Status,
}

/// Optionen für die Nix-Umgebung (`new` und `init`).
#[derive(Args)]
struct EnvOpts {
    /// PHP-Version, z.B. 8.3
    #[arg(long)]
    php: Option<String>,
    /// Node-Major, z.B. 22
    #[arg(long)]
    node: Option<String>,
    /// Python-Version, z.B. 3.12
    #[arg(long)]
    python: Option<String>,
    /// Bestehende flake.nix überschreiben
    #[arg(long)]
    force: bool,
    /// Umgebung nicht vorab bauen
    #[arg(long)]
    no_build: bool,
}

/// Gibt eine Meldung mit `luggage:` davor aus.
fn info(msg: &str) {
    println!("luggage: {msg}");
}

/// Führt ein Kommando aus; `quiet` verschluckt die Ausgabe. Liefert, ob es erfolgreich war.
fn run(
    cwd: &Path,
    program: &str,
    args: &[&str],
    env: &[(&str, &str)],
    quiet: bool,
) -> Result<bool> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd).envs(env.iter().copied());
    if quiet {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let status = cmd.status().with_context(|| format!("{program} nicht startbar"))?;
    Ok(status.success())
}

/// Wie `run`, aber Fehler, wenn das Kommando scheitert.
fn run_ok(cwd: &Path, program: &str, args: &[&str]) -> Result<()> {
    if !run(cwd, program, args, &[], false)? {
        bail!("fehlgeschlagen: {program} {}", args.join(" "));
    }
    Ok(())
}

/// `$HOME`, sonst das aktuelle Verzeichnis.
fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Ersetzt ein führendes `~/` durch `$HOME`.
fn expand_tilde(p: &str) -> PathBuf {
    p.strip_prefix("~/").map_or_else(|| PathBuf::from(p), |rest| home().join(rest))
}

/// Ob `target` geklont wird: URL, scp-artig (`git@host:pfad`) oder lokales Repo.
fn is_remote(target: &str) -> bool {
    let scp_like =
        target.split_once(':').is_some_and(|(host, _)| host.contains('@') && !host.contains('/'));
    target.contains("://") || scp_like || expand_tilde(target).join(".git").exists()
}

/// `git@host:gruppe/name.git` → `name`
fn repo_name(target: &str) -> &str {
    let last = target.trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or(target);
    last.strip_suffix(".git").unwrap_or(last)
}

/// Hängt `entry` (z.B. `.direnv/`) an .gitignore, falls es in keiner Schreibweise drinsteht.
fn ensure_ignored(root: &Path, entry: &str, comment: &str) -> Result<()> {
    let path = root.join(".gitignore");
    let content = fs::read_to_string(&path).unwrap_or_default();
    let bare = entry.trim_matches('/');
    if content.lines().any(|l| l.trim().trim_matches('/') == bare) {
        return Ok(());
    }
    let sep = if content.is_empty() || content.ends_with('\n') { "" } else { "\n" };
    let gap = if content.trim().is_empty() { "" } else { "\n" };
    fs::write(&path, format!("{content}{sep}{gap}# {comment}\n{entry}\n"))?;
    Ok(())
}

/// Was im Projekt erkannt wurde.
struct Detected {
    /// PHP, falls erkannt
    php: Option<detect::Php>,
    /// Node, falls erkannt
    node: Option<detect::Node>,
    /// Python, falls erkannt
    python: Option<detect::Python>,
    /// Rust, falls erkannt
    rust: Option<detect::Rust>,
}

impl Detected {
    /// Meldet, was erkannt wurde, samt Warnungen und Zusatzpaketen.
    fn report(&self, rustc: Option<(u32, u32)>, extra: &[String]) {
        let with_tool = |tool: Option<&str>| tool.map(|t| format!(" + {t}")).unwrap_or_default();
        if let Some(p) = &self.php {
            let exts = if p.exts.is_empty() {
                String::new()
            } else {
                format!(", Extensions: {}", p.exts.join(", "))
            };
            info(&format!("PHP {}.{} ({}){exts}", p.version.0, p.version.1, p.source));
        }
        if let Some(n) = &self.node {
            let version = n.version.map_or_else(|| "Standard".into(), |v| v.to_string());
            info(&format!("Node {version} ({}){}", n.source, with_tool(n.tool)));
        }
        if let Some(p) = &self.python {
            let version = p.version.map_or_else(|| "Standard".into(), |(a, b)| format!("{a}.{b}"));
            info(&format!("Python {version} ({}){}", p.source, with_tool(p.tool)));
        }
        if let Some(r) = &self.rust {
            let have = rustc.map(|(a, b)| format!(" {a}.{b}")).unwrap_or_default();
            info(&format!("Rust{have} (nixpkgs)"));
            for w in &r.warnings {
                info(&format!("WARNUNG: {w}"));
            }
        }
        if !extra.is_empty() {
            info(&format!("zusätzliche Pakete (Config): {}", extra.join(", ")));
        }
        if self.php.is_none() && self.node.is_none() && self.python.is_none() && self.rust.is_none()
        {
            info("keine Sprache erkannt — Pakete in flake.nix unter packages eintragen");
        }
    }

    /// Shell-Befehle, die nach dem Bauen die Versionen ausgeben.
    fn checks(&self) -> String {
        let mut checks: Vec<String> = Vec::new();
        if self.php.is_some() {
            checks.push(r#"php -r 'echo "php ", PHP_VERSION, PHP_EOL;'"#.into());
            checks.push("composer --version 2>/dev/null | head -1".into());
        }
        if self.node.is_some() {
            checks.push("echo node $(node -v)".into());
        }
        if let Some(p) = &self.python {
            checks.push("python --version".into());
            if let Some(tool) = p.tool {
                checks.push(format!("{tool} --version"));
            }
        }
        if self.rust.is_some() {
            checks.push("rustc --version".into());
        }
        if checks.is_empty() { "true".into() } else { checks.join("; ") }
    }
}

/// Schreibt flake.nix und .envrc, ergänzt .gitignore, stagt alles und baut die Umgebung.
fn setup_env(root: &Path, opts: &EnvOpts, config: &Config) -> Result<()> {
    if !run(root, "git", &["rev-parse", "--git-dir"], &[], true)? {
        bail!(
            "{} ist kein git-Repo (nix sieht nur getrackte Dateien) — erst git init oder luggage new",
            root.display()
        );
    }
    let flake_path = root.join("flake.nix");
    if flake_path.exists() && !opts.force {
        bail!("{} existiert schon (--force überschreibt)", flake_path.display());
    }

    let nixpkgs = config.nixpkgs();
    let available = versions::query(&nixpkgs)?;
    let ctx = detect::Ctx { root, available: &available, config };
    let found = Detected {
        php: detect::php(&ctx, opts.php.as_deref())?,
        node: detect::node(&ctx, opts.node.as_deref())?,
        python: detect::python(&ctx, opts.python.as_deref())?,
        rust: detect::rust(&ctx),
    };
    let extra = config.nix.packages.clone().unwrap_or_default();
    found.report(available.rustc, &extra);

    let name = root.file_name().map_or_else(|| "projekt".into(), |n| n.to_string_lossy());
    let memory_limit = config.memory_limit();
    let plan = flake::Plan {
        name: &name,
        nixpkgs: &nixpkgs,
        php: found.php.as_ref(),
        memory_limit: &memory_limit,
        node: found.node.as_ref(),
        python: found.python.as_ref(),
        rust: found.rust.is_some(),
        extra_packages: &extra,
    };
    fs::write(&flake_path, flake::render(&plan))?;
    fs::write(root.join(".envrc"), flake::render_envrc(found.python.as_ref()))?;
    ensure_ignored(root, ".direnv/", "nix-direnv Cache")?;
    if found.python.is_some() {
        ensure_ignored(root, ".venv/", "Python venv")?;
    }
    // erst stagen: nix sieht nur Dateien, die git kennt
    run_ok(root, "git", &["add", "flake.nix", ".envrc", ".gitignore"])?;
    run_ok(root, "nix", &["flake", "lock"])?;
    run_ok(root, "git", &["add", "flake.lock"])?;
    let root_str = root.to_string_lossy();
    run_ok(root, "direnv", &["allow", &root_str])?;

    if !opts.no_build {
        info("baue Umgebung (beim ersten Mal kann das dauern) ...");
        run_ok(root, "direnv", &["exec", &root_str, "sh", "-c", &found.checks()])?;
    }

    info(&format!("fertig: {}", root.display()));
    info("gestaged: flake.nix flake.lock .envrc .gitignore — Commit liegt bei dir");
    Ok(())
}

/// Argumente von `luggage new` ohne die Umgebungs-Optionen.
struct NewArgs {
    /// git-URL, lokaler Repo-Pfad oder Projektname
    target: String,
    /// Zielverzeichnis; `None` heißt Projektordner aus der Config
    dir: Option<PathBuf>,
    /// GitLab-Gruppe; `Some` legt ein Projekt an, leer heißt Gruppe aus der Config
    gitlab: Option<String>,
    /// GitHub-Owner; `Some` legt ein Repo an, leer heißt Owner aus der Config
    github: Option<String>,
    /// GitLab-Host
    host: Option<String>,
    /// Sichtbarkeit des neuen Remote-Projekts
    visibility: Option<String>,
}

/// `None` statt leerem String.
fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.is_empty())
}

/// `luggage new`: klont das Repo oder legt ein neues an (optional mit Remote) und richtet die Umgebung ein.
fn cmd_new(args: NewArgs, opts: &EnvOpts, config: &Config) -> Result<()> {
    let target = args.target.as_str();
    let remote = is_remote(target);
    let name = if remote { repo_name(target) } else { target };
    let dir = args.dir.unwrap_or_else(|| expand_tilde(&config.projects_dir()));
    let root = dir.join(name);
    if root.exists() {
        bail!("{} existiert schon — dort luggage init benutzen", root.display());
    }

    let forge = match (args.gitlab, args.github) {
        (Some(group), _) => Some((
            Forge::Gitlab {
                group: non_empty(Some(group)).or_else(|| non_empty(config.gitlab.group.clone())),
                host: args.host.or_else(|| config.gitlab.host.clone()),
            },
            args.visibility.clone().unwrap_or_else(|| config.gitlab_visibility()),
        )),
        (None, Some(owner)) => Some((
            Forge::Github {
                owner: non_empty(Some(owner)).or_else(|| non_empty(config.github.owner.clone())),
            },
            args.visibility.clone().unwrap_or_else(|| config.github_visibility()),
        )),
        (None, None) => None,
    };

    if remote {
        if forge.is_some() {
            bail!("--gitlab/--github nur ohne URL (legt ein neues Projekt an)");
        }
        let source = expand_tilde(target);
        let source =
            if source.exists() { source.to_string_lossy().into_owned() } else { target.to_owned() };
        let root_str = root.to_string_lossy();
        run_ok(Path::new("."), "git", &["clone", &source, &root_str])?;
    } else {
        fs::create_dir_all(&root)?;
        run_ok(&root, "git", &["init", "-q", "-b", "main"])?;
        if let Some((forge, visibility)) = &forge {
            remote::create(&root, name, forge, visibility)?;
        }
    }
    setup_env(&root, opts, config)
}

/// `luggage config`: zeigt die wirksame Config oder legt mit `init` die Vorlage an.
fn cmd_config(init: bool) -> Result<()> {
    if init {
        let path = config::init()?;
        info(&format!("Vorlage geschrieben: {}", path.display()));
        return Ok(());
    }
    let path = config::path();
    let state = if path.exists() {
        ""
    } else {
        " (nicht vorhanden — `luggage config --init` legt sie an)"
    };
    println!("# {}{state}", path.display());
    let rows = config::load()?.describe();
    let width = rows.iter().map(|(k, _, _)| k.len()).max().unwrap_or(0);
    for (key, value, source) in rows {
        println!("{key:width$} = {value}   # {source}");
    }
    Ok(())
}

/// Führt den Unterbefehl aus.
fn dispatch(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Config { init } => cmd_config(init),
        Cmd::New { target, dir, gitlab, github, host, visibility, env } => {
            let args = NewArgs { target, dir, gitlab, github, host, visibility };
            cmd_new(args, &env, &config::load()?)
        }
        Cmd::Init { path, env } => setup_env(&std::path::absolute(&path)?, &env, &config::load()?),
        Cmd::Run { net, cmd } => {
            let nixpkgs = config::load()?.nixpkgs();
            truhe::Truhe::find(&nixpkgs, false)?.cmd_run(net, &cmd, &nixpkgs)
        }
        Cmd::Up => {
            let nixpkgs = config::load()?.nixpkgs();
            truhe::Truhe::find(&nixpkgs, false)?.cmd_up(&nixpkgs)
        }
        Cmd::Exec { cmd } => truhe::Truhe::find(&config::load()?.nixpkgs(), false)?.cmd_exec(&cmd),
        Cmd::Open => truhe::Truhe::find(&config::load()?.nixpkgs(), false)?.cmd_open(),
        Cmd::Down => truhe::Truhe::find(&config::load()?.nixpkgs(), true)?.cmd_down(),
        Cmd::Status => truhe::Truhe::find(&config::load()?.nixpkgs(), false)?.cmd_status(),
    }
}

fn main() -> ExitCode {
    match dispatch(Cli::parse().cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("luggage: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_remote_targets() {
        assert!(is_remote("https://gitlab.example.com/team/nyx.git"));
        assert!(is_remote("git@gitlab.example.com:team/nyx.git"));
        assert!(!is_remote("mein-projekt"));
    }

    #[test]
    fn extracts_repo_name() {
        assert_eq!(repo_name("https://gitlab.example.com/team/nyx.git"), "nyx");
        assert_eq!(repo_name("git@gitlab.example.com:team/nyx.git"), "nyx");
        assert_eq!(repo_name("git@host:nyx"), "nyx");
        assert_eq!(repo_name("/home/x/sparencon/"), "sparencon");
    }

    #[test]
    fn gitignore_entries_added_once() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "/vendor\n/.venv").unwrap();
        ensure_ignored(dir.path(), ".direnv/", "nix-direnv Cache").unwrap();
        ensure_ignored(dir.path(), ".direnv/", "nix-direnv Cache").unwrap();
        ensure_ignored(dir.path(), ".venv/", "Python venv").unwrap();
        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(content, "/vendor\n/.venv\n\n# nix-direnv Cache\n.direnv/\n");
    }
}
