# luggage

Projekt klonen oder anlegen und dabei gleich die passende Entwicklungsumgebung einrichten:
PHP, Composer, Node und pnpm/yarn in genau der Version, die das Projekt braucht — per
[Nix](https://nixos.org) und [direnv](https://direnv.net), auf jeder Linux-Distribution.

Wie die Truhe aus der Scheibenwelt: Sie folgt dir überallhin und hat immer das Richtige dabei.
`cd` ins Projekt lädt die Umgebung, `cd` hinaus entlädt sie.

## Installation

```bash
cargo install luggage-env
```

Das Binary heißt `luggage`. Alternativ: statisches Linux-Binary von den
[Releases](https://github.com/elmar-roeser/luggage/releases).

### Voraussetzungen

- [Nix](https://nixos.org/download) mit aktivierten Flakes (`experimental-features = nix-command flakes`)
- [direnv](https://direnv.net) mit Shell-Hook und [nix-direnv](https://github.com/nix-community/nix-direnv)
- `git`; für `--gitlab` zusätzlich [`glab`](https://gitlab.com/gitlab-org/cli) (angemeldet)

## Benutzung

```bash
# Bestehendes Repo klonen (Standard: ~/projects/<name>)
luggage new git@github.com:user/app.git

# Neues Projekt anlegen, optional mit privatem GitLab-Projekt
luggage new meine-app
luggage new meine-app --gitlab meine-gruppe [--host gitlab.example.com]

# Umgebung in einem vorhandenen Repo einrichten
luggage init
```

Gemeinsame Optionen: `--php 8.3`, `--node 22`, `--force` (vorhandene `flake.nix`
überschreiben), `--no-build` (Umgebung nicht vorab bauen), bei `new` außerdem `--dir DIR`.

Ohne `--host` benutzt `--gitlab` den Standard-Host aus der glab-Konfiguration
(`glab config set host gitlab.example.com`).

## Was erkannt wird

| | Quelle, in dieser Reihenfolge |
|---|---|
| PHP-Version | `--php`, `config.platform.php`, Untergrenze von `require.php`, sonst 8.4 |
| PHP-Extensions | alle `ext-*` aus `require` und `require-dev` |
| Node-Version | `--node`, `.nvmrc` / `.node-version`, `engines.node`, sonst nixpkgs-Standard |
| Paketmanager | `pnpm-lock.yaml` → pnpm, `yarn.lock` → yarn |

Gewählt wird jeweils die kleinste verfügbare Version, die die Anforderung erfüllt
(nixos-26.05: PHP 8.2–8.5, Node 20/22/24/26). Extensions, die nixpkgs nicht kennt,
werden beim Bauen gemeldet statt still ignoriert.

## Was luggage schreibt

- `flake.nix` mit einer `devShell` und `.envrc` (`use flake`)
- `.direnv/` in `.gitignore`
- `flake.lock` über `nix flake lock`

Alle Dateien werden gestaged (Nix sieht nur Dateien, die git kennt), aber **nicht committet**.

## Lizenz

MIT oder Apache-2.0, nach Wahl.
