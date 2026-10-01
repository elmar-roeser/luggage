//! `luggage down` in bubblewrap mode with a service that ignores SIGTERM.
#![allow(unused_crate_dependencies)] // the dependencies belong to the binary, not to this test

use std::fs;
use std::path::Path;
use std::process::Command;

const FLAKE: &str = r#"{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  outputs = { nixpkgs, ... }: {
    devShells.x86_64-linux.default = nixpkgs.legacyPackages.x86_64-linux.mkShell { };
  };
}
"#;

const CHEST: &str = r#"[services.polite]
command = "trap 'echo TERM >> polite.txt; exit 0' TERM; while :; do sleep 0.2; done"

[services.stubborn]
command = "trap 'echo TERM >> stubborn.txt' TERM; while :; do sleep 0.2; done"
"#;

/// The SIGTERM to all processes and the final SIGKILL must stop a chest that process-compose cannot stop.
/// Needs nix and bubblewrap, so run it with `cargo test -- --ignored`.
#[test]
#[ignore = "needs nix and bubblewrap"]
fn down_stops_a_stubborn_service() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let project = tmp.path().join("down-test");
    fs::create_dir_all(project.join(".luggage"))?;
    fs::write(project.join("flake.nix"), FLAKE)?;
    fs::write(project.join(".luggage/chest.toml"), CHEST)?;
    let luggage = |cmd: &str| {
        Command::new(env!("CARGO_BIN_EXE_luggage"))
            .arg(cmd)
            .current_dir(&project)
            .env("XDG_CONFIG_HOME", tmp.path().join("config"))
            .env("XDG_DATA_HOME", tmp.path().join("data"))
            .env("XDG_RUNTIME_DIR", tmp.path().join("run"))
            .output()
    };
    let up = luggage("up")?;
    assert!(up.status.success(), "{}", String::from_utf8_lossy(&up.stderr));
    let pid = fs::read_to_string(tmp.path().join("run/luggage/down-test/chest.pid"))?;
    let down = luggage("down")?;
    assert!(down.status.success(), "{}", String::from_utf8_lossy(&down.stderr));
    assert!(!Path::new(&format!("/proc/{}", pid.trim())).exists(), "chest still running");
    // polite stops with the first SIGTERM; stubborn also gets the second one from the fallback
    assert_eq!(fs::read_to_string(project.join("polite.txt"))?, "TERM\n");
    assert_eq!(fs::read_to_string(project.join("stubborn.txt"))?, "TERM\nTERM\n");
    Ok(())
}
