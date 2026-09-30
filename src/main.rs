//! luggage — Projekt anlegen/klonen und eine Nix-Dev-Umgebung (flake + direnv) einrichten.

mod detect;
mod flake;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

/// Projekt anlegen/klonen und eine Nix-Dev-Umgebung (flake + direnv) einrichten.
///
/// Versionen werden aus composer.json / package.json / .nvmrc erkannt,
/// --php und --node überschreiben das. Committet wird nichts.
#[derive(Parser)]
#[command(name = "luggage", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Repo klonen (URL oder lokaler Pfad) oder neues Projekt anlegen
    New {
        /// git-URL, lokaler Repo-Pfad oder Projektname
        target: String,
        /// Zielverzeichnis [Standard: ~/projects]
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Privates GitLab-Projekt anlegen, optional in GRUPPE
        #[arg(long, value_name = "GRUPPE", num_args = 0..=1, default_missing_value = "")]
        gitlab: Option<String>,
        /// GitLab-Host [Standard: Host aus der glab-Config]
        #[arg(long)]
        host: Option<String>,
        #[command(flatten)]
        env: EnvOpts,
    },
    /// Umgebung in einem bestehenden Repo einrichten
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[command(flatten)]
        env: EnvOpts,
    },
}

#[derive(Args)]
struct EnvOpts {
    /// PHP-Version, z.B. 8.3
    #[arg(long)]
    php: Option<String>,
    /// Node-Major, z.B. 22
    #[arg(long)]
    node: Option<String>,
    /// Bestehende flake.nix überschreiben
    #[arg(long)]
    force: bool,
    /// Umgebung nicht vorab bauen
    #[arg(long)]
    no_build: bool,
}

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

fn run_ok(cwd: &Path, program: &str, args: &[&str]) -> Result<()> {
    if !run(cwd, program, args, &[], false)? {
        bail!("fehlgeschlagen: {program} {}", args.join(" "));
    }
    Ok(())
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn expand_tilde(p: &str) -> PathBuf {
    p.strip_prefix("~/").map_or_else(|| PathBuf::from(p), |rest| home().join(rest))
}

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

fn ensure_gitignore(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    let content = fs::read_to_string(&path).unwrap_or_default();
    if content
        .lines()
        .any(|l| matches!(l.trim(), ".direnv" | ".direnv/" | "/.direnv" | "/.direnv/"))
    {
        return Ok(());
    }
    let sep = if content.is_empty() || content.ends_with('\n') { "" } else { "\n" };
    let gap = if content.trim().is_empty() { "" } else { "\n" };
    fs::write(&path, format!("{content}{sep}{gap}# nix-direnv Cache\n.direnv/\n"))?;
    Ok(())
}

fn setup_env(root: &Path, opts: &EnvOpts) -> Result<()> {
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

    let php = detect::detect_php(root, opts.php.as_deref())?;
    let node = detect::detect_node(root, opts.node.as_deref())?;
    if let Some(p) = &php {
        let exts = if p.exts.is_empty() {
            String::new()
        } else {
            format!(", Extensions: {}", p.exts.join(", "))
        };
        info(&format!("PHP {}.{} ({}){exts}", p.version.0, p.version.1, p.source));
    }
    if let Some(n) = &node {
        let version = n.version.map_or_else(|| "Standard".into(), |v| v.to_string());
        let tool = n.tool.map(|t| format!(" + {t}")).unwrap_or_default();
        info(&format!("Node {version} ({}){tool}", n.source));
    }
    if php.is_none() && node.is_none() {
        info("kein PHP/Node erkannt — leere Shell, Pakete in flake.nix unter packages eintragen");
    }

    let name = root.file_name().map_or_else(|| "projekt".into(), |n| n.to_string_lossy());
    fs::write(&flake_path, flake::render(&name, php.as_ref(), node.as_ref()))?;
    fs::write(root.join(".envrc"), "use flake\n")?;
    ensure_gitignore(root)?;
    // erst stagen: nix sieht nur Dateien, die git kennt
    run_ok(root, "git", &["add", "flake.nix", ".envrc", ".gitignore"])?;
    run_ok(root, "nix", &["flake", "lock"])?;
    run_ok(root, "git", &["add", "flake.lock"])?;
    let root_str = root.to_string_lossy();
    run_ok(root, "direnv", &["allow", &root_str])?;

    if !opts.no_build {
        info("baue Umgebung (beim ersten Mal kann das dauern) ...");
        let mut checks = Vec::new();
        if php.is_some() {
            checks.extend([
                r#"php -r 'echo "php ", PHP_VERSION, PHP_EOL;'"#,
                "composer --version 2>/dev/null | head -1",
            ]);
        }
        if node.is_some() {
            checks.push("echo node $(node -v)");
        }
        let script = if checks.is_empty() { "true".into() } else { checks.join("; ") };
        run_ok(root, "direnv", &["exec", &root_str, "sh", "-c", &script])?;
    }

    info(&format!("fertig: {}", root.display()));
    info("gestaged: flake.nix flake.lock .envrc .gitignore — Commit liegt bei dir");
    Ok(())
}

fn cmd_new(
    target: &str,
    dir: Option<PathBuf>,
    gitlab: Option<&str>,
    host: Option<&str>,
    opts: &EnvOpts,
) -> Result<()> {
    let remote = is_remote(target);
    let name = if remote { repo_name(target) } else { target };
    let root = dir.unwrap_or_else(|| home().join("projects")).join(name);
    if root.exists() {
        bail!("{} existiert schon — dort luggage init benutzen", root.display());
    }
    let root_str = root.to_string_lossy();

    if remote {
        if gitlab.is_some() {
            bail!("--gitlab nur ohne URL (legt ein neues Projekt an)");
        }
        let source = expand_tilde(target);
        let source =
            if source.exists() { source.to_string_lossy().into_owned() } else { target.to_owned() };
        run_ok(Path::new("."), "git", &["clone", &source, &root_str])?;
    } else {
        fs::create_dir_all(&root)?;
        run_ok(&root, "git", &["init", "-q", "-b", "main"])?;
        if let Some(group) = gitlab {
            let path = if group.is_empty() { name.to_owned() } else { format!("{group}/{name}") };
            let on = host.map(|h| format!(" auf {h}")).unwrap_or_default();
            info(&format!("lege privates Projekt {path}{on} an"));
            let args = ["repo", "create", &path, "--private", "--defaultBranch", "main"];
            let env: Vec<(&str, &str)> = host.map(|h| ("GITLAB_HOST", h)).into_iter().collect();
            if !run(&root, "glab", &args, &env, false)? {
                bail!("glab repo create fehlgeschlagen");
            }
            if !run(&root, "git", &["remote", "get-url", "origin"], &[], true)? {
                info("WARNUNG: glab hat kein Remote 'origin' gesetzt — bitte manuell hinzufügen");
            }
        }
    }
    setup_env(&root, opts)
}

fn main() -> ExitCode {
    let result = match Cli::parse().cmd {
        Cmd::New { target, dir, gitlab, host, env } => {
            cmd_new(&target, dir, gitlab.as_deref(), host.as_deref(), &env)
        }
        Cmd::Init { path, env } => std::path::absolute(&path)
            .map_err(anyhow::Error::from)
            .and_then(|p| setup_env(&p, &env)),
    };
    match result {
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
    fn gitignore_gets_direnv_once() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "/vendor").unwrap();
        ensure_gitignore(dir.path()).unwrap();
        ensure_gitignore(dir.path()).unwrap();
        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(content, "/vendor\n\n# nix-direnv Cache\n.direnv/\n");
    }
}
