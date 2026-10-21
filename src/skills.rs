//! Skills for coding agents (Claude Code, Codex) from sources in `[skills]` of the config.
//!
//! Git sources go into the Nix store at a locked commit; `roots/<source>` in `$XDG_DATA_HOME/luggage/skills`
//! points to each source. The agents' skill directories get one link per skill into `roots`, so luggage
//! knows its own links by their target and never touches other entries.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::SkillSource;
use crate::{expand_tilde, home, info};

/// The skill directory of each agent, below `$HOME`, and the directory that shows the agent is installed.
const AGENTS: [(&str, &str, &str); 2] =
    [("claude", ".claude", ".claude/skills"), ("codex", ".codex", ".agents/skills")];

/// A git source as it was fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Locked {
    /// The Nix URL from the config, without the commit
    url: String,
    /// The commit
    rev: String,
    /// The source in the Nix store
    store_path: String,
}

/// What `sync` changed in one skill directory.
#[derive(Debug, Default, PartialEq, Eq)]
struct Linked {
    /// Links created or pointed elsewhere
    changed: usize,
    /// Links removed, since their skill is gone
    removed: usize,
    /// Names that exist but were not created by luggage
    skipped: Vec<String>,
}

/// Where luggage keeps the lock and the roots.
fn data_dir() -> PathBuf {
    crate::chest::xdg("XDG_DATA_HOME", || home().join(".local/share")).join("luggage/skills")
}

/// The Nix URL of a git address: `git@host:path` and `https://` become `git+ssh://` and `git+https://`.
fn nix_url(git: &str, git_ref: Option<&str>) -> String {
    let url = if git.starts_with("git+") || git.starts_with("file:") {
        git.to_owned()
    } else if let Some(rest) = git.strip_prefix("ssh://").or(git.strip_prefix("https://")) {
        let scheme = if git.starts_with("ssh://") { "ssh" } else { "https" };
        format!("git+{scheme}://{rest}")
    } else if let Some((host, path)) = git.split_once(':').filter(|(h, _)| !h.contains('/')) {
        format!("git+ssh://{host}/{path}")
    } else {
        format!("git+file://{}", expand_tilde(git).display())
    };
    match git_ref {
        Some(r) => format!("{url}{}ref={r}", if url.contains('?') { '&' } else { '?' }),
        None => url,
    }
}

/// The URL that fetches exactly the locked commit.
fn locked_url(l: &Locked) -> String {
    format!("{}{}rev={}", l.url, if l.url.contains('?') { '&' } else { '?' }, l.rev)
}

/// Fetches `url` into the Nix store with `nix flake prefetch`.
#[cfg(not(tarpaulin_include))]
fn prefetch(url: &str) -> Result<(String, String)> {
    let out = Command::new("nix")
        .args(["flake", "prefetch", "--json", url])
        .stderr(Stdio::inherit())
        .output()
        .context("cannot start nix")?;
    if !out.status.success() {
        bail!("cannot fetch {url}");
    }
    parse_prefetch(&String::from_utf8_lossy(&out.stdout))
        .with_context(|| format!("cannot fetch {url}"))
}

/// The commit and store path from the output of `nix flake prefetch --json`.
fn parse_prefetch(json: &str) -> Result<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(json)?;
    let rev = v["locked"]["rev"].as_str().context("nix names no commit")?;
    let path = v["storePath"].as_str().context("nix names no store path")?;
    Ok((rev.to_owned(), path.to_owned()))
}

/// Points `root` to `store_path` and keeps it from the garbage collector.
#[cfg(not(tarpaulin_include))]
fn add_root(root: &Path, store_path: &str) -> Result<()> {
    let status = Command::new("nix")
        .args(["build", "--out-link"])
        .arg(root)
        .arg(store_path)
        .status()
        .context("cannot start nix")?;
    if !status.success() {
        bail!("cannot keep {store_path} in the Nix store");
    }
    Ok(())
}

/// Reads the lock; a missing file means nothing is locked.
fn read_lock(file: &Path) -> Result<BTreeMap<String, Locked>> {
    match fs::read_to_string(file) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("{} is invalid", file.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", file.display())),
    }
}

/// Writes the lock next to its old version and then swaps it in.
fn write_lock(file: &Path, lock: &BTreeMap<String, Locked>) -> Result<()> {
    let tmp = file.with_extension("tmp");
    fs::write(&tmp, serde_json::to_string_pretty(lock)? + "\n")?;
    fs::rename(&tmp, file)?;
    Ok(())
}

/// Brings `roots/<name>` of every source up to date and drops the roots of removed sources.
/// Git sources are fetched if `update` names them (empty: all), if they are new or if their URL changed.
#[cfg(not(tarpaulin_include))]
fn fetch_sources(
    dir: &Path,
    sources: &BTreeMap<String, SkillSource>,
    update: Option<&[String]>,
) -> Result<()> {
    let roots = dir.join("roots");
    fs::create_dir_all(&roots).with_context(|| format!("cannot create {}", roots.display()))?;
    let lock_file = dir.join("lock.json");
    let old = read_lock(&lock_file)?;
    let mut lock = BTreeMap::new();
    for (name, source) in sources {
        let root = roots.join(name);
        let Some(git) = &source.git else {
            let path = expand_tilde(source.path.as_deref().unwrap_or_default());
            let path = fs::canonicalize(&path)
                .ok()
                .filter(|p| p.is_dir())
                .with_context(|| format!("skills.{name}: {} is not a directory", path.display()))?;
            let _ = fs::remove_file(&root);
            symlink(&path, &root)?;
            continue;
        };
        let url = nix_url(git, source.git_ref.as_deref());
        let wanted = update.is_some_and(|u| u.is_empty() || u.contains(name));
        let locked = match old.get(name).filter(|l| l.url == url && !wanted) {
            Some(l) if Path::new(&l.store_path).exists() => l.clone(),
            // the garbage collector took it: fetch the same commit again
            Some(l) => Locked { store_path: prefetch(&locked_url(l))?.1, ..l.clone() },
            None => {
                info(&format!("fetching skills.{name} from {git}"));
                let (rev, store_path) = prefetch(&url)?;
                Locked { url, rev, store_path }
            }
        };
        if fs::read_link(&root).ok().as_deref() != Some(Path::new(&locked.store_path)) {
            let _ = fs::remove_file(&root);
            add_root(&root, &locked.store_path)?;
        }
        lock.insert(name.clone(), locked);
    }
    drop_roots(&roots, sources)?;
    write_lock(&lock_file, &lock)
}

/// Removes the roots of sources that are no longer in the config.
fn drop_roots(roots: &Path, sources: &BTreeMap<String, SkillSource>) -> Result<()> {
    for entry in fs::read_dir(roots)? {
        let entry = entry?;
        if !entry.file_name().to_str().is_some_and(|n| sources.contains_key(n)) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// The skills of each source as name and directory below `roots`: each folder with a `SKILL.md`.
///
/// # Errors
/// If two sources have a skill of the same name.
fn find_skills(
    roots: &Path,
    sources: &BTreeMap<String, SkillSource>,
) -> Result<BTreeMap<String, (String, PathBuf)>> {
    let mut skills: BTreeMap<String, (String, PathBuf)> = BTreeMap::new();
    for (source, s) in sources {
        let base = roots.join(source).join(s.dir.as_deref().unwrap_or_default());
        let mut names: Vec<String> = fs::read_dir(&base)
            .with_context(|| format!("skills.{source}: cannot read {}", base.display()))?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.') && base.join(n).join("SKILL.md").is_file())
            .collect();
        names.sort();
        if let Some(only) = &s.only {
            for missing in only.iter().filter(|o| !names.contains(o)) {
                info(&format!("warning: skills.{source} has no skill {missing}"));
            }
            names.retain(|n| only.contains(n));
        }
        for name in names {
            if let Some((other, _)) = skills.get(&name) {
                bail!(
                    "skills.{other} and skills.{source} both have a skill {name}; leave one out with `only`"
                );
            }
            skills.insert(name.clone(), (source.clone(), base.join(&name)));
        }
    }
    Ok(skills)
}

/// Links each skill into `target` and removes luggage's links to skills that are gone.
/// A link is luggage's if it points below `roots`; everything else in `target` stays as it is.
fn link_skills(
    target: &Path,
    roots: &Path,
    skills: &BTreeMap<String, (String, PathBuf)>,
) -> Result<Linked> {
    if target.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        let to = fs::read_link(target).unwrap_or_default();
        bail!(
            "{} is a link to {}, so another tool manages it; move it away first",
            target.display(),
            to.display()
        );
    }
    fs::create_dir_all(target).with_context(|| format!("cannot create {}", target.display()))?;
    let mut done = Linked::default();
    for entry in fs::read_dir(target)? {
        let entry = entry?;
        let Ok(to) = fs::read_link(entry.path()) else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if to.starts_with(roots) && !skills.contains_key(&name) {
            fs::remove_file(entry.path())?;
            done.removed += 1;
        }
    }
    for (name, (_, dir)) in skills {
        let link = target.join(name);
        match fs::read_link(&link) {
            Ok(to) if to == *dir => {}
            Ok(to) if to.starts_with(roots) => {
                fs::remove_file(&link)?;
                symlink(dir, &link)?;
                done.changed += 1;
            }
            _ if link.symlink_metadata().is_ok() => done.skipped.push(name.clone()),
            _ => {
                symlink(dir, &link)?;
                done.changed += 1;
            }
        }
    }
    Ok(done)
}

/// The skill directories of the agents installed for the user.
fn agent_dirs(home: &Path) -> Vec<(&'static str, PathBuf)> {
    AGENTS
        .iter()
        .filter(|(_, installed, _)| home.join(installed).is_dir())
        .map(|(agent, _, dir)| (*agent, home.join(dir)))
        .collect()
}

/// `luggage skills sync` and `update`: fetches the sources and links their skills for each agent.
///
/// # Errors
/// If a source cannot be fetched, two skills share a name or a skill directory belongs to another tool.
#[cfg(not(tarpaulin_include))]
pub fn sync(sources: &BTreeMap<String, SkillSource>, update: Option<&[String]>) -> Result<()> {
    if let Some(unknown) = update.into_iter().flatten().find(|n| !sources.contains_key(*n)) {
        bail!("there is no skills.{unknown} in the config");
    }
    let dir = data_dir();
    fetch_sources(&dir, sources, update)?;
    let roots = dir.join("roots");
    let skills = find_skills(&roots, sources)?;
    let agents = agent_dirs(&home());
    if agents.is_empty() {
        info("no agent found (neither ~/.claude nor ~/.codex)");
    }
    for (agent, target) in agents {
        let done = link_skills(&target, &roots, &skills)?;
        info(&format!(
            "{agent}: {} skills in {}, {} linked, {} removed",
            skills.len(),
            target.display(),
            done.changed,
            done.removed
        ));
        for name in done.skipped {
            info(&format!(
                "warning: skipped {}: it exists and luggage did not create it",
                target.join(name).display()
            ));
        }
    }
    Ok(())
}

/// `luggage skills`: prints the sources with their commit and the skills each agent sees from luggage.
///
/// # Errors
/// If the lock cannot be read.
#[cfg(not(tarpaulin_include))]
pub fn status(sources: &BTreeMap<String, SkillSource>) -> Result<()> {
    if sources.is_empty() {
        println!(
            "No skill sources; add them as [skills.<name>] to {}",
            crate::config::path().display()
        );
        return Ok(());
    }
    let dir = data_dir();
    let lock = read_lock(&dir.join("lock.json"))?;
    let roots = dir.join("roots");
    for (name, s) in sources {
        let from = match (&s.git, &s.path) {
            (Some(git), _) => match lock.get(name) {
                Some(l) => format!("{git} @ {}", l.rev.get(..12).unwrap_or(&l.rev)),
                None => format!("{git} (not fetched yet: luggage skills sync)"),
            },
            (None, path) => path.clone().unwrap_or_default(),
        };
        println!("{name}: {from}");
    }
    if let Some(name) = sources.keys().find(|n| !roots.join(n).exists()) {
        println!("skills.{name} is not synced yet: luggage skills sync");
        return Ok(());
    }
    match find_skills(&roots, sources) {
        Ok(skills) => {
            for (agent, target) in agent_dirs(&home()) {
                let linked = skills
                    .iter()
                    .filter(|(n, (_, d))| fs::read_link(target.join(n)).ok().as_ref() == Some(d));
                println!(
                    "{agent}: {} of {} skills linked in {}",
                    linked.count(),
                    skills.len(),
                    target.display()
                );
            }
        }
        Err(e) => println!("Skills: {e:#}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(url: &str) -> SkillSource {
        SkillSource { git: Some(url.into()), ..SkillSource::default() }
    }

    #[test]
    fn git_addresses_become_nix_urls() {
        assert_eq!(
            nix_url("git@gitlab.example.com:dev/skills.git", None),
            "git+ssh://git@gitlab.example.com/dev/skills.git"
        );
        assert_eq!(
            nix_url("https://github.com/a/b", Some("v2")),
            "git+https://github.com/a/b?ref=v2"
        );
        assert_eq!(nix_url("ssh://git@h:2222/a.git", None), "git+ssh://git@h:2222/a.git");
        assert_eq!(
            nix_url("git+https://h/a?dir=x", Some("main")),
            "git+https://h/a?dir=x&ref=main"
        );
        assert_eq!(nix_url("/srv/skills", None), "git+file:///srv/skills");
        let l =
            Locked { url: "git+ssh://h/a".into(), rev: "abc".into(), store_path: String::new() };
        assert_eq!(locked_url(&l), "git+ssh://h/a?rev=abc");
    }

    #[test]
    fn reads_the_commit_from_nix() {
        let json = r#"{"hash":"sha256-x","locked":{"rev":"4569","type":"git"},"storePath":"/nix/store/0k-source"}"#;
        assert_eq!(parse_prefetch(json).unwrap(), ("4569".into(), "/nix/store/0k-source".into()));
        assert!(parse_prefetch(r#"{"locked":{}}"#).is_err());
    }

    /// A roots directory with source `a` (skills x, y in `skills/`) and `b` (skill z at the top).
    fn roots() -> (tempfile::TempDir, BTreeMap<String, SkillSource>) {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["a/skills/x", "a/skills/y", "b/z"] {
            fs::create_dir_all(tmp.path().join(d)).unwrap();
            fs::write(tmp.path().join(d).join("SKILL.md"), "---\nname: s\n---\n").unwrap();
        }
        fs::create_dir_all(tmp.path().join("a/skills/no-skill")).unwrap();
        let mut sources = BTreeMap::new();
        sources.insert("a".to_owned(), SkillSource { dir: Some("skills".into()), ..git("h:a") });
        sources.insert("b".to_owned(), git("h:b"));
        (tmp, sources)
    }

    #[test]
    fn finds_skills_and_refuses_doubles() {
        let (tmp, mut sources) = roots();
        let skills = find_skills(tmp.path(), &sources).unwrap();
        assert_eq!(skills.keys().collect::<Vec<_>>(), ["x", "y", "z"]);
        assert_eq!(skills["x"], ("a".into(), tmp.path().join("a/skills/x")));
        sources.get_mut("a").unwrap().only = Some(vec!["y".into()]);
        assert_eq!(
            find_skills(tmp.path(), &sources).unwrap().keys().collect::<Vec<_>>(),
            ["y", "z"]
        );
        fs::create_dir_all(tmp.path().join("b/y")).unwrap();
        fs::write(tmp.path().join("b/y/SKILL.md"), "").unwrap();
        let e = find_skills(tmp.path(), &sources).unwrap_err();
        assert!(e.to_string().contains("skills.a and skills.b both have a skill y"), "{e}");
    }

    #[test]
    fn links_skills_and_leaves_other_entries_alone() {
        let (tmp, mut sources) = roots();
        let roots = tmp.path();
        let target = tempfile::tempdir().unwrap();
        let t = target.path().join("skills");
        fs::create_dir_all(t.join("own")).unwrap();
        symlink("/elsewhere", t.join("other")).unwrap();
        symlink("/elsewhere", t.join("z")).unwrap();

        let done = link_skills(&t, roots, &find_skills(roots, &sources).unwrap()).unwrap();
        assert_eq!(done, Linked { changed: 2, removed: 0, skipped: vec!["z".into()] });
        assert_eq!(fs::read_link(t.join("x")).unwrap(), roots.join("a/skills/x"));
        // a second run changes nothing
        let again = link_skills(&t, roots, &find_skills(roots, &sources).unwrap()).unwrap();
        assert_eq!(again, Linked { changed: 0, removed: 0, skipped: vec!["z".into()] });

        // a skill that is gone loses its link, foreign entries stay
        sources.get_mut("a").unwrap().only = Some(vec!["y".into()]);
        let done = link_skills(&t, roots, &find_skills(roots, &sources).unwrap()).unwrap();
        assert_eq!(done.removed, 1);
        assert!(t.join("x").symlink_metadata().is_err());
        assert!(t.join("own").is_dir());
        assert_eq!(fs::read_link(t.join("other")).unwrap(), Path::new("/elsewhere"));
        assert_eq!(fs::read_link(t.join("z")).unwrap(), Path::new("/elsewhere"));
    }

    #[test]
    fn a_skill_directory_that_is_a_link_belongs_to_another_tool() {
        let (tmp, sources) = roots();
        let target = tempfile::tempdir().unwrap();
        let t = target.path().join("skills");
        symlink(target.path(), &t).unwrap();
        let e =
            link_skills(&t, tmp.path(), &find_skills(tmp.path(), &sources).unwrap()).unwrap_err();
        assert!(e.to_string().contains("another tool manages it"), "{e}");
    }

    #[test]
    fn roots_of_removed_sources_go_away() {
        let tmp = tempfile::tempdir().unwrap();
        for n in ["a", "local", "git"] {
            symlink("/elsewhere", tmp.path().join(n)).unwrap();
        }
        let mut sources = BTreeMap::new();
        sources.insert("a".to_owned(), git("h:a"));
        drop_roots(tmp.path(), &sources).unwrap();
        let mut left: Vec<_> =
            fs::read_dir(tmp.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(left, ["a"]);
    }

    #[test]
    fn only_installed_agents_get_skills() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(agent_dirs(home.path()), []);
        fs::create_dir(home.path().join(".codex")).unwrap();
        assert_eq!(agent_dirs(home.path()), [("codex", home.path().join(".agents/skills"))]);
    }

    #[test]
    fn the_lock_survives_a_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("lock.json");
        assert_eq!(read_lock(&file).unwrap(), BTreeMap::new());
        let mut lock = BTreeMap::new();
        let l = Locked {
            url: "git+ssh://h/a".into(),
            rev: "abc".into(),
            store_path: "/nix/store/x".into(),
        };
        lock.insert("a".to_owned(), l);
        write_lock(&file, &lock).unwrap();
        assert_eq!(read_lock(&file).unwrap(), lock);
    }
}
