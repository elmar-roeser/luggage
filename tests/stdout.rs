//! luggage's own messages must stay off stdout where it is transparent.
#![allow(unused_crate_dependencies)] // the dependencies belong to the binary, not to this test

use std::fs;
use std::process::Command;

/// direnv evaluates the hook's stdout, so a warning about a broken chest.toml must go to stderr.
#[test]
fn hook_reports_on_stderr_only() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let project = tmp.path().join("demo");
    fs::create_dir_all(project.join(".luggage"))?;
    fs::write(project.join("flake.nix"), "{}")?;
    fs::write(project.join(".luggage/chest.toml"), "mode = [")?;
    let out = Command::new(env!("CARGO_BIN_EXE_luggage"))
        .arg("hook")
        .current_dir(&project)
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("XDG_RUNTIME_DIR", tmp.path().join("run"))
        .env("HOME", tmp.path())
        .output()?;
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert!(String::from_utf8_lossy(&out.stderr).contains("luggage: warning:"));
    Ok(())
}
