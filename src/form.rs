//! The form in `luggage tui` for the common fields of `config.toml` and `chest.toml`.
//!
//! It edits the file with `toml_edit`, so comments and everything the form does not show stay as they are.

use std::path::Path;

use anyhow::{Context, Result, bail};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use toml_edit::{Array, DocumentMut, Item, Table, value};

/// What kind of value a field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A string
    Text,
    /// A whole number
    Number,
    /// A list of strings, typed with commas
    List,
    /// One of these strings
    Choice(&'static [&'static str]),
    /// Some of these strings, as a list
    Names(&'static [&'static str]),
}

/// One field of the form.
#[derive(Debug)]
pub struct Field {
    /// Where the value lives, e.g. `["nix", "nixpkgs"]`
    pub path: &'static [&'static str],
    /// What luggage does without the field
    pub default: &'static str,
    /// The kind of value
    pub kind: Kind,
    /// One line about the field
    pub help: &'static str,
}

/// The fields of `config.toml`; the skill sources stay in the editor.
pub const CONFIG_FIELDS: &[Field] = &[
    Field {
        path: &["projects_dir"],
        default: "~/projects",
        kind: Kind::Text,
        help: "Where `luggage new` clones or creates projects; the TUI lists the flakes in it",
    },
    Field {
        path: &["nix", "nixpkgs"],
        default: crate::config::DEFAULT_NIXPKGS,
        kind: Kind::Text,
        help: "Channel for new flakes; it decides which versions are available",
    },
    Field {
        path: &["nix", "packages"],
        default: "none",
        kind: Kind::List,
        help: "Packages that go into every environment, e.g. just, jq",
    },
    Field {
        path: &["gitlab", "host"],
        default: "the default host of glab",
        kind: Kind::Text,
        help: "GitLab host for --gitlab, e.g. gitlab.example.com",
    },
    Field {
        path: &["gitlab", "group"],
        default: "none",
        kind: Kind::Text,
        help: "Group for --gitlab without a group",
    },
    Field {
        path: &["gitlab", "visibility"],
        default: "private",
        kind: Kind::Choice(&["private", "internal", "public"]),
        help: "Visibility of new GitLab projects",
    },
    Field {
        path: &["github", "owner"],
        default: "the logged-in gh account",
        kind: Kind::Text,
        help: "Owner of new GitHub repos",
    },
    Field {
        path: &["github", "visibility"],
        default: "private",
        kind: Kind::Choice(&["private", "internal", "public"]),
        help: "Visibility of new GitHub repos",
    },
    Field {
        path: &["php", "default"],
        default: "the nixpkgs default",
        kind: Kind::Text,
        help: "PHP version if composer.json does not set one, e.g. 8.4",
    },
    Field {
        path: &["php", "memory_limit"],
        default: "512M",
        kind: Kind::Text,
        help: "PHP memory_limit",
    },
    Field {
        path: &["php", "extensions"],
        default: "none",
        kind: Kind::List,
        help: "PHP extensions in addition to the ext-* from composer.json, e.g. xdebug",
    },
    Field {
        path: &["node", "default"],
        default: "the nixpkgs default",
        kind: Kind::Number,
        help: "Node major version if package.json and .nvmrc do not set one, e.g. 24",
    },
    Field {
        path: &["python", "default"],
        default: "the nixpkgs default",
        kind: Kind::Text,
        help: "Python version if .python-version and pyproject.toml do not set one, e.g. 3.13",
    },
];

/// The fields of `chest.toml`; the services stay in the editor.
pub const CHEST_FIELDS: &[Field] = &[
    Field {
        path: &["mode"],
        default: "bubblewrap",
        kind: Kind::Choice(&["bubblewrap", "vm"]),
        help: "bubblewrap: namespaces on the host kernel; vm: a small virtual machine with its own kernel",
    },
    Field {
        path: &["network"],
        default: "VM: open, bubblewrap: none",
        kind: Kind::List,
        help: "The only hosts the chest may reach, e.g. github.com, git.example.com:22",
    },
    Field {
        path: &["agents"],
        default: "none",
        kind: Kind::Names(&crate::agents::NAMES),
        help: "Coding agents that `up` installs into the home of the chest",
    },
    Field {
        path: &["skills"],
        default: "none",
        kind: Kind::List,
        help: "Skills from the sources in the config, as <source>/<skill> with *, e.g. own/*",
    },
    Field {
        path: &["hosts"],
        default: "none",
        kind: Kind::List,
        help: "Names that point to 127.0.0.1 in the chest, e.g. api.local",
    },
    Field {
        path: &["packages"],
        default: "none",
        kind: Kind::List,
        help: "Extra nixpkgs packages for the services, e.g. curl",
    },
    Field {
        path: &["pass_env"],
        default: "none",
        kind: Kind::List,
        help: "Host variables that commands in the chest get, e.g. GITHUB_TOKEN",
    },
    Field {
        path: &["ignore"],
        default: "none",
        kind: Kind::List,
        help: "Detected Compose services to skip, e.g. mailer",
    },
];

/// The value of `field` in `doc` as the form shows it; `None` if it is not set.
pub fn get(doc: &DocumentMut, field: &Field) -> Option<String> {
    let mut item = doc.as_item();
    for key in field.path {
        item = item.get(key)?;
    }
    let v = item.as_value()?;
    Some(match v {
        toml_edit::Value::String(s) => s.value().clone(),
        toml_edit::Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_owned))
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string().trim().to_owned(),
    })
}

/// The table that holds `field`, created if it is missing.
fn parent<'a>(doc: &'a mut DocumentMut, field: &Field) -> Result<&'a mut Table> {
    let (_, sections) = field.path.split_last().context("empty field path")?;
    let mut table = doc.as_table_mut();
    for key in sections {
        let item = table.entry(key).or_insert_with(toml_edit::table);
        table =
            item.as_table_mut().with_context(|| format!("{key} is not a table like [{key}]"))?;
    }
    Ok(table)
}

/// Splits `text` at the first line that comments out `key = …`.
/// Returns the lines before it, the note at the end of that line (like `   # private | public`) and the lines after it.
fn split_at_comment<'a>(text: &'a str, key: &str) -> Option<(&'a str, &'a str, &'a str)> {
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let end = start + line.len();
        let commented = line.trim_start().strip_prefix('#').map(str::trim_start);
        if commented
            .and_then(|c| c.strip_prefix(key))
            .is_some_and(|r| r.trim_start().starts_with('='))
        {
            let body = line.trim_end_matches('\n');
            // the note keeps the spaces in front of it, so it stays in its column
            let note = body.find("  #").map_or("", |i| &body[body[..i].trim_end().len()..]);
            return Some((&text[..start], note, &text[end..]));
        }
        start = end;
    }
    None
}

/// Where the comments below the last key of a table live: at the next table header, or at the end of the file.
enum Holder {
    /// The prefix of the table at this path
    Table(Vec<String>),
    /// The end of the document
    Trailing,
}

/// The tables of `table` with their place in the file, at any depth.
fn tables(table: &Table, path: &[String], out: &mut Vec<(isize, Vec<String>)>) {
    for (k, item) in table {
        let Some(sub) = item.as_table() else {
            continue;
        };
        let mut p = path.to_vec();
        p.push(k.to_owned());
        if let (false, Some(pos)) = (sub.is_implicit(), sub.position()) {
            out.push((pos, p.clone()));
        }
        tables(sub, &p, out);
    }
}

/// The table at `path`; the root for an empty path.
fn table_at<'a>(doc: &'a mut DocumentMut, path: &[String]) -> Option<&'a mut Table> {
    let mut t = doc.as_table_mut();
    for k in path {
        t = t.get_mut(k)?.as_table_mut()?;
    }
    Some(t)
}

/// What holds the comments after the last key of the table at `path`; `None` for a table not in the file yet.
fn holder(doc: &mut DocumentMut, path: &[String]) -> Option<Holder> {
    let own = if path.is_empty() { isize::MIN } else { table_at(doc, path)?.position()? };
    let mut all = Vec::new();
    tables(doc.as_table(), &[], &mut all);
    let next = all.into_iter().filter(|(p, _)| *p > own).min();
    Some(next.map_or(Holder::Trailing, |(_, p)| Holder::Table(p)))
}

/// The text that `holder` keeps.
fn holder_text(doc: &mut DocumentMut, holder: &Holder) -> String {
    match holder {
        Holder::Table(p) => table_at(doc, p)
            .and_then(|t| t.decor().prefix().and_then(|r| r.as_str()).map(str::to_owned))
            .unwrap_or_default(),
        Holder::Trailing => doc.trailing().as_str().unwrap_or_default().to_owned(),
    }
}

/// Replaces the text that `holder` keeps.
fn set_holder_text(doc: &mut DocumentMut, holder: &Holder, text: String) {
    match holder {
        Holder::Table(p) => {
            if let Some(t) = table_at(doc, p) {
                t.decor_mut().set_prefix(text);
            }
        }
        Holder::Trailing => doc.set_trailing(text),
    }
}

/// The comments above `key` in `table`.
fn key_prefix(table: &Table, key: &str) -> String {
    table
        .key(key)
        .and_then(|k| k.leaf_decor().prefix())
        .and_then(|r| r.as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Adds the new `key` with `item` to the table at `path`, in the place of its commented-out line if there is one.
/// That line sits in the comments above a later key of the table, or below its last key.
fn insert_new(doc: &mut DocumentMut, path: &[String], key: &str, item: Item) -> Result<()> {
    let table = table_at(doc, path).context("the table is missing")?;
    let keys: Vec<String> = table.iter().map(|(k, _)| k.to_owned()).collect();
    let at_key = keys.iter().find_map(|k| {
        let text = key_prefix(table, k);
        split_at_comment(&text, key)
            .map(|(b, n, a)| (Some(k.clone()), b.to_owned(), n.to_owned(), a.to_owned()))
    });
    let found = at_key.or_else(|| {
        let h = holder(doc, path);
        let text = h.as_ref().map(|h| holder_text(doc, h)).unwrap_or_default();
        let split = split_at_comment(&text, key)
            .map(|(b, n, a)| (None, b.to_owned(), n.to_owned(), a.to_owned()));
        if let (Some(h), Some((_, _, _, after))) = (&h, &split) {
            set_holder_text(doc, h, after.clone());
        }
        split
    });
    let table = table_at(doc, path).context("the table is missing")?;
    table.insert(key, item);
    let Some((next, before, note, after)) = found else {
        return Ok(());
    };
    if let Some(next) = &next {
        if let Some(mut k) = table.key_mut(next) {
            k.leaf_decor_mut().set_prefix(after);
        }
        // the new key goes right before the key whose comments held its line
        let rank = |k: &str| {
            let at = keys.iter().position(|o| o == k);
            let next_at = keys.iter().position(|o| o == next).unwrap_or(0);
            at.map_or(next_at * 2, |i| i * 2 + 1)
        };
        table.sort_values_by(|a, _, b, _| rank(a.get()).cmp(&rank(b.get())));
    }
    if let Some(mut k) = table.key_mut(key) {
        k.leaf_decor_mut().set_prefix(before);
    }
    if let Some(v) = table.get_mut(key).and_then(Item::as_value_mut)
        && !note.is_empty()
    {
        v.decor_mut().set_suffix(note);
    }
    Ok(())
}

/// Removes `key` from the table at `path` and leaves a commented-out line with its value, so its comments stay.
fn remove(doc: &mut DocumentMut, path: &[String], key: &str) {
    let Some(table) = table_at(doc, path) else {
        return;
    };
    let keys: Vec<String> = table.iter().map(|(k, _)| k.to_owned()).collect();
    let prefix = key_prefix(table, key);
    let Some(old) = table.remove(key) else {
        return;
    };
    let line = match old.into_value() {
        Ok(v) => {
            let note = v.decor().suffix().and_then(|r| r.as_str()).unwrap_or_default().to_owned();
            format!("# {key} = {}{note}\n", v.decorated("", ""))
        }
        Err(item) => format!("# {key} = {item}\n"),
    };
    let text = prefix + &line;
    match keys.iter().skip_while(|k| *k != key).nth(1) {
        Some(next) => {
            let rest = key_prefix(table, next);
            if let Some(mut k) = table.key_mut(next) {
                k.leaf_decor_mut().set_prefix(text + &rest);
            }
        }
        None => {
            if let Some(h) = holder(doc, path) {
                let rest = holder_text(doc, &h);
                set_holder_text(doc, &h, text + &rest);
            }
        }
    }
}

/// Sets `field` in `doc` from the `input` that the user typed; an empty input removes it.
///
/// # Errors
/// If the input does not fit the field, e.g. text for a number.
pub fn set(doc: &mut DocumentMut, field: &Field, input: &str) -> Result<()> {
    let input = input.trim();
    let (key, sections) = field.path.split_last().context("empty field path")?;
    let path: Vec<String> = sections.iter().map(|&s| s.to_owned()).collect();
    if input.is_empty() {
        remove(doc, &path, key);
        return Ok(());
    }
    let item: Item = match field.kind {
        Kind::Text => value(input),
        Kind::Number => match input.parse::<i64>() {
            Ok(n) if n >= 0 => value(n),
            _ => bail!("a whole number, e.g. 24"),
        },
        Kind::List | Kind::Names(_) => {
            let items: Array = input.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
            value(items)
        }
        Kind::Choice(options) => {
            if !options.contains(&input) {
                bail!("one of {}", options.join(", "));
            }
            value(input)
        }
    };
    let table = parent(doc, field)?;
    match table.get_mut(key).and_then(Item::as_value_mut) {
        // an existing value keeps its place and its note
        Some(v) => {
            if let Ok(new) = item.into_value() {
                let decor = v.decor().clone();
                *v = new;
                *v.decor_mut() = decor;
            }
        }
        None => insert_new(doc, &path, key, item)?,
    }
    Ok(())
}

/// The open dropdown of a choice or a set of names.
#[derive(Debug)]
pub struct Pick {
    /// The highlighted row; row 0 is the default
    pub at: usize,
    /// For names: which ones have a tick
    pub ticked: Vec<bool>,
}

/// The rows of the dropdown for `field`: the default first, then the options.
fn options(field: &Field) -> Vec<String> {
    let rest = match field.kind {
        Kind::Choice(o) | Kind::Names(o) => o,
        Kind::Text | Kind::Number | Kind::List => &[],
    };
    std::iter::once(format!("default: {}", field.default))
        .chain(rest.iter().map(|&o| o.to_owned()))
        .collect()
}

/// The form for one file.
#[derive(Debug)]
pub struct Form {
    /// Its text when the form opened; `None` if it did not exist
    pub before: Option<String>,
    /// The fields the form shows
    pub fields: &'static [Field],
    /// The file as the form changes it
    pub doc: DocumentMut,
    /// The selected field
    pub selected: usize,
    /// The text being typed into the selected field
    pub input: Option<String>,
    /// The open dropdown of the selected field
    pub pick: Option<Pick>,
    /// Has the form changed the file?
    pub dirty: bool,
    /// The last problem, shown above the keys
    pub error: Option<String>,
    /// Was Esc pressed once with unsaved changes?
    pub leaving: bool,
}

/// What the TUI has to do after a key in the form.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing more
    Stay,
    /// Close the form without saving
    Close,
    /// Write the file and check it
    Save,
    /// Close the form and open the file in the editor
    Editor,
}

impl Form {
    /// A form for `file`, or for `template` if it does not exist yet.
    ///
    /// # Errors
    /// If the file is not valid TOML; the editor has to fix it first.
    pub fn open(file: &Path, template: &str, fields: &'static [Field]) -> Result<Self> {
        let before = std::fs::read_to_string(file).ok();
        let doc = before.as_deref().unwrap_or(template).parse::<DocumentMut>()?;
        Ok(Self {
            before,
            fields,
            doc,
            selected: 0,
            input: None,
            pick: None,
            dirty: false,
            error: None,
            leaving: false,
        })
    }

    /// The selected field.
    fn field(&self) -> &'static Field {
        &self.fields[self.selected.min(self.fields.len() - 1)]
    }

    /// Sets the selected field to `input` and notes a problem instead if it does not fit.
    fn apply(&mut self, input: &str) {
        let field = self.field();
        match set(&mut self.doc, field, input) {
            Ok(()) => {
                self.dirty = true;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Opens the dropdown of the selected field at its value.
    fn open_pick(&mut self) {
        let field = self.field();
        let now = get(&self.doc, field).unwrap_or_default();
        let picked: Vec<&str> = now.split(',').map(str::trim).collect();
        let options = options(field);
        let ticked = match field.kind {
            Kind::Names(_) => options.iter().map(|o| picked.contains(&o.as_str())).collect(),
            _ => Vec::new(),
        };
        let at = options.iter().position(|o| *o == now).unwrap_or(0);
        self.pick = Some(Pick { at, ticked });
    }

    /// Handles a key in the open dropdown.
    fn pick_key(&mut self, code: KeyCode) {
        let field = self.field();
        let options = options(field);
        let Some(pick) = &mut self.pick else {
            return;
        };
        let names = matches!(field.kind, Kind::Names(_));
        match code {
            KeyCode::Down | KeyCode::Char('j') => pick.at = (pick.at + 1).min(options.len() - 1),
            KeyCode::Up | KeyCode::Char('k') => pick.at = pick.at.saturating_sub(1),
            KeyCode::Char(' ') if names => {
                if pick.at == 0 {
                    pick.ticked.fill(false);
                } else if let Some(t) = pick.ticked.get_mut(pick.at) {
                    *t = !*t;
                }
            }
            KeyCode::Enter => {
                // a name without any tick takes the highlighted one, so Enter alone works too
                if names && pick.at > 0 && !pick.ticked.contains(&true) {
                    pick.ticked[pick.at] = true;
                }
                let value = if names {
                    let on = options.iter().zip(&pick.ticked).filter(|(_, t)| **t);
                    on.map(|(o, _)| o.as_str()).collect::<Vec<_>>().join(", ")
                } else if pick.at == 0 {
                    String::new()
                } else {
                    options[pick.at].clone()
                };
                self.pick = None;
                self.apply(&value);
            }
            KeyCode::Esc | KeyCode::Char('q') => self.pick = None,
            _ => {}
        }
    }

    /// Handles a key.
    pub fn key(&mut self, code: KeyCode) -> Action {
        if self.pick.is_some() {
            self.pick_key(code);
            return Action::Stay;
        }
        if let Some(input) = &mut self.input {
            match code {
                KeyCode::Enter => {
                    let typed = std::mem::take(input);
                    self.input = None;
                    self.apply(&typed);
                }
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            }
            return Action::Stay;
        }
        let was_leaving = std::mem::take(&mut self.leaving);
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.dirty && !was_leaving {
                    self.leaving = true;
                    self.error = Some("unsaved changes — s saves, Esc again discards them".into());
                } else {
                    return Action::Close;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.fields.len() - 1);
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => {
                let field = self.field();
                match field.kind {
                    Kind::Choice(_) | Kind::Names(_) => self.open_pick(),
                    Kind::Text | Kind::Number | Kind::List => {
                        self.input = Some(get(&self.doc, field).unwrap_or_default());
                    }
                }
            }
            KeyCode::Delete | KeyCode::Backspace => self.apply(""),
            KeyCode::Char('s') => return Action::Save,
            KeyCode::Char('E') => {
                if self.dirty {
                    self.error =
                        Some("unsaved changes — s saves them before the editor opens".into());
                } else {
                    return Action::Editor;
                }
            }
            _ => {}
        }
        Action::Stay
    }

    /// Draws the form into `area`.
    pub fn draw(&self, f: &mut Frame<'_>, area: Rect, title: &str) {
        let [rows, help] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(4)]).areas(area);
        let width = self.fields.iter().map(|x| x.path.join(".").len()).max().unwrap_or(0) + 2;
        let lines: Vec<Line<'_>> = self
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let name = Span::raw(format!(" {:<width$}", field.path.join(".")));
                let shown = if i == self.selected
                    && let Some(input) = &self.input
                {
                    Span::styled(format!("{input}▏"), Style::new().fg(Color::Yellow))
                } else {
                    match get(&self.doc, field) {
                        Some(v) => Span::styled(v, Style::new().add_modifier(Modifier::BOLD)),
                        None => Span::styled(
                            format!("default: {}", field.default),
                            Style::new().fg(Color::Gray),
                        ),
                    }
                };
                let mut spans = vec![name, shown];
                if matches!(field.kind, Kind::Choice(_) | Kind::Names(_)) {
                    spans.push(Span::raw(" ▾"));
                }
                let line = Line::from(spans);
                if i == self.selected {
                    line.style(Style::new().add_modifier(Modifier::REVERSED))
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(title.to_owned())),
            rows,
        );
        let mut about = vec![Line::raw(self.field().help)];
        let hint = match (self.input.is_some(), self.pick.is_some(), self.field().kind) {
            (true, ..) => "Enter keeps it, Esc cancels; lists take commas, empty means default",
            (_, true, Kind::Names(_)) => "Space ticks, Enter takes the ticked ones, Esc cancels",
            (_, true, _) => "Enter takes it, Esc cancels",
            _ => "",
        };
        match &self.error {
            Some(e) => about.push(Line::styled(e.clone(), Style::new().fg(Color::Red))),
            None if !hint.is_empty() => about.push(Line::raw(hint)),
            None => {}
        }
        f.render_widget(
            Paragraph::new(about).block(Block::bordered()).wrap(Wrap { trim: false }),
            help,
        );
        if let Some(pick) = &self.pick {
            self.draw_pick(f, rows, width, pick);
        }
    }

    /// Draws the open dropdown below the selected row of `rows`.
    fn draw_pick(&self, f: &mut Frame<'_>, rows: Rect, width: usize, pick: &Pick) {
        let names = matches!(self.field().kind, Kind::Names(_));
        let lines: Vec<Line<'_>> = options(self.field())
            .into_iter()
            .enumerate()
            .map(|(i, o)| {
                let tick = match pick.ticked.get(i) {
                    Some(true) => "[x] ",
                    Some(false) => "[ ] ",
                    None => "",
                };
                let text =
                    if names && i == 0 { format!("{o} (clears)") } else { format!("{tick}{o}") };
                let line = Line::raw(format!(" {text} "));
                if i == pick.at {
                    line.style(Style::new().add_modifier(Modifier::REVERSED))
                } else {
                    line
                }
            })
            .collect();
        let wide = lines.iter().map(Line::width).max().unwrap_or(0) + 2;
        let left = rows.x + u16::try_from(width + 2).unwrap_or(0);
        let below = rows.y + 2 + u16::try_from(self.selected).unwrap_or(0);
        let mut area = Rect {
            x: left,
            y: below,
            width: u16::try_from(wide).unwrap_or(u16::MAX).min(rows.right().saturating_sub(left)),
            height: u16::try_from(lines.len() + 2).unwrap_or(u16::MAX).min(rows.height),
        };
        // above the row when there is no room below it
        if area.bottom() > rows.bottom() {
            area.y = below.saturating_sub(area.height + 1).max(rows.y);
        }
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines)
                .block(Block::bordered().border_style(Style::new().fg(Color::Cyan))),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(path: &str) -> &'static Field {
        CONFIG_FIELDS.iter().chain(CHEST_FIELDS).find(|f| f.path.join(".") == path).unwrap()
    }

    #[test]
    fn fields_set_in_the_template_give_a_valid_config_with_its_comments() {
        let mut doc: DocumentMut = crate::config::TEMPLATE.parse().unwrap();
        set(&mut doc, field("projects_dir"), "~/Projekte").unwrap();
        set(&mut doc, field("nix.packages"), "just, jq,").unwrap();
        set(&mut doc, field("gitlab.visibility"), "internal").unwrap();
        set(&mut doc, field("node.default"), "24").unwrap();
        let text = doc.to_string();
        let config: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(config.projects_dir.as_deref(), Some("~/Projekte"));
        assert_eq!(config.nix.packages, Some(vec!["just".into(), "jq".into()]));
        assert_eq!(config.gitlab.visibility.as_deref(), Some("internal"));
        assert_eq!(config.node.default, Some(24));
        // the comments of the template stay
        assert!(text.contains("# Skills for coding agents"));
        assert_eq!(get(&doc, field("nix.packages")).as_deref(), Some("just, jq"));
        // empty removes the key again
        set(&mut doc, field("projects_dir"), " ").unwrap();
        assert_eq!(get(&doc, field("projects_dir")), None);
    }

    /// The line of `text` that starts with `start`.
    fn line<'a>(text: &'a str, start: &str) -> &'a str {
        text.lines().find(|l| l.starts_with(start)).unwrap_or_default()
    }

    #[test]
    fn a_set_value_takes_the_place_of_its_commented_line() {
        let mut doc: DocumentMut = crate::config::TEMPLATE.parse().unwrap();
        set(&mut doc, field("projects_dir"), "~/Projekte").unwrap();
        set(&mut doc, field("gitlab.visibility"), "internal").unwrap();
        set(&mut doc, field("gitlab.host"), "git.example.com").unwrap();
        set(&mut doc, field("python.default"), "3.12").unwrap();
        let text = doc.to_string();
        assert!(text.starts_with("# luggage — user config"), "{text}");
        assert!(
            text.contains(
                "# Where `luggage new` clones or creates projects\nprojects_dir = \"~/Projekte\"\n"
            ),
            "{text}"
        );
        assert!(text.contains("[gitlab]\nhost = \"git.example.com\"   # if unset"), "{text}");
        assert!(
            text.contains("# if unset: default host of glab\n# group = \"my-group\""),
            "{text}"
        );
        assert!(
            line(&text, "visibility = \"internal\"").ends_with("# private | internal | public")
        );
        assert!(line(&text, "default = \"3.12\"").contains("# if .python-version"), "{text}");
        // the commented lines are gone where a value took their place
        for gone in [
            "# projects_dir",
            "# host",
            "# visibility = \"private\"        # private | internal | public\n\n[github]",
        ] {
            assert!(!text.contains(gone), "{gone} in {text}");
        }
        toml::from_str::<crate::config::Config>(&text).unwrap();
        // back to the default: the line is commented out again, with its note and comments
        set(&mut doc, field("gitlab.host"), "").unwrap();
        set(&mut doc, field("projects_dir"), "").unwrap();
        let text = doc.to_string();
        assert!(text.contains("[gitlab]\n# host = \"git.example.com\"   # if unset"), "{text}");
        assert!(
            text.contains("# Where `luggage new` clones or creates projects\n# projects_dir = \"~/Projekte\"\n"),
            "{text}"
        );
        let config: crate::config::Config = toml::from_str(&text).unwrap();
        assert!(config.projects_dir.is_none() && config.gitlab.host.is_none());
        assert_eq!(config.gitlab.visibility.as_deref(), Some("internal"));
    }

    #[test]
    fn a_changed_value_keeps_its_place_and_note() {
        let mut doc: DocumentMut =
            "[php]\nmemory_limit = \"1G\"   # big\ndefault = \"8.3\"\n".parse().unwrap();
        set(&mut doc, field("php.memory_limit"), "2G").unwrap();
        assert_eq!(doc.to_string(), "[php]\nmemory_limit = \"2G\"   # big\ndefault = \"8.3\"\n");
    }

    #[test]
    fn wrong_input_is_refused() {
        let mut doc = DocumentMut::new();
        assert!(set(&mut doc, field("node.default"), "twenty").is_err());
        assert!(set(&mut doc, field("mode"), "docker").is_err());
        assert_eq!(doc.to_string(), "");
    }

    #[test]
    fn a_new_chest_definition_from_the_template_is_valid() {
        let mut doc: DocumentMut = crate::chest::TEMPLATE.parse().unwrap();
        set(&mut doc, field("mode"), "vm").unwrap();
        set(&mut doc, field("agents"), "claude").unwrap();
        set(&mut doc, field("network"), "github.com").unwrap();
        let def: crate::chest::Definition = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(def.mode, crate::chest::Mode::Vm);
        assert_eq!(def.agents, ["claude"]);
        assert_eq!(def.network, Some(vec!["github.com".into()]));
    }

    /// Selects the field at `path` in `form`.
    fn select(form: &mut Form, path: &str) {
        form.selected = form.fields.iter().position(|f| f.path.join(".") == path).unwrap();
    }

    #[test]
    fn a_dropdown_picks_one_choice_or_the_default() {
        let mut form =
            Form::open(Path::new("/x/config.toml"), crate::config::TEMPLATE, CONFIG_FIELDS)
                .unwrap();
        select(&mut form, "gitlab.visibility");
        form.key(KeyCode::Enter);
        assert_eq!(form.pick.as_ref().map(|p| p.at), Some(0));
        form.key(KeyCode::Down);
        form.key(KeyCode::Down);
        form.key(KeyCode::Enter);
        assert!(form.pick.is_none());
        assert_eq!(get(&form.doc, field("gitlab.visibility")).as_deref(), Some("internal"));
        // it opens at the value; Esc leaves it as it is
        form.key(KeyCode::Enter);
        assert_eq!(form.pick.as_ref().map(|p| p.at), Some(2));
        form.key(KeyCode::Down);
        form.key(KeyCode::Esc);
        assert_eq!(get(&form.doc, field("gitlab.visibility")).as_deref(), Some("internal"));
        form.key(KeyCode::Enter);
        form.key(KeyCode::Up);
        form.key(KeyCode::Up);
        form.key(KeyCode::Enter);
        assert_eq!(get(&form.doc, field("gitlab.visibility")), None);
    }

    #[test]
    fn a_dropdown_of_names_takes_the_ticked_ones() {
        let mut form =
            Form::open(Path::new("/x/chest.toml"), crate::chest::TEMPLATE, CHEST_FIELDS).unwrap();
        select(&mut form, "agents");
        form.key(KeyCode::Enter);
        form.key(KeyCode::Down);
        form.key(KeyCode::Enter);
        let first = crate::agents::NAMES[0];
        assert_eq!(get(&form.doc, field("agents")).as_deref(), Some(first));
        form.key(KeyCode::Enter);
        assert_eq!(form.pick.as_ref().map(|p| p.ticked[1]), Some(true));
        form.key(KeyCode::Down);
        form.key(KeyCode::Char(' '));
        form.key(KeyCode::Enter);
        let both = crate::agents::NAMES[..2].join(", ");
        assert_eq!(get(&form.doc, field("agents")), Some(both));
        form.key(KeyCode::Enter);
        form.key(KeyCode::Up);
        form.key(KeyCode::Up);
        form.key(KeyCode::Char(' '));
        form.key(KeyCode::Enter);
        assert_eq!(get(&form.doc, field("agents")), None);
    }

    #[test]
    fn keys_edit_a_text_field_and_esc_asks_before_it_discards() {
        let mut form =
            Form::open(Path::new("/x/config.toml"), crate::config::TEMPLATE, CONFIG_FIELDS)
                .unwrap();
        form.key(KeyCode::Enter);
        for c in "~/src".chars() {
            form.key(KeyCode::Char(c));
        }
        form.key(KeyCode::Enter);
        assert_eq!(get(&form.doc, field("projects_dir")).as_deref(), Some("~/src"));
        assert_eq!(form.key(KeyCode::Char('E')), Action::Stay);
        assert_eq!(form.key(KeyCode::Esc), Action::Stay);
        assert_eq!(form.key(KeyCode::Esc), Action::Close);
        assert_eq!(form.key(KeyCode::Char('s')), Action::Save);
    }
}
