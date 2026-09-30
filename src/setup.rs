//! `luggage doctor` checks what luggage needs from the system; `luggage setup` fixes gaps after asking.
//!
//! Only Nix, git and the direnv hook come from the system; nixpkgs provides everything else.

use std::fs;
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::chest::Tools;
use crate::{config, find_in_path, home, info};

/// Official Nix installer of the `NixOS` community (upstream Nix, systemd daemon).
const NIX_INSTALLER: &str = "https://artifacts.nixos.org/nix-installer";
/// Where `nix` lives after a fresh install, before a new shell sets the PATH.
const NIX_DEFAULT_PROFILE: &str = "/nix/var/nix/profiles/default/bin/nix";

/// Result of a check.
enum State {
    /// Fine, with a short detail (e.g. the version)
    Ok(String),
    /// Missing or set up wrong
    Missing(String),
    /// Optional and not present; not an error
    Optional(String),
}

/// A fix that `setup` runs after asking.
enum Action {
    /// Run a command
    Run(Vec<String>),
    /// Append a line to a file (creates it if needed)
    Append(PathBuf, String),
}

impl Action {
    /// Description for the prompt.
    fn describe(&self) -> String {
        match self {
            Self::Run(cmd) => match cmd.as_slice() {
                [sh, c, script] if sh == "sh" && c == "-c" => format!("run: {script}"),
                _ => format!("run: {}", cmd.join(" ")),
            },
            Self::Append(file, line) => format!("append to {}: {line}", file.display()),
        }
    }

    /// Runs the fix.
    fn apply(&self) -> Result<()> {
        match self {
            Self::Run(cmd) => {
                let (program, args) = cmd.split_first().context("empty command")?;
                let status = Command::new(program)
                    .args(args)
                    .status()
                    .with_context(|| format!("cannot start {program}"))?;
                if !status.success() {
                    bail!("failed: {}", cmd.join(" "));
                }
            }
            Self::Append(file, line) => append_line(file, line)?,
        }
        Ok(())
    }
}

/// A check with its result and the fixes for it.
struct Check {
    /// Display name, e.g. "direnv hook"
    name: &'static str,
    /// Result
    state: State,
    /// Fixes for `setup`; empty means it can only be fixed by hand
    fix: Vec<Action>,
}

impl Check {
    /// Check that passed.
    const fn ok(name: &'static str, detail: String) -> Self {
        Self { name, state: State::Ok(detail), fix: Vec::new() }
    }

    /// Check that failed, with its fixes.
    const fn missing(name: &'static str, detail: String, fix: Vec<Action>) -> Self {
        Self { name, state: State::Missing(detail), fix }
    }

    /// One line for the output.
    fn line(&self) -> String {
        let (mark, detail) = match &self.state {
            State::Ok(d) => ("✓", d),
            State::Missing(d) => ("✗", d),
            State::Optional(d) => ("–", d),
        };
        format!("  {mark} {:<16} {detail}", self.name)
    }
}

/// Appends `line` to `file` and creates the file and directory if needed.
fn append_line(file: &Path, line: &str) -> Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let content = fs::read_to_string(file).unwrap_or_default();
    let sep = match (content.is_empty(), content.ends_with('\n')) {
        (true, _) => "",
        (false, true) => "\n",
        (false, false) => "\n\n",
    };
    let mut f = fs::OpenOptions::new().create(true).append(true).open(file)?;
    writeln!(f, "{sep}# added by luggage setup\n{line}")?;
    Ok(())
}

/// First output line of a command, if it succeeded.
fn first_line(program: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_owned()
    })
}

/// `$XDG_CONFIG_HOME`, or else `~/.config`.
fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(|| home().join(".config"), PathBuf::from)
}

/// Finds a program in the PATH and in the Nix profiles. A new shell adds these profiles to the PATH only later.
fn find_tool(program: &str) -> Option<PathBuf> {
    find_in_path(program).or_else(|| {
        [home().join(".nix-profile/bin"), PathBuf::from("/nix/var/nix/profiles/default/bin")]
            .into_iter()
            .map(|d| d.join(program))
            .find(|p| p.exists())
    })
}

/// The `nix` command, also right after a fresh install.
fn nix_bin() -> Option<PathBuf> {
    find_tool("nix").or_else(|| Some(PathBuf::from(NIX_DEFAULT_PROFILE)).filter(|p| p.exists()))
}

/// `nix profile install` for a package from `nixpkgs`, whether flakes are enabled or not.
fn profile_install(nix: &Path, nixpkgs: &str, attr: &str) -> Action {
    Action::Run(vec![
        nix.to_string_lossy().into_owned(),
        "--extra-experimental-features".into(),
        "nix-command flakes".into(),
        "profile".into(),
        "install".into(),
        format!("{nixpkgs}#{attr}"),
    ])
}

/// Checks Nix; the official installer can fix it (it asks for sudo itself).
fn check_nix(yes: bool) -> Check {
    if let Some(nix) = nix_bin()
        && let Some(v) = first_line(&nix, &["--version"])
    {
        return Check::ok("Nix", v);
    }
    let mut install = format!("curl -sSfL {NIX_INSTALLER} | sh -s -- install --enable-flakes");
    if yes {
        install += " --no-confirm";
    }
    let fix = if find_in_path("curl").is_some() {
        vec![Action::Run(vec!["sh".into(), "-c".into(), install])]
    } else {
        Vec::new()
    };
    Check::missing("Nix", "missing (install needs curl and sudo)".into(), fix)
}

/// Checks that `nix-command` and `flakes` are enabled; fixes it in the user's Nix config.
fn check_flakes(nix: &Path) -> Check {
    let features = first_line(
        nix,
        &[
            "--extra-experimental-features",
            "nix-command",
            "config",
            "show",
            "experimental-features",
        ],
    )
    .unwrap_or_default();
    let on: Vec<&str> = features.split_whitespace().collect();
    if on.contains(&"flakes") && on.contains(&"nix-command") {
        return Check::ok("Flakes", "enabled".into());
    }
    let conf = config_home().join("nix/nix.conf");
    Check::missing(
        "Flakes",
        "not enabled".into(),
        vec![Action::Append(conf, "extra-experimental-features = nix-command flakes".into())],
    )
}

/// Checks a program that `nix profile install` can add later.
fn check_program(name: &'static str, nix: Option<&Path>, nixpkgs: &str) -> Check {
    if let Some(p) = find_tool(name) {
        let v = first_line(&p, &["--version"]).unwrap_or_default();
        return Check::ok(name, if v.is_empty() { p.display().to_string() } else { v });
    }
    let fix = nix.map(|n| vec![profile_install(n, nixpkgs, name)]).unwrap_or_default();
    Check::missing(name, "missing".into(), fix)
}

/// Checks that nix-direnv is loaded in the direnvrc.
fn check_nix_direnv(nix: Option<&Path>, nixpkgs: &str) -> Check {
    let rc = config_home().join("direnv/direnvrc");
    if fs::read_to_string(&rc).is_ok_and(|t| t.contains("nix-direnv")) {
        return Check::ok("nix-direnv", format!("loaded in {}", rc.display()));
    }
    // Prefer the Debian/Ubuntu package, else the Nix profile
    let system = Path::new("/usr/share/nix-direnv/direnvrc");
    let mut fix = Vec::new();
    let source = if system.exists() {
        system.display().to_string()
    } else {
        if !home().join(".nix-profile/share/nix-direnv/direnvrc").exists()
            && let Some(n) = nix
        {
            fix.push(profile_install(n, nixpkgs, "nix-direnv"));
        }
        "$HOME/.nix-profile/share/nix-direnv/direnvrc".to_owned()
    };
    fix.push(Action::Append(rc, format!("source \"{source}\"")));
    Check::missing("nix-direnv", "not set up".into(), fix)
}

/// Shell config and hook line for the login shell from `$SHELL`.
fn shell_hook() -> Option<(PathBuf, &'static str)> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    match shell.rsplit('/').next().unwrap_or("") {
        "bash" => Some((home().join(".bashrc"), r#"eval "$(direnv hook bash)""#)),
        "zsh" => {
            let dir = std::env::var_os("ZDOTDIR").map_or_else(home, PathBuf::from);
            Some((dir.join(".zshrc"), r#"eval "$(direnv hook zsh)""#))
        }
        "fish" => Some((config_home().join("fish/config.fish"), "direnv hook fish | source")),
        _ => None,
    }
}

/// Checks that the shell config loads the direnv hook.
fn check_hook() -> Check {
    let Some((rc, line)) = shell_hook() else {
        let shell = std::env::var("SHELL").unwrap_or_default();
        return Check::missing(
            "direnv hook",
            format!("shell {shell} unknown — add the hook by hand (direnv.net/docs/hook.html)"),
            Vec::new(),
        );
    };
    if fs::read_to_string(&rc).is_ok_and(|t| t.contains("direnv hook")) {
        return Check::ok("direnv hook", format!("in {}", rc.display()));
    }
    Check::missing(
        "direnv hook",
        format!("missing in {}", rc.display()),
        vec![Action::Append(rc, line.into())],
    )
}

/// Checks that the chest may create user namespaces, with the same programs as `luggage up`.
fn check_userns(nixpkgs: &str) -> Check {
    let tools = match Tools::fetch(nixpkgs, &[]) {
        Ok(t) => t,
        Err(e) => {
            return Check::missing("chest", format!("cannot load tools: {e:#}"), Vec::new());
        }
    };
    let ok = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    // bash instead of `true`: the PATH may be empty, but the store paths always exist
    let bash = tools.bash();
    let unshare =
        ok(&tools.util("unshare"), &["--user", "--map-root-user", "--net", &bash, "-c", ":"]);
    let bwrap = ok(&tools.bwrap, &["--unshare-all", "--ro-bind", "/", "/", &bash, "-c", ":"]);
    if unshare && bwrap {
        return Check::ok("chest", format!("user namespaces allowed (bwrap: {})", tools.bwrap));
    }
    let failed = if unshare {
        "bwrap"
    } else if bwrap {
        "unshare"
    } else {
        "unshare and bwrap"
    };
    Check::missing(
        "chest",
        format!(
            "{failed} may not create user namespaces — Ubuntu 24.04 and later: install the bubblewrap package \
             or set kernel.apparmor_restrict_unprivileged_userns=0"
        ),
        Vec::new(),
    )
}

/// Checks gh/glab; both are optional, luggage fetches a missing one with `nix run` when needed.
fn check_forge(name: &'static str) -> Check {
    let Some(p) = find_tool(name) else {
        return Check {
            name,
            state: State::Optional(format!(
                "missing — fetched with `nix run nixpkgs#{name}` when needed"
            )),
            fix: Vec::new(),
        };
    };
    let logged_in = Command::new(&p)
        .args(["auth", "status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if logged_in {
        Check::ok(name, "logged in".into())
    } else {
        Check {
            name,
            state: State::Optional(format!("not logged in — `{name} auth login`")),
            fix: Vec::new(),
        }
    }
}

/// All checks, in the order in which `setup` fixes them.
fn checks(yes: bool, nixpkgs: &str) -> Vec<Box<dyn Fn() -> Check + '_>> {
    vec![
        Box::new(move || check_nix(yes)),
        Box::new(|| match nix_bin() {
            Some(n) => check_flakes(&n),
            None => Check::missing("Flakes", "Nix missing".into(), Vec::new()),
        }),
        Box::new(|| check_program("git", nix_bin().as_deref(), nixpkgs)),
        Box::new(|| check_program("direnv", nix_bin().as_deref(), nixpkgs)),
        Box::new(|| check_nix_direnv(nix_bin().as_deref(), nixpkgs)),
        Box::new(check_hook),
        Box::new(|| {
            if nix_bin().is_some() {
                check_userns(nixpkgs)
            } else {
                Check::missing("chest", "Nix missing".into(), Vec::new())
            }
        }),
        Box::new(|| check_forge("gh")),
        Box::new(|| check_forge("glab")),
    ]
}

/// Only checks; fails if something required is missing.
pub fn doctor() -> Result<()> {
    let nixpkgs = config::load()?.nixpkgs();
    let mut missing = 0;
    for check in checks(false, &nixpkgs) {
        let c = check();
        println!("{}", c.line());
        if matches!(c.state, State::Missing(_)) {
            missing += 1;
        }
    }
    if missing > 0 {
        bail!("{missing} item(s) missing — `luggage setup` fixes what can be fixed automatically");
    }
    Ok(())
}

/// Asks the user; `yes` answers everything with yes.
fn confirm(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    print!("    Run it? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Checks and fixes gaps after asking.
pub fn setup(yes: bool) -> Result<()> {
    let nixpkgs = config::load()?.nixpkgs();
    let (mut changed, mut open) = (false, 0);
    for check in checks(yes, &nixpkgs) {
        let mut c = check();
        println!("{}", c.line());
        if !matches!(c.state, State::Missing(_)) {
            continue;
        }
        if c.fix.is_empty() {
            open += 1;
            continue;
        }
        for action in &c.fix {
            println!("    → {}", action.describe());
        }
        if !confirm(yes)? {
            open += 1;
            continue;
        }
        for action in &c.fix {
            if let Err(e) = action.apply() {
                info(&format!("{e:#}"));
                break;
            }
        }
        changed = true;
        c = check();
        println!("{}", c.line());
        if matches!(c.state, State::Missing(_)) {
            open += 1;
        }
    }
    if changed {
        info("changes take effect in a new shell (PATH, direnv hook)");
    }
    if open > 0 {
        bail!("{open} item(s) still open");
    }
    info("setup complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_line_separates_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/rc");
        append_line(&file, "a").unwrap();
        fs::write(&file, fs::read_to_string(&file).unwrap() + "b").unwrap();
        append_line(&file, "c").unwrap();
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(text, "# added by luggage setup\na\nb\n\n# added by luggage setup\nc\n");
    }

    #[test]
    fn check_lines() {
        assert_eq!(Check::ok("git", "2.47".into()).line(), "  ✓ git              2.47");
        let c = Check::missing("Nix", "missing".into(), Vec::new());
        assert_eq!(c.line(), "  ✗ Nix              missing");
    }
}
