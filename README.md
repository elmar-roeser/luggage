# luggage

Clone or create a project and set up the right development environment right away:
PHP, Node, Python and Rust with Composer, pnpm/yarn and uv/poetry in exactly the version
the project needs — via [Nix](https://nixos.org) and [direnv](https://direnv.net), on any Linux distribution.

Like the Luggage from Discworld: it follows you everywhere and always has the right things with it.
`cd` into the project loads the environment, `cd` out unloads it.

## Installation

```bash
cargo install luggage-env
```

The binary is called `luggage`. Alternatively: a static Linux binary from the
[Releases](https://github.com/elmar-roeser/luggage/releases).

### Requirements

- [Nix](https://nixos.org/download) with flakes enabled (`experimental-features = nix-command flakes`)
- [direnv](https://direnv.net) with its shell hook and [nix-direnv](https://github.com/nix-community/nix-direnv)
- `git`; for `--gitlab` also [`glab`](https://gitlab.com/gitlab-org/cli), for `--github` [`gh`](https://cli.github.com) (each logged in)
- for the chest: `bwrap` ([bubblewrap](https://github.com/containers/bubblewrap)), `unshare`, `nsenter`, `setsid` (util-linux) and `ip` (iproute2)

## Usage

```bash
# Clone an existing repo (default: ~/projects/<name>)
luggage new git@github.com:user/app.git

# Create a new project, optionally with a remote project (default: private)
luggage new my-app
luggage new my-app --gitlab my-group [--host gitlab.example.com]
luggage new my-app --github [my-org] [--visibility public]

# Set up the environment in an existing repo
luggage init
```

Shared options: `--php 8.3`, `--node 22`, `--python 3.12`, `--force` (overwrite an existing
`flake.nix`), `--no-build` (do not build the environment in advance), and for `new` also
`--dir DIR`.

GitHub repos get an SSH remote (`git@github.com:…`). Without `--host` and without
`gitlab.host` in the config, `--gitlab` uses the default host of glab.

## Config

`luggage config` shows the effective settings and where they come from.
`luggage config --init` creates a commented template at
`~/.config/luggage/config.toml`. Order: command line beats config, config beats default.

```toml
projects_dir = "~/projects"

[nix]
nixpkgs = "github:NixOS/nixpkgs/nixos-26.05"
packages = ["just"]            # in every environment

[gitlab]
host = "gitlab.example.com"
group = "my-group"
visibility = "private"         # private | internal | public

[github]
owner = "my-org"
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

Unknown keys are an error, so typos do not silently have no effect.

## What is detected

| | Source, in this order |
|---|---|
| PHP version | `--php`, `config.platform.php`, lower bound of `require.php`, config, else the nixpkgs default |
| PHP extensions | all `ext-*` from `require` and `require-dev`, plus `php.extensions` from the config |
| Node version | `--node`, `.nvmrc` / `.node-version`, `engines.node`, config, else the nixpkgs default |
| Package manager | `pnpm-lock.yaml` → pnpm, `yarn.lock` → yarn |
| Python version | `--python`, `.python-version`, lower bound of `requires-python` or `tool.poetry.dependencies.python`, config, else the nixpkgs default |
| Python tool | `uv.lock` → uv, `poetry.lock` or `[tool.poetry]` → poetry, else uv if there is a `pyproject.toml` |
| Rust | `Cargo.toml` → rustc, cargo, clippy, rustfmt, rust-analyzer from nixpkgs |

luggage asks the channel which versions exist (cached for one day
in `~/.cache/luggage/`). It always picks the lowest available version that meets the
requirement. Extensions that nixpkgs does not know are reported during the build instead of
being silently ignored.

**Python:** Packages go into `.venv` in the project via uv/poetry, as usual; direnv activates
them. uv and poetry are pinned to the Nix Python.

**Rust:** Only the Rust version of the channel is available. A `rust-toolchain.toml` is
ignored; a higher `rust-version` in `Cargo.toml` is reported.

## What luggage writes

- `flake.nix` with a `devShell`, and `.envrc` (`use flake`, with `.venv` for Python)
- `.direnv/` (and `.venv/` for Python) in `.gitignore`
- `flake.lock` via `nix flake lock`

All files are staged (Nix only sees files that git knows), but **not committed**.

## The chest

The chest is the project in an isolated environment: its own home, its own services,
its own network — without Docker and without root. It only sees the project and the Nix store;
`~/.ssh`, credentials and other projects stay outside. The IDE stays outside too.

```bash
luggage run --net composer install   # one-off command, --net allows internet access
luggage up                           # start services in the background, waits until they are ready
luggage status                       # is it running? how are the services doing?
luggage exec php bin/console about   # command in the running chest
luggage open                         # shell in the running chest
luggage down
```

luggage reads which services are needed from the project's `compose.yaml` (or
`docker-compose.yml`). It takes over MariaDB, PostgreSQL, Redis/Valkey
and Mailpit/Mailhog: in the version from the image tag (else the next higher one from nixpkgs),
with the database and user from `environment`, and the Compose service name as host name.
So a `.env` with `DATABASE_URL=mysql://app:app@database:3306/app` works unchanged.
`luggage status` lists everything else (custom images, nginx, PHP) as skipped.

You can extend this with a `chest.toml` in `.luggage/` in the project or, if the
team repo is off limits, in `~/.config/luggage/chests/<project>/`. A service with the same name
replaces the detected one. The directory of the file is readable in the chest at `/chest`
(e.g. for a Caddyfile).

```toml
ignore = ["mailer"]         # skip these detected Compose services
hosts = ["api.local"]       # point to 127.0.0.1 in the chest
packages = ["curl"]         # extra nixpkgs packages, here for the ready check

[services.web]
command = "exec php -S 127.0.0.1:80 -t public"
depends_on = ["database-setup"]
ready = "curl -sf http://127.0.0.1/"
ready_timeout = 60          # seconds (default)

[services.cache-warmup]
command = "php bin/console cache:warmup"
once = true                 # runs once to the end, dependents wait for success
```

In the chest, data lives in `/data` and sockets in `/run/luggage`. Ports below 1024
are allowed, but only apply inside the chest. Outside, home and data live in
`~/.local/share/luggage/chests/<project>/`, runtime files in `$XDG_RUNTIME_DIR/luggage/<project>/`.

The chest protects against mistakes, install scripts and curious agents, but it is not a
hard security boundary (same UID, no seccomp).

## License

MIT or Apache-2.0, at your option.
