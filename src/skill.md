---
name: luggage
description: How to work in a project set up by luggage (a flake.nix and .envrc with a `luggage hook` line, or a .luggage/chest.toml), and inside a luggage chest ($LUGGAGE_CHEST is set). Use it to run commands and services of the project, to find out why a host or file is blocked, and to change chest.toml.
---

# luggage

luggage sets up a Nix dev environment for a project (`flake.nix`, `.envrc` for direnv) and runs the project in a
**chest**: an isolated environment with its own home, services and network. A chest runs with bubblewrap on the
host kernel or, with `mode = "vm"`, in a small VM.

First check where you are: `echo "$LUGGAGE_CHEST"`. If it prints a name, you are **in** that chest; read
"Inside a chest". If it is empty, you are on the host; read "On the host".

## On the host

Run these in the project directory (or below it):

| Command | What it does |
|---|---|
| `luggage status --json` | Mode, running or not, services with ready state, ports, branch, network list |
| `luggage list --json` | All chests and known projects |
| `luggage up` | Starts the services in the background and waits until they are ready |
| `luggage exec <cmd> …` | Runs a command in the running chest, e.g. `luggage exec php bin/console about` |
| `luggage run [--net] <cmd> …` | One-off command in a chest; `--net` gives it the network of the host |
| `luggage down` | Stops the chest |
| `luggage init` | Sets up `flake.nix` and `.envrc` in an existing repo; then `direnv allow` |

- Prefer `luggage exec` for tests, migrations and tools of the project: there they see the services and the
  same environment as in the chest. Never start the project's services yourself with Docker.
- Read `--json` output instead of the text; the text may change.
- `luggage open` and `luggage tui` are interactive. Do not run them; ask the user.

### chest.toml

The chest reads `.luggage/chest.toml` in the project, or `~/.config/luggage/chests/<project>/chest.toml`.
luggage takes the services from `compose.yaml` (MariaDB, PostgreSQL, Redis/Valkey, Mailpit); chest.toml adds or
changes them:

```toml
mode = "bubblewrap"            # or "vm"
network = ["github.com"]       # the only hosts the chest may reach (with subdomains, ports 80 and 443)
hosts = ["api.local"]          # names that point to 127.0.0.1 in the chest
packages = ["curl"]            # extra nixpkgs packages
pass_env = ["GITHUB_TOKEN"]    # host variables that commands in the chest get
agents = ["claude", "codex"]   # coding agents that `up` installs into the chest
skills = ["team/*"]            # skills from the sources in ~/.config/luggage/config.toml

[services.web]
command = "exec php -S 127.0.0.1:80 -t public"
ports = ["8000:80"]            # http://127.0.0.1:8000 on the host
ready = "curl -sf http://127.0.0.1/"
```

`network`, `pass_env` and `mode` decide how isolated the chest is. Change them only when the user asks for it,
and say which host or variable you add and why. Run `luggage status` after a change: it checks the file.

## Inside a chest

- You see the project, the Nix store and the home of the chest. The user's real home, `~/.ssh`, credentials and
  other projects are not there. Do not try to reach them.
- The `luggage` command is not available here. The user runs it on the host.
- Services run in the chest and are reachable by the Compose service name or `127.0.0.1`, so `.env` files like
  `DATABASE_URL=mysql://app:app@database:3306/app` work as they are. Service data lives in `/data`, sockets in
  `/run/luggage`.
- **Network:** if `http_proxy` is set, the chest reaches only the hosts in its `network` list, through that
  proxy; there is no DNS and no direct connection. curl, git over HTTPS, Composer and npm follow the proxy; git
  over ssh does not work. A blocked host fails with an error from the proxy. If `http_proxy` is not set, a
  bubblewrap chest has no network at all, and a VM has open network access.
- When a download fails because a host is blocked, do not look for a way around it. Tell the user the host and
  that they can add it to `network` in chest.toml.
- **Read-only files:** `.luggage/`, `flake.nix`, `flake.lock`, `.envrc`, `.direnv/`, `.git/config`,
  `.git/hooks/`, `.git/modules/` and `.git/worktrees/`. Programs on the host read or run them. To change them,
  give the user the change to make on the host. The rest of the project, including `git commit`, is writable.
- Environment variables of the host do not reach the chest, unless chest.toml names them in `pass_env`. If a
  token is missing, ask the user to add its name there.
- Skills in `~/.claude/skills` and `~/.agents/skills` are read-only.
