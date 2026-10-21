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
comes from nixpkgs: the tools of the chest (bubblewrap, util-linux, iproute2, socat) and, when needed,
`gh`/`glab` for `--github`/`--gitlab` (log in with `gh auth login` or `glab auth login`).

### Ubuntu 24.04 and later

Ubuntu lets only programs with an AppArmor profile use user namespaces, and the chest needs them
(in both modes). On such a system `luggage setup` asks to do two things with `sudo`:

- copy luggage to `/usr/local/lib/luggage/luggage`, owned by root, so no other program can change it,
- install an AppArmor profile for this copy only (`/etc/apparmor.d/luggage`) and load it.

After that, the `luggage` you call starts the copy by itself. When you update luggage, the copy is
out of date: chest commands stop with a hint, and `luggage setup` updates it (with `sudo` again).
Note that the programs in the chest may then use user namespaces too, as on other distributions.

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
  `luggage hook` line that starts a [chest in a VM](#the-chest-in-a-vm) and moves a running chest to the
  [data of a newly checked-out branch](#service-data-per-branch))
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
if one is taken, `up` only warns and the chest starts without it.

```toml
[ports]
database = ["3307:3306"]    # MariaDB on 127.0.0.1:3307 instead of 3306
```

In the bubblewrap chest, socat bridges each port from 127.0.0.1 on the host and enters the network of the chest
with nsenter for each connection; the bridges end with the chest. In a VM, qemu forwards the ports.

Commands in the chest get the project's dev environment from the flake, not your shell. Of your shell's
variables only harmless ones go along: `TERM`, `COLORTERM`, `TZ`, `EDITOR`, `VISUAL`, `PAGER`, `NO_COLOR`,
`FORCE_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE`, `VIRTUAL_ENV` and, with an open network, the proxy variables like
`https_proxy`. `LANG` and `LC_*` stay on the host; the VM has its own locale (`en_US.UTF-8`). Everything
else stays there too, including tokens like `GITHUB_TOKEN`, `NPM_TOKEN`, `AWS_*` and `ANTHROPIC_*`, and
settings like `APP_ENV`. Name the variables a command in the chest needs in `pass_env`:

```toml
pass_env = ["ANTHROPIC_API_KEY", "GITHUB_TOKEN"]
```

luggage reads the values of all passed variables from your environment at each command and hands them to
bwrap, nsenter or ssh in their environment, never on a command line or in a file; only the names are stored.
They reach `exec`, `open`, `run` and the stand-ins, not the services.

**Without `network`, the two modes differ:** the bubblewrap chest has no network (its services, `exec` and
`open` reach only the chest, and `luggage run --net` uses the network of the host), but the VM has open network
access. To allow only some hosts in either mode, list them in `network`; an entry allows the host and all its
subdomains (`github.com` also allows `api.github.com`) on ports 80 and 443, and `network = []` closes the
network. An entry with a port, like `"git.example.com:22"`, allows only that port; write IPv6 addresses in
brackets (`"[::1]:8080"`):

```toml
network = ["packagist.org", "github.com", "npmjs.org"]
```

A name that resolves to a loopback, private (LAN, `100.64.0.0/10`) or link-local address is refused, so a
listed domain cannot lead to services on your machine or network. To reach those, list the IP address itself
or `localhost`, for example `"localhost:8080"` or `"192.168.1.10:443"`.

The chest then reaches the outside only through a proxy that luggage runs on the host for each connection;
`luggage run` without `--net` stays offline. Commands and services get `http_proxy`, `https_proxy` and
`no_proxy` (localhost and the `hosts`), which curl, git over HTTPS, Composer and npm follow. The chest has no
DNS and no direct connections, so tools that ignore these variables, and git over ssh, cannot get out. At most
256 connections go through the proxy at once; more get the error 503. In the bubblewrap chest, a socat on
`127.0.0.1:3128` passes the connections to the host. A changed list applies to the next connection after the
next luggage command; adding or removing `network` needs a restart. `luggage status` shows the list.

Some files in the project are read-only in the chest, because programs on the host read or run them:

- `.luggage/`: luggage reads `chest.toml` at each command, so the chest could add `pass_env` or `network`.
- `flake.nix`, `flake.lock`, `.envrc` and `.direnv/`: direnv on the host loads them when you `cd` into the project.
- `.git/config` and `.git/hooks/`: git on the host runs hooks, and the config can name programs to run.
- `.git/modules/` and `.git/worktrees/`: they hold the config and hooks of submodules and worktrees. So git
  inside a submodule works read-only in the chest; `git submodule update` and `git worktree add` run on the host.

Everything else, including the rest of `.git`, stays writable, so `git commit` works in the chest. luggage creates
an empty `.luggage/`, `.direnv/`, `.git/hooks/`, `.git/modules/` and `.git/worktrees/` if they are missing. If a program on the host replaces one of
the files instead of writing into it (`git config` does, some editors do), the kernel drops its read-only mount in
the chest. luggage notices this at the next luggage command or direnv hook and makes the new file read-only again;
the same happens to a file that was missing at the start, like a new `flake.lock`. Until then the file is
writable in the chest, and a program that opened it before stays able to write it.

The rest of `.git` can still lead git on the host to another config, for example `.git/commondir`,
`.git/config.worktree` or a repo nested in the project. So luggage notes these files when the chest starts
and checks the ones in `.git` again at each command. If one changed, luggage stops the chest and refuses to go on
until you checked the file and ran `luggage down`. Nested repos are checked only at `up` and `down`, and only with a warning,
because tools in the chest create them on purpose (`composer` from source). It is a check, not a lock: a `git`
command that you run on the host before the next luggage command still reads the changed file.

The chest protects against mistakes, install scripts and curious agents, but it is not a
hard security boundary (same UID, no seccomp).

### Service data per branch

Each git branch has its own service data (databases and everything else in `/data`); the home stays shared.
When you check out another branch, the next luggage command (`up`, `exec`, `run`, a stand-in, or the direnv hook
in the background) stops the chest, puts the data of the old branch aside and starts the chest again with the
data of the new one. A branch that has no data yet gets a copy of the data in use, so a new feature branch starts
where its base branch was; run its migrations then. The copy uses reflinks (btrfs, XFS), which take no time and
no space until the data differs; elsewhere luggage copies in full and says how much. A detached HEAD, a rebase
or a bisect keeps the data in use. Each worktree is its own chest with its own branches.

`luggage status` shows the branch in use and the size of each branch's data. `luggage prune` removes the data of
branches that no longer exist; luggage never removes it by itself. The data that is put aside lives in
`~/.local/share/luggage/chests/<project>/branches/`.

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

## Skills for coding agents

luggage hands skills to Claude Code and Codex from git repos or local directories that you list in the config:

```toml
[skills.team]
git = "git@gitlab.example.com:dev/skills.git"
dir = "skills"              # folder with one subfolder per skill; if unset: the top
# ref = "main"              # branch or tag; if unset: the default branch
# only = ["glab"]           # if unset: all skills of the source

[skills.own]
path = "~/skills"           # a local directory; changes show at once
```

```bash
luggage skills sync         # fetch the sources and link their skills
luggage skills update       # fetch the newest commit of each git source (or: update team)
luggage skills              # sources, their commit and what is linked
```

A skill is a folder with a `SKILL.md`. `sync` links each one into `~/.claude/skills` and `~/.agents/skills`
(the folder Codex reads), for each agent that has its `~/.claude` or `~/.codex`. Git sources stay at their
commit until `update`; they live read-only in the Nix store, kept from the garbage collector. luggage only
changes its own links: an entry it did not create stays as it is and `sync` names it. If a skill folder is
itself a link, another tool manages it, and `sync` refuses until you move it away.

A chest gets only the skills that its `chest.toml` names, as `<source>/<skill>`, where `*` stands for any text:

```toml
skills = ["team/*", "own/review-*"]
```

At each start (`up`, `run`, and a VM start) luggage copies these skills into one path in the Nix store and links
them into `~/.claude/skills` and `~/.agents/skills` of the chest, in both modes. There they are read-only: an agent
in the chest cannot change the skills that it runs. A changed list or source applies at the next start. If a source
cannot be fetched, the chest does not start.

### Agents in the chest

```toml
agents = ["claude", "codex"]
```

`luggage up` installs each missing agent with its own installer into `~/.local/bin` of the chest: Claude Code from
claude.ai, Codex from its GitHub releases. They update themselves there like on the host. Each chest has its own
home, so each keeps its own login: log in once per chest, e.g. with another account per project.

```bash
luggage exec claude                      # log in by pasting the code from the browser
luggage exec codex login --device-auth   # log in with a code, without a browser in the chest
```

The agents need the network for their login and API. A bubblewrap chest, which has none otherwise, gets a
`network` list with their hosts (`anthropic.com`, `claude.ai`, `claude.com`; `openai.com`, `chatgpt.com`,
`github.com`, `githubusercontent.com`); a `network` list of your own gets them added; a VM without a list stays
open. luggage also adds `curl`, `gnutar` and `gzip` for the installers and, in both modes, the glibc loader at
`/lib64/ld-linux-x86-64.so.2` from nixpkgs, which Claude Code needs.

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
