//! Erzeugt die flake.nix aus den erkannten Versionen.

use crate::detect::{Node, Php};

pub const NIXPKGS: &str = "github:NixOS/nixpkgs/nixos-26.05";

const FLAKE: &str = r#"{
  description = "@NAME@ — Dev-Umgebung (erzeugt von luggage)";

  inputs.nixpkgs.url = "@NIXPKGS@";

  outputs = { nixpkgs, ... }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
@LETS@    in {
      devShells.x86_64-linux.default = pkgs.mkShell {
        packages = [ @PACKAGES@ ];@HOOK@
      };
    };
}
"#;

const PHP_LET: &str = r#"      # ext-* aus composer.json; aktive werden übersprungen, fehlende gewarnt
      phpExtensions = [ @EXTS@ ];
      php = pkgs.@ATTR@.buildEnv {
        extensions = { enabled, all }:
          let
            have = map (e: e.extensionName) enabled;
            wanted = builtins.filter (n: !(builtins.elem n have)) phpExtensions;
            known = builtins.filter (n: all ? ${n} || builtins.trace "luggage: PHP-Extension ${n} fehlt in nixpkgs" false) wanted;
          in enabled ++ map (n: all.${n}) known;
        extraConfig = "memory_limit = 512M";
      };
"#;

pub fn render(name: &str, php: Option<&Php>, node: Option<&Node>) -> String {
    let mut lets = String::new();
    let mut packages: Vec<String> = Vec::new();
    let mut hook = "";

    if let Some(php) = php {
        let (major, minor) = php.version;
        let exts: Vec<String> = php.exts.iter().map(|e| format!("\"{e}\"")).collect();
        lets.push_str(
            &PHP_LET
                .replace("@ATTR@", &format!("php{major}{minor}"))
                .replace("@EXTS@", &exts.join(" ")),
        );
        packages.extend(["php".into(), "php.packages.composer".into()]);
        // herd-lite o.ä. setzt PHP_INI_SCAN_DIR global und würde fremde .ini laden
        hook = "\n        shellHook = \"unset PHP_INI_SCAN_DIR\";";
    }
    if let Some(node) = node {
        packages.push(
            node.version.map_or_else(|| "pkgs.nodejs".into(), |v| format!("pkgs.nodejs_{v}")),
        );
        if let Some(tool) = node.tool {
            packages.push(format!("pkgs.{tool}"));
        }
    }

    FLAKE
        .replace("@NAME@", name)
        .replace("@NIXPKGS@", NIXPKGS)
        .replace("@LETS@", &lets)
        .replace("@PACKAGES@", &packages.join(" "))
        .replace("@HOOK@", hook)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_and_node() {
        let php = Php { version: (8, 3), source: "", exts: vec!["redis".into()] };
        let node = Node { version: Some(22), source: "", tool: Some("pnpm") };
        let flake = render("demo", Some(&php), Some(&node));
        assert!(flake.contains("pkgs.php83.buildEnv"));
        assert!(flake.contains(r#"phpExtensions = [ "redis" ];"#));
        assert!(
            flake.contains("packages = [ php php.packages.composer pkgs.nodejs_22 pkgs.pnpm ];")
        );
        assert!(flake.contains("unset PHP_INI_SCAN_DIR"));
    }

    #[test]
    fn empty_shell() {
        let flake = render("leer", None, None);
        assert!(flake.contains("packages = [  ];"));
        assert!(!flake.contains("shellHook"));
        assert!(!flake.contains('@'));
    }
}
