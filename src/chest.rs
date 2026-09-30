//! The chest: the project in an isolated environment (bubblewrap) with its own services (process-compose).
//!
//! State (home, data) lives in `$XDG_DATA_HOME/luggage/chests/<name>`, runtime files (sockets,
//! generated files) in `$XDG_RUNTIME_DIR/luggage/<name>` — Unix socket paths must not exceed 108 characters.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::compose::{self, Detected};
use crate::{config, find_in_path, home, info, versions, vm};

/// Default wait time in seconds for `ready` when `ready_timeout` is missing.
const DEFAULT_READY_TIMEOUT: u32 = 60;
/// The path of the runtime dir inside the chest.
const RUN_INSIDE: &str = "/run/luggage";
/// process-compose in the VM guest, from its system packages.
const PC_GUEST: &str = "/run/current-system/sw/bin/process-compose";
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
}

/// Reads `chest.toml`. `once` and `ready` cannot be used together.
fn parse(text: &str) -> Result<Definition> {
    let def: Definition = toml::from_str(text)?;
    for (name, svc) in &def.services {
        if svc.once && svc.ready.is_some() {
            bail!("service {name}: once and ready cannot be used together");
        }
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
fn xdg(var: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
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

    /// Socket of process-compose
    fn socket(&self) -> PathBuf {
        self.run.join("pc.sock")
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
        format!(
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
        )
    }

    /// Generates `process-compose.yaml` from the services.
    fn render_compose(&self) -> String {
        let wd = yaml_str(&self.project.to_string_lossy());
        let mut y = String::from("version: \"0.5\"\nprocesses:\n");
        for (name, svc) in &self.def.services {
            let _ = write!(
                y,
                "  {}:\n    command: {}\n    working_dir: {wd}\n    disable_env_expansion: true\n",
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

    /// Arguments for bwrap. Order matters: later mounts hide earlier ones.
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
                "--bind",
                &s(&self.state.join("data")),
                "/data",
                "--bind",
                &s(&self.run),
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
                "--clearenv",
                "--setenv",
                "HOME",
                &home,
                "--setenv",
                "TERM",
                &std::env::var("TERM").unwrap_or_else(|_| "xterm".into()),
            ]
            .map(str::to_owned),
        );
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
    /// With `profile`, nix also keeps the environment there so garbage collection does not remove it.
    fn dev_env(&self, profile: Option<&Path>) -> Result<PathBuf> {
        let env = self.run.join("env.sh");
        let flake_changed =
            ["flake.nix", "flake.lock"].iter().filter_map(|f| mtime(&self.project.join(f))).max();
        let stale = mtime(&env).is_none_or(|t| flake_changed.is_some_and(|f| f > t));
        if stale || profile.is_some_and(|p| !p.exists()) {
            info("building the environment (nix print-dev-env)");
            let mut c = Command::new("nix");
            c.arg("print-dev-env");
            if let Some(p) = profile {
                c.arg("--profile").arg(p);
            }
            let out = c
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
        for d in [self.state.join("home"), self.state.join("data"), self.run.clone()] {
            fs::create_dir_all(&d).with_context(|| format!("cannot create {}", d.display()))?;
        }
        fs::set_permissions(&self.run, fs::Permissions::from_mode(0o700))?;
        let tools = Tools::fetch(nixpkgs, &self.def.packages)?;
        self.dev_env(None)?;
        fs::write(self.run.join("tools.json"), serde_json::to_string_pretty(&tools)?)?;
        fs::write(self.run.join("rc"), self.render_rc(&tools))?;
        fs::write(self.run.join("hosts"), self.render_hosts())?;
        fs::write(self.run.join("process-compose.yaml"), self.render_compose())?;
        Ok(tools)
    }

    /// Runs a one-off command in a fresh chest; without a command, a shell.
    pub fn cmd_run(&self, net: bool, cmd: &[String], nixpkgs: &str) -> Result<()> {
        self.not_in_vm_mode()?;
        let tools = self.prepare(nixpkgs)?;
        let mut c = Command::new(&tools.bwrap);
        c.args(self.bwrap_args(&tools, if net { Net::Share } else { Net::Off })?)
            .arg("--die-with-parent")
            .arg(tools.bash())
            .arg("--noprofile");
        shell_args(&mut c, cmd);
        pass_through(c.status().context("cannot start bwrap")?);
        Ok(())
    }

    /// Brings the dev environment in the guest up to date with the project flake.
    fn vm_sync(&self) -> Result<()> {
        let env = self.dev_env(Some(&self.state.join("vm/dev-env")))?;
        let compose = self.run.join("process-compose.yaml");
        let yaml = self.render_compose();
        // only write on change: the copy into the guest follows the modification time
        if fs::read_to_string(&compose).ok().as_deref() != Some(yaml.as_str()) {
            fs::write(&compose, yaml)?;
        }
        vm::sync_env(&self.name, &self.state.join("vm"), &self.run, &env, &compose)
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
        let pid = vm::start(&self.name, &self.project, &dir, &self.run, &self.pid_file())?;
        self.vm_sync()?;
        self.vm_services(pid)?;
        Ok(pid)
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
    pub fn cmd_hook(&self) -> Result<()> {
        if self.def.mode != Mode::Vm {
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
        if self.running().is_none() {
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
        }
        Ok(())
    }

    /// Commands that do not work with a VM yet.
    fn not_in_vm_mode(&self) -> Result<()> {
        if self.def.mode == Mode::Vm {
            bail!("this command does not work with mode = \"vm\" yet");
        }
        Ok(())
    }

    /// Prints the notes from service detection.
    fn print_notes(&self) {
        for n in self.detected.iter().flat_map(|d| &d.notes) {
            info(&format!("note: {n}"));
        }
    }

    /// Starts the chest in the background and waits until all services are ready.
    pub fn cmd_up(&self, nixpkgs: &str) -> Result<()> {
        if self.def.mode == Mode::Vm {
            let pid = self.vm_up(nixpkgs)?;
            info(&format!(
                "chest {} is running (VM, pid {pid}) — `luggage status`, `luggage down`",
                self.name
            ));
            return Ok(());
        }
        if let Some(pid) = self.running() {
            info(&format!("chest {} is already running (pid {pid})", self.name));
            return Ok(());
        }
        self.print_notes();
        let tools = self.prepare(nixpkgs)?;
        let info_json = self.run.join("info.json");
        let _ = fs::remove_file(&info_json);
        let _ = fs::remove_file(self.socket());
        let main = if self.def.services.is_empty() {
            "exec sleep infinity".to_owned()
        } else {
            format!(
                "exec process-compose up -t=false -U -u {RUN_INSIDE}/pc.sock -L {RUN_INSIDE}/pc.log -f {RUN_INSIDE}/process-compose.yaml"
            )
        };
        let log = File::create(self.run.join("chest.log"))?;
        // Own network namespace via unshare: we are root in it and allow ports < 1024, only for the chest.
        // bwrap reports the PID of its child (PID 1 of the chest) on fd 3.
        let ip = format!("{}/bin/ip", tools.iproute2);
        Command::new(tools.bash())
            .args(["-c", r#"exec 3>"$1"; shift; exec "$@""#, "sh"])
            .arg(&info_json)
            .arg(tools.util("setsid"))
            .args([&tools.util("unshare"), "--user", "--map-root-user", "--net"])
            .args([&tools.bash(), "-c"])
            .arg(format!(
                r#"{ip} link set lo up && echo 0 > /proc/sys/net/ipv4/ip_unprivileged_port_start && exec "$@""#
            ))
            .args(["sh", &tools.bwrap])
            .args(self.bwrap_args(&tools, Net::Own)?)
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
            bail!("chest stopped right away — see {}/chest.log and pc.log", self.run.display());
        }
        // Return only once the services are ready. Also, a `down` while a readiness_probe
        // is still pending hangs in process-compose.
        if !self.def.services.is_empty() {
            let limit = self.ready_limit();
            let mut ready = false;
            for _ in 0..limit * 4 {
                if self.services_ready(&tools)? {
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

    /// Are all services ready? Fails if a service stopped that should keep running.
    fn services_ready(&self, tools: &Tools) -> Result<bool> {
        let out =
            self.pc(tools, &["process", "list", "-o", "json"]).stderr(Stdio::null()).output()?;
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
        bail!("chest does not start — see {}", self.run.join("chest.log").display())
    }

    /// Runs `cmd` in the running chest; without a command, a shell.
    fn enter(&self, cmd: &[String], nixpkgs: &str) -> Result<()> {
        if self.def.mode == Mode::Vm {
            // starts the VM or waits for a start that is under way
            self.vm_up(nixpkgs)?;
            self.vm_sync()?;
            // only the project is shared with the guest
            let cwd = std::env::current_dir()?;
            let cwd = if cwd.starts_with(&self.project) { cwd } else { self.project.clone() };
            pass_through(vm::exec(&self.name, &self.state.join("vm"), &self.run, &cwd, cmd)?);
            return Ok(());
        }
        let pid = self.running().with_context(|| {
            format!("chest {} is not running — run `luggage up` first", self.name)
        })?;
        let tools = self.tools()?;
        let mut c = nsenter(pid, &tools);
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

    /// The process-compose client on the host. The socket is in the runtime dir.
    fn pc(&self, tools: &Tools, args: &[&str]) -> Command {
        let mut c = Command::new(format!("{}/bin/process-compose", tools.process_compose));
        c.args(args)
            .arg("-U")
            .arg("-u")
            .arg(self.socket())
            .arg("-L")
            .arg(self.run.join("pc-client.log"));
        c
    }

    /// Stops the chest: first the services in order, SIGKILL as a last resort.
    pub fn cmd_down(&self) -> Result<()> {
        // control.socket also finds a VM whose chest.toml is broken right now
        let is_vm = self.def.mode == Mode::Vm || self.run.join("control.socket").exists();
        let Some(pid) = self.running() else {
            if is_vm {
                vm::stop(&self.name, &self.run, None);
            }
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
            let client =
                self.pc(&tools, &["down"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
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
        }
        let _ = fs::remove_file(self.pid_file());
        let _ = fs::remove_file(self.socket());
        info(&format!("chest {} stopped", self.name));
        Ok(())
    }

    /// Shows the definition, services and state of the chest.
    pub fn cmd_status(&self) -> Result<()> {
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
        let Some(pid) = self.running() else {
            println!("Status:      not running");
            return Ok(());
        };
        println!("Status:      running (pid {pid})");
        if self.def.mode == Mode::Vm && !self.def.services.is_empty() {
            println!();
            // stderr is hidden: process-compose prints debug lines there about its missing config
            self.vm_pc("process list -o wide").stderr(Stdio::null()).status()?;
        } else if self.socket().exists() {
            println!();
            let tools = self.tools()?;
            // stderr is hidden: process-compose prints debug lines there about its missing config
            self.pc(&tools, &["process", "list", "-o", "wide"]).stderr(Stdio::null()).status()?;
        }
        Ok(())
    }
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
    .current_dir("/")
    .env_clear()
    .env("HOME", home())
    .env("TERM", std::env::var("TERM").unwrap_or_else(|_| "xterm".into()));
    c
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
            bwrap: "/usr/bin/bwrap".into(),
            packages: vec!["/nix/store/m-mariadb".into()],
        }
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
        assert!(y.contains("  \"db\":\n    command: \"mariadbd --no-defaults --datadir=/data/mariadb\"\n    working_dir: \"/home/u/projects/demo\"\n    disable_env_expansion: true\n"));
        assert!(
            y.contains("      \"db-init\":\n        condition: process_completed_successfully\n")
        );
        assert!(y.contains("      \"db\":\n        condition: process_healthy\n"));
        assert!(y.contains("timeout_seconds: 60\n      failure_threshold: 60\n"));
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
