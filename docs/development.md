# Development notes

## Architecture

```
main.rs    CLI (clap)
pull.rs    registry pull; the only place tokio is used (current_thread + block_on)
image.rs   local image records
layer/     layer import and scratch layer mount
  import.rs  tar -> base layer, port of hcsshim baseLayerWriter + go-winio backuptar
backup.rs  Win32 backup stream encoding, privileges, FILETIME helpers
hcs.rs     synchronous wrappers over computecore.dll (HCS v2)
run.rs     foreground lifecycle: scratch -> mount -> create -> start -> exec -> wait
stdio.rs   one blocking thread per stream; raw VT console for ttys
state.rs   container state in %ProgramData%\winner\containers\<id>\state.json
shim.rs    planned per-container supervisor for `run -d`
```

There is no daemon. `vmcompute` (the system HCS service) owns container
lifetimes; winner keeps its own state on disk.

On-disk layout under `%ProgramData%\winner`:

```
blobs/<algo>/<hex>          downloaded layer blobs
layers/<algo>-<hex>/        imported read-only layers
images/<escaped-ref>.json   image records
containers/<id>/            state.json + scratch/
```

## Which APIs, and why

Process-isolated (wcifs) containers follow hcsshim, which still uses the
legacy **vmcompute v1** layer APIs, not computestorage:

| Step | API |
|---|---|
| Process base layer after extraction | `vmcompute!ProcessBaseImage` (+ `ProcessUtilityImage` if `UtilityVM/` exists) |
| Create scratch (`sandbox.vhdx`) | `vmcompute!CreateSandboxLayer` |
| Mount scratch | `ActivateLayer` -> `PrepareLayer` -> `GetLayerMountPath` |
| Unmount | `UnprepareLayer` -> `DeactivateLayer` |
| Delete a layer dir | `computestorage!HcsDestroyLayer` |
| Container / process | `computecore` (HCS v2 JSON) |

`HcsInitializeWritableLayer` / `HcsAttachLayerStorageFilter` target
VHD/CimFS layers and fail with `ERROR_PATH_NOT_FOUND` for this use.

Details that matter:

- v1 calls take a `DriverInfo { Flavour: 1, HomeDir: L"" }` and the full
  layer path as the "id".
- Layer GUIDs come from `NameToGuid(<layer directory name>)`, as hcsshim does.
- `WC_LAYER_DESCRIPTOR` lists parent layers topmost first.
- `Container.Storage.Path` is the volume path from `GetLayerMountPath`
  (`\\?\Volume{...}`), not the scratch directory.
- vmcompute v1 exports are not in the `windows` crate; they are bound with
  `windows_link::link!` (raw-dylib, no import library needed).

## Layer import

A base layer tar has `Files/`, `Hives/` and optionally `UtilityVM/`. Each
entry is created with backup semantics and restored through one `BackupWrite`
stream carrying the security descriptor (`MSWINDOWS.rawsd`), EAs
(`MSWINDOWS.xattr.*`), reparse data (symlinks / `MSWINDOWS.mountpoint`
junctions), file data and alternate data streams (`name:stream` entries that
follow the file). Directory times are re-applied at the end, children first.
After `ProcessBaseImage` the layer contains `blank.vhdx`, `blank-base.vhdx`
and a `layout` file (`vhd-with-hives`).

Paths are validated so nothing escapes the layer root: no `..`, no drive or
stream syntax, and no traversal through a reparse point created earlier in
the same layer.

Non-base layers need hcsshim's `legacyLayerWriter` (staging directory,
`tombstones.txt`, then vmcompute `ImportLayer`); not ported yet.

## Environment pitfalls

- **Containers feature**: `Get-WindowsOptionalFeature` may fail ("class not
  registered") or hang while servicing is pending a reboot. A registry check
  works without DISM:
  - `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\Notifications\OptionalFeatures\Containers`,
    value `Selection` (1 = enabled)
  - `CmService` is not a reliable signal: on Windows 11 26100 the feature is
    enabled and containers run without that service present
  - `...\Component Based Servicing\RebootPending` exists while a reboot is pending
- **Defender Controlled Folder Access** blocks direct disk writes from
  unknown executables (event 1127 in
  `Microsoft-Windows-Windows Defender/Operational`). It showed up as
  `ProcessBaseImage: Access is denied`.
- **HCS diagnostics**: `Microsoft-Windows-Hyper-V-Compute-Operational`
  (needs elevation) logs every create call with the full JSON document and
  its HRESULT.
- **Host build**: the host may report a build like 26220 while the kernel is
  26100.x (enablement package); check `ntdll.dll`'s file version. hcsshim's
  rule (`osversion.CheckHostAndContainerCompat`): from LTSC2022 on, a host
  runs images from the latest LTSC below it up to its own build.

- **Stdin EOF**: closing our end of the stdin pipe from
  `HcsCreateProcess` does not reach the container, because vmcompute relays
  the stream. Send `HcsModifyProcess` with
  `{"Operation":"CloseHandle","CloseHandle":{"Handle":"StdIn"}}` after the
  copy finishes (hcsshim's `CloseStdin`), or `echo x | winner run ... cmd`
  hangs forever.
- **`cmd /c` quoting**: cmd does not parse its command line with the MSVCRT
  rules, so an argument ending in a backslash (`"... dir C:\"`) arrives with
  the backslash doubled. Pass such commands as separate arguments.

## Debugging

- Run the built exe elevated rather than `sudo cargo run`, so `target/`
  is not populated with files owned by an elevated token.
- `WINNER_KEEP_FAILED=1` keeps a partially imported layer on disk instead of
  destroying it (use `sudo --preserve-env`).
- hcsshim (`internal/wclayer`, `internal/layers/wcow_mount.go`,
  `pkg/ociwclayer`) and go-winio (`backuptar`, `backup.go`, `reparse.go`,
  `ea.go`) are the reference implementations.
