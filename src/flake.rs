//! Erzeugt flake.nix und .envrc aus den erkannten Sprachen.

use crate::detect::{Node, Php, Python};

/// Vorlage für flake.nix; die `@...@`-Platzhalter füllt `render`.
const FLAKE: &str = r#"{
  description = "@NAME@ — Dev-Umgebung (erzeugt von luggage)";

  inputs.nixpkgs.url = "@NIXPKGS@";

  outputs = { nixpkgs, ... }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
@LETS@    in {
      devShells.x86_64-linux.default = pkgs.mkShell {
        packages = [ @PACKAGES@ ];@ENV@
      };
    };
}
"#;

/// `let`-Block für PHP mit Extensions und `memory_limit`.
const PHP_LET: &str = r#"      # ext-* aus composer.json und Config; aktive werden übersprungen, fehlende gewarnt
      phpExtensions = [ @EXTS@ ];
      php = pkgs.@ATTR@.buildEnv {
        extensions = { enabled, all }:
          let
            have = map (e: e.extensionName) enabled;
            wanted = builtins.filter (n: !(builtins.elem n have)) phpExtensions;
            known = builtins.filter (n: all ? ${n} || builtins.trace "luggage: PHP-Extension ${n} fehlt in nixpkgs" false) wanted;
          in enabled ++ map (n: all.${n}) known;
        extraConfig = "memory_limit = @MEMORY@";
      };
"#;

/// `let`-Block für Python.
const PYTHON_LET: &str = "      python = pkgs.@ATTR@;\n";

/// Alles, was in die flake.nix kommt.
pub struct Plan<'a> {
    /// Projektname für `description`
    pub name: &'a str,
    /// URL des nixpkgs-Inputs
    pub nixpkgs: &'a str,
    /// PHP, falls erkannt
    pub php: Option<&'a Php>,
    /// `memory_limit` für PHP
    pub memory_limit: &'a str,
    /// Node, falls erkannt
    pub node: Option<&'a Node>,
    /// Python, falls erkannt
    pub python: Option<&'a Python>,
    /// Rust-Toolchain aus nixpkgs aufnehmen
    pub rust: bool,
    /// Zusätzliche nixpkgs-Pakete aus der Config
    pub extra_packages: &'a [String],
}

/// Erzeugt den Inhalt der flake.nix.
pub fn render(plan: &Plan) -> String {
    let mut lets = String::new();
    let mut packages: Vec<String> = Vec::new();
    let mut env: Vec<String> = Vec::new();

    if let Some(php) = plan.php {
        let (major, minor) = php.version;
        let exts: Vec<String> = php.exts.iter().map(|e| format!("\"{e}\"")).collect();
        lets.push_str(
            &PHP_LET
                .replace("@ATTR@", &format!("php{major}{minor}"))
                .replace("@EXTS@", &exts.join(" "))
                .replace("@MEMORY@", plan.memory_limit),
        );
        packages.extend(["php".into(), "php.packages.composer".into()]);
        // herd-lite o.ä. setzt PHP_INI_SCAN_DIR global und würde fremde .ini laden
        env.push("shellHook = \"unset PHP_INI_SCAN_DIR\";".into());
    }
    if let Some(node) = plan.node {
        packages.push(
            node.version.map_or_else(|| "pkgs.nodejs".into(), |v| format!("pkgs.nodejs_{v}")),
        );
        if let Some(tool) = node.tool {
            packages.push(format!("pkgs.{tool}"));
        }
    }
    if let Some(python) = plan.python {
        let attr =
            python.version.map_or_else(|| "python3".into(), |(a, b)| format!("python{a}{b}"));
        lets.push_str(&PYTHON_LET.replace("@ATTR@", &attr));
        packages.push("python".into());
        match python.tool {
            Some("uv") => {
                packages.push("pkgs.uv".into());
                // uv soll das Nix-Python nehmen und keins herunterladen
                env.push("UV_PYTHON = \"${python}/bin/python\";".into());
                env.push("UV_PYTHON_DOWNLOADS = \"never\";".into());
            }
            Some("poetry") => {
                packages.push("pkgs.poetry".into());
                env.push("POETRY_VIRTUALENVS_IN_PROJECT = \"true\";".into());
            }
            _ => {}
        }
    }
    if plan.rust {
        packages.extend(
            ["rustc", "cargo", "clippy", "rustfmt", "rust-analyzer"].map(|p| format!("pkgs.{p}")),
        );
        // rust-analyzer braucht die Quellen der Standardbibliothek
        env.push("RUST_SRC_PATH = \"${pkgs.rustPlatform.rustLibSrc}\";".into());
    }
    packages.extend(plan.extra_packages.iter().map(|p| format!("pkgs.{p}")));

    let env: String = env.iter().flat_map(|line| ["\n        ", line.as_str()]).collect();
    FLAKE
        .replace("@NAME@", plan.name)
        .replace("@NIXPKGS@", plan.nixpkgs)
        .replace("@LETS@", &lets)
        .replace("@PACKAGES@", &packages.join(" "))
        .replace("@ENV@", &env)
}

/// .envrc-Zusatz für Python: aktiviert die `.venv` im Projekt.
const ENVRC_VENV: &str = "
# Python: .venv im Projekt, von uv/poetry angelegt
export VIRTUAL_ENV=\"$PWD/.venv\"
PATH_add .venv/bin
";

/// .envrc-Zusatz für poetry: legt die `.venv` mit dem Python aus dem PATH an.
/// poetry 2 nimmt sonst sein eigenes Python statt des Nix-Pythons.
const ENVRC_POETRY: &str =
    "if [[ ! -d .venv ]]; then poetry env use -q \"$(command -v python)\"; fi
";

/// Erzeugt den Inhalt der .envrc.
pub fn render_envrc(python: Option<&Python>) -> String {
    let mut envrc = String::from("use flake\n");
    if let Some(python) = python {
        envrc.push_str(ENVRC_VENV);
        if python.tool == Some("poetry") {
            envrc.push_str(ENVRC_POETRY);
        }
    }
    envrc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(name: &str) -> Plan<'_> {
        Plan {
            name,
            nixpkgs: "github:NixOS/nixpkgs/nixos-26.05",
            php: None,
            memory_limit: "512M",
            node: None,
            python: None,
            rust: false,
            extra_packages: &[],
        }
    }

    #[test]
    fn php_and_node() {
        let php = Php { version: (8, 3), source: "", exts: vec!["redis".into()] };
        let node = Node { version: Some(22), source: "", tool: Some("pnpm") };
        let flake = render(&Plan {
            php: Some(&php),
            node: Some(&node),
            memory_limit: "1G",
            ..plan("demo")
        });
        assert!(flake.contains("pkgs.php83.buildEnv"));
        assert!(flake.contains(r#"phpExtensions = [ "redis" ];"#));
        assert!(flake.contains("memory_limit = 1G"));
        assert!(
            flake.contains("packages = [ php php.packages.composer pkgs.nodejs_22 pkgs.pnpm ];")
        );
        assert!(flake.contains("unset PHP_INI_SCAN_DIR"));
    }

    #[test]
    fn python_with_uv_and_rust_and_extras() {
        let python = Python { version: Some((3, 12)), source: "", tool: Some("uv") };
        let extra = ["just".to_string()];
        let flake = render(&Plan {
            python: Some(&python),
            rust: true,
            extra_packages: &extra,
            ..plan("x")
        });
        assert!(flake.contains("python = pkgs.python312;"));
        assert!(flake.contains("packages = [ python pkgs.uv pkgs.rustc pkgs.cargo pkgs.clippy pkgs.rustfmt pkgs.rust-analyzer pkgs.just ];"));
        assert!(flake.contains("UV_PYTHON = \"${python}/bin/python\";"));
        assert!(flake.contains("RUST_SRC_PATH"));
    }

    #[test]
    fn empty_shell() {
        let flake = render(&plan("leer"));
        assert!(flake.contains("packages = [  ];"));
        assert!(!flake.contains("shellHook"));
        assert!(!flake.contains('@'));
    }

    #[test]
    fn envrc_activates_venv_only_for_python() {
        assert_eq!(render_envrc(None), "use flake\n");
        let uv = Python { version: None, source: "", tool: Some("uv") };
        assert!(render_envrc(Some(&uv)).contains("PATH_add .venv/bin"));
        assert!(!render_envrc(Some(&uv)).contains("poetry env use"));
        let poetry = Python { tool: Some("poetry"), ..uv };
        assert!(render_envrc(Some(&poetry)).contains("poetry env use"));
    }
}
