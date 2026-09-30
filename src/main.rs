//! luggage — create or clone a project and set up a Nix dev environment (flake + direnv).

mod chest;
mod compose;
mod config;
mod detect;
mod flake;
mod remote;
mod setup;
mod versions;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use config::Config;
use remote::Forge;

/// Create or clone a project and set up a Nix dev environment (flake + direnv).
///
/// Detects PHP, Node, Python and Rust from the project files. Defaults live in
/// ~/.config/luggage/config.toml (see `luggage config`). Nothing is committed.
#[derive(Parser)]
#[command(name = "luggage", version)]
struct Cli {
    /// Subcommand
    #[command(subcommand)]
    cmd: Cmd,
}

/// Subcommands of luggage.
#[derive(Subcommand)]
enum Cmd {
    /// Clone a repo (URL or local path) or create a new project
    New {
        /// Git URL, local repo path or project name
        target: String,
        /// Target directory [default: projects directory from the config]
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Create a GitLab project, optionally in GROUP
        #[arg(long, value_name = "GROUP", num_args = 0..=1, default_missing_value = "", conflicts_with = "github")]
        gitlab: Option<String>,
        /// Create a GitHub repo, optionally under OWNER (organization)
        #[arg(long, value_name = "OWNER", num_args = 0..=1, default_missing_value = "")]
        github: Option<String>,
        /// GitLab host [default: config, else glab]
        #[arg(long, requires = "gitlab")]
        host: Option<String>,
        /// Visibility of the new remote project [default: config, else private]
        #[arg(long, value_parser = ["private", "internal", "public"])]
        visibility: Option<String>,
        /// Options for the Nix environment
        #[command(flatten)]
        env: EnvOpts,
    },
    /// Set up the environment in an existing repo
    Init {
        /// Directory of the repo
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Options for the Nix environment
        #[command(flatten)]
        env: EnvOpts,
    },
    /// Show the effective config
    Config {
        /// Create a commented template (never overwrites)
        #[arg(long)]
        init: bool,
    },
    /// Check that the system has everything luggage needs
    Doctor,
    /// Install what is missing: Nix, git, direnv, nix-direnv, shell hook (asks before each step)
    Setup {
        /// Answer yes to all questions
        #[arg(long)]
        yes: bool,
    },
    /// Run a command in a fresh chest (no command: a shell)
    Run {
        /// Allow internet access in the chest
        #[arg(long)]
        net: bool,
        /// Command with arguments
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Start the chest and its services in the background
    Up,
    /// Run a command in the running chest
    Exec {
        /// Command with arguments
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Open a shell in the running chest
    Open,
    /// Stop the running chest
    Down,
    /// Show whether the chest is running and how its services are doing
    Status,
}

/// Options for the Nix environment (`new` and `init`).
#[derive(Args)]
struct EnvOpts {
    /// PHP version, e.g. 8.3
    #[arg(long)]
    php: Option<String>,
    /// Node major version, e.g. 22
    #[arg(long)]
    node: Option<String>,
    /// Python version, e.g. 3.12
    #[arg(long)]
    python: Option<String>,
    /// Overwrite an existing flake.nix
    #[arg(long)]
    force: bool,
    /// Do not build the environment up front
    #[arg(long)]
    no_build: bool,
}

/// Prints a message prefixed with `luggage:`.
fn info(msg: &str) {
    println!("luggage: {msg}");
}

/// Runs a command and returns whether it succeeded. `quiet` hides its output.
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
    let status = cmd.status().with_context(|| format!("cannot start {program}"))?;
    Ok(status.success())
}

/// Like `run`, but returns an error if the command fails.
fn run_ok(cwd: &Path, program: &str, args: &[&str]) -> Result<()> {
    if !run(cwd, program, args, &[], false)? {
        bail!("failed: {program} {}", args.join(" "));
    }
    Ok(())
}

/// `$HOME`, or the current directory if it is not set.
fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Finds an executable program in `PATH`.
fn find_in_path(program: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// Replaces a leading `~/` with `$HOME`.
fn expand_tilde(p: &str) -> PathBuf {
    p.strip_prefix("~/").map_or_else(|| PathBuf::from(p), |rest| home().join(rest))
}

/// Whether `target` gets cloned: a URL, scp-like (`git@host:path`) or a local repo.
fn is_remote(target: &str) -> bool {
    let scp_like =
        target.split_once(':').is_some_and(|(host, _)| host.contains('@') && !host.contains('/'));
    target.contains("://") || scp_like || expand_tilde(target).join(".git").exists()
}

/// `git@host:group/name.git` → `name`
fn repo_name(target: &str) -> &str {
    let last = target.trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or(target);
    last.strip_suffix(".git").unwrap_or(last)
}

/// Appends `entry` (e.g. `.direnv/`) to .gitignore. Skips it if any spelling of it is already there.
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

/// What was detected in the project.
struct Detected {
    /// PHP, if detected
    php: Option<detect::Php>,
    /// Node, if detected
    node: Option<detect::Node>,
    /// Python, if detected
    python: Option<detect::Python>,
    /// Rust, if detected
    rust: Option<detect::Rust>,
}

impl Detected {
    /// Reports what was detected, with warnings and extra packages.
    fn report(&self, rustc: Option<(u32, u32)>, extra: &[String]) {
        let with_tool = |tool: Option<&str>| tool.map(|t| format!(" + {t}")).unwrap_or_default();
        if let Some(p) = &self.php {
            let exts = if p.exts.is_empty() {
                String::new()
            } else {
                format!(", extensions: {}", p.exts.join(", "))
            };
            info(&format!("PHP {}.{} ({}){exts}", p.version.0, p.version.1, p.source));
        }
        if let Some(n) = &self.node {
            let version = n.version.map_or_else(|| "default".into(), |v| v.to_string());
            info(&format!("Node {version} ({}){}", n.source, with_tool(n.tool)));
        }
        if let Some(p) = &self.python {
            let version = p.version.map_or_else(|| "default".into(), |(a, b)| format!("{a}.{b}"));
            info(&format!("Python {version} ({}){}", p.source, with_tool(p.tool)));
        }
        if let Some(r) = &self.rust {
            let have = rustc.map(|(a, b)| format!(" {a}.{b}")).unwrap_or_default();
            info(&format!("Rust{have} (nixpkgs)"));
            for w in &r.warnings {
                info(&format!("warning: {w}"));
            }
        }
        if !extra.is_empty() {
            info(&format!("extra packages (config): {}", extra.join(", ")));
        }
        if self.php.is_none() && self.node.is_none() && self.python.is_none() && self.rust.is_none()
        {
            info("no language detected — add packages to flake.nix under packages");
        }
    }

    /// Shell commands that print the versions after the build.
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

/// Writes flake.nix and .envrc, updates .gitignore, stages it all and builds the environment.
fn setup_env(root: &Path, opts: &EnvOpts, config: &Config) -> Result<()> {
    if !run(root, "git", &["rev-parse", "--git-dir"], &[], true)? {
        bail!(
            "{} is not a git repo (nix only sees tracked files) — run git init or luggage new first",
            root.display()
        );
    }
    let flake_path = root.join("flake.nix");
    if flake_path.exists() && !opts.force {
        bail!("{} already exists (--force overwrites it)", flake_path.display());
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

    let name = root.file_name().map_or_else(|| "project".into(), |n| n.to_string_lossy());
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
    ensure_ignored(root, ".direnv/", "nix-direnv cache")?;
    if found.python.is_some() {
        ensure_ignored(root, ".venv/", "Python venv")?;
    }
    // stage first: nix only sees files that git knows
    run_ok(root, "git", &["add", "flake.nix", ".envrc", ".gitignore"])?;
    run_ok(root, "nix", &["flake", "lock"])?;
    run_ok(root, "git", &["add", "flake.lock"])?;
    let root_str = root.to_string_lossy();
    run_ok(root, "direnv", &["allow", &root_str])?;

    if !opts.no_build {
        info("building the environment (the first time can take a while) ...");
        run_ok(root, "direnv", &["exec", &root_str, "sh", "-c", &found.checks()])?;
    }

    info(&format!("done: {}", root.display()));
    info("staged: flake.nix flake.lock .envrc .gitignore — committing is up to you");
    Ok(())
}

/// Arguments of `luggage new` without the environment options.
struct NewArgs {
    /// Git URL, local repo path or project name
    target: String,
    /// Target directory; `None` means the projects directory from the config
    dir: Option<PathBuf>,
    /// GitLab group; `Some` creates a project, empty means the group from the config
    gitlab: Option<String>,
    /// GitHub owner; `Some` creates a repo, empty means the owner from the config
    github: Option<String>,
    /// GitLab host
    host: Option<String>,
    /// Visibility of the new remote project
    visibility: Option<String>,
}

/// `None` instead of an empty string.
fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.is_empty())
}

/// `luggage new`: clones the repo or creates a new one (optionally with a remote). Then sets up the environment.
fn cmd_new(args: NewArgs, opts: &EnvOpts, config: &Config) -> Result<()> {
    let target = args.target.as_str();
    let remote = is_remote(target);
    let name = if remote { repo_name(target) } else { target };
    let dir = args.dir.unwrap_or_else(|| expand_tilde(&config.projects_dir()));
    let root = dir.join(name);
    if root.exists() {
        bail!("{} already exists — use luggage init there", root.display());
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
            bail!("--gitlab/--github only work without a URL (they create a new project)");
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

/// `luggage config`: shows the effective config, or creates the template with `init`.
fn cmd_config(init: bool) -> Result<()> {
    if init {
        let path = config::init()?;
        info(&format!("template written: {}", path.display()));
        return Ok(());
    }
    let path = config::path();
    let state =
        if path.exists() { "" } else { " (missing — `luggage config --init` creates it)" };
    println!("# {}{state}", path.display());
    let rows = config::load()?.describe();
    let width = rows.iter().map(|(k, _, _)| k.len()).max().unwrap_or(0);
    for (key, value, source) in rows {
        println!("{key:width$} = {value}   # {source}");
    }
    Ok(())
}

/// Runs the subcommand.
fn dispatch(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Config { init } => cmd_config(init),
        Cmd::New { target, dir, gitlab, github, host, visibility, env } => {
            let args = NewArgs { target, dir, gitlab, github, host, visibility };
            cmd_new(args, &env, &config::load()?)
        }
        Cmd::Init { path, env } => setup_env(&std::path::absolute(&path)?, &env, &config::load()?),
        Cmd::Doctor => setup::doctor(),
        Cmd::Setup { yes } => setup::setup(yes),
        Cmd::Run { net, cmd } => {
            let nixpkgs = config::load()?.nixpkgs();
            chest::Chest::find(&nixpkgs, false)?.cmd_run(net, &cmd, &nixpkgs)
        }
        Cmd::Up => {
            let nixpkgs = config::load()?.nixpkgs();
            chest::Chest::find(&nixpkgs, false)?.cmd_up(&nixpkgs)
        }
        Cmd::Exec { cmd } => chest::Chest::find(&config::load()?.nixpkgs(), false)?.cmd_exec(&cmd),
        Cmd::Open => chest::Chest::find(&config::load()?.nixpkgs(), false)?.cmd_open(),
        Cmd::Down => chest::Chest::find(&config::load()?.nixpkgs(), true)?.cmd_down(),
        Cmd::Status => chest::Chest::find(&config::load()?.nixpkgs(), false)?.cmd_status(),
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
        assert!(!is_remote("my-project"));
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
        ensure_ignored(dir.path(), ".direnv/", "nix-direnv cache").unwrap();
        ensure_ignored(dir.path(), ".direnv/", "nix-direnv cache").unwrap();
        ensure_ignored(dir.path(), ".venv/", "Python venv").unwrap();
        let content = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(content, "/vendor\n/.venv\n\n# nix-direnv cache\n.direnv/\n");
    }
}
