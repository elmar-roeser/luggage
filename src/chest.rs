//! The chest: the project in an isolated environment (bubblewrap) with its own services (process-compose).
//!
//! State (home, data) lives in `$XDG_DATA_HOME/luggage/chests/<name>`, runtime files (sockets,
//! generated files) in `$XDG_RUNTIME_DIR/luggage/<name>` — Unix socket paths must not exceed 108 characters.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::branch::{self, Head};
use crate::compose::{self, Detected};
use crate::{config, find_in_path, gitcheck, home, info, remount, skills, versions, vm};

/// Default wait time in seconds for `ready` when `ready_timeout` is missing.
const DEFAULT_READY_TIMEOUT: u32 = 60;
/// The path of the runtime dir inside the chest.
const RUN_INSIDE: &str = "/run/luggage";
/// process-compose in the VM guest, from its system packages.
const PC_GUEST: &str = "/run/current-system/sw/bin/process-compose";
/// How many ports after a taken host port luggage tries before it gives up.
const PORT_TRIES: u16 = 100;
/// Address of the proxy for `network` in the bubblewrap chest; a socat passes it on to the host.
const PROXY_INSIDE: &str = "127.0.0.1:3128";
/// Name of the guard process that ends the port bridges of a bubblewrap chest together with the chest.
const BRIDGES: &str = "luggage-bridges";
/// Closes every inherited file descriptor above 2, then runs the arguments.
const CLOSE_FDS_AND_EXEC: &str = r#"for fd in /proc/self/fd/*; do fd=${fd##*/}; if [ "$fd" -gt 2 ] 2>/dev/null; then eval "exec $fd>&-" 2>/dev/null; fi; done; exec "$@""#;

/// Contents of `chest.toml`; it adds to what was detected in the Compose file.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Definition {
    /// Name of the chest; defaults to the name of the project directory
    pub name: Option<String>,
    /// Names that point to 127.0.0.1 inside the chest
    pub hosts: Vec<String>,
    /// Extra nixpkgs packages for the services, e.g. "mariadb"
    pub packages: Vec<String>,
    /// Custom services; they replace detected services with the same name
    pub services: BTreeMap<String, Service>,
    /// Detected Compose services that should be skipped
    pub ignore: Vec<String>,
    /// How the chest isolates the project
    pub mode: Mode,
    /// Fixed host ports per service, replacing its own; a taken one is not moved to a free port
    pub ports: BTreeMap<String, Vec<String>>,
    /// Host variables that commands in the chest get besides the harmless defaults, e.g. tokens; read at each command
    pub pass_env: Vec<String>,
    /// The only hosts the chest may reach, each with its subdomains and on 80 and 443 unless it names a port
    pub network: Option<Vec<String>>,
    /// Skills for coding agents from the sources in the config, as `<source>/<skill>` with `*`
    pub skills: Vec<String>,
}

/// How the chest isolates the project.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Namespaces with bubblewrap on the host kernel
    #[default]
    Bubblewrap,
    /// A microVM with its own kernel
    Vm,
}

/// A service that process-compose starts in the chest.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Command that starts the service
    pub command: String,
    /// Services that must run first
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Command that succeeds once the service is ready
    pub ready: Option<String>,
    /// Seconds that `ready` may take (default 60)
    pub ready_timeout: Option<u32>,
    /// Runs once to the end (e.g. to create a database). Dependent services wait until it succeeds.
    #[serde(default)]
    pub once: bool,
    /// Ports that the chest makes reachable on 127.0.0.1 of the host: `"8026:8025"` (host:guest) or `"8025"`
    #[serde(default)]
    pub ports: Vec<String>,
}

/// Host and guest port of `"8026:8025"`, or the same port twice for `"8025"`.
pub fn port_pair(spec: &str) -> Result<(u16, u16)> {
    let port = |p: &str| {
        p.trim().parse::<u16>().ok().filter(|p| *p > 0).with_context(|| {
            format!("port \"{spec}\": use \"host:guest\" or \"port\" with ports from 1 to 65535")
        })
    };
    match spec.split_once(':') {
        Some((host, guest)) => Ok((port(host)?, port(guest)?)),
        None => port(spec).map(|p| (p, p)),
    }
}

/// The first port after `host` that is not `reserved` and is `free`, within `PORT_TRIES` ports.
fn next_free_port(host: u16, reserved: &BTreeSet<u16>, free: impl Fn(u16) -> bool) -> Option<u16> {
    (1..=PORT_TRIES).filter_map(|i| host.checked_add(i)).find(|p| !reserved.contains(p) && free(*p))
}

/// Reads `chest.toml`. `once` and `ready` cannot be used together.
fn parse(text: &str) -> Result<Definition> {
    let def: Definition = toml::from_str(text)?;
    for (name, svc) in &def.services {
        if svc.once && svc.ready.is_some() {
            bail!("service {name}: once and ready cannot be used together");
        }
        for p in &svc.ports {
            port_pair(p).with_context(|| format!("service {name}"))?;
        }
    }
    for (name, specs) in &def.ports {
        for p in specs {
            port_pair(p).with_context(|| format!("ports of {name}"))?;
        }
    }
    if let Some(n) = def.pass_env.iter().find(|n| !vm::is_env_name(n)) {
        bail!("pass_env: {n:?} is not a variable name");
    }
    if let Some(network) = &def.network
        && let Some(h) = network.iter().find(|h| !crate::proxy::is_entry(h))
    {
        bail!(
            "network: {h:?} is not a host like \"github.com\", \"git.example.com:22\" or \"[::1]:8080\""
        );
    }
    for p in &def.skills {
        skills::check_pattern(p)?;
    }
    Ok(def)
}

/// Merges detected services with the definition. Services from `chest.toml` win over detected ones with the same name.
fn merge(mut def: Definition, detected: Option<&Detected>) -> Result<Definition> {
    for i in &def.ignore {
        if !detected.is_some_and(|d| d.found.contains_key(i)) {
            bail!("ignore: \"{i}\" was not detected in any Compose file");
        }
    }
    if let Some(d) = detected {
        let ignored = |svc: &str| {
            let base = svc
                .strip_suffix("-init")
                .or_else(|| svc.strip_suffix("-setup"))
                .filter(|b| d.found.contains_key(*b))
                .unwrap_or(svc);
            def.ignore.iter().any(|i| i == base)
        };
        let services: Vec<_> = d
            .services
            .iter()
            .filter(|(name, _)| !ignored(name) && !def.services.contains_key(*name))
            .map(|(n, s)| (n.clone(), s.clone()))
            .collect();
        let hosts: Vec<_> = d.hosts.iter().filter(|h| !ignored(h)).cloned().collect();
        def.services.extend(services);
        def.hosts.splice(0..0, hosts);
        def.packages.splice(0..0, d.packages.iter().cloned());
        def.hosts.dedup();
        def.packages.sort();
        def.packages.dedup();
    }
    for (name, specs) in &def.ports {
        let svc = def
            .services
            .get_mut(name)
            .with_context(|| format!("ports: \"{name}\" is not a service"))?;
        svc.ports.clone_from(specs);
    }
    for (name, svc) in &def.services {
        for dep in &svc.depends_on {
            if dep == name || !def.services.contains_key(dep) {
                bail!("service {name}: depends_on \"{dep}\" does not exist");
            }
        }
    }
    Ok(def)
}

/// Tools from nixpkgs that the chest needs, independent of the project flake.
#[derive(Debug, Serialize, Deserialize)]
pub struct Tools {
    /// Store path of `bashInteractive`
    bash: String,
    /// Store path of coreutils
    coreutils: String,
    /// Store path of process-compose
    process_compose: String,
    /// Store path of util-linux (unshare, nsenter, setsid, kill)
    util_linux: String,
    /// Store path of iproute2 (ip)
    iproute2: String,
    /// Store path of cacert; the chest has no `/etc/ssl`
    cacert: String,
    /// Store path of socat, which bridges the service ports to the host
    #[serde(default)]
    socat: String,
    /// The bwrap program: from the system if present, else from nixpkgs.
    /// Ubuntu uses AppArmor to allow user namespaces only for programs with a profile, like `/usr/bin/bwrap`.
    pub bwrap: String,
    /// Store paths of the `packages` from the definition
    packages: Vec<String>,
}

/// Builds the installables with `nix build` and returns their store paths.
fn nix_build(installables: &[String]) -> Result<Vec<String>> {
    let out = Command::new("nix")
        .args(["build", "--no-link", "--print-out-paths"])
        .args(installables)
        .stderr(Stdio::inherit())
        .output()
        .context("cannot start nix")?;
    if !out.status.success() {
        bail!("nix build failed: {}", installables.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(str::to_owned).collect())
}

impl Tools {
    /// Builds the chest tools and the extra packages from `nixpkgs`.
    pub fn fetch(nixpkgs: &str, packages: &[String]) -> Result<Self> {
        let host_bwrap = find_in_path("bwrap");
        let mut attrs = vec![
            "bashInteractive^out",
            "coreutils^out",
            "process-compose^out",
            "util-linux^bin",
            "iproute2^out",
            "cacert^out",
            "socat^out",
        ];
        if host_bwrap.is_none() {
            attrs.push("bubblewrap^out");
        }
        let paths = nix_build(&attrs.iter().map(|a| format!("{nixpkgs}#{a}")).collect::<Vec<_>>())?;
        let find = |name: &str| {
            paths
                .iter()
                .find(|p| p.contains(&format!("-{name}-")))
                .cloned()
                .with_context(|| format!("{name} not found in the output of nix build"))
        };
        let packages = if packages.is_empty() {
            Vec::new()
        } else {
            nix_build(&packages.iter().map(|p| format!("{nixpkgs}#{p}")).collect::<Vec<_>>())?
        };
        Ok(Self {
            bash: find("bash-interactive")?,
            coreutils: find("coreutils")?,
            process_compose: find("process-compose")?,
            util_linux: find("util-linux")?,
            iproute2: find("iproute2")?,
            cacert: find("nss-cacert")?,
            socat: find("socat")?,
            bwrap: match host_bwrap {
                Some(p) => p.to_string_lossy().into_owned(),
                None => format!("{}/bin/bwrap", find("bubblewrap")?),
            },
            packages,
        })
    }

    /// Path to the bash binary
    pub fn bash(&self) -> String {
        format!("{}/bin/bash", self.bash)
    }

    /// A program from util-linux, e.g. `unshare`.
    pub fn util(&self, program: &str) -> String {
        format!("{}/bin/{program}", self.util_linux)
    }
}

/// Network mode of the chest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Net {
    /// no network
    Off,
    /// the host network (internet)
    Share,
    /// the network namespace comes from outside (unshare); bwrap creates only the other namespaces
    Own,
}

/// A chest for one project: definition, detected services and their directories.
pub struct Chest {
    /// Name of the chest (only `[A-Za-z0-9_-]`)
    name: String,
    /// Project directory (contains flake.nix)
    project: PathBuf,
    /// Directory with `chest.toml`, if there is one
    def_dir: Option<PathBuf>,
    /// What was detected in Compose files
    detected: Option<Detected>,
    /// Merged definition
    def: Definition,
    /// State directory (home, data)
    state: PathBuf,
    /// Runtime dir (sockets, generated files)
    run: PathBuf,
}

/// Path from the environment variable `var`. If it is empty or missing, `fallback` is used.
pub fn xdg(var: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    std::env::var_os(var).filter(|v| !v.is_empty()).map_or_else(fallback, PathBuf::from)
}

/// Replaces everything except ASCII letters, digits, `-` and `_` with `-`.
fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

/// Puts `s` in single quotes for bash. It uses no backslash because process-compose drops it.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// Puts `s` in double quotes as a YAML string.
fn yaml_str(s: &str) -> String {
    let escaped = s.replace('\\', r"\\").replace('"', "\\\"").replace('\n', r"\n");
    format!("\"{escaped}\"")
}

/// Is the process `pid` still alive?
pub fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Waits up to `tenths` tenths of a second for `pid` to go away.
pub fn wait_dead(pid: u32, tenths: u32) -> bool {
    for _ in 0..tenths {
        if !alive(pid) {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    !alive(pid)
}

/// One row from `process-compose process list -o json`.
#[derive(Deserialize)]
struct ProcState {
    /// Name of the service
    name: String,
    /// Status, e.g. "Completed" or "Error"
    status: String,
    /// "Ready" once the `readiness_probe` has succeeded
    is_ready: String,
    /// Exit code of the service
    exit_code: i32,
    /// Is the service running right now?
    is_running: bool,
}

/// Modification time of the file, if it can be read.
fn mtime(p: &Path) -> Option<SystemTime> {
    fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Paths in the project that luggage, direnv or git read or run on the host. The chest may only read them,
/// else it could add host variables to `pass_env` or run its code on the host.
pub const PROTECTED: [&str; 9] = [
    ".luggage",
    ".direnv",
    ".envrc",
    "flake.nix",
    "flake.lock",
    ".git/config",
    ".git/hooks",
    ".git/modules",
    ".git/worktrees",
];

/// Creates the missing directories of `PROTECTED`, so the chest cannot create them with its own content.
///
/// # Errors
/// If a directory cannot be created.
pub fn create_protected_dirs(project: &Path) -> Result<()> {
    let mut dirs = vec![".luggage", ".direnv"];
    if project.join(".git").is_dir() {
        dirs.extend([".git/hooks", ".git/modules", ".git/worktrees"]);
    }
    for d in dirs.into_iter().map(|d| project.join(d)) {
        if d.symlink_metadata().is_err() {
            fs::create_dir(&d).with_context(|| format!("cannot create {}", d.display()))?;
        }
    }
    Ok(())
}

/// bwrap options that make the existing `PROTECTED` paths read-only; they go after the bind of the project.
/// `.git` becomes a mount point first, so the chest cannot rename it and put its own in its place.
pub fn protect_args(project: &Path) -> Vec<String> {
    let mut a = Vec::new();
    let mut bind = |option: &str, p: &Path| {
        let p = p.to_string_lossy().into_owned();
        a.extend([option.to_owned(), p.clone(), p]);
    };
    let git = project.join(".git");
    if git.is_dir() {
        bind("--bind", &git);
    } else if git.exists() {
        // a worktree or submodule: the file names the git dir
        bind("--ro-bind", &git);
    }
    for p in PROTECTED.map(|n| project.join(n)) {
        if p.exists() {
            bind("--ro-bind", &p);
        }
    }
    a
}

/// Exits luggage with the exit code of the child process if it was not 0.
fn pass_through(status: ExitStatus) {
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

impl Chest {
    /// Searches upward from the current directory for the project (flake.nix), detects services and reads the definition.
    /// With `lenient`, a broken definition is only reported.
    pub fn find(nixpkgs: &str, lenient: bool) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let project = cwd
            .ancestors()
            .find(|d| d.join("flake.nix").exists())
            .with_context(|| {
                format!("no flake.nix in {} or above — run `luggage init` first", cwd.display())
            })?
            .to_path_buf();
        let base = safe_name(
            &project.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        );
        let def_dir =
            [project.join(".luggage"), config::path().with_file_name("chests").join(&base)]
                .into_iter()
                .find(|d| d.join("chest.toml").exists());
        let load = || -> Result<(Option<Detected>, Definition)> {
            let def = match &def_dir {
                Some(d) => {
                    let file = d.join("chest.toml");
                    let text = fs::read_to_string(&file)?;
                    parse(&text).with_context(|| format!("{} is invalid", file.display()))?
                }
                None => Definition::default(),
            };
            let detected = compose::detect(&project, &mut || versions::query(nixpkgs))?;
            let def = merge(def, detected.as_ref())?;
            Ok((detected, def))
        };
        let (detected, def) = match load() {
            Ok(loaded) => loaded,
            // down only needs the runtime dir, so a broken definition must not block it
            Err(e) if lenient => {
                info(&format!("warning: {e:#}"));
                (None, Definition::default())
            }
            Err(e) => return Err(e),
        };
        let name = def.name.as_deref().map_or(base, safe_name);
        let state =
            xdg("XDG_DATA_HOME", || home().join(".local/share")).join("luggage/chests").join(&name);
        let run = xdg("XDG_RUNTIME_DIR", std::env::temp_dir).join("luggage").join(&name);
        Ok(Self { name, project, def_dir, detected, def, state, run })
    }

    /// PID file of the running chest
    fn pid_file(&self) -> PathBuf {
        self.run.join("chest.pid")
    }

    /// The part of the runtime dir that the bubblewrap chest can write, at `/run/luggage`.
    /// luggage never reads or follows what is in it; its own files stay outside.
    fn inside(&self) -> PathBuf {
        self.run.join("chest")
    }

    /// bwrap options that show the file `name` of the runtime dir read-only at `/run/luggage/<name>`.
    fn bind_inside(&self, name: &str) -> [String; 3] {
        [
            "--ro-bind".into(),
            self.run.join(name).to_string_lossy().into_owned(),
            format!("{RUN_INSIDE}/{name}"),
        ]
    }

    /// Socket of process-compose in the bubblewrap chest
    fn socket(&self) -> PathBuf {
        self.inside().join("pc.sock")
    }

    /// PID of the running chest, if it is alive.
    fn running(&self) -> Option<u32> {
        let pid = fs::read_to_string(self.pid_file()).ok()?.trim().parse().ok()?;
        alive(pid).then_some(pid)
    }

    /// Reads the tools that `prepare` saved in `tools.json`.
    fn tools(&self) -> Result<Tools> {
        let text = fs::read_to_string(self.run.join("tools.json"))
            .context("chest not prepared — run `luggage up` first")?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Contents of `/etc/hosts` for the chest.
    fn render_hosts(&self) -> String {
        let mut names = vec![format!("chest-{}", self.name)];
        names.extend(self.def.hosts.iter().cloned());
        format!("127.0.0.1 localhost {}\n::1 localhost\n", names.join(" "))
    }

    /// The rc file for bash: loads the dev environment, sets `PATH` and changes to the project.
    fn render_rc(&self, tools: &Tools) -> String {
        let user = std::env::var("USER").unwrap_or_default();
        let mut rc = format!(
            "source {RUN_INSIDE}/env.sh\n\
             export HOME={home} USER={user} TMPDIR=/tmp LUGGAGE_CHEST={name}\n\
             unset NIX_BUILD_TOP TEMP TMP TEMPDIR\n\
             export SSL_CERT_FILE={ca} NIX_SSL_CERT_FILE={ca}\n\
             export PATH={pkgs}{pc}/bin:{bash}/bin:{core}/bin:\"$PATH\"\n\
             cd {project}\n\
             PS1='[chest:{name_plain}] \\w\\$ '\n",
            home = sh_quote(&home().to_string_lossy()),
            user = sh_quote(&user),
            name = sh_quote(&self.name),
            ca = format!("{}/etc/ssl/certs/ca-bundle.crt", tools.cacert),
            pc = tools.process_compose,
            bash = tools.bash,
            core = tools.coreutils,
            pkgs = tools.packages.iter().fold(String::new(), |acc, p| acc + p + "/bin:"),
            project = sh_quote(&self.project.to_string_lossy()),
            name_plain = self.name,
        );
        if self.def.network.is_some() {
            rc.push_str(&vm::proxy_env(PROXY_INSIDE, &self.def.hosts));
        }
        let _ = writeln!(rc, "eval \"${{{p}-}}\"; unset {p}", p = vm::PASS_ENV);
        rc
    }

    /// Generates `process-compose.yaml` from the services.
    fn render_compose(&self) -> String {
        let wd = yaml_str(&self.project.to_string_lossy());
        // Only bash expands variables; process-compose ignores this key per process.
        let mut y = String::from("version: \"0.5\"\ndisable_env_expansion: true\nprocesses:\n");
        for (name, svc) in &self.def.services {
            let _ = write!(
                y,
                "  {}:\n    command: {}\n    working_dir: {wd}\n",
                yaml_str(name),
                yaml_str(&svc.command)
            );
            if !svc.depends_on.is_empty() {
                y += "    depends_on:\n";
                for dep in &svc.depends_on {
                    let d = &self.def.services[dep];
                    let condition = if d.once {
                        "process_completed_successfully"
                    } else if d.ready.is_some() {
                        "process_healthy"
                    } else {
                        "process_started"
                    };
                    let _ = write!(y, "      {}:\n        condition: {condition}\n", yaml_str(dep));
                }
            }
            if let Some(ready) = &svc.ready {
                // The process-compose default timeout is 1 s, but first requests (cache warm-up) take longer.
                let t = svc.ready_timeout.unwrap_or(DEFAULT_READY_TIMEOUT);
                let _ = write!(
                    y,
                    "    readiness_probe:\n      exec:\n        command: {}\n      period_seconds: 1\n      timeout_seconds: {t}\n      failure_threshold: {t}\n",
                    yaml_str(ready)
                );
            }
        }
        y
    }

    /// Arguments for bwrap without the environment. Order matters: later mounts hide earlier ones.
    fn bwrap_args(&self, tools: &Tools, net: Net) -> Result<Vec<String>> {
        let meta = fs::metadata("/proc/self")?;
        let home = home().to_string_lossy().into_owned();
        let project = self.project.to_string_lossy().into_owned();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut a: Vec<String> = if net == Net::Own {
            vec![
                "--unshare-user".into(),
                "--uid".into(),
                meta.uid().to_string(),
                "--gid".into(),
                meta.gid().to_string(),
                "--unshare-pid".into(),
                "--unshare-ipc".into(),
                "--unshare-uts".into(),
                "--unshare-cgroup-try".into(),
            ]
        } else {
            vec!["--unshare-all".into()]
        };
        a.extend(
            [
                "--hostname",
                &format!("chest-{}", self.name),
                "--tmpfs",
                "/tmp",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--ro-bind",
                "/nix/store",
                "/nix/store",
                // home first, then the project: if the project is under $HOME, the home mount would hide it otherwise
                "--bind",
                &s(&self.state.join("home")),
                &home,
                "--bind",
                &project,
                &project,
            ]
            .map(str::to_owned),
        );
        a.extend(protect_args(&self.project));
        a.extend(
            [
                "--bind",
                &s(&self.state.join("data")),
                "/data",
                "--bind",
                &s(&self.inside()),
                RUN_INSIDE,
                "--ro-bind",
                &s(&self.run.join("hosts")),
                "/etc/hosts",
                "--ro-bind",
                "/etc/passwd",
                "/etc/passwd",
                "--ro-bind",
                "/etc/group",
                "/etc/group",
                "--ro-bind-try",
                "/etc/nsswitch.conf",
                "/etc/nsswitch.conf",
                "--symlink",
                &format!("{}/bin/env", tools.coreutils),
                "/usr/bin/env",
                "--symlink",
                &tools.bash(),
                "/bin/sh",
            ]
            .map(str::to_owned),
        );
        for f in ["rc", "env.sh", "process-compose.yaml"] {
            a.extend(self.bind_inside(f));
        }
        if let Some(d) = &self.def_dir {
            a.extend(["--ro-bind".into(), s(d), "/chest".into()]);
        }
        if net == Net::Share {
            a.extend(
                ["--share-net", "--ro-bind-try", "/etc/resolv.conf", "/etc/resolv.conf"]
                    .map(str::to_owned),
            );
        }
        Ok(a)
    }

    /// Writes `env.sh` in the runtime dir with the dev environment of the project, if the flake changed since.
    /// nix also keeps the environment in the profile `<state>/dev-env`, a GC root for its store paths.
    fn dev_env(&self) -> Result<PathBuf> {
        let env = self.run.join("env.sh");
        let profile = self.state.join("dev-env");
        let flake_changed =
            ["flake.nix", "flake.lock"].iter().filter_map(|f| mtime(&self.project.join(f))).max();
        let stale = mtime(&env).is_none_or(|t| flake_changed.is_some_and(|f| f > t));
        if stale || !profile.exists() {
            info("building the environment (nix print-dev-env)");
            let out = Command::new("nix")
                .arg("print-dev-env")
                .arg("--profile")
                .arg(&profile)
                .current_dir(&self.project)
                .stderr(Stdio::inherit())
                .output()
                .context("cannot start nix")?;
            if !out.status.success() {
                bail!("nix print-dev-env failed");
            }
            fs::write(&env, out.stdout)?;
        }
        Ok(env)
    }

    /// Creates the directories and writes env.sh, rc, hosts and process-compose.yaml.
    fn prepare(&self, nixpkgs: &str) -> Result<Tools> {
        for d in [self.state.join("home"), self.state.join("data"), self.inside()] {
            fs::create_dir_all(&d).with_context(|| format!("cannot create {}", d.display()))?;
        }
        create_protected_dirs(&self.project)?;
        fs::set_permissions(&self.run, fs::Permissions::from_mode(0o700))?;
        let names = self.skill_set()?;
        skills::link_home(&self.state.join("home"), &home(), &self.state.join("skills"), &names)?;
        let tools = Tools::fetch(nixpkgs, &self.def.packages)?;
        self.dev_env()?;
        fs::write(self.run.join("tools.json"), serde_json::to_string_pretty(&tools)?)?;
        fs::write(self.run.join("rc"), self.render_rc(&tools))?;
        fs::write(self.run.join("hosts"), self.render_hosts())?;
        fs::write(self.run.join("process-compose.yaml"), self.render_compose())?;
        if self.def.network.is_none() {
            let _ = fs::remove_file(self.network_file());
        }
        self.sync_network(true)?;
        Ok(tools)
    }

    /// The allowlist that the proxy reads: in the runtime dir for the VM, in the state dir for bubblewrap,
    /// whose chest can write part of its runtime dir.
    fn network_file(&self) -> PathBuf {
        if self.def.mode == Mode::Vm { &self.run } else { &self.state }.join("network")
    }

    /// Writes `network` to `network_file` if it changed and the file exists or `create`.
    /// The proxy reads it for each connection, so a changed list applies without a restart.
    fn sync_network(&self, create: bool) -> Result<()> {
        let file = self.network_file();
        if let (Some(hosts), true) = (&self.def.network, create || file.exists()) {
            let hosts = hosts.join("\n");
            if fs::read_to_string(&file).ok().as_deref() != Some(hosts.as_str()) {
                fs::write(&file, hosts)?;
            }
        }
        Ok(())
    }

    /// Runs a one-off command in a fresh chest; without a command, a shell.
    /// In VM mode it runs in the VM, without network unless `net`.
    pub fn cmd_run(&self, net: bool, cmd: &[String], nixpkgs: &str) -> Result<()> {
        self.guard()?;
        self.follow_branch(nixpkgs)?;
        if self.def.mode == Mode::Vm {
            return self.vm_enter(net, cmd, nixpkgs);
        }
        crate::setup::require_userns()?;
        let tools = self.prepare(nixpkgs)?;
        let mut c = self.run_command(&tools, net, cmd, &self.host_env(std::env::vars_os()))?;
        // this chest is gone when the command ends, so luggage compares right after it
        let before = gitcheck::snapshot(&self.project, false);
        if !(net && self.def.network.is_some()) {
            let status = c.status().context("cannot start bwrap")?;
            check_run(&before, &self.project)?;
            pass_through(status);
            return Ok(());
        }
        // the proxy for this command alone; its guard follows luggage, which waits for the command
        let sock = self.run.join(run_proxy_sock());
        let log = File::create(self.run.join("proxy.log"))?;
        let listener = bind_proxy(&sock)?;
        let mut group = guard(&tools, std::process::id(), &log)?;
        let status = self
            .serve_proxy(&tools, group.id(), listener, &log)
            .and_then(|()| c.status().context("cannot start bwrap"));
        kill_group(group.id());
        let _ = group.wait();
        let _ = fs::remove_file(&sock);
        // bwrap left an empty file in the chest's part to mount the socket on
        let _ = fs::remove_file(self.inside().join(run_proxy_sock()));
        let status = status?;
        check_run(&before, &self.project)?;
        pass_through(status);
        Ok(())
    }

    /// The bwrap command of `cmd_run`; the host variables in `pass` travel in its environment, never on its command line.
    fn run_command(
        &self,
        tools: &Tools,
        net: bool,
        cmd: &[String],
        pass: &OsStr,
    ) -> Result<Command> {
        let proxied = net && self.def.network.is_some();
        let mut c = if proxied {
            let own = own_net(tools);
            let mut c = Command::new(&own[0]);
            c.args(&own[1..]).arg(&tools.bwrap);
            c
        } else {
            Command::new(&tools.bwrap)
        };
        let net = match (net, proxied) {
            (_, true) => Net::Own,
            (true, false) => Net::Share,
            (false, false) => Net::Off,
        };
        clean_env(&mut c)
            .env(vm::PASS_ENV, pass)
            .args(self.bwrap_args(tools, net)?)
            .arg("--die-with-parent");
        if proxied {
            c.args(self.bind_inside(&run_proxy_sock()));
            // the command must not start before the proxy listens on 127.0.0.1:3128 (0C38 in /proc/net/tcp)
            let wait = format!(
                r#"for _ in {{1..100}}; do [[ $(</proc/net/tcp) == *" 0100007F:0C38 00000000:0000 0A "* ]] && break; {}/bin/sleep 0.01; done; exec "$@""#,
                tools.coreutils
            );
            c.args([tools.bash(), "--noprofile".into(), "-c".into()])
                .arg(self.inside_proxy(tools, &run_proxy_sock()) + &wait)
                .arg("sh");
        }
        c.arg(tools.bash()).arg("--noprofile");
        shell_args(&mut c, cmd);
        Ok(c)
    }

    /// The `host` variables for `exec`, `open` and `run` as shell lines: the harmless ones and `pass_env`, as in the VM.
    fn host_env(&self, host: impl IntoIterator<Item = (OsString, OsString)>) -> OsString {
        vm::env_script(&vm::passed_env(host, &self.def.pass_env, self.def.network.is_some()))
    }

    /// Brings the dev environment in the guest up to date with the project flake.
    fn vm_sync(&self) -> Result<()> {
        let env = self.dev_env()?;
        let compose = self.run.join("process-compose.yaml");
        let yaml = self.render_compose();
        // only write on change: the copy into the guest follows the modification time
        if fs::read_to_string(&compose).ok().as_deref() != Some(yaml.as_str()) {
            fs::write(&compose, yaml)?;
        }
        self.sync_network(false)?;
        vm::sync_env(&self.name, &self.state.join("vm"), &self.run, &env, &compose, &self.def.hosts)
    }

    /// Builds the skill set of the chest in `<state>/skills` and returns the names of its skills.
    /// A chest without `skills` reads no config, unless it had a set before that must go.
    fn skill_set(&self) -> Result<Vec<String>> {
        let root = self.state.join("skills");
        if self.def.skills.is_empty() && root.symlink_metadata().is_err() {
            return Ok(Vec::new());
        }
        skills::chest_set(&config::load()?.skills, &self.def.skills, &root)
    }

    /// Links the skill set with `names` into the home in the guest; `had` says the chest had a set before.
    fn vm_skills(&self, had: bool, names: &[String]) -> Result<()> {
        if names.is_empty() && !had {
            return Ok(());
        }
        let store_path = fs::read_link(self.state.join("skills")).ok();
        let script = skills::guest_script(store_path.as_deref().and_then(Path::to_str), names);
        if !vm::script(&self.name, &self.state.join("vm"), &self.run, &script).status()?.success() {
            bail!("cannot link the skills in the VM");
        }
        Ok(())
    }

    /// A process-compose command in the guest, e.g. `process list -o json`.
    fn vm_pc(&self, args: &str) -> Command {
        vm::script(
            &self.name,
            &self.state.join("vm"),
            &self.run,
            &format!("{PC_GUEST} {args} -U -u {RUN_INSIDE}/pc.sock"),
        )
    }

    /// Starts the services in the guest and waits until they are ready, like `up` in the bubblewrap chest.
    fn vm_services(&self, pid: u32) -> Result<()> {
        if self.def.services.is_empty() {
            return Ok(());
        }
        let start = format!(
            // `cd … && setsid … &` would put the whole list in a background subshell that keeps the ssh session open
            "source ~/.luggage/rc >/dev/null; export PATH=\"$(cat /etc/luggage/path):$PATH\"; cd {} || exit 1; setsid {PC_GUEST} up -t=false -U -u {RUN_INSIDE}/pc.sock -L {RUN_INSIDE}/pc.log -f {RUN_INSIDE}/process-compose.yaml </dev/null >{RUN_INSIDE}/services.log 2>&1 &",
            sh_quote(&self.project.to_string_lossy())
        );
        if !vm::script(&self.name, &self.state.join("vm"), &self.run, &start).status()?.success() {
            bail!("cannot start the services in the VM");
        }
        let log = format!("`luggage exec cat {RUN_INSIDE}/pc.log`");
        let limit = self.ready_limit();
        for _ in 0..limit * 4 {
            let out = self.vm_pc("process list -o json").stderr(Stdio::null()).output()?;
            if self.check_services(&out.stdout, &log)? {
                return Ok(());
            }
            if !alive(pid) {
                bail!("VM stopped before the services were ready");
            }
            sleep(Duration::from_millis(250));
        }
        bail!(
            "services not ready after {limit} s (the chest keeps running) — `luggage status`, log: {log}"
        )
    }

    /// Starts the VM unless it runs already and returns its PID. Only one start at a time:
    /// a second caller (direnv in the background, a stand-in) waits and then finds the VM ready.
    fn vm_up(&self, nixpkgs: &str) -> Result<u32> {
        fs::create_dir_all(&self.run)
            .with_context(|| format!("cannot create {}", self.run.display()))?;
        // Rust opens files with O_CLOEXEC, so qemu and virtiofsd do not inherit the lock
        let lock = File::create(self.run.join("up.lock"))?;
        lock.lock()?;
        if let Some(pid) = self.running() {
            return Ok(pid);
        }
        self.print_notes();
        self.stop_other_vms();
        self.ensure_envrc_hook()?;
        let dir = self.state.join("vm");
        vm::build(&self.name, &self.project, nixpkgs, &dir, &self.def.packages, &self.def.hosts)?;
        // before the start: a source that cannot be fetched stops it, not a running VM
        let had_skills = self.state.join("skills").symlink_metadata().is_ok();
        let skills = self.skill_set()?;
        self.note_git_files()?;
        let pid = vm::start(
            &self.name,
            &self.project,
            &dir,
            &self.run,
            &self.pid_file(),
            self.def.network.as_deref(),
        )?;
        self.forward_ports(|host, guest| vm::forward_port(&self.run, host, guest))?;
        self.vm_sync()?;
        self.vm_skills(had_skills, &skills)?;
        fs::write(self.run.join("started"), self.vm_fingerprint(nixpkgs)?)?;
        self.vm_services(pid)?;
        Ok(pid)
    }

    /// Hash of what a VM start depends on: guest flake, services, ports and, for the services, env.sh.
    /// Only renders and reads files, so it is cheap enough for every stand-in call.
    fn vm_fingerprint(&self, nixpkgs: &str) -> Result<String> {
        let flake = vm::guest_flake(
            &self.name,
            &self.project,
            nixpkgs,
            &self.state.join("vm"),
            &self.def.packages,
            &self.def.hosts,
        )?;
        // the configured specs, not the ports chosen at start: those differ whenever one was taken
        let ports: Vec<String> = self
            .def
            .services
            .iter()
            .flat_map(|(name, svc)| {
                let fixed = if self.def.ports.contains_key(name) { "fixed " } else { "" };
                svc.ports.iter().map(move |p| format!("{fixed}{name}={p}"))
            })
            .collect();
        // commands get a fresh env.sh anyway, only the running services keep the old one
        let env = if self.def.services.is_empty() {
            String::new()
        } else {
            fs::read_to_string(self.run.join("env.sh")).unwrap_or_default()
        };
        // only whether the network is restricted: the proxy follows a changed list by itself
        let network = if self.def.network.is_some() { "restricted" } else { "" };
        Ok(vm::fingerprint(&[&flake, &self.render_compose(), &ports.join(" "), &env, network]))
    }

    /// Warns if the running VM was started with another definition than the current one; never restarts it.
    fn vm_warn_outdated(&self, nixpkgs: &str) {
        // no record: the VM was started by an older luggage
        let Ok(started) = fs::read_to_string(self.run.join("started")) else {
            return;
        };
        // stderr: stdout belongs to the command of a stand-in, and direnv runs the hook's stdout as shell code
        if self.vm_fingerprint(nixpkgs).is_ok_and(|now| now != started) {
            eprintln!(
                "luggage: warning: chest {} runs with an outdated definition — run `luggage down && luggage up`",
                self.name
            );
        }
    }

    /// Makes the service ports reachable on 127.0.0.1 of the host with `forward(host, guest)` and lists them in `<run>/ports`.
    /// A taken host port moves to the next free one, except a fixed one from `[ports]`, which is only reported.
    fn forward_ports(&self, mut forward: impl FnMut(u16, u16) -> Result<()>) -> Result<()> {
        let mut wanted = Vec::new();
        for (name, svc) in &self.def.services {
            let fixed = self.def.ports.contains_key(name);
            for spec in &svc.ports {
                let (host, guest) = port_pair(spec)?;
                wanted.push((!fixed, name, host, guest));
            }
        }
        // fixed ports first, and a moved port never takes one that another service asks for
        wanted.sort_by_key(|w| w.0);
        let reserved: BTreeSet<u16> = wanted.iter().map(|w| w.2).collect();
        let mut forwarded = String::new();
        for (movable, name, host, guest) in wanted {
            let mut result = forward(host, guest).map(|()| host);
            if let (Err(e), true) = (&result, movable) {
                let free = |p| std::net::TcpListener::bind(("127.0.0.1", p)).is_ok();
                result = match next_free_port(host, &reserved, free) {
                    Some(p) => forward(p, guest).map(|()| {
                        info(&format!("{name}:{guest} is on 127.0.0.1:{p} — {e:#}"));
                        p
                    }),
                    None => {
                        Err(anyhow::anyhow!("{e:#}, and no free port up to {PORT_TRIES} above"))
                    }
                };
            }
            match result {
                Ok(p) => {
                    let _ = writeln!(forwarded, "127.0.0.1:{p} → {name}:{guest}");
                }
                Err(e) => {
                    info(&format!(
                        "warning: {name}:{guest} is not reachable from the host — {e:#}"
                    ));
                }
            }
        }
        fs::write(self.run.join("ports"), forwarded)?;
        Ok(())
    }

    /// Stops the VMs of other chests: one at a time keeps memory free, and switching projects is the moment for it.
    fn stop_other_vms(&self) {
        let Some(Ok(dirs)) = self.run.parent().map(fs::read_dir) else {
            return;
        };
        for run in dirs.filter_map(|d| Some(d.ok()?.path())) {
            let Some(name) = run.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let pid = fs::read_to_string(run.join("chest.pid"))
                .ok()
                .and_then(|p| p.trim().parse::<u32>().ok())
                .filter(|p| alive(*p));
            if name == self.name || !run.join("control.socket").exists() {
                continue;
            }
            if let Some(pid) = pid {
                info(&format!("stopping chest {name}: only one VM runs at a time"));
                vm::stop(&name, &run, Some(pid));
                let _ = fs::remove_file(run.join("chest.pid"));
            }
        }
    }

    /// Adds the luggage line to the .envrc of the project if it is missing.
    fn ensure_envrc_hook(&self) -> Result<()> {
        let path = self.project.join(".envrc");
        let Ok(content) = fs::read_to_string(&path) else {
            info("note: the project has no .envrc, so direnv does not start the VM on `cd`");
            return Ok(());
        };
        if content.contains("luggage hook") {
            return Ok(());
        }
        let sep = if content.is_empty() || content.ends_with('\n') { "" } else { "\n" };
        fs::write(&path, format!("{content}{sep}{}", crate::flake::ENVRC_HOOK))?;
        info("added the luggage line to .envrc — run `direnv allow` once");
        Ok(())
    }

    /// Shell code for direnv: in VM mode the stand-ins come first on the `PATH` and the VM starts in the background.
    /// Must be fast, it runs on every `cd` into the project.
    pub fn cmd_hook(&self, nixpkgs: &str) -> Result<()> {
        self.guard()?;
        if self.def.mode != Mode::Vm {
            // a running bubblewrap chest follows a newly checked-out branch in the background
            if self.running().is_some() && self.branch_changed() {
                self.up_in_background()?;
            }
            return Ok(());
        }
        fs::create_dir_all(&self.run)?;
        let shims = self.run.join("shims");
        // `use flake` in the .envrc has just loaded the project packages into our environment,
        // so the stand-ins are there for the very first command, even before the VM is up.
        let from_env: Vec<String> = ["nativeBuildInputs", "buildInputs"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .collect();
        let mut inputs = from_env.join(" ");
        if inputs.trim().is_empty() {
            inputs = fs::read_to_string(self.run.join("env.sh"))
                .map(|e| vm::inputs_from_env_sh(&e))
                .unwrap_or_default();
        }
        vm::write_shims(&self.run, &inputs)?;
        println!("PATH_add {}", sh_quote(&shims.to_string_lossy()));
        if let Some(d) = &self.def_dir {
            println!("watch_file {}", sh_quote(&d.join("chest.toml").to_string_lossy()));
        }
        // `up` also moves the chest to the data of a newly checked-out branch
        if self.running().is_some() && !self.branch_changed() {
            self.vm_warn_outdated(nixpkgs);
        } else {
            self.up_in_background()?;
        }
        Ok(())
    }

    /// Starts `luggage up` in the background, for the hook: it must not wait for the chest.
    fn up_in_background(&self) -> Result<()> {
        // direnv reads the environment from fd 3 and waits until every holder closed it,
        // and Rust passes such descriptors on to children, so a small shell closes them all first.
        let log = File::create(self.run.join("up.log"))?;
        Command::new("/bin/sh")
            .args(["-c", CLOSE_FDS_AND_EXEC, "sh"])
            .arg(std::env::current_exe()?)
            .arg("up")
            .current_dir(&self.project)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("cannot start luggage up in the background")?;
        Ok(())
    }

    /// The file that names the branch whose service data is in use.
    fn branch_file(&self) -> PathBuf {
        self.state.join("branch")
    }

    /// The service data in use: `/data` of the bubblewrap chest, or the VM disk that holds it.
    fn data_slot(&self) -> PathBuf {
        if self.def.mode == Mode::Vm {
            self.state.join("vm/var.img")
        } else {
            self.state.join("data")
        }
    }

    /// Where the service data of the branch with directory name `dir` waits while another branch is checked out.
    fn parked(&self, dir: &str) -> PathBuf {
        let slot = self.data_slot();
        self.state.join("branches").join(dir).join(slot.file_name().unwrap_or_default())
    }

    /// The branch whose service data is in use, if luggage noted one.
    fn active_branch(&self) -> Option<String> {
        let b = fs::read_to_string(self.branch_file()).ok()?;
        let b = b.trim();
        (!b.is_empty()).then(|| b.to_owned())
    }

    /// The checked-out branch whose data the chest should use; `None` keeps the data in use.
    /// A detached HEAD (also during a rebase or bisect) gets a note once.
    fn wanted_branch(&self) -> Option<String> {
        let note = self.run.join("branch-note");
        let skip = match branch::head(&self.project) {
            Head::Branch(b) if branch::dir_name(&b).is_some() => {
                let _ = fs::remove_file(&note);
                return Some(b);
            }
            Head::Branch(b) => format!("the branch name {b} is too long"),
            Head::Detached => "HEAD is detached (or a rebase or bisect is under way)".to_owned(),
            Head::None => return None,
        };
        let active = self.active_branch()?;
        if fs::read_to_string(&note).ok().as_deref() != Some(skip.as_str()) {
            info(&format!("note: {skip} — chest {} keeps the data of branch {active}", self.name));
            let _ = fs::create_dir_all(&self.run).and_then(|()| fs::write(&note, &skip));
        }
        None
    }

    /// Does the checked-out branch differ from the one whose data is in use?
    fn branch_changed(&self) -> bool {
        match (self.wanted_branch(), self.active_branch()) {
            (Some(want), Some(active)) => want != active,
            _ => false,
        }
    }

    /// Gives each git branch its own service data: after a checkout the chest stops, its data is put aside, and the branch
    /// gets its own data back or a copy of the data in use. A running chest starts again, and the result tells whether it did.
    fn follow_branch(&self, nixpkgs: &str) -> Result<bool> {
        let Some(want) = self.wanted_branch() else {
            return Ok(false);
        };
        if self.active_branch().as_deref() == Some(want.as_str()) {
            return Ok(false);
        }
        fs::create_dir_all(&self.run)
            .with_context(|| format!("cannot create {}", self.run.display()))?;
        let lock = File::create(self.run.join("branch.lock"))?;
        lock.lock()?;
        // another luggage may have switched while this one waited
        let Some(active) = self.active_branch().filter(|a| *a != want) else {
            if self.active_branch().is_none() {
                // the first time: the data in use belongs to the branch checked out now
                fs::create_dir_all(&self.state)?;
                fs::write(self.branch_file(), &want)?;
            }
            return Ok(false);
        };
        let started = std::time::Instant::now();
        info(&format!("chest {}: branch {active} → {want}, switching its service data", self.name));
        let pid = {
            // a VM start under way must not use the data while it moves
            let up = File::create(self.run.join("up.lock"))?;
            up.lock()?;
            // read under the lock: a start that finished while this one waited must be stopped as well
            let pid = self.running();
            if pid.is_some() {
                self.stop()?;
            }
            self.swap_data(&active, &want)?;
            pid
        };
        if pid.is_some() {
            if self.def.mode == Mode::Vm {
                self.vm_up(nixpkgs)?;
            } else {
                self.bwrap_up(nixpkgs)?;
            }
        }
        info(&format!(
            "chest {} uses the data of branch {want} ({:.1} s)",
            self.name,
            started.elapsed().as_secs_f32()
        ));
        Ok(pid.is_some())
    }

    /// Puts the data of branch `active` aside and brings in that of `want`, or a copy of `active`'s if `want` has none.
    /// A switch that was cut short continues: data that is already aside stays there.
    fn swap_data(&self, active: &str, want: &str) -> Result<()> {
        let dir =
            |b: &str| branch::dir_name(b).with_context(|| format!("unusable branch name {b}"));
        let slot = self.data_slot();
        let aside = self.parked(&dir(active)?);
        let back = self.parked(&dir(want)?);
        if slot.symlink_metadata().is_ok() {
            if aside.symlink_metadata().is_ok() {
                bail!(
                    "both {} and {} exist — keep one of them, then run luggage again",
                    slot.display(),
                    aside.display()
                );
            }
            if let Some(parent) = aside.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&slot, &aside).with_context(|| format!("cannot move {}", slot.display()))?;
        }
        if back.symlink_metadata().is_ok() {
            fs::rename(&back, &slot).with_context(|| format!("cannot move {}", back.display()))?;
            if let Some(parent) = back.parent() {
                let _ = fs::remove_dir(parent);
            }
        } else if aside.symlink_metadata().is_ok() {
            branch::copy(&aside, &slot)?;
        }
        fs::write(self.branch_file(), want)?;
        Ok(())
    }

    /// Removes the service data of branches that no longer exist in the repo; never the data in use.
    pub fn cmd_prune(&self) -> Result<()> {
        self.guard()?;
        let out = Command::new("git")
            .args(["for-each-ref", "--format=%(refname)", "refs/heads/"])
            .current_dir(&self.project)
            .stderr(Stdio::inherit())
            .output()
            .context("cannot start git")?;
        if !out.status.success() {
            bail!("git for-each-ref failed");
        }
        let refs = String::from_utf8_lossy(&out.stdout);
        let exists: BTreeSet<&str> =
            refs.lines().filter_map(|r| r.strip_prefix("refs/heads/")).collect();
        let mut removed = 0;
        for (name, dir) in self.parked_branches() {
            if exists.contains(name.as_str()) || self.active_branch().as_deref() == Some(&name) {
                continue;
            }
            let size = branch::usage(&dir);
            fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
            info(&format!("removed the data of branch {name} ({})", branch::human(size)));
            removed += 1;
        }
        if removed == 0 {
            info("no data of deleted branches");
        }
        Ok(())
    }

    /// The branches whose data is put aside, with their directories, sorted by name.
    fn parked_branches(&self) -> Vec<(String, PathBuf)> {
        let mut v: Vec<(String, PathBuf)> = fs::read_dir(self.state.join("branches"))
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let e = e.ok()?;
                Some((branch::branch_name(&e.file_name().to_string_lossy())?, e.path()))
            })
            .collect();
        v.sort();
        v
    }

    /// The `Branch:` lines of `status`: the branch in use, what HEAD wants, and the data of each branch with its size.
    fn print_branches(&self) {
        let Some(active) = self.active_branch() else {
            return;
        };
        let pending = match branch::head(&self.project) {
            Head::Branch(b) if b != active => {
                format!(" (checked out: {b} — switches with the next command)")
            }
            Head::Detached => " (HEAD is detached)".to_owned(),
            _ => String::new(),
        };
        println!("Branch:      {active}{pending}");
        let mut rows =
            vec![format!("{active} {} (in use)", branch::human(branch::usage(&self.data_slot())))];
        rows.extend(
            self.parked_branches()
                .into_iter()
                .map(|(name, dir)| format!("{name} {}", branch::human(branch::usage(&dir)))),
        );
        println!("Branch data: {}", rows.join(", "));
    }

    /// Prints the notes from service detection.
    fn print_notes(&self) {
        for n in self.detected.iter().flat_map(|d| &d.notes) {
            info(&format!("note: {n}"));
        }
    }

    /// Starts the chest in the background and waits until all services are ready.
    pub fn cmd_up(&self, nixpkgs: &str) -> Result<()> {
        self.guard_up()?;
        let switched = self.follow_branch(nixpkgs)?;
        if self.def.mode == Mode::Vm {
            let pid = self.vm_up(nixpkgs)?;
            self.vm_warn_outdated(nixpkgs);
            info(&format!(
                "chest {} is running (VM, pid {pid}) — `luggage status`, `luggage down`",
                self.name
            ));
            return Ok(());
        }
        if let Some(pid) = self.running() {
            if !switched {
                info(&format!("chest {} is already running (pid {pid})", self.name));
            }
            return Ok(());
        }
        self.bwrap_up(nixpkgs)
    }

    /// Starts the bubblewrap chest and waits until all services are ready.
    fn bwrap_up(&self, nixpkgs: &str) -> Result<()> {
        crate::setup::require_userns()?;
        self.print_notes();
        let tools = self.prepare(nixpkgs)?;
        let info_json = self.run.join("info.json");
        let _ = fs::remove_file(&info_json);
        let _ = fs::remove_file(self.socket());
        stop_bridges(&self.run);
        // bound before the chest starts, so early connections wait for the proxy instead of failing
        let proxy = self.def.network.as_ref().map(|_| bind_proxy(&self.run.join("proxy.sock")));
        let main = self.inside_proxy(&tools, "proxy.sock")
            + &if self.def.services.is_empty() {
                "exec sleep infinity".to_owned()
            } else {
                format!(
                    "exec process-compose up -t=false -U -u {RUN_INSIDE}/pc.sock -L {RUN_INSIDE}/pc.log -f {RUN_INSIDE}/process-compose.yaml"
                )
            };
        self.note_git_files()?;
        let log = File::create(self.run.join("chest.log"))?;
        // bwrap reports the PID of its child (PID 1 of the chest) on fd 3.
        Command::new(tools.bash())
            .args(["-c", r#"exec 3>"$1"; shift; exec "$@""#, "sh"])
            .arg(&info_json)
            .arg(tools.util("setsid"))
            .args(own_net(&tools))
            .arg(&tools.bwrap)
            .args(self.bwrap_args(&tools, Net::Own)?)
            .args(proxy.as_ref().map(|_| self.bind_inside("proxy.sock")).into_iter().flatten())
            .args([
                "--clearenv",
                "--setenv",
                "HOME",
                &home().to_string_lossy(),
                "--setenv",
                "TERM",
                &term(),
            ])
            .args(["--info-fd", "3"])
            .arg(tools.bash())
            .args(["--noprofile", "-c", &format!("source {RUN_INSIDE}/rc; {main}")])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("cannot start the chest")?;
        let pid = self.wait_for_pid(&info_json)?;
        fs::write(self.pid_file(), pid.to_string())?;
        if !self.def.services.is_empty() {
            for _ in 0..50 {
                if self.socket().exists() || !alive(pid) {
                    break;
                }
                sleep(Duration::from_millis(100));
            }
        }
        if !alive(pid) {
            bail!(
                "chest stopped right away — see {}/chest.log and chest/pc.log{}",
                self.run.display(),
                crate::setup::userns_hint()
            );
        }
        self.bridge_ports(&tools, pid, proxy.transpose()?)?;
        // Return only once the services are ready. Also, a `down` while a readiness_probe
        // is still pending hangs in process-compose.
        if !self.def.services.is_empty() {
            let limit = self.ready_limit();
            let mut ready = false;
            for _ in 0..limit * 4 {
                if self.services_ready(&tools, pid)? {
                    ready = true;
                    break;
                }
                if !alive(pid) {
                    bail!("chest stopped before the services were ready");
                }
                sleep(Duration::from_millis(250));
            }
            if !ready {
                bail!(
                    "services not ready after {limit} s (the chest keeps running) — `luggage status`, log: {}",
                    self.run.join("chest.log").display()
                );
            }
        }
        info(&format!(
            "chest {} is running (pid {pid}) — `luggage status`, `luggage open`",
            self.name
        ));
        Ok(())
    }

    /// Shell code for the chest that starts a socat from `PROXY_INSIDE` to the host proxy at `/run/luggage/<sock>`,
    /// if the chest has a `network`.
    fn inside_proxy(&self, tools: &Tools, sock: &str) -> String {
        if self.def.network.is_none() {
            return String::new();
        }
        let port = PROXY_INSIDE.rsplit_once(':').map_or("", |(_, p)| p);
        format!(
            "{}/bin/socat TCP-LISTEN:{port},bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:{RUN_INSIDE}/{sock} 2>>{RUN_INSIDE}/proxy.log & ",
            tools.socat
        )
    }

    /// Serves each connection to `listener` with a `luggage network-proxy` of its own, in the process group `group`.
    fn serve_proxy(
        &self,
        tools: &Tools,
        group: u32,
        listener: UnixListener,
        log: &File,
    ) -> Result<()> {
        Command::new(format!("{}/bin/socat", tools.socat))
            .arg("ACCEPT-FD:0,fork")
            // socat drops plain quotes; the paths come from the environment, so they need no quoting
            .arg(r#"SYSTEM:exec \"$LUGGAGE_EXE\" network-proxy \"$LUGGAGE_NETWORK\""#)
            .env("LUGGAGE_EXE", std::env::current_exe()?)
            .env("LUGGAGE_NETWORK", self.network_file())
            .process_group(i32::try_from(group)?)
            .stdin(OwnedFd::from(listener))
            .stdout(Stdio::null())
            .stderr(log.try_clone()?)
            .spawn()
            .context("cannot start the proxy")?;
        Ok(())
    }

    /// Bridges the service ports from 127.0.0.1 on the host into the network namespace of the chest `pid` and serves
    /// the `proxy` socket. They share a process group with a guard that ends it once the chest `pid` is gone.
    fn bridge_ports(&self, tools: &Tools, pid: u32, proxy: Option<UnixListener>) -> Result<()> {
        if self.def.services.values().all(|s| s.ports.is_empty()) && proxy.is_none() {
            return Ok(());
        }
        let log = File::create(self.run.join("bridges.log"))?;
        let group = guard(tools, pid, &log)?.id();
        fs::write(self.run.join("bridges.pid"), group.to_string())?;
        if let Some(listener) = proxy {
            self.serve_proxy(tools, group, listener, &log)?;
        }
        let socat = format!("{}/bin/socat", tools.socat);
        self.forward_ports(|host, guest| {
            // luggage binds the port itself, so a taken one fails here and not later in socat
            let listener = std::net::TcpListener::bind(("127.0.0.1", host))
                .map_err(|e| anyhow::anyhow!("127.0.0.1:{host}: {e}"))?;
            Command::new(&socat)
                .arg("ACCEPT-FD:0,fork")
                .arg(bridge_address(tools, pid, guest))
                .process_group(i32::try_from(group)?)
                .stdin(OwnedFd::from(listener))
                .stdout(Stdio::null())
                .stderr(log.try_clone()?)
                .spawn()
                .context("cannot start socat")?;
            Ok(())
        })
    }

    /// Are all services ready? Fails if a service stopped that should keep running.
    fn services_ready(&self, tools: &Tools, pid: u32) -> Result<bool> {
        let out = Self::pc(tools, pid, &["process", "list", "-o", "json"])
            .stderr(Stdio::null())
            .output()?;
        self.check_services(&out.stdout, &self.run.join("chest.log").display().to_string())
    }

    /// Seconds `up` waits for the services: the longest `ready_timeout` plus a little.
    fn ready_limit(&self) -> u32 {
        self.def
            .services
            .values()
            .map(|s| s.ready_timeout.unwrap_or(DEFAULT_READY_TIMEOUT))
            .max()
            .unwrap_or(DEFAULT_READY_TIMEOUT)
            + 5
    }

    /// Reads `process-compose process list -o json` and tells whether all services are ready.
    /// Fails if a service stopped that should keep running; `log` says where to look.
    fn check_services(&self, json: &[u8], log: &str) -> Result<bool> {
        let Ok(list) = serde_json::from_slice::<Vec<ProcState>>(json) else {
            return Ok(false);
        };
        let mut ready = 0;
        for p in &list {
            let Some(svc) = self.def.services.get(&p.name) else {
                continue;
            };
            let finished = !p.is_running && matches!(p.status.as_str(), "Completed" | "Error");
            if finished && (!svc.once || p.exit_code != 0) {
                bail!("service {} stopped (exit {}) — log: {log}", p.name, p.exit_code);
            }
            let ok = if svc.once {
                finished
            } else if svc.ready.is_some() {
                p.is_ready == "Ready"
            } else {
                p.is_running
            };
            if ok {
                ready += 1;
            }
        }
        Ok(ready == self.def.services.len())
    }

    /// Waits until bwrap writes the PID of the chest to `info_json`.
    fn wait_for_pid(&self, info_json: &Path) -> Result<u32> {
        #[derive(Deserialize)]
        struct BwrapInfo {
            #[serde(rename = "child-pid")]
            child_pid: u32,
        }
        for _ in 0..100 {
            if let Some(i) = fs::read_to_string(info_json)
                .ok()
                .and_then(|t| serde_json::from_str::<BwrapInfo>(&t).ok())
            {
                return Ok(i.child_pid);
            }
            sleep(Duration::from_millis(100));
        }
        bail!(
            "chest does not start — see {}{}",
            self.run.join("chest.log").display(),
            crate::setup::userns_hint()
        )
    }

    /// Runs `cmd` in the VM, which starts if needed; without a command, a shell.
    fn vm_enter(&self, net: bool, cmd: &[String], nixpkgs: &str) -> Result<()> {
        // starts the VM or waits for a start that is under way
        self.vm_up(nixpkgs)?;
        self.vm_sync()?;
        self.vm_warn_outdated(nixpkgs);
        // only the project is shared with the guest
        let cwd = std::env::current_dir()?;
        let cwd = if cwd.starts_with(&self.project) { cwd } else { self.project.clone() };
        let dir = self.state.join("vm");
        pass_through(vm::exec(&self.name, &dir, &self.run, &cwd, net, &self.def.pass_env, cmd)?);
        Ok(())
    }

    /// Runs `cmd` in the running chest; without a command, a shell.
    fn enter(&self, cmd: &[String], nixpkgs: &str) -> Result<()> {
        self.guard()?;
        self.follow_branch(nixpkgs)?;
        if self.def.mode == Mode::Vm {
            return self.vm_enter(true, cmd, nixpkgs);
        }
        let pid = self.running().with_context(|| {
            format!("chest {} is not running — run `luggage up` first", self.name)
        })?;
        let tools = self.tools()?;
        self.sync_network(false)?;
        let mut c = nsenter(pid, &tools);
        c.env(vm::PASS_ENV, self.host_env(std::env::vars_os()));
        shell_args(&mut c, cmd);
        pass_through(c.status().context("cannot start nsenter")?);
        Ok(())
    }

    /// Runs `cmd` in the running chest.
    pub fn cmd_exec(&self, cmd: &[String], nixpkgs: &str) -> Result<()> {
        self.enter(cmd, nixpkgs)
    }

    /// Opens a shell in the running chest.
    pub fn cmd_open(&self, nixpkgs: &str) -> Result<()> {
        self.enter(&[], nixpkgs)
    }

    /// The process-compose client, run in the chest `pid`: its socket could be swapped for a link to a host socket.
    fn pc(tools: &Tools, pid: u32, args: &[&str]) -> Command {
        let mut c = nsenter(pid, tools);
        c.args(["-c", r#"exec "$@""#, "bash"])
            .arg(format!("{}/bin/process-compose", tools.process_compose))
            .args(args)
            .args(["-U", "-u", &format!("{RUN_INSIDE}/pc.sock")])
            .args(["-L", &format!("{RUN_INSIDE}/pc-client.log")]);
        c
    }

    /// The git files as the chest found them at its start, in the state dir, which the chest cannot write.
    fn git_files(&self) -> PathBuf {
        self.state.join("git-files")
    }

    /// The first git file that the host trusts and that changed since the chest started, if any.
    /// `deep` also looks for nested repos in the whole project.
    fn changed_git_file(&self, deep: bool) -> Option<String> {
        let before = fs::read_to_string(self.git_files()).ok()?;
        let before = if deep { before } else { gitcheck::shallow(&before) };
        gitcheck::changed(&before, &gitcheck::snapshot(&self.project, deep))
    }

    /// Stops the chest and fails if a file of the top `.git` that the host trusts changed since the start.
    fn check_git_files(&self) -> Result<()> {
        let Some(path) = self.changed_git_file(false) else {
            return Ok(());
        };
        if self.running().is_some() {
            self.stop()?;
        }
        bail!(
            "{path} changed while chest {} ran, and git on the host could run code from it — the chest is stopped; check the file, then `luggage down` accepts it",
            self.name
        )
    }

    /// Warns if a nested repo changed since the start; it walks the whole project, so only `up` calls it.
    /// Only a warning: tools in the chest create such repos on purpose, like `composer` from source.
    fn warn_nested_repos(&self) {
        let Ok(before) = fs::read_to_string(self.git_files()) else {
            return;
        };
        let now = gitcheck::snapshot(&self.project, true);
        if let Some(path) =
            gitcheck::changed(&gitcheck::nested_repos(&before), &gitcheck::nested_repos(&now))
        {
            info(&format!(
                "warning: the repo {path} changed while chest {} ran, and git on the host could run code from it there — check it",
                self.name
            ));
        }
    }

    /// Checks the git files, then binds the files that the host trusts read-only again where the host replaced one.
    fn guard(&self) -> Result<()> {
        self.check_git_files()?;
        let pid = self.running();
        let target = if self.def.mode == Mode::Vm {
            // the guest sees the files through virtiofsd, whose namespace has the project at /
            pid.and_then(|_| vm::project_server(&self.run)).map(|p| (p, PathBuf::from("/")))
        } else {
            pid.map(|p| (p, self.project.clone()))
        };
        if let Some((pid, root)) = target
            && let Err(e) = remount::protect_again(pid, &self.project, &root)
        {
            info(&format!("warning: {e:#} — run `luggage down && luggage up`"));
        }
        Ok(())
    }

    /// `guard` for `up`; a running chest also gets the check for nested repos, which a start does anyway.
    fn guard_up(&self) -> Result<()> {
        self.guard()?;
        if self.running().is_some() {
            self.warn_nested_repos();
        }
        Ok(())
    }

    /// Checks the git files and notes them anew, right before the chest starts.
    fn note_git_files(&self) -> Result<()> {
        self.check_git_files()?;
        self.warn_nested_repos();
        fs::create_dir_all(&self.state)?;
        fs::write(self.git_files(), gitcheck::snapshot(&self.project, true))?;
        Ok(())
    }

    /// Stops the chest and accepts the git files as they are, after a warning if one changed.
    pub fn cmd_down(&self) -> Result<()> {
        if let Some(path) = self.changed_git_file(true) {
            info(&format!(
                "warning: {path} changed while chest {} ran, and git on the host could run code from it — check the file",
                self.name
            ));
        }
        self.stop()?;
        let _ = fs::remove_file(self.git_files());
        Ok(())
    }

    /// Stops the chest: first the services in order, SIGKILL as a last resort.
    fn stop(&self) -> Result<()> {
        // control.socket also finds a VM whose chest.toml is broken right now
        let is_vm = self.def.mode == Mode::Vm || self.run.join("control.socket").exists();
        let Some(pid) = self.running() else {
            if is_vm {
                vm::stop(&self.name, &self.run, None);
            }
            stop_bridges(&self.run);
            let _ = fs::remove_file(self.pid_file());
            info(&format!("chest {} is not running", self.name));
            return Ok(());
        };
        if is_vm {
            // stop the services in order first, so the databases shut down cleanly
            let _ = self.vm_pc("down").stdout(Stdio::null()).stderr(Stdio::null()).status();
            vm::stop(&self.name, &self.run, Some(pid));
        } else if self.socket().exists() {
            // Stop the services in order; the SIGKILL below is only for when nothing else works.
            let tools = self.tools()?;
            let client = Self::pc(&tools, pid, &["down"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let stopped = wait_dead(pid, 50);
            if !stopped {
                // process-compose hangs: all processes in the chest get SIGTERM, so databases shut down cleanly.
                let _ = nsenter(pid, &tools)
                    .args(["-c", "kill -TERM -1"])
                    .stderr(Stdio::null())
                    .status();
                wait_dead(pid, 100);
            }
            if let Ok(mut c) = client {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        // PID 1 in its own PID namespace ignores SIGTERM from outside; SIGKILL tears down the whole namespace.
        if alive(pid) {
            let kill = self.tools().map_or_else(|_| "kill".into(), |t| t.util("kill"));
            let _ = Command::new(kill).args(["-9", &pid.to_string()]).status();
            // the kernel still tears down the namespace; report "stopped" only once it is gone
            wait_dead(pid, 50);
        }
        stop_bridges(&self.run);
        let _ = fs::remove_file(self.pid_file());
        let _ = fs::remove_file(self.socket());
        info(&format!("chest {} stopped", self.name));
        Ok(())
    }

    /// Shows the definition, services and state of the chest.
    pub fn cmd_status(&self, nixpkgs: &str) -> Result<()> {
        self.guard()?;
        println!("Chest:       {}", self.name);
        println!("Project:     {}", self.project.display());
        println!("Mode:        {}", if self.def.mode == Mode::Vm { "vm" } else { "bubblewrap" });
        println!(
            "Definition:  {}",
            self.def_dir
                .as_ref()
                .map_or_else(|| "none".into(), |d| d.join("chest.toml").display().to_string())
        );
        if let Some(d) = &self.detected {
            let found: Vec<_> = d.found.iter().map(|(n, what)| format!("{n} ({what})")).collect();
            let file = d.file.file_name().unwrap_or_default().to_string_lossy();
            println!(
                "Detected:    {file}: {}",
                if found.is_empty() { "nothing".into() } else { found.join(", ") }
            );
            if !d.skipped.is_empty() {
                println!("Skipped:     {}", d.skipped.join(", "));
            }
        }
        let services: Vec<_> = self.def.services.keys().map(String::as_str).collect();
        println!(
            "Services:    {}",
            if services.is_empty() { "none".into() } else { services.join(", ") }
        );
        self.print_notes();
        println!("State:       {}", self.state.display());
        self.print_branches();
        let Some(pid) = self.running() else {
            println!("Status:      not running");
            return Ok(());
        };
        println!("Status:      running (pid {pid})");
        let ports = fs::read_to_string(self.run.join("ports")).unwrap_or_default();
        let ports: Vec<_> = ports.lines().collect();
        println!(
            "Ports:       {}",
            if ports.is_empty() { "none".into() } else { ports.join(", ") }
        );
        // without the proxy, the VM is open and the bubblewrap chest has no network
        let vm = self.def.mode == Mode::Vm;
        let proxied = if vm { self.network_file() } else { self.run.join("proxy.sock") }.exists();
        let hosts = fs::read_to_string(self.network_file()).unwrap_or_default();
        let network = if vm && !proxied {
            "open".into()
        } else if !proxied || hosts.trim().is_empty() {
            "closed".into()
        } else {
            format!("only {}", hosts.lines().collect::<Vec<_>>().join(", "))
        };
        println!("Network:     {network}");
        if vm {
            self.vm_warn_outdated(nixpkgs);
        }
        if self.def.mode == Mode::Vm && !self.def.services.is_empty() {
            println!();
            // stderr is hidden: process-compose prints debug lines there about its missing config
            self.vm_pc("process list -o wide").stderr(Stdio::null()).status()?;
        } else if self.socket().exists() {
            println!();
            let tools = self.tools()?;
            // stderr is hidden: process-compose prints debug lines there about its missing config
            Self::pc(&tools, pid, &["process", "list", "-o", "wide"])
                .stderr(Stdio::null())
                .status()?;
        }
        Ok(())
    }
}

/// Fails if a git file that the host trusts changed during `luggage run`, compared with `before`.
fn check_run(before: &str, project: &Path) -> Result<()> {
    if let Some(path) = gitcheck::changed(before, &gitcheck::snapshot(project, false)) {
        bail!(
            "{path} changed in the chest, and git on the host could run code from it — check the file"
        );
    }
    Ok(())
}

/// Stops the port bridges and the proxy of a bubblewrap chest, if they still run, and forgets their ports.
fn stop_bridges(run: &Path) {
    stop_group(&run.join("bridges.pid"), BRIDGES);
    for f in [run.join("ports"), run.join("proxy.sock")] {
        let _ = fs::remove_file(f);
    }
}

/// Stops the process group of the process in `pid_file` if it still runs `marker`, and removes the file.
pub fn stop_group(pid_file: &Path, marker: &str) {
    if let Some(pid) = fs::read_to_string(pid_file).ok().and_then(|p| p.trim().parse::<u32>().ok())
        // the id may belong to another process by now
        && fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|c| c.windows(marker.len()).any(|w| w == marker.as_bytes()))
    {
        kill_group(pid);
        wait_dead(pid, 20);
    }
    let _ = fs::remove_file(pid_file);
}

/// The socat address of a port bridge: for each connection, nsenter and a socat to port `guest` in the network
/// namespace of the chest `pid`. It names no path the chest can write, which it could swap for a link to a host socket.
fn bridge_address(tools: &Tools, pid: u32, guest: u16) -> String {
    format!(
        // socat splits parameters at ":", so those of the inner address are escaped; store paths have none
        r"EXEC:{} -t {pid} --user --net --preserve-credentials {}/bin/socat STDIO TCP\:127.0.0.1\:{guest}",
        tools.util("nsenter"),
        tools.socat
    )
}

/// Sends SIGTERM to the process group `group`.
fn kill_group(group: u32) {
    let _ = Command::new("kill").args(["--", &format!("-{group}")]).stderr(Stdio::null()).status();
}

/// Starts the leader of a new process group that ends the group once `pid` is gone; it logs to `log`.
fn guard(tools: &Tools, pid: u32, log: &File) -> Result<Child> {
    Command::new(tools.bash())
        .args(["-c", r#""$2" --pid="$1" -f /dev/null; kill -TERM 0"#, BRIDGES])
        .arg(pid.to_string())
        .arg(format!("{}/bin/tail", tools.coreutils))
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log.try_clone()?)
        .spawn()
        .context("cannot start the port bridges")
}

/// Listens on the socket `sock` for the proxy, in place of an old one.
fn bind_proxy(sock: &Path) -> Result<UnixListener> {
    let _ = fs::remove_file(sock);
    UnixListener::bind(sock).with_context(|| format!("cannot listen on {}", sock.display()))
}

/// The socket of the proxy for one `luggage run`, in the runtime dir.
fn run_proxy_sock() -> String {
    format!("proxy-{}.sock", std::process::id())
}

/// unshare and a shell that run the rest of the command in its own network namespace with only `lo`,
/// where it is root and may use ports below 1024.
fn own_net(tools: &Tools) -> Vec<String> {
    let ip = format!("{}/bin/ip", tools.iproute2);
    vec![
        tools.util("unshare"),
        "--user".into(),
        "--map-root-user".into(),
        "--net".into(),
        tools.bash(),
        "-c".into(),
        format!(
            r#"{ip} link set lo up && echo 0 > /proc/sys/net/ipv4/ip_unprivileged_port_start && exec "$@""#
        ),
        "sh".into(),
    ]
}

/// bash in all namespaces of the running chest (like `docker exec`).
fn nsenter(pid: u32, tools: &Tools) -> Command {
    let mut c = Command::new(tools.util("nsenter"));
    // no --wd: nsenter --root with --wd gives a broken cwd, so the rc file does the cd
    c.args([
        "-t",
        &pid.to_string(),
        "--user",
        "--mount",
        "--net",
        "--uts",
        "--ipc",
        "--pid",
        "--root",
        "--preserve-credentials",
    ])
    .arg(tools.bash())
    .arg("--noprofile")
    .current_dir("/");
    clean_env(&mut c);
    c
}

/// The terminal type of the host, `xterm` if it has none.
fn term() -> String {
    std::env::var("TERM").unwrap_or_else(|_| "xterm".into())
}

/// Clears the environment of `c` down to `HOME` and `TERM`.
fn clean_env(c: &mut Command) -> &mut Command {
    c.env_clear().env("HOME", home()).env("TERM", term())
}

/// `-c 'source rc; exec "$@"' bash CMD…` or an interactive shell with rc.
fn shell_args(c: &mut Command, cmd: &[String]) {
    if cmd.is_empty() {
        c.args(["--rcfile", &format!("{RUN_INSIDE}/rc"), "-i"]);
    } else {
        c.args(["-c", &format!("source {RUN_INSIDE}/rc; exec \"$@\""), "bash"]).args(cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPARENCON: &str = r#"
hosts = ["database"]
packages = ["mariadb"]

[services.db-init]
command = "mariadb-install-db --datadir=/data/mariadb"
once = true

[services.db]
command = "mariadbd --no-defaults --datadir=/data/mariadb"
depends_on = ["db-init"]
ready = "mariadb-admin -u root ping"

[services.web]
command = "php -S 127.0.0.1:8000 -t public"
depends_on = ["db"]
"#;

    fn chest(def: Definition) -> Chest {
        Chest {
            name: "demo".into(),
            project: "/home/u/projects/demo".into(),
            def_dir: None,
            detected: None,
            def,
            state: "/home/u/.local/share/luggage/chests/demo".into(),
            run: "/run/user/1000/luggage/demo".into(),
        }
    }

    fn tools() -> Tools {
        Tools {
            bash: "/nix/store/b-bash".into(),
            coreutils: "/nix/store/c-coreutils".into(),
            process_compose: "/nix/store/p-pc".into(),
            util_linux: "/nix/store/u-util-linux".into(),
            iproute2: "/nix/store/i-iproute2".into(),
            cacert: "/nix/store/c-nss-cacert".into(),
            socat: "/nix/store/s-socat".into(),
            bwrap: "/usr/bin/bwrap".into(),
            packages: vec!["/nix/store/m-mariadb".into()],
        }
    }

    #[test]
    fn each_branch_keeps_its_own_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = chest(Definition::default());
        c.state = dir.path().join("state");
        let data = c.state.join("data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("db"), "main").unwrap();
        // a new branch starts with a copy of the data in use
        c.swap_data("main", "feature/x").unwrap();
        assert_eq!(c.active_branch().as_deref(), Some("feature/x"));
        assert_eq!(fs::read_to_string(data.join("db")).unwrap(), "main");
        fs::write(data.join("db"), "feature").unwrap();
        // back on main, its own data returns unchanged
        c.swap_data("feature/x", "main").unwrap();
        assert_eq!(fs::read_to_string(data.join("db")).unwrap(), "main");
        let parked = c.state.join("branches/feature%2Fx/data/db");
        assert_eq!(fs::read_to_string(&parked).unwrap(), "feature");
        assert!(!c.state.join("branches/main").exists());
        let names: Vec<_> = c.parked_branches().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["feature/x"]);
        // a switch cut short after the data was put aside continues
        fs::create_dir_all(c.state.join("branches/main")).unwrap();
        fs::rename(&data, c.state.join("branches/main/data")).unwrap();
        c.swap_data("main", "feature/x").unwrap();
        assert_eq!(fs::read_to_string(data.join("db")).unwrap(), "feature");
        // data in use and data put aside for the same branch: luggage does not pick one
        fs::create_dir_all(c.state.join("branches/feature%2Fx/data")).unwrap();
        assert!(c.swap_data("feature/x", "main").is_err());
    }

    #[test]
    fn fingerprint_follows_the_definition() {
        // the guest flake needs the user; tests cannot set it
        if std::env::var("USER").is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let vm_dir = tmp.path().join("state/vm");
        fs::create_dir_all(&vm_dir).unwrap();
        fs::write(vm_dir.join("id_ed25519"), "").unwrap();
        fs::write(vm_dir.join("id_ed25519.pub"), "ssh-ed25519 AAAA luggage\n").unwrap();
        let at = |def: Definition| Chest {
            state: tmp.path().join("state"),
            run: tmp.path().to_path_buf(),
            ..chest(def)
        };
        let print = |text: &str| at(parse(text).unwrap()).vm_fingerprint("nixpkgs").unwrap();
        let base = print(SPARENCON);
        assert_eq!(base, print(SPARENCON));
        let ported = SPARENCON.replace("-t public\"", "-t public\"\nports = [\"8000\"]");
        for changed in [
            SPARENCON.replace("[\"database\"]", "[\"db\"]"),
            SPARENCON.replace("[\"mariadb\"]", "[\"mariadb\", \"redis\"]"),
            SPARENCON.replace("8000", "8001"),
            ported.clone(),
        ] {
            assert_ne!(base, print(&changed), "{changed}");
        }
        // a fixed port behaves differently when it is taken
        assert_ne!(print(&ported), print(&format!("{ported}\n[ports]\nweb = [\"8000\"]")));
        // env.sh only counts for the services
        fs::write(tmp.path().join("env.sh"), "export A=1").unwrap();
        assert_ne!(base, print(SPARENCON));
        let no_services = print("");
        fs::write(tmp.path().join("env.sh"), "export A=2").unwrap();
        assert_eq!(no_services, print(""));
        // restricting the network needs a restart, a changed list does not
        let closed = print("mode = \"vm\"\nnetwork = []");
        assert_ne!(print("mode = \"vm\""), closed);
        assert_eq!(closed, print("mode = \"vm\"\nnetwork = [\"github.com\"]"));
    }

    #[test]
    fn parses_definition() {
        let def = parse(SPARENCON).unwrap();
        assert_eq!(def.hosts, ["database"]);
        assert_eq!(def.services.len(), 3);
        assert!(def.services["db-init"].once);
        assert_eq!(def.mode, Mode::Bubblewrap);
        assert_eq!(parse("mode = \"vm\"").unwrap().mode, Mode::Vm);
        assert!(parse("mode = \"docker\"").is_err());
        assert_eq!(parse("mode = \"vm\"").unwrap().network, None);
        let def = parse("mode = \"vm\"\nnetwork = [\"packagist.org\", \"github.com\"]").unwrap();
        assert_eq!(def.network.unwrap(), ["packagist.org", "github.com"]);
        assert!(parse("mode = \"vm\"\nnetwork = [\"*.github.com\"]").is_err());
        let def =
            parse("mode = \"vm\"\nnetwork = [\"git.example.com:22\", \"[::1]:8080\"]").unwrap();
        assert_eq!(def.network.unwrap(), ["git.example.com:22", "[::1]:8080"]);
        assert!(parse("mode = \"vm\"\nnetwork = [\"github.com:ssh\"]").is_err());
        assert_eq!(parse("network = [\"github.com\"]").unwrap().network.unwrap(), ["github.com"]);
    }

    #[test]
    fn rejects_bad_definitions() {
        assert!(parse("net = true").is_err());
        let merged = |t: &str| merge(parse(t).unwrap(), None);
        assert!(merged("[services.a]\ncommand = \"x\"\ndepends_on = [\"b\"]").is_err());
        assert!(merged("[services.a]\ncommand = \"x\"\ndepends_on = [\"a\"]").is_err());
        assert!(merged("ignore = [\"db\"]").is_err());
        assert!(parse("[services.a]\ncommand = \"x\"\nonce = true\nready = \"y\"").is_err());
        assert!(parse("[services.a]\nready = \"y\"").is_err());
        assert!(parse("[services.a]\ncommand = \"x\"\nports = [\"80:0\"]").is_err());
        assert!(parse("[services.a]\ncommand = \"x\"\nports = [8000]").is_err());
        assert!(parse("[ports]\na = [\"80:0\"]").is_err());
        assert_eq!(parse("skills = [\"team/*\"]").unwrap().skills, ["team/*"]);
        assert!(parse("skills = [\"glab\"]").is_err());
        let vm = parse("mode = \"vm\"\npass_env = [\"ANTHROPIC_API_KEY\"]").unwrap();
        assert_eq!(vm.pass_env, ["ANTHROPIC_API_KEY"]);
        assert!(parse("mode = \"vm\"\npass_env = [\"A=B\"]").is_err());
        assert_eq!(parse("pass_env = [\"GITHUB_TOKEN\"]").unwrap().pass_env, ["GITHUB_TOKEN"]);
        assert!(merged("[ports]\na = [\"8000\"]").is_err());
    }

    #[test]
    fn port_table_replaces_the_ports_of_a_service() {
        let detected = Detected {
            found: BTreeMap::from([("db".into(), "mariadb 10.11".into())]),
            services: BTreeMap::from([(
                "db".into(),
                Service {
                    command: "mariadbd".into(),
                    ports: vec!["3306:3306".into()],
                    ..Service::default()
                },
            )]),
            ..Detected::default()
        };
        let m = merge(parse("[ports]\ndb = [\"3307:3306\"]").unwrap(), Some(&detected)).unwrap();
        assert_eq!(m.services["db"].ports, ["3307:3306"]);
        assert_eq!(m.services["db"].command, "mariadbd");
        let none = merge(parse("[ports]\ndb = []").unwrap(), Some(&detected)).unwrap();
        assert_eq!(none.services["db"].ports, Vec::<String>::new());
        assert!(
            merge(parse("ignore = [\"db\"]\n[ports]\ndb = []").unwrap(), Some(&detected)).is_err()
        );
    }

    #[test]
    fn next_free_port_is_bounded_and_skips_reserved() {
        let reserved = BTreeSet::from([3307]);
        assert_eq!(next_free_port(3306, &reserved, |_| true), Some(3308));
        assert_eq!(next_free_port(3306, &reserved, |p| p > 3310), Some(3311));
        assert_eq!(next_free_port(3306, &reserved, |p| p == 3306 + PORT_TRIES), Some(3406));
        assert_eq!(next_free_port(3306, &reserved, |p| p > 3306 + PORT_TRIES), None);
        assert_eq!(next_free_port(65534, &BTreeSet::new(), |_| true), Some(65535));
        assert_eq!(next_free_port(65535, &BTreeSet::new(), |_| true), None);
    }

    #[test]
    fn port_pairs() {
        assert_eq!(port_pair("8026:8025").unwrap(), (8026, 8025));
        assert_eq!(port_pair("8000").unwrap(), (8000, 8000));
        for bad in ["", "x", "70000", "1:2:3", "8000-8001"] {
            assert!(port_pair(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn merge_prefers_definition_and_honours_ignore() {
        let detected = Detected {
            found: BTreeMap::from([
                ("db".into(), "mariadb 10.11".into()),
                ("mail".into(), "mailpit".into()),
            ]),
            services: BTreeMap::from([
                (
                    "db".into(),
                    Service {
                        command: "mariadbd".into(),
                        depends_on: vec!["db-init".into()],
                        ..Service::default()
                    },
                ),
                (
                    "db-init".into(),
                    Service { command: "init".into(), once: true, ..Service::default() },
                ),
                ("mail".into(), Service { command: "mailpit".into(), ..Service::default() }),
            ]),
            packages: vec!["mariadb_1011".into(), "mailpit".into()],
            hosts: vec!["db".into(), "mail".into()],
            ..Detected::default()
        };
        let def = parse("ignore = [\"mail\"]\nhosts = [\"extra\"]\n[services.db]\ncommand = \"custom\"\n[services.web]\ncommand = \"php -S 0:80\"\ndepends_on = [\"db\"]").unwrap();
        let m = merge(def, Some(&detected)).unwrap();
        assert_eq!(m.services.keys().collect::<Vec<_>>(), ["db", "db-init", "web"]);
        assert_eq!(m.services["db"].command, "custom");
        assert_eq!(m.hosts, ["db", "extra"]);
    }

    #[test]
    fn compose_uses_matching_conditions() {
        let y = chest(parse(SPARENCON).unwrap()).render_compose();
        // process-compose 1.110 only honours the key at the top level and would expand `${VAR}` itself.
        assert!(y.starts_with("version: \"0.5\"\ndisable_env_expansion: true\nprocesses:\n"));
        assert!(y.contains("  \"db\":\n    command: \"mariadbd --no-defaults --datadir=/data/mariadb\"\n    working_dir: \"/home/u/projects/demo\"\n    depends_on:\n"));
        assert!(
            y.contains("      \"db-init\":\n        condition: process_completed_successfully\n")
        );
        assert!(y.contains("      \"db\":\n        condition: process_healthy\n"));
        assert!(y.contains("timeout_seconds: 60\n      failure_threshold: 60\n"));
    }

    #[test]
    fn bridges_enter_the_chest_instead_of_using_its_files() {
        let address = bridge_address(&tools(), 42, 3306);
        assert_eq!(
            address,
            "EXEC:/nix/store/u-util-linux/bin/nsenter -t 42 --user --net --preserve-credentials \
             /nix/store/s-socat/bin/socat STDIO TCP\\:127.0.0.1\\:3306"
        );
        assert!(!address.contains(RUN_INSIDE) && !address.contains(".sock"));
    }

    #[test]
    fn yaml_strings_are_escaped() {
        assert_eq!(yaml_str(r#"sh -c "a\b""#), r#""sh -c \"a\\b\"""#);
    }

    #[test]
    fn rc_puts_packages_first_on_path() {
        let rc = chest(Definition::default()).render_rc(&tools());
        assert!(rc.contains(
            "export PATH=/nix/store/m-mariadb/bin:/nix/store/p-pc/bin:/nix/store/b-bash/bin:/nix/store/c-coreutils/bin:\"$PATH\"\n"
        ));
        assert!(rc.contains("cd '/home/u/projects/demo'\n"));
    }

    #[test]
    fn hosts_point_to_loopback() {
        let h = chest(parse(SPARENCON).unwrap()).render_hosts();
        assert_eq!(h, "127.0.0.1 localhost chest-demo database\n::1 localhost\n");
    }

    #[test]
    fn sh_quote_handles_single_quotes() {
        assert_eq!(sh_quote("it's"), r#"'it'"'"'s'"#);
    }

    #[test]
    fn host_values_reach_the_chest_only_in_the_environment() {
        let t = chest(parse("pass_env = [\"GITHUB_TOKEN\"]").unwrap());
        let host =
            [("GITHUB_TOKEN".into(), "s3cret 'q' $(id)".into()), ("NPM_TOKEN".into(), "n".into())];
        let script = t.host_env(host);
        assert_eq!(script, "export GITHUB_TOKEN='s3cret '\"'\"'q'\"'\"' $(id)'\n");
        let c = t.run_command(&tools(), false, &["env".to_owned()], &script).unwrap();
        assert!(!c.get_args().any(|a| a.to_string_lossy().contains("s3cret")));
        let envs: Vec<_> = c.get_envs().filter_map(|(k, v)| Some((k.to_str()?, v?))).collect();
        assert!(envs.contains(&(vm::PASS_ENV, script.as_os_str())));
        assert!(envs.iter().all(|(k, _)| ["HOME", "TERM", vm::PASS_ENV].contains(k)));
        // the rc file reads them after the dev environment, so they win over it as in the VM
        assert!(
            t.render_rc(&tools())
                .ends_with("eval \"${LUGGAGE_PASS_ENV-}\"; unset LUGGAGE_PASS_ENV\n")
        );
    }

    #[test]
    fn bubblewrap_network_goes_through_the_proxy() {
        let open = chest(Definition::default());
        let t = chest(parse("hosts = [\"database\"]\nnetwork = [\"github.com\"]").unwrap());
        assert!(!open.render_rc(&tools()).contains("proxy"));
        let rc = t.render_rc(&tools());
        assert!(
            rc.contains(" https_proxy='http://127.0.0.1:3128' HTTPS_PROXY='http://127.0.0.1:3128'")
        );
        assert!(rc.contains(" no_proxy='localhost,127.0.0.1,::1,database' NO_PROXY="));
        // the chest must not change its own allowlist, so the list lives outside its runtime dir
        assert!(!t.network_file().starts_with(&t.run));
        assert_eq!(open.inside_proxy(&tools(), "proxy.sock"), "");
        assert_eq!(
            t.inside_proxy(&tools(), "proxy.sock"),
            "/nix/store/s-socat/bin/socat TCP-LISTEN:3128,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:/run/luggage/proxy.sock 2>>/run/luggage/proxy.log & "
        );
        let args = |c: &Chest, net| -> Vec<String> {
            let c = c.run_command(&tools(), net, &["true".to_owned()], OsStr::new("")).unwrap();
            std::iter::once(c.get_program())
                .chain(c.get_args())
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        let proxied = args(&t, true);
        assert_eq!(proxied[0], "/nix/store/u-util-linux/bin/unshare");
        assert!(proxied.contains(&"--unshare-user".to_owned()));
        assert!(proxied.iter().any(|a| a.contains("UNIX-CONNECT:/run/luggage/proxy-")));
        assert!(!proxied.contains(&"--share-net".to_owned()));
        assert!(args(&t, false).contains(&"--unshare-all".to_owned()));
        assert!(args(&open, true).contains(&"--share-net".to_owned()));
    }

    #[test]
    fn chest_writes_only_its_own_part_of_the_runtime_dir() {
        let t = chest(parse("network = []").unwrap());
        let a = t.bwrap_args(&tools(), Net::Own).unwrap();
        let writable: Vec<_> =
            a.windows(2).filter(|w| w[0] == "--bind").map(|w| PathBuf::from(&w[1])).collect();
        assert!(writable.contains(&t.inside()));
        assert!(
            writable
                .iter()
                .all(|w| !t.run.starts_with(w) && (w == &t.inside() || !w.starts_with(&t.run)))
        );
        // luggage reads, runs or kills what these files name, so the chest must not write them
        for f in [
            t.pid_file(),
            t.run.join("tools.json"),
            t.run.join("info.json"),
            t.run.join("bridges.pid"),
        ] {
            assert!(!f.starts_with(t.inside()), "{}", f.display());
        }
        for f in ["rc", "env.sh", "process-compose.yaml"] {
            let bind = t.bind_inside(f);
            assert!(a.windows(3).any(|w| w == bind), "{f}");
        }
    }

    #[test]
    fn files_the_host_trusts_are_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        fs::create_dir(p.join(".git")).unwrap();
        for f in ["flake.nix", ".envrc", ".git/config"] {
            fs::write(p.join(f), "").unwrap();
        }
        create_protected_dirs(p).unwrap();
        for d in [".luggage", ".direnv", ".git/hooks", ".git/modules", ".git/worktrees"] {
            assert!(p.join(d).is_dir(), "{d}");
        }
        let t = Chest { project: p.to_path_buf(), ..chest(Definition::default()) };
        let a = t.bwrap_args(&tools(), Net::Off).unwrap();
        let at = |option: &str, path: &Path| {
            let path = path.to_string_lossy().into_owned();
            a.windows(3).position(|w| w == [option, path.as_str(), path.as_str()])
        };
        let bind = |option: &str, name: &str| at(option, &p.join(name));
        let project = at("--bind", p).unwrap();
        // .git is a mount point before its parts are protected, so it cannot be renamed away
        let git = bind("--bind", ".git").unwrap();
        assert!(project < git && git < bind("--ro-bind", ".git/config").unwrap());
        for f in [".luggage", ".direnv", ".envrc", "flake.nix", ".git/hooks", ".git/modules"] {
            assert!(bind("--ro-bind", f).is_some_and(|i| i > project), "{f}");
        }
        // missing files are left out: bwrap would fail on them
        assert!(bind("--ro-bind", "flake.lock").is_none());

        // a worktree or submodule: .git is a file that names the git dir
        let wt = tempfile::tempdir().unwrap();
        fs::write(wt.path().join(".git"), "gitdir: /elsewhere").unwrap();
        create_protected_dirs(wt.path()).unwrap();
        let git = wt.path().join(".git").to_string_lossy().into_owned();
        assert_eq!(protect_args(wt.path())[..3], ["--ro-bind", git.as_str(), git.as_str()]);
    }

    #[test]
    fn a_git_file_that_leads_to_another_config_stops_the_chest_until_down() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("demo");
        fs::create_dir_all(p.join(".git/objects")).unwrap();
        let t = Chest {
            project: p.clone(),
            state: tmp.path().join("state"),
            run: tmp.path().join("run"),
            ..chest(Definition::default())
        };
        // no start yet: nothing to compare with
        fs::write(p.join(".git/commondir"), "../evil").unwrap();
        t.check_git_files().unwrap();
        fs::remove_file(p.join(".git/commondir")).unwrap();
        t.note_git_files().unwrap();
        t.check_git_files().unwrap();
        fs::write(p.join(".git/commondir"), "../evil").unwrap();
        let e = t.check_git_files().unwrap_err().to_string();
        assert!(e.contains(".git/commondir") && e.contains("stopped"), "{e}");
        // the next start refuses as well, until `down` accepts the files
        assert!(t.note_git_files().is_err());
        t.cmd_down().unwrap();
        t.note_git_files().unwrap();
        t.check_git_files().unwrap();
        // a nested repo only warns: tools in the chest create them on purpose
        fs::create_dir_all(p.join("vendor/x/.git")).unwrap();
        t.check_git_files().unwrap();
        t.note_git_files().unwrap();
    }

    #[test]
    fn bwrap_mount_order() {
        let t = chest(Definition::default());
        let a = t.bwrap_args(&tools(), Net::Off).unwrap();
        let pos = |s: &str| a.iter().position(|x| x == s).unwrap();
        // tmpfs /tmp before all binds, home before the project (the project is under $HOME)
        assert!(pos("/tmp") < pos("--bind"));
        assert!(
            pos("/home/u/.local/share/luggage/chests/demo/home") < pos("/home/u/projects/demo")
        );
        assert_eq!(a[0], "--unshare-all");
        assert!(!a.contains(&"--share-net".to_owned()));
        assert!(t.bwrap_args(&tools(), Net::Share).unwrap().contains(&"--share-net".to_owned()));
        let own = t.bwrap_args(&tools(), Net::Own).unwrap();
        assert!(!own.contains(&"--unshare-all".to_owned()));
        assert_eq!(own[0], "--unshare-user");
    }
}
