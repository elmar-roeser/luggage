# luggage

Projekt klonen oder anlegen und dabei gleich die passende Entwicklungsumgebung einrichten:
PHP, Node, Python und Rust samt Composer, pnpm/yarn und uv/poetry in genau der Version,
die das Projekt braucht — per [Nix](https://nixos.org) und [direnv](https://direnv.net), auf jeder Linux-Distribution.

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
- `git`; für `--gitlab` zusätzlich [`glab`](https://gitlab.com/gitlab-org/cli), für `--github` [`gh`](https://cli.github.com) (jeweils angemeldet)

## Benutzung

```bash
# Bestehendes Repo klonen (Standard: ~/projects/<name>)
luggage new git@github.com:user/app.git

# Neues Projekt anlegen, optional mit Remote-Projekt (Standard: privat)
luggage new meine-app
luggage new meine-app --gitlab meine-gruppe [--host gitlab.example.com]
luggage new meine-app --github [meine-org] [--visibility public]

# Umgebung in einem vorhandenen Repo einrichten
luggage init
```

Gemeinsame Optionen: `--php 8.3`, `--node 22`, `--python 3.12`, `--force` (vorhandene
`flake.nix` überschreiben), `--no-build` (Umgebung nicht vorab bauen), bei `new` außerdem
`--dir DIR`.

GitHub-Repos bekommen ein SSH-Remote (`git@github.com:…`). Ohne `--host` und ohne
`gitlab.host` in der Config benutzt `--gitlab` den Standard-Host von glab.

## Config

`luggage config` zeigt die wirksamen Einstellungen und woher sie kommen,
`luggage config --init` legt eine kommentierte Vorlage unter
`~/.config/luggage/config.toml` an. Reihenfolge: Kommandozeile vor Config vor Standard.

```toml
projects_dir = "~/projects"

[nix]
nixpkgs = "github:NixOS/nixpkgs/nixos-26.05"
packages = ["just"]            # in jede Umgebung

[gitlab]
host = "gitlab.example.com"
group = "meine-gruppe"
visibility = "private"         # private | internal | public

[github]
owner = "meine-org"
visibility = "private"

[php]
default = "8.4"
memory_limit = "512M"
extensions = ["xdebug"]

[node]
default = 24

[python]
default = "3.13"
```

Unbekannte Schlüssel sind ein Fehler, damit Tippfehler nicht still wirkungslos bleiben.

## Was erkannt wird

| | Quelle, in dieser Reihenfolge |
|---|---|
| PHP-Version | `--php`, `config.platform.php`, Untergrenze von `require.php`, Config, sonst nixpkgs-Standard |
| PHP-Extensions | alle `ext-*` aus `require` und `require-dev`, dazu `php.extensions` aus der Config |
| Node-Version | `--node`, `.nvmrc` / `.node-version`, `engines.node`, Config, sonst nixpkgs-Standard |
| Paketmanager | `pnpm-lock.yaml` → pnpm, `yarn.lock` → yarn |
| Python-Version | `--python`, `.python-version`, Untergrenze von `requires-python` bzw. `tool.poetry.dependencies.python`, Config, sonst nixpkgs-Standard |
| Python-Werkzeug | `uv.lock` → uv, `poetry.lock` oder `[tool.poetry]` → poetry, sonst bei `pyproject.toml` uv |
| Rust | `Cargo.toml` → rustc, cargo, clippy, rustfmt, rust-analyzer aus nixpkgs |

Welche Versionen es gibt, fragt luggage beim Channel ab (einen Tag zwischengespeichert
unter `~/.cache/luggage/`). Gewählt wird jeweils die kleinste verfügbare Version, die die
Anforderung erfüllt. Extensions, die nixpkgs nicht kennt, werden beim Bauen gemeldet statt
still ignoriert.

**Python:** Pakete landen wie gewohnt per uv/poetry in `.venv` im Projekt; direnv aktiviert
sie. uv und poetry werden auf das Nix-Python festgelegt.

**Rust:** Es gibt nur die Rust-Version des Channels. Eine `rust-toolchain.toml` wird
ignoriert, eine höhere `rust-version` in `Cargo.toml` gemeldet.

## Was luggage schreibt

- `flake.nix` mit einer `devShell` und `.envrc` (`use flake`, bei Python mit `.venv`)
- `.direnv/` (und bei Python `.venv/`) in `.gitignore`
- `flake.lock` über `nix flake lock`

Alle Dateien werden gestaged (Nix sieht nur Dateien, die git kennt), aber **nicht committet**.

## Lizenz

MIT oder Apache-2.0, nach Wahl.
