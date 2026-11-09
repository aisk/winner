# winner

A small, daemonless command line tool for running Windows containers, built
directly on the Host Compute Service (HCS) APIs. No Docker, no containerd,
no background service of its own: a single executable that talks to
`vmcompute` the same way hcsshim does.

> **Status: early work in progress.** Pulling a base image and running a
> process-isolated container in the foreground works.

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
winner image list
winner run --rm mcr.microsoft.com/windows/nanoserver:ltsc2025 cmd /c ver
winner list
winner delete <id>
```

Container operations live at the top level; other resources are grouped as
`<noun> <verb>` (`image pull`, `image list`, ...). `ls` and `rm` are accepted
as aliases for `list` and `delete`. A tty is detected automatically;
`--tty` / `--no-tty` override it.

Data is stored under `%ProgramData%\winner`.

## Building

```powershell
cargo build --release
```

The release binary is a single ~7 MB executable with no runtime dependencies.

## Roadmap

- [x] Registry pull with Windows platform selection
- [x] Base layer import
- [x] Scratch layer create / mount (process isolation)
- [x] Run a process-isolated container in the foreground
- [ ] Non-base layer import
- [ ] Networking (HCN)
- [ ] Hyper-V isolation
- [ ] Detached containers (`run -d`) and logs

See [docs/development.md](docs/development.md) for design notes.
