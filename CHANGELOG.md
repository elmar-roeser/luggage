# Changelog

All notable changes to luggage. Generated from the commit messages with git-cliff.

## [0.3.0] - 2026-10-22

### Features

- *(chest)* Build a microVM guest for mode = "vm"
- *(chest)* Start and stop the VM with luggage up and down
- *(chest)* Run commands in the VM with luggage exec and open
- *(chest)* Start the VM from direnv on cd into the project
- *(chest)* Run the services in the VM
- *(chest)* Run one-off commands in the VM with luggage run
- *(chest)* Forward service ports from the VM to 127.0.0.1
- *(doctor)* Check kvm, vhost-vsock and ssh for the VM mode
- *(chest)* Keep a GC root for the dev environment in both modes
- *(chest)* Warn when the running VM uses an outdated definition
- *(chest)* Move taken host ports to a free one, fix them in [ports]
- *(chest)* Keep the VM home on its own disk per chest
- *(chest)* Pass chosen host variables into the VM with pass_env
- *(chest)* Limit the VM network to listed hosts with network
- *(chest)* Pass host file changes to file watchers in the VM
- *(chest)* Limit network to ports 80, 443 and public addresses
- *(chest)* Pass only harmless host variables into the VM
- *(chest)* Reach bubblewrap chest ports from 127.0.0.1 via socat
- *(chest)* Pass pass_env variables into the bubblewrap chest
- *(chest)* Limit the bubblewrap network to listed hosts with network
- *(chest)* Keep service data per git branch
- *(doctor)* Explain when AppArmor restricts user namespaces
- *(setup)* Run a root-owned copy with an AppArmor profile on Ubuntu
- *(skills)* Link skills for Claude Code and Codex from sources in the config
- *(chest)* Give a chest the skills that its chest.toml names
- *(chest)* Install Claude Code and Codex in the chest with agents
- *(chest)* List chests and print their status as JSON
- *(tui)* Show the chests in a terminal UI
- *(tui)* List projects that have no chest yet
- *(tui)* Create and edit config.toml and chest.toml
- *(tui)* Edit the common config fields in a form
- *(tui)* Pick choices and agents from a dropdown
- *(skills)* Bring a luggage skill for agents on the host and in chests

### Bug fixes

- *(chest)* Print luggage's own messages to stderr
- *(chest)* Disable process-compose env expansion at the top level
- *(chest)* Wait for the chest to die after SIGKILL in down
- *(chest)* Retry the VM build after a failed nix build
- *(chest)* Send host variables to the VM in the ssh environment
- *(chest)* Refuse odd hosts and slow request heads in the proxy
- *(chest)* Serve at most 256 proxy connections of a chest at once
- *(chest)* Enter the chest per connection instead of using its sockets
- *(chest)* Keep luggage's own runtime files out of the chest's reach
- *(chest)* Remove the proxy mount point after luggage run
- *(chest)* Make files the host trusts read-only in the chest
- *(chest)* Stop the chest when it plants git files the host would read
- *(chest)* Make replaced trusted files read-only again
- *(chest)* Only warn about nested repos and check them at up and down
- *(chest)* Find the namespace of the chest for rebind on Ubuntu
- *(vm)* Run the project's virtiofsd with bwrap from nixpkgs
- *(setup)* Test the chest only once nix is in the PATH
- *(chest)* Make .git/modules and .git/worktrees read-only in the chest
- *(chest)* Keep the locked mount flags when binding a file read-only again
- *(chest)* Stop a VM start that finished while a branch switch waited
- *(vm)* Tell a host error from a taken vsock address
- *(vm)* Leave the mode of the host's home alone without a home disk
- *(proxy)* Answer 500 when the slot directory cannot be used
- *(setup)* Do not start the AppArmor copy again from the copy
- *(vm)* Send the whole project when inotify drops events
- *(hook)* Fall back to the default nixpkgs on a broken config.toml
- *(tui)* Make the key line readable on dark themes

### Performance

- *(chest)* Skip node_modules when looking for nested repos
- *(vm)* Rewrite stand-ins only when they change

### Refactoring

- *(chest)* Share stopping a process group from a pid file

### Documentation

- Describe the chest in a VM
- *(readme)* Say clearly that the modes differ without network

### Tests

- Compare with an empty vec for the new assert_is_empty lint

### Build and CI

- Move actions to Node 24 and pin the runner to Ubuntu 24.04
- Run on Ubuntu 26.04

## [0.2.0] - 2026-09-30

### Features

- Add config file, Python and Rust support, GitHub remotes
- Add the Truhe, an isolated environment per project
- Add doctor and setup, take system tools from nixpkgs

### Refactoring

- **breaking:** Rename Truhe to chest and write all text in English

### Style

- Adopt house rustfmt and clippy config

### Build and CI

- Adopt house lint set and add audit and secret checks
- Tag and release automatically when the version changes

## [0.1.1] - 2026-09-29

### Bug fixes

- Show luggage as program name in --version

## [0.1.0] - 2026-09-29

### Features

- Initial release of luggage

[0.3.0]: https://github.com/elmar-roeser/luggage/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/elmar-roeser/luggage/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/elmar-roeser/luggage/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/elmar-roeser/luggage/releases/tag/v0.1.0

