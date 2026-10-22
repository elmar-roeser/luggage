//! `luggage tui`: a terminal UI over `luggage list`, `status`, `up`, `down` and `open`.
//!
//! It runs luggage itself for every action, so the TUI does exactly what the commands do.

use std::collections::BTreeSet;
use std::io::{self, BufRead as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nix::sys::signal::{SigSet, SigmaskHow, Signal};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde::Deserialize;

/// How often the TUI reads the list and the selected chest again.
const REFRESH: Duration = Duration::from_secs(3);
/// The keys at the bottom of the screen.
const HELP: &str = "↑↓ select  u up  d down  o open  e chest.toml  c config  r refresh  q quit";

/// One chest from `luggage list --json`.
#[derive(Debug, Clone, Deserialize)]
struct Entry {
    /// Name of the chest
    name: String,
    /// Project directory; unknown for a chest that has not started since luggage records it
    project: Option<PathBuf>,
    /// Has the project a chest yet?
    chest: bool,
    /// Is the chest running?
    running: bool,
}

/// The part of `luggage status --json` that the TUI shows.
#[derive(Debug, Deserialize)]
struct Status {
    /// "vm" or "bubblewrap"
    mode: String,
    /// The `chest.toml`, if there is one
    definition: Option<PathBuf>,
    /// Branch whose service data the chest uses
    branch: Option<String>,
    /// Branch checked out in the project
    checked_out: Option<String>,
    /// PID of the running chest
    pid: Option<u32>,
    /// Forwarded ports
    ports: Vec<String>,
    /// "open", "closed" or "only <hosts>"
    network: Option<String>,
    /// Services with their state
    services: Vec<Service>,
    /// Skills in the home of the chest
    skills: Vec<String>,
    /// Coding agents from `chest.toml`
    agents: Vec<String>,
}

/// One service in [`Status`].
#[derive(Debug, Deserialize)]
struct Service {
    /// Name of the service
    name: String,
    /// Status from process-compose; null when the chest is not running
    status: Option<String>,
    /// Has the `ready` check succeeded? Null without one
    ready: Option<bool>,
    /// Exit code of the service
    exit_code: Option<i32>,
}

/// What a background job reports back.
enum Done {
    /// `luggage list --json`
    List(Result<Vec<Entry>, String>),
    /// `luggage status --json` of the chest with this name
    Status(String, Result<Status, String>),
    /// An action (`up`, `down`) on the chest with this name has finished
    Action(&'static str, String, Result<(), String>),
}

/// What the main loop has to do after a key.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Nothing more
    Stay,
    /// Leave the TUI
    Quit,
    /// Open a shell in the chest of this project
    Open(PathBuf),
    /// Edit a config file in the editor
    Edit(Edit),
}

/// A config file to edit, and how luggage checks it afterwards.
#[derive(Debug, PartialEq, Eq)]
struct Edit {
    /// The file
    file: PathBuf,
    /// Text for a file that does not exist yet
    template: &'static str,
    /// `luggage <args>` in `dir` fails if the file is invalid
    check: (&'static [&'static str], Option<PathBuf>),
    /// Is the chest running? Then a change applies after down and up
    running: bool,
}

/// Runs luggage with `args` in `dir` and returns its stdout, or the last line of its stderr on failure.
fn luggage(args: &[&str], dir: Option<&Path>) -> Result<Vec<u8>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut c = Command::new(exe);
    c.args(args).stdin(Stdio::null());
    if let Some(d) = dir {
        c.current_dir(d);
    }
    let out = c.output().map_err(|e| e.to_string())?;
    if out.status.success() { Ok(out.stdout) } else { Err(last_error(&out.stderr)) }
}

/// The last non-empty line of `stderr`, without the `luggage: ` prefix.
fn last_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("failed");
    line.strip_prefix("luggage: ").unwrap_or(line).to_owned()
}

/// The state of the TUI.
struct App {
    /// The chests from the last list
    entries: Vec<Entry>,
    /// Which chest is selected
    list: ListState,
    /// The last status, with the name of its chest
    status: Option<(String, Result<Status, String>)>,
    /// The last message for the bottom line
    message: String,
    /// Chests with an action that has not finished yet
    busy: BTreeSet<String>,
    /// Is a list job running?
    listing: bool,
    /// Is a status job running?
    reading: bool,
    /// When the list was last requested
    refreshed: Instant,
    /// Sender for the background jobs
    tx: Sender<Done>,
    /// Receiver for the background jobs
    rx: Receiver<Done>,
}

impl App {
    /// An empty app; `refresh` fills it.
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            entries: Vec::new(),
            list: ListState::default(),
            status: None,
            message: String::new(),
            busy: BTreeSet::new(),
            listing: false,
            reading: false,
            refreshed: Instant::now(),
            tx,
            rx,
        }
    }

    /// The selected chest.
    fn selected(&self) -> Option<&Entry> {
        self.list.selected().and_then(|i| self.entries.get(i))
    }

    /// Reads the list and the selected chest again in the background, unless a job for it still runs.
    fn refresh(&mut self) {
        self.refreshed = Instant::now();
        if !self.listing {
            self.listing = true;
            let tx = self.tx.clone();
            thread::spawn(move || {
                let entries = luggage(&["list", "--json"], None)
                    .and_then(|out| serde_json::from_slice(&out).map_err(|e| e.to_string()));
                let _ = tx.send(Done::List(entries));
            });
        }
        self.read_status();
    }

    /// Reads the status of the selected chest in the background.
    fn read_status(&mut self) {
        if self.reading {
            return;
        }
        let Some(Entry { name, project: Some(project), .. }) = self.selected().cloned() else {
            return;
        };
        self.reading = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let status = luggage(&["status", "--json"], Some(&project))
                .and_then(|out| serde_json::from_slice(&out).map_err(|e| e.to_string()));
            let _ = tx.send(Done::Status(name, status));
        });
    }

    /// Takes in what a background job reports.
    fn apply(&mut self, done: Done) {
        match done {
            Done::List(Ok(entries)) => {
                self.listing = false;
                let name = self.selected().map(|e| e.name.clone());
                let at = name.and_then(|n| entries.iter().position(|e| e.name == n));
                self.entries = entries;
                self.list.select(if self.entries.is_empty() {
                    None
                } else {
                    Some(at.unwrap_or(0))
                });
                if self.status.as_ref().map(|s| &s.0) != self.selected().map(|e| &e.name) {
                    self.read_status();
                }
            }
            Done::List(Err(e)) => {
                self.listing = false;
                self.message = format!("list: {e}");
            }
            Done::Status(name, status) => {
                self.reading = false;
                self.status = Some((name, status));
                // the selection may have moved while the job ran
                if self.status.as_ref().map(|s| &s.0) != self.selected().map(|e| &e.name) {
                    self.read_status();
                }
            }
            Done::Action(what, name, result) => {
                self.busy.remove(&name);
                self.message = match result {
                    Ok(()) => format!("{what} {name}: done"),
                    Err(e) => format!("{what} {name}: {e}"),
                };
                self.refresh();
            }
        }
    }

    /// Runs `luggage <what>` for the selected chest in the background.
    fn act(&mut self, what: &'static str) {
        let Some(entry) = self.selected().cloned() else {
            return;
        };
        let Some(project) = entry.project else {
            self.message = format!("{}: project unknown", entry.name);
            return;
        };
        if !self.busy.insert(entry.name.clone()) {
            return;
        }
        self.message = format!("{what} {} …", entry.name);
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = luggage(&[what], Some(&project)).map(|_| ());
            let _ = tx.send(Done::Action(what, entry.name, result));
        });
    }

    /// Handles a key.
    fn key(&mut self, code: KeyCode) -> Step {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Step::Quit,
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('u') => self.act("up"),
            KeyCode::Char('d') => self.act("down"),
            KeyCode::Char('c') => {
                return Step::Edit(Edit {
                    file: crate::config::path(),
                    template: crate::config::TEMPLATE,
                    check: (&["config"], None),
                    running: false,
                });
            }
            KeyCode::Char('e') => match self.selected() {
                Some(Entry { project: Some(p), running, .. }) => {
                    return Step::Edit(Edit {
                        file: crate::chest::definition_file(p),
                        template: crate::chest::TEMPLATE,
                        check: (&["status", "--json"], Some(p.clone())),
                        running: *running,
                    });
                }
                Some(e) => self.message = format!("{}: project unknown", e.name),
                None => {}
            },
            KeyCode::Char('o') => match self.selected() {
                Some(Entry { project: Some(p), .. }) => return Step::Open(p.clone()),
                Some(e) => self.message = format!("{}: project unknown", e.name),
                None => {}
            },
            _ => {}
        }
        Step::Stay
    }

    /// Moves the selection by `by` rows and stops at the ends.
    fn move_by(&mut self, by: isize) {
        let Some(last) = self.entries.len().checked_sub(1) else {
            return;
        };
        let now = self.list.selected().unwrap_or(0);
        let next = now.saturating_add_signed(by).min(last);
        if next != now {
            self.list.select(Some(next));
            self.read_status();
        }
    }

    /// Draws the list on the left, the selected chest on the right and the keys at the bottom.
    fn draw(&mut self, f: &mut Frame<'_>) {
        let [main, bottom] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        let [left, right] =
            Layout::horizontal([Constraint::Length(28), Constraint::Min(1)]).areas(main);
        let items: Vec<ListItem<'_>> = self
            .entries
            .iter()
            .map(|e| {
                let (mark, color) = match (e.chest, e.running) {
                    (_, true) => ("● ", Color::Green),
                    (true, false) => ("○ ", Color::DarkGray),
                    (false, false) => ("· ", Color::DarkGray),
                };
                let busy = if self.busy.contains(&e.name) { " …" } else { "" };
                ListItem::new(Line::from(vec![
                    Span::styled(mark, Style::new().fg(color)),
                    Span::raw(format!("{}{busy}", e.name)),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(Block::bordered().title(" projects "))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, left, &mut self.list);
        let (title, lines) = match self.selected() {
            Some(e) => {
                let status = self.status.as_ref().filter(|s| s.0 == e.name).map(|s| &s.1);
                (format!(" {} ", e.name), detail_lines(e, status))
            }
            None => (
                String::new(),
                vec![Line::raw("no chests or projects yet — `luggage init` in a repo sets one up")],
            ),
        };
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(title)).wrap(Wrap { trim: false }),
            right,
        );
        let bottom_line = if self.message.is_empty() {
            Line::styled(HELP, Style::new().fg(Color::DarkGray))
        } else {
            Line::from(vec![
                Span::raw(self.message.clone()),
                Span::styled(format!("   {HELP}"), Style::new().fg(Color::DarkGray)),
            ])
        };
        f.render_widget(bottom_line, bottom);
    }
}

/// `path` with the home directory `home` shortened to `~`.
fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if home != Path::new("/") => Path::new("~").join(rest).display().to_string(),
        _ => path.display().to_string(),
    }
}

/// The lines about the chest `e` with its `status`, if read already.
fn detail_lines(e: &Entry, status: Option<&Result<Status, String>>) -> Vec<Line<'static>> {
    let Some(project) = &e.project else {
        return vec![Line::raw("project unknown — run `luggage up` once in its project")];
    };
    let row = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label:<12}"), Style::new().fg(Color::DarkGray)),
            Span::raw(value),
        ])
    };
    let mut lines = vec![row("Project", tilde(project, &crate::home()))];
    let s = match status {
        None => {
            lines.push(Line::raw("reading …"));
            return lines;
        }
        Some(Err(err)) => {
            lines.push(Line::styled(err.clone(), Style::new().fg(Color::Red)));
            return lines;
        }
        Some(Ok(s)) => s,
    };
    let list = |v: &[String]| if v.is_empty() { "none".to_owned() } else { v.join(", ") };
    lines.push(row("Mode", s.mode.clone()));
    lines.push(row(
        "Definition",
        s.definition.as_ref().map_or_else(|| "none".into(), |d| tilde(d, &crate::home())),
    ));
    if let Some(b) = &s.branch {
        let pending = match &s.checked_out {
            Some(c) if c != b => format!(" (checked out: {c})"),
            _ => String::new(),
        };
        lines.push(row("Branch", format!("{b}{pending}")));
    }
    lines.push(row(
        "Status",
        match s.pid {
            Some(p) => format!("running (pid {p})"),
            None if e.chest => "stopped".into(),
            None => "no chest yet — u starts one".into(),
        },
    ));
    if s.pid.is_some() {
        lines.push(row("Ports", list(&s.ports)));
        lines.push(row("Network", s.network.clone().unwrap_or_default()));
    }
    lines.push(row("Skills", list(&s.skills)));
    lines.push(row("Agents", list(&s.agents)));
    if !s.services.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("Services", Style::new().add_modifier(Modifier::BOLD)));
        for svc in &s.services {
            let (state, color) = match (&svc.status, svc.ready, svc.exit_code) {
                (None, ..) => ("-".to_owned(), Color::DarkGray),
                (Some(st), Some(false), _) => (format!("{st}, not ready"), Color::Yellow),
                (Some(st), _, Some(code)) if st == "Error" || (st == "Completed" && code != 0) => {
                    (format!("{st} (exit {code})"), Color::Red)
                }
                (Some(st), ..) => (st.clone(), Color::Green),
            };
            lines.push(Line::from(vec![
                Span::raw(format!("  {:<20}", svc.name)),
                Span::styled(state, Style::new().fg(color)),
            ]));
        }
    }
    lines
}

/// Leaves the TUI, runs `luggage open` in `project` and comes back.
fn open(terminal: &mut DefaultTerminal, project: &Path) -> Result<String> {
    ratatui::restore();
    let status = Command::new(std::env::current_exe()?).arg("open").current_dir(project).status();
    take_terminal()?;
    let message = match status {
        Ok(s) if s.success() => String::new(),
        Ok(_) | Err(_) => {
            // the error is on the normal screen, which the TUI hides again
            ask("press Enter to go back to the TUI");
            "open failed".to_owned()
        }
    };
    back(terminal)?;
    Ok(message)
}

/// Prints `question` on the normal screen and returns the answer.
fn ask(question: &str) -> String {
    eprint!("luggage: {question} ");
    let mut answer = String::new();
    let _ = io::stdin().lock().read_line(&mut answer);
    answer.trim().to_owned()
}

/// Returns to the TUI screen after a program ran on the normal screen.
fn back(terminal: &mut DefaultTerminal) -> Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    terminal.clear()?;
    Ok(())
}

/// The editor from `VISUAL` or `EDITOR`, else vi.
fn editor() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|e| !e.trim().is_empty()))
        .unwrap_or_else(|| "vi".into())
}

/// Leaves the TUI, opens the file of `edit` in the editor until it is valid or the user gives up, and comes back.
/// A missing file starts from the template; reverting brings back what was there before.
fn edit(terminal: &mut DefaultTerminal, edit: &Edit) -> Result<String> {
    ratatui::restore();
    let before = std::fs::read_to_string(&edit.file).ok();
    if before.is_none() {
        if let Some(dir) = edit.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&edit.file, edit.template)?;
    }
    let name = edit.file.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let message = loop {
        // the editor may hold arguments, like "code -w"
        let ran = Command::new("sh")
            .arg("-c")
            .arg(format!("{} \"$1\"", editor()))
            .arg("sh")
            .arg(&edit.file)
            .status();
        take_terminal()?;
        if !ran.is_ok_and(|s| s.success()) {
            ask("the editor failed — press Enter to go back to the TUI");
        }
        let (args, dir) = &edit.check;
        let Err(e) = luggage(args, dir.as_deref()) else {
            let now = std::fs::read_to_string(&edit.file).ok();
            break if now == before {
                format!("{name}: unchanged")
            } else if edit.running {
                format!("{name}: saved — d and u apply it to the running chest")
            } else {
                format!("{name}: saved")
            };
        };
        eprintln!("luggage: {name} is invalid: {e}");
        match ask("[e]dit again, [r]evert or [k]eep it?").as_str() {
            "r" | "R" => {
                match &before {
                    Some(text) => std::fs::write(&edit.file, text)?,
                    None => std::fs::remove_file(&edit.file)?,
                }
                break format!("{name}: reverted");
            }
            "k" | "K" => break format!("{name}: kept, but invalid: {e}"),
            _ => {}
        }
    };
    back(terminal)?;
    Ok(message)
}

/// Makes the process group of the TUI the foreground of the terminal again.
/// The shell in the chest takes the terminal and cannot give it back from its PID namespace.
fn take_terminal() -> Result<()> {
    let mut ttou = SigSet::empty();
    ttou.add(Signal::SIGTTOU);
    // from the background, tcsetpgrp would stop the TUI with SIGTTOU unless it is blocked
    let old = ttou.thread_swap_mask(SigmaskHow::SIG_BLOCK)?;
    let result = rustix::termios::tcsetpgrp(io::stdin(), rustix::process::getpgrp());
    old.thread_set_mask()?;
    result.context("cannot take the terminal back")
}

/// Runs the TUI until the user quits.
///
/// # Errors
/// If the terminal cannot be set up or drawn.
#[cfg(not(tarpaulin_include))]
pub fn run() -> Result<()> {
    let mut terminal = ratatui::try_init().context("cannot set up the terminal")?;
    let result = run_loop(&mut terminal);
    ratatui::restore();
    result
}

/// The main loop: draw, take in finished jobs, handle keys.
fn run_loop(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut app = App::new();
    app.refresh();
    loop {
        terminal.draw(|f| app.draw(f))?;
        while let Ok(done) = app.rx.try_recv() {
            app.apply(done);
        }
        if app.refreshed.elapsed() >= REFRESH {
            app.refresh();
        }
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match app.key(k.code) {
            Step::Stay => {}
            Step::Quit => return Ok(()),
            Step::Open(project) => {
                app.message = open(terminal, &project)?;
                app.refresh();
            }
            Step::Edit(e) => {
                app.message = edit(terminal, &e)?;
                app.refresh();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, project: Option<&str>) -> Entry {
        Entry {
            name: name.into(),
            project: project.map(PathBuf::from),
            chest: true,
            running: false,
        }
    }

    fn text(lines: &[Line<'_>]) -> String {
        lines.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn paths_under_home_start_with_a_tilde() {
        let home = Path::new("/home/u");
        assert_eq!(tilde(Path::new("/home/u/projects/demo"), home), "~/projects/demo");
        assert_eq!(tilde(Path::new("/home/user2/x"), home), "/home/user2/x");
    }

    #[test]
    fn errors_show_the_last_line_without_the_prefix() {
        assert_eq!(last_error(b"building\nluggage: no flake.nix in /x\n\n"), "no flake.nix in /x");
        assert_eq!(last_error(b""), "failed");
    }

    #[test]
    fn details_of_a_running_chest() {
        let status: Status = serde_json::from_str(
            r#"{"name":"demo","project":"/p/demo","mode":"vm","definition":null,"state":"/s",
                "branch":"main","checked_out":"feat/x","running":true,"pid":42,"ports":["web 8080:80"],
                "network":"closed","skills":[],"agents":["claude"],"notes":[],
                "services":[{"name":"db","status":"Running","ready":false,"exit_code":0},
                            {"name":"init","status":"Completed","ready":null,"exit_code":3}]}"#,
        )
        .unwrap();
        let t = text(&detail_lines(&entry("demo", Some("/p/demo")), Some(&Ok(status))));
        for want in [
            "/p/demo",
            "main (checked out: feat/x)",
            "running (pid 42)",
            "web 8080:80",
            "claude",
            "Running, not ready",
            "Completed (exit 3)",
        ] {
            assert!(t.contains(want), "{want} missing in:\n{t}");
        }
    }

    #[test]
    fn a_project_without_a_chest_offers_to_start_one() {
        let status: Status = serde_json::from_str(
            r#"{"mode":"bubblewrap","definition":null,"branch":null,"checked_out":"main","pid":null,
                "ports":[],"network":null,"services":[],"skills":[],"agents":[]}"#,
        )
        .unwrap();
        let e = Entry { chest: false, ..entry("film", Some("/p/film")) };
        assert!(text(&detail_lines(&e, Some(&Ok(status)))).contains("no chest yet — u starts one"));
    }

    #[test]
    fn details_without_project_or_status() {
        let t = text(&detail_lines(&entry("old", None), None));
        assert!(t.contains("project unknown"));
        let t = text(&detail_lines(&entry("demo", Some("/p")), Some(&Err("broken".into()))));
        assert!(t.contains("broken"));
    }

    #[test]
    fn keys_move_within_the_list_and_open_needs_a_project() {
        let mut app = App::new();
        app.entries = vec![entry("a", None), entry("b", Some("/p/b"))];
        app.list.select(Some(0));
        // the first chest has no project, so no status job starts
        assert_eq!(app.key(KeyCode::Char('o')), Step::Stay);
        assert!(app.message.contains("project unknown"));
        app.key(KeyCode::Up);
        assert_eq!(app.list.selected(), Some(0));
        app.reading = true; // no real status job in the test
        app.key(KeyCode::Down);
        app.key(KeyCode::Char('j'));
        assert_eq!(app.list.selected(), Some(1));
        assert_eq!(app.key(KeyCode::Char('o')), Step::Open("/p/b".into()));
        let e = app.key(KeyCode::Char('e'));
        assert!(
            matches!(&e, Step::Edit(Edit { file, check: (_, Some(dir)), .. })
                if file == Path::new("/p/b/.luggage/chest.toml") && dir == Path::new("/p/b")),
            "{e:?}"
        );
        let c = app.key(KeyCode::Char('c'));
        assert!(
            matches!(&c, Step::Edit(Edit { file, check: (_, None), .. })
                if file.ends_with("luggage/config.toml")),
            "{c:?}"
        );
        assert_eq!(app.key(KeyCode::Char('q')), Step::Quit);
    }

    #[test]
    fn a_new_list_keeps_the_selected_chest() {
        let mut app = App::new();
        app.reading = true;
        app.entries = vec![entry("a", None), entry("b", None)];
        app.list.select(Some(1));
        app.listing = true;
        app.apply(Done::List(Ok(vec![entry("0", None), entry("a", None), entry("b", None)])));
        assert_eq!(app.selected().map(|e| e.name.as_str()), Some("b"));
        app.apply(Done::List(Ok(vec![entry("x", None)])));
        assert_eq!(app.list.selected(), Some(0));
    }
}
