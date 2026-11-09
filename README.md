# winner

A small, daemonless command line tool for running Windows containers, built
directly on the Host Compute Service (HCS) APIs. No Docker, no containerd,
no background service of its own: a single executable that talks to
`vmcompute` the same way hcsshim does.

> **Status: early work in progress.** Process-isolated containers run in
> the foreground or background; there is no networking yet.

## Requirements

- Windows 11 or Windows Server with the **Containers** optional feature
  enabled (and a reboot afterwards):

  ```powershell
  Enable-WindowsOptionalFeature -Online -FeatureName Containers -All
  ```

  `vmcompute`, `wcifs` and `bindflt` also ship with Hyper-V / WSL, so their
  presence does not prove the feature is on. Without it, creating a container
  fails with `The request is not supported (0x80070032)`.
- An elevated terminal. Layer import needs `SeBackupPrivilege` /
  `SeRestorePrivilege`, and HCS requires administrator rights. Windows'
  built-in `sudo` in inline mode works.
- If Defender's Controlled Folder Access is on, allow `winner.exe`: layer
  processing writes directly to mounted VHDs and is otherwise blocked
  (surfacing as `Access is denied`).
- Process isolation requires the image build to be compatible with the host
  (for example `ltsc2025` / 26100 on Windows 11 24H2+).

## Usage

```powershell
winner image pull mcr.microsoft.com/windows/nanoserver:ltsc2025
winner run --remove mcr.microsoft.com/windows/nanoserver:ltsc2025 cmd /c ver

# background containers
winner run -d --name demo -v C:\data:C:\data mcr.microsoft.com/windows/nanoserver:ltsc2025 ping -t 127.0.0.1
winner logs --follow demo
winner exec demo cmd /c dir C:\data
winner stop demo
winner start demo
winner delete --force demo

winner create --name later nanoserver:ltsc2025 cmd /c echo hi
winner start later
winner list
winner inspect later
winner image list
```

Container operations live at the top level; other resources are grouped as
`<noun> <verb>` (`image pull`, `image list`, ...). `ls` and `rm` are accepted
as aliases for `list` and `delete`. A tty is detected automatically;
`--tty` / `--no-tty` override it.

- Without a command, `run` uses the image's entrypoint and command;
  `--entrypoint` replaces the entrypoint. The image's environment, working
  directory and user apply unless `--env`, `--workdir` or `--user` override
  them.
- `run --detach` and `start` run the container in the background under a
  small supervisor process (`winner shim`); its output goes to the log that
  `logs` prints. `stop` asks the container to shut down and terminates it
  after `--timeout` seconds; `kill` terminates it at once. `exec`, `stop` and
  `kill` work on foreground containers too.
- `--volume HOST:CONTAINER[:ro]` maps a host directory, e.g.
  `-v C:\data:C:\data:ro`. Host ACLs still apply: the image's default
  `ContainerUser` cannot read a directory only your account may access; grant
  access or run with `-u ContainerAdministrator`.
- Isolation is picked automatically: process isolation when the image build
  is compatible with the host.
- `run` pulls a missing image itself (`--pull missing|always|never`).
- Image references must name the registry; there is no implicit Docker Hub.
  Once pulled, a short form such as `nanoserver:ltsc2025` finds the local image.
- Containers are addressed by ID, a unique ID prefix, or `--name`. `delete`
  takes several at once and refuses running containers unless `--force`.
- `image delete` also removes layers no image or container uses anymore.
- `list` and `image list` print tables; pass `--format json` for scripts.
- `run` and `exec` exit with the process's exit code, or 125 if winner itself fails.

Data is stored under `%ProgramData%\winner`.

## Building

```powershell
cargo build --release
```

The release binary is a single ~7 MB executable with no runtime dependencies.

End-to-end tests need elevation; run the whole suite under one prompt:

```powershell
sudo pwsh -NoProfile -File scripts/e2e.ps1          # add -Full for image deletion tests
```

Results are also written to `target\e2e.log`.

## Roadmap

- [x] Registry pull with Windows platform selection
- [x] Base layer import
- [x] Scratch layer create / mount (process isolation)
- [x] Run a process-isolated container in the foreground
- [x] Non-base layer import
- [x] Detached containers, logs, exec, stop / kill
- [ ] Networking (HCN)
- [ ] Hyper-V isolation

See [docs/development.md](docs/development.md) for design notes.
