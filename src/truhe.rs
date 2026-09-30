//! Die Truhe: das Projekt in einer abgeschotteten Umgebung (bubblewrap) mit eigenen Diensten (process-compose).
//!
//! Zustand (Home, Daten) liegt unter `$XDG_DATA_HOME/luggage/truhen/<name>`, Laufzeit (Sockets,
//! erzeugte Dateien) unter `$XDG_RUNTIME_DIR/luggage/<name>` — Unix-Socket-Pfade dürfen 108 Zeichen nicht überschreiten.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::compose::{self, Detected};
use crate::{config, home, info, versions};

/// Standard-Wartezeit in Sekunden für `ready`, wenn `ready_timeout` fehlt.
const DEFAULT_READY_TIMEOUT: u32 = 60;
/// So heißt das Laufzeitverzeichnis in der Truhe.
const RUN_INSIDE: &str = "/run/luggage";

/// Inhalt von `truhe.toml`; ergänzt, was aus der Compose-Datei erkannt wurde.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Definition {
    /// Name der Truhe; sonst der Name des Projektverzeichnisses
    pub name: Option<String>,
    /// Namen, die in der Truhe auf 127.0.0.1 zeigen
    pub hosts: Vec<String>,
    /// Zusätzliche nixpkgs-Pakete für die Dienste, z.B. "mariadb"
    pub packages: Vec<String>,
    /// Eigene Dienste; überschreiben gleichnamige erkannte Dienste
    pub services: BTreeMap<String, Service>,
    /// Erkannte Compose-Dienste, die nicht übernommen werden sollen
    pub ignore: Vec<String>,
}

/// Ein Dienst, den process-compose in der Truhe startet.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Startbefehl des Dienstes
    pub command: String,
    /// Dienste, die vorher laufen müssen
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Befehl, der Erfolg meldet, sobald der Dienst bereit ist
    pub ready: Option<String>,
    /// Sekunden, die `ready` Zeit hat (Standard 60)
    pub ready_timeout: Option<u32>,
    /// Läuft einmal durch (z.B. Datenbank anlegen); Abhängige warten auf erfolgreiches Ende.
    #[serde(default)]
    pub once: bool,
}

/// Liest `truhe.toml`; `once` und `ready` schließen sich aus.
fn parse(text: &str) -> Result<Definition> {
    let def: Definition = toml::from_str(text)?;
    for (name, svc) in &def.services {
        if svc.once && svc.ready.is_some() {
            bail!("Dienst {name}: once und ready schließen sich aus");
        }
    }
    Ok(def)
}

/// Legt Erkanntes und Definition zusammen; gleichnamige Dienste aus `truhe.toml` gewinnen.
fn merge(mut def: Definition, detected: Option<&Detected>) -> Result<Definition> {
    for i in &def.ignore {
        if !detected.is_some_and(|d| d.found.contains_key(i)) {
            bail!("ignore: \"{i}\" wurde in keiner Compose-Datei erkannt");
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
                bail!("Dienst {name}: depends_on \"{dep}\" gibt es nicht");
            }
        }
    }
    Ok(def)
}

/// Werkzeuge aus nixpkgs, die die Truhe unabhängig vom Projekt-Flake braucht.
#[derive(Debug, Serialize, Deserialize)]
struct Tools {
    /// Store-Pfad von bashInteractive
    bash: String,
    /// Store-Pfad von coreutils
    coreutils: String,
    /// Store-Pfad von process-compose
    process_compose: String,
    /// Store-Pfade der `packages` aus der Definition
    packages: Vec<String>,
}

/// Baut die Installables mit `nix build` und gibt ihre Store-Pfade zurück.
fn nix_build(installables: &[String]) -> Result<Vec<String>> {
    let out = Command::new("nix")
        .args(["build", "--no-link", "--print-out-paths"])
        .args(installables)
        .stderr(Stdio::inherit())
        .output()
        .context("nix nicht startbar")?;
    if !out.status.success() {
        bail!("nix build fehlgeschlagen: {}", installables.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(str::to_owned).collect())
}

impl Tools {
    /// Baut bash, coreutils, process-compose und die Zusatzpakete aus `nixpkgs`.
    fn fetch(nixpkgs: &str, packages: &[String]) -> Result<Self> {
        const ATTRS: [&str; 3] = ["bashInteractive", "coreutils", "process-compose"];
        let paths = nix_build(&ATTRS.map(|a| format!("{nixpkgs}#{a}^out")))?;
        let find = |name: &str| {
            paths
                .iter()
                .find(|p| p.contains(&format!("-{name}-")))
                .cloned()
                .with_context(|| format!("{name} nicht in der Ausgabe von nix build"))
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
            packages,
        })
    }

    /// Pfad zur bash-Binary
    fn bash(&self) -> String {
        format!("{}/bin/bash", self.bash)
    }
}

/// Netzwerk-Modus der Truhe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Net {
    /// kein Netz
    Off,
    /// Netz des Hosts (Internet)
    Share,
    /// Netz-NS kommt von außen (unshare), bwrap legt nur noch die übrigen an
    Own,
}

/// Eine Truhe für ein Projekt: Definition, erkannte Dienste und ihre Verzeichnisse.
pub struct Truhe {
    /// Name der Truhe (nur `[A-Za-z0-9_-]`)
    name: String,
    /// Projektverzeichnis (enthält flake.nix)
    project: PathBuf,
    /// Verzeichnis mit `truhe.toml`, falls vorhanden
    def_dir: Option<PathBuf>,
    /// Aus Compose-Dateien Erkanntes
    detected: Option<Detected>,
    /// Zusammengeführte Definition
    def: Definition,
    /// Zustandsverzeichnis (Home, Daten)
    state: PathBuf,
    /// Laufzeitverzeichnis (Sockets, erzeugte Dateien)
    run: PathBuf,
}

/// Pfad aus der Umgebungsvariable `var`; leer oder fehlend ergibt `fallback`.
fn xdg(var: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    std::env::var_os(var).filter(|v| !v.is_empty()).map_or_else(fallback, PathBuf::from)
}

/// Ersetzt alles außer ASCII-Buchstaben, Ziffern, `-` und `_` durch `-`.
fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

/// Für bash in einfache Anführungszeichen setzen. Ohne Backslash: process-compose verschluckt ihn.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// Setzt `s` als YAML-String in doppelte Anführungszeichen.
fn yaml_str(s: &str) -> String {
    let escaped = s.replace('\\', r"\\").replace('"', "\\\"").replace('\n', r"\n");
    format!("\"{escaped}\"")
}

/// Lebt der Prozess `pid` noch?
fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Wartet bis zu `tenths` Zehntelsekunden, dass `pid` verschwindet.
fn wait_dead(pid: u32, tenths: u32) -> bool {
    for _ in 0..tenths {
        if !alive(pid) {
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    !alive(pid)
}

/// Eine Zeile aus `process-compose process list -o json`.
#[derive(Deserialize)]
struct ProcState {
    /// Name des Dienstes
    name: String,
    /// Status, z.B. "Completed" oder "Error"
    status: String,
    /// "Ready", sobald die `readiness_probe` erfolgreich war
    is_ready: String,
    /// Exit-Code des Dienstes
    exit_code: i32,
    /// Läuft der Dienst gerade?
    is_running: bool,
}

/// Änderungszeit der Datei, falls lesbar.
fn mtime(p: &Path) -> Option<SystemTime> {
    fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Beendet luggage mit dem Exit-Code des Kindprozesses, wenn der nicht 0 war.
fn pass_through(status: ExitStatus) {
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

impl Truhe {
    /// Sucht vom aktuellen Verzeichnis aufwärts das Projekt (flake.nix), erkennt Dienste und liest die Definition.
    /// Mit `lenient` wird eine fehlerhafte Definition nur gemeldet.
    pub fn find(nixpkgs: &str, lenient: bool) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let project = cwd
            .ancestors()
            .find(|d| d.join("flake.nix").exists())
            .with_context(|| {
                format!("keine flake.nix in {} oder darüber — erst `luggage init`", cwd.display())
            })?
            .to_path_buf();
        let base = safe_name(
            &project.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        );
        let def_dir =
            [project.join(".luggage"), config::path().with_file_name("truhen").join(&base)]
                .into_iter()
                .find(|d| d.join("truhe.toml").exists());
        let load = || -> Result<(Option<Detected>, Definition)> {
            let def = match &def_dir {
                Some(d) => {
                    let file = d.join("truhe.toml");
                    let text = fs::read_to_string(&file)?;
                    parse(&text).with_context(|| format!("{} fehlerhaft", file.display()))?
                }
                None => Definition::default(),
            };
            let detected = compose::detect(&project, &mut || versions::query(nixpkgs))?;
            let def = merge(def, detected.as_ref())?;
            Ok((detected, def))
        };
        let (detected, def) = match load() {
            Ok(loaded) => loaded,
            // down braucht nur das Laufzeitverzeichnis; eine kaputte Definition darf es nicht blockieren
            Err(e) if lenient => {
                info(&format!("Warnung: {e:#}"));
                (None, Definition::default())
            }
            Err(e) => return Err(e),
        };
        let name = def.name.as_deref().map_or(base, safe_name);
        let state =
            xdg("XDG_DATA_HOME", || home().join(".local/share")).join("luggage/truhen").join(&name);
        let run = xdg("XDG_RUNTIME_DIR", std::env::temp_dir).join("luggage").join(&name);
        Ok(Self { name, project, def_dir, detected, def, state, run })
    }

    /// PID-Datei der laufenden Truhe
    fn pid_file(&self) -> PathBuf {
        self.run.join("truhe.pid")
    }

    /// Socket von process-compose
    fn socket(&self) -> PathBuf {
        self.run.join("pc.sock")
    }

    /// PID der laufenden Truhe, falls sie lebt.
    fn running(&self) -> Option<u32> {
        let pid = fs::read_to_string(self.pid_file()).ok()?.trim().parse().ok()?;
        alive(pid).then_some(pid)
    }

    /// Liest die von `prepare` gespeicherten Werkzeuge aus `tools.json`.
    fn tools(&self) -> Result<Tools> {
        let text = fs::read_to_string(self.run.join("tools.json"))
            .context("Truhe nicht vorbereitet — erst `luggage up`")?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Inhalt von `/etc/hosts` für die Truhe.
    fn render_hosts(&self) -> String {
        let mut names = vec![format!("truhe-{}", self.name)];
        names.extend(self.def.hosts.iter().cloned());
        format!("127.0.0.1 localhost {}\n::1 localhost\n", names.join(" "))
    }

    /// rc-Datei für bash: lädt die Dev-Umgebung, setzt PATH und wechselt ins Projekt.
    fn render_rc(&self, tools: &Tools) -> String {
        let user = std::env::var("USER").unwrap_or_default();
        format!(
            "source {RUN_INSIDE}/env.sh\n\
             export HOME={home} USER={user} TMPDIR=/tmp LUGGAGE_TRUHE={name}\n\
             unset NIX_BUILD_TOP TEMP TMP TEMPDIR\n\
             export PATH={pkgs}{pc}/bin:{bash}/bin:{core}/bin:\"$PATH\"\n\
             cd {project}\n\
             PS1='[truhe:{name_plain}] \\w\\$ '\n",
            home = sh_quote(&home().to_string_lossy()),
            user = sh_quote(&user),
            name = sh_quote(&self.name),
            pc = tools.process_compose,
            bash = tools.bash,
            core = tools.coreutils,
            pkgs = tools.packages.iter().fold(String::new(), |acc, p| acc + p + "/bin:"),
            project = sh_quote(&self.project.to_string_lossy()),
            name_plain = self.name,
        )
    }

    /// Erzeugt `process-compose.yaml` aus den Diensten.
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
                // Standard-Timeout von process-compose ist 1 s: erste Anfragen (Cache-Aufbau) dauern länger.
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

    /// Argumente für bwrap. Reihenfolge zählt: spätere Mounts verdecken frühere.
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
                &format!("truhe-{}", self.name),
                "--tmpfs",
                "/tmp",
                "--dev",
                "/dev",
                "--proc",
                "/proc",
                "--ro-bind",
                "/nix/store",
                "/nix/store",
                // erst das Home, dann das Projekt: liegt es unter $HOME, würde das Home es sonst verdecken
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
            a.extend(["--ro-bind".into(), s(d), "/truhe".into()]);
        }
        if net == Net::Share {
            a.extend(
                ["--share-net", "--ro-bind-try", "/etc/resolv.conf", "/etc/resolv.conf"]
                    .map(str::to_owned),
            );
        }
        Ok(a)
    }

    /// Legt Verzeichnisse an und schreibt env.sh, rc, hosts und process-compose.yaml.
    fn prepare(&self, nixpkgs: &str) -> Result<Tools> {
        for d in [self.state.join("home"), self.state.join("data"), self.run.clone()] {
            fs::create_dir_all(&d).with_context(|| format!("{} nicht anlegbar", d.display()))?;
        }
        fs::set_permissions(&self.run, fs::Permissions::from_mode(0o700))?;
        let tools = Tools::fetch(nixpkgs, &self.def.packages)?;
        let env = self.run.join("env.sh");
        let flake_changed =
            ["flake.nix", "flake.lock"].iter().filter_map(|f| mtime(&self.project.join(f))).max();
        if mtime(&env).is_none_or(|t| flake_changed.is_some_and(|f| f > t)) {
            info("baue die Umgebung (nix print-dev-env)");
            let out = Command::new("nix")
                .arg("print-dev-env")
                .current_dir(&self.project)
                .stderr(Stdio::inherit())
                .output()
                .context("nix nicht startbar")?;
            if !out.status.success() {
                bail!("nix print-dev-env fehlgeschlagen");
            }
            fs::write(&env, out.stdout)?;
        }
        fs::write(self.run.join("tools.json"), serde_json::to_string_pretty(&tools)?)?;
        fs::write(self.run.join("rc"), self.render_rc(&tools))?;
        fs::write(self.run.join("hosts"), self.render_hosts())?;
        fs::write(self.run.join("process-compose.yaml"), self.render_compose())?;
        Ok(tools)
    }

    /// Einmaliger Befehl in einer frischen Truhe; ohne Befehl eine Shell.
    pub fn cmd_run(&self, net: bool, cmd: &[String], nixpkgs: &str) -> Result<()> {
        let tools = self.prepare(nixpkgs)?;
        let mut c = Command::new("bwrap");
        c.args(self.bwrap_args(&tools, if net { Net::Share } else { Net::Off })?)
            .arg("--die-with-parent")
            .arg(tools.bash())
            .arg("--noprofile");
        shell_args(&mut c, cmd);
        pass_through(c.status().context("bwrap nicht startbar")?);
        Ok(())
    }

    /// Gibt die Hinweise der Dienst-Erkennung aus.
    fn print_notes(&self) {
        for n in self.detected.iter().flat_map(|d| &d.notes) {
            info(&format!("Hinweis: {n}"));
        }
    }

    /// Startet die Truhe im Hintergrund und wartet, bis alle Dienste bereit sind.
    pub fn cmd_up(&self, nixpkgs: &str) -> Result<()> {
        if let Some(pid) = self.running() {
            info(&format!("Truhe {} läuft schon (pid {pid})", self.name));
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
        let log = File::create(self.run.join("truhe.log"))?;
        // Eigener Netz-NS per unshare: darin sind wir root und geben Ports < 1024 frei — nur für die Truhe.
        // bwrap meldet die PID seines Kindes (PID 1 der Truhe) über fd 3.
        Command::new("sh")
            .args(["-c", r#"exec 3>"$1"; shift; exec setsid "$@""#, "sh"])
            .arg(&info_json)
            .args(["unshare", "--user", "--map-root-user", "--net", "sh", "-c"])
            .arg(r#"/usr/bin/ip link set lo up && echo 0 > /proc/sys/net/ipv4/ip_unprivileged_port_start && exec "$@""#)
            .args(["sh", "bwrap"])
            .args(self.bwrap_args(&tools, Net::Own)?)
            .args(["--info-fd", "3"])
            .arg(tools.bash())
            .args(["--noprofile", "-c", &format!("source {RUN_INSIDE}/rc; {main}")])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("Truhe nicht startbar")?;
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
                "Truhe hat sich sofort beendet — siehe {}/truhe.log und pc.log",
                self.run.display()
            );
        }
        // Erst zurückkehren, wenn die Dienste bereit sind. Nebenbei: ein `down`, während eine
        // readiness_probe noch aussteht, bleibt in process-compose hängen.
        if !self.def.services.is_empty() {
            let limit = self
                .def
                .services
                .values()
                .map(|s| s.ready_timeout.unwrap_or(DEFAULT_READY_TIMEOUT))
                .max()
                .unwrap_or(DEFAULT_READY_TIMEOUT)
                + 5;
            let mut ready = false;
            for _ in 0..limit * 4 {
                if self.services_ready(&tools)? {
                    ready = true;
                    break;
                }
                if !alive(pid) {
                    bail!("Truhe wurde beendet, bevor die Dienste bereit waren");
                }
                sleep(Duration::from_millis(250));
            }
            if !ready {
                bail!(
                    "Dienste nach {limit} s nicht bereit (Truhe läuft weiter) — `luggage status`, Log: {}",
                    self.run.join("truhe.log").display()
                );
            }
        }
        info(&format!("Truhe {} läuft (pid {pid}) — `luggage status`, `luggage open`", self.name));
        Ok(())
    }

    /// Sind alle Dienste bereit? Fehler, wenn einer sich beendet hat, der laufen sollte.
    fn services_ready(&self, tools: &Tools) -> Result<bool> {
        let out =
            self.pc(tools, &["process", "list", "-o", "json"]).stderr(Stdio::null()).output()?;
        let Ok(list) = serde_json::from_slice::<Vec<ProcState>>(&out.stdout) else {
            return Ok(false);
        };
        let log = self.run.join("truhe.log");
        let mut ready = 0;
        for p in &list {
            let Some(svc) = self.def.services.get(&p.name) else {
                continue;
            };
            let finished = !p.is_running && matches!(p.status.as_str(), "Completed" | "Error");
            if finished && (!svc.once || p.exit_code != 0) {
                bail!("Dienst {} beendet (exit {}) — Log: {}", p.name, p.exit_code, log.display());
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

    /// Wartet, bis bwrap die PID der Truhe in `info_json` schreibt.
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
        bail!("Truhe startet nicht — siehe {}", self.run.join("truhe.log").display())
    }

    /// Führt `cmd` in der laufenden Truhe aus; ohne Befehl eine Shell.
    fn enter(&self, cmd: &[String]) -> Result<()> {
        let pid = self
            .running()
            .with_context(|| format!("Truhe {} läuft nicht — erst `luggage up`", self.name))?;
        let tools = self.tools()?;
        let mut c = nsenter(pid, &tools);
        shell_args(&mut c, cmd);
        pass_through(c.status().context("nsenter nicht startbar")?);
        Ok(())
    }

    /// Führt `cmd` in der laufenden Truhe aus.
    pub fn cmd_exec(&self, cmd: &[String]) -> Result<()> {
        self.enter(cmd)
    }

    /// Öffnet eine Shell in der laufenden Truhe.
    pub fn cmd_open(&self) -> Result<()> {
        self.enter(&[])
    }

    /// process-compose-Client auf dem Host; der Socket liegt im Laufzeitverzeichnis.
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

    /// Stoppt die Truhe: erst die Dienste geordnet, zuletzt per SIGKILL.
    pub fn cmd_down(&self) -> Result<()> {
        let Some(pid) = self.running() else {
            let _ = fs::remove_file(self.pid_file());
            info(&format!("Truhe {} läuft nicht", self.name));
            return Ok(());
        };
        if self.socket().exists() {
            // Dienste geordnet stoppen; SIGKILL unten nur, wenn alles andere nicht reicht.
            let tools = self.tools()?;
            let client =
                self.pc(&tools, &["down"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
            let stopped = wait_dead(pid, 50);
            if !stopped {
                // process-compose hängt: alle Prozesse der Truhe bekommen SIGTERM, Datenbanken fahren sauber herunter.
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
        // PID 1 im eigenen PID-NS ignoriert SIGTERM von außen; SIGKILL räumt den ganzen NS ab.
        if alive(pid) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
        let _ = fs::remove_file(self.pid_file());
        let _ = fs::remove_file(self.socket());
        info(&format!("Truhe {} beendet", self.name));
        Ok(())
    }

    /// Zeigt Definition, Dienste und Zustand der Truhe.
    pub fn cmd_status(&self) -> Result<()> {
        println!("Truhe:       {}", self.name);
        println!("Projekt:     {}", self.project.display());
        println!(
            "Definition:  {}",
            self.def_dir
                .as_ref()
                .map_or_else(|| "keine".into(), |d| d.join("truhe.toml").display().to_string())
        );
        if let Some(d) = &self.detected {
            let found: Vec<_> = d.found.iter().map(|(n, what)| format!("{n} ({what})")).collect();
            let file = d.file.file_name().unwrap_or_default().to_string_lossy();
            println!(
                "Erkannt:     {file}: {}",
                if found.is_empty() { "nichts".into() } else { found.join(", ") }
            );
            if !d.skipped.is_empty() {
                println!("Nicht übern.: {}", d.skipped.join(", "));
            }
        }
        let services: Vec<_> = self.def.services.keys().map(String::as_str).collect();
        println!(
            "Dienste:     {}",
            if services.is_empty() { "keine".into() } else { services.join(", ") }
        );
        self.print_notes();
        println!("Zustand:     {}", self.state.display());
        let Some(pid) = self.running() else {
            println!("Status:      läuft nicht");
            return Ok(());
        };
        println!("Status:      läuft (pid {pid})");
        if self.socket().exists() {
            println!();
            let tools = self.tools()?;
            // stderr: process-compose meldet dort Debug-Zeilen zu seiner fehlenden Config
            self.pc(&tools, &["process", "list", "-o", "wide"]).stderr(Stdio::null()).status()?;
        }
        Ok(())
    }
}

/// bash in allen Namespaces der laufenden Truhe (wie `docker exec`).
fn nsenter(pid: u32, tools: &Tools) -> Command {
    let mut c = Command::new("nsenter");
    // ohne --wd: nsenter --root mit --wd liefert ein kaputtes cwd, die rc-Datei macht das cd
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

/// `-c 'source rc; exec "$@"' bash CMD…` oder interaktive Shell mit rc.
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

    fn truhe(def: Definition) -> Truhe {
        Truhe {
            name: "demo".into(),
            project: "/home/u/projects/demo".into(),
            def_dir: None,
            detected: None,
            def,
            state: "/home/u/.local/share/luggage/truhen/demo".into(),
            run: "/run/user/1000/luggage/demo".into(),
        }
    }

    fn tools() -> Tools {
        Tools {
            bash: "/nix/store/b-bash".into(),
            coreutils: "/nix/store/c-coreutils".into(),
            process_compose: "/nix/store/p-pc".into(),
            packages: vec!["/nix/store/m-mariadb".into()],
        }
    }

    #[test]
    fn parses_definition() {
        let def = parse(SPARENCON).unwrap();
        assert_eq!(def.hosts, ["database"]);
        assert_eq!(def.services.len(), 3);
        assert!(def.services["db-init"].once);
    }

    #[test]
    fn rejects_bad_definitions() {
        assert!(parse("netz = true").is_err());
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
        let def = parse("ignore = [\"mail\"]\nhosts = [\"extra\"]\n[services.db]\ncommand = \"eigenes\"\n[services.web]\ncommand = \"php -S 0:80\"\ndepends_on = [\"db\"]").unwrap();
        let m = merge(def, Some(&detected)).unwrap();
        assert_eq!(m.services.keys().collect::<Vec<_>>(), ["db", "db-init", "web"]);
        assert_eq!(m.services["db"].command, "eigenes");
        assert_eq!(m.hosts, ["db", "extra"]);
    }

    #[test]
    fn compose_uses_matching_conditions() {
        let y = truhe(parse(SPARENCON).unwrap()).render_compose();
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
        let rc = truhe(Definition::default()).render_rc(&tools());
        assert!(rc.contains(
            "export PATH=/nix/store/m-mariadb/bin:/nix/store/p-pc/bin:/nix/store/b-bash/bin:/nix/store/c-coreutils/bin:\"$PATH\"\n"
        ));
        assert!(rc.contains("cd '/home/u/projects/demo'\n"));
    }

    #[test]
    fn hosts_point_to_loopback() {
        let h = truhe(parse(SPARENCON).unwrap()).render_hosts();
        assert_eq!(h, "127.0.0.1 localhost truhe-demo database\n::1 localhost\n");
    }

    #[test]
    fn sh_quote_handles_single_quotes() {
        assert_eq!(sh_quote("it's"), r#"'it'"'"'s'"#);
    }

    #[test]
    fn bwrap_mount_order() {
        let t = truhe(Definition::default());
        let a = t.bwrap_args(&tools(), Net::Off).unwrap();
        let pos = |s: &str| a.iter().position(|x| x == s).unwrap();
        // tmpfs /tmp vor allen Binds, Home vor dem Projekt (Projekt liegt unter $HOME)
        assert!(pos("/tmp") < pos("--bind"));
        assert!(
            pos("/home/u/.local/share/luggage/truhen/demo/home") < pos("/home/u/projects/demo")
        );
        assert_eq!(a[0], "--unshare-all");
        assert!(!a.contains(&"--share-net".to_owned()));
        assert!(t.bwrap_args(&tools(), Net::Share).unwrap().contains(&"--share-net".to_owned()));
        let own = t.bwrap_args(&tools(), Net::Own).unwrap();
        assert!(!own.contains(&"--unshare-all".to_owned()));
        assert_eq!(own[0], "--unshare-user");
    }
}
