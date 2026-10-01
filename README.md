# luggage

Clone or create a project and set up the right development environment right away:
PHP, Node, Python and Rust with Composer, pnpm/yarn and uv/poetry in exactly the version
the project needs — via [Nix](https://nixos.org) and [direnv](https://direnv.net), on any Linux distribution.

Like the Luggage from Discworld: it follows you everywhere and always has the right things with it.
`cd` into the project loads the environment, `cd` out unloads it.

## Installation

1. **Get luggage**: download the static Linux binary from the
   [releases](https://github.com/elmar-roeser/luggage/releases) into `~/.local/bin`
   (no Rust needed), or with Rust: `cargo install luggage-env`. The binary is called `luggage`.
2. **Set up**:

   ```bash
   luggage setup
   ```

   `setup` checks what is missing and asks before each step:
   - **Nix** via the official [Nix installer](https://github.com/NixOS/nix-installer)
     with flakes (needs `curl` and `sudo` once),
   - **git**, **direnv** and **nix-direnv** via `nix profile install`,
   - the **direnv hook** in `~/.bashrc`, `~/.zshrc` or the fish config.

   Then open a new shell. `luggage setup --yes` answers all questions with yes.

`luggage doctor` checks the same things without changing anything.

After that, luggage only needs Nix, git and the direnv hook from your system. Everything else
comes from nixpkgs: the tools of the chest (bubblewrap, util-linux, iproute2) and, when needed,
`gh`/`glab` for `--github`/`--gitlab` (log in with `gh auth login` or `glab auth login`).

**Ubuntu 24.04 and later** restricts user namespaces with AppArmor. `luggage doctor` shows
whether the chest works there; its hint names the settings to change.

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

- `flake.nix` with a `devShell`, and `.envrc` (`use flake`, with `.venv` for Python, and a
  `luggage hook` line that only does something for a [chest in a VM](#the-chest-in-a-vm))
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

### The chest in a VM

With `mode = "vm"` in `chest.toml`, the chest is a small virtual machine with its own kernel
([microvm.nix](https://github.com/microvm-nix/microvm.nix) with qemu, no root needed).
Everything runs in it: `php`, `composer` and the other programs of the project go to the VM.
The project and the Nix store come from the host, so files belong to you and the IDE stays outside.

```toml
mode = "vm"
```

When you `cd` into the project, direnv starts the VM in the background (about 3.5 s) and puts
a stand-in for each project program first on the `PATH`. The first command waits until the VM is up,
later ones take a few milliseconds more than on the host. Only one VM runs at a time:
starting the VM of another project stops the old one. `luggage run`, `up`, `exec`, `open`, `status` and
`down` work as above; `run`, `exec` and `open` start the VM if needed. `run` uses the running VM, and without
`--net` its command has no network, not even to the services.

This needs `/dev/kvm` and `/dev/vhost-vsock` (your user in the group `kvm`) and ssh; `luggage doctor`
checks them in its optional VM section. `luggage up` adds the line
`if has luggage; then eval "$(luggage hook)"; fi` to the `.envrc` if it is missing; then run `direnv allow` once.
The services (from `compose.yaml` and `chest.toml`) run in the VM too, with their data on the VM disk.
The running VM keeps the definition it was started with. After changing services, packages, hosts, `ports`,
whether there is a `network` or (with services) the project flake, `up`, `exec`, `status` and the stand-ins warn
until you restart it with `luggage down && luggage up`.

Your home in the VM (same path as on the host, but not your host files) lives on its own disk,
`~/.local/share/luggage/chests/<project>/vm/home.img` (up to 8 GB, sparse), so caches such as
Composer's survive a restart. `/var` with the service data lives next to it in `var.img`.
To reset the home, delete the image while the VM is stopped: `luggage down`, `rm …/vm/home.img`, `luggage up`.

File watchers in the VM (Vite, webpack/Encore, `npm run dev`, phpunit-watcher) see your edits on the host:
inotify in the VM does not notice host writes over virtiofs, so luggage watches the project on the host and
marks each changed file in the VM with a `chmod` to its own mode, which the watchers report as a change
(within about 0.1 s). It leaves out `.git`, `vendor`, `var`, `.idea`, `.direnv`, `.luggage` and every
`node_modules`. Changes made in the VM reach the watchers anyway; a bare `chmod` on the host is not passed on.
If a watcher still misses changes, e.g. in a left-out directory, switch it to polling: `CHOKIDAR_USEPOLLING=1`
(Vite, chokidar), `WATCHPACK_POLLING=true` (webpack), or the `usePolling`/`poll` option of the tool.

The `ports` of the taken-over Compose services are reachable from the host browser, but only on 127.0.0.1,
never from the network (`"8026:8025"` in `compose.yaml` → `127.0.0.1:8026`). Mailpit without `ports`
gets its web UI on `127.0.0.1:8025`. Your own services list theirs in `chest.toml`, as `"host:guest"` or `"port"`:

```toml
[services.web]
command = "exec php -S 127.0.0.1:80 -t public"
ports = ["8000:80"]         # http://127.0.0.1:8000 on the host
```

If a host port is taken, `up` moves it to the next free one (at most 100 higher) and says so;
`luggage status` shows the real addresses. To choose the host ports of a service yourself, e.g. of a detected
one, without redefining it, use the `[ports]` table. Its ports replace those of the service and stay fixed:
if one is taken, `up` only warns and the VM starts without it.

```toml
[ports]
database = ["3307:3306"]    # MariaDB on 127.0.0.1:3307 instead of 3306
```

In the bubblewrap chest, `ports` has no effect yet.

Commands in the VM get your shell's variables, except those that describe the host session and
API keys: `ANTHROPIC_*` and `CLAUDE_*` stay on the host. To let an agent in the VM use its key,
name the variables in `pass_env`:

```toml
pass_env = ["ANTHROPIC_API_KEY"]
```

luggage reads the values of all passed variables from your environment at each command and hands them to ssh
in its environment, never on a command line or in a file; only the names are stored. They reach `exec`, `open`, `run` and the
stand-ins, not the services. `pass_env` needs `mode = "vm"`: the bubblewrap chest gets no host variables at all.

The VM has open network access. To allow only some hosts, list them in `network`; an entry allows the host
and all its subdomains (`github.com` also allows `api.github.com`), and `network = []` closes the network:

```toml
network = ["packagist.org", "github.com", "npmjs.org"]
```

The VM then reaches the outside only through a proxy that luggage runs on the host for each connection.
Commands and services get `http_proxy`, `https_proxy` and `no_proxy` (localhost and the `hosts`), which curl,
git over HTTPS, Composer and npm follow. The guest has no DNS and no direct connections, so tools that ignore
these variables, and git over ssh, cannot get out. The forwarded ports work as before. A changed list applies to
the next connection after the next luggage command; adding or removing `network` needs a restart.
`luggage status` shows the list. `network` needs `mode = "vm"`.

## Development

`make ci` runs every check locally (format, clippy, tests, `cargo audit`, gitleaks).
Commit messages follow [Conventional Commits](https://www.conventionalcommits.org);
[CHANGELOG.md](CHANGELOG.md) is generated from them with [git-cliff](https://git-cliff.org).

Releasing a new version:

1. `make bump VERSION=X.Y.Z` sets the version in `Cargo.toml`, updates the changelog and
   commits `chore(release): vX.Y.Z`.
2. `git push origin main`. The release workflow sees a version without a tag, runs the tests,
   builds the Linux binary, creates the tag and the GitHub release (notes from the changelog)
   and runs `cargo publish` when the repository secret `CARGO_REGISTRY_TOKEN` is set.

## License

MIT or Apache-2.0, at your option.
