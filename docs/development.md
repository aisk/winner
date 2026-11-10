# Development notes

## Architecture

```
main.rs     CLI (clap): command definitions and dispatch only; value types
            the commands take live with the code that uses them
output.rs   `--format`, human-readable tables
pull.rs     registry pull (download only); the only place tokio is used
            (current_thread + block_on)
image.rs    local image records, reference matching
store.rs    layer store: store lock, layer import bookkeeping (import lock,
            markers), garbage collection of layers and blobs
layer/      layer import and scratch layer mount
  import.rs   tar -> backup streams (go-winio backuptar), base layer writer
  legacy.rs   non-base layers: hcsshim legacyLayerWriter + vmcompute ImportLayer
backup.rs   Win32 backup stream encoding, privileges, FILETIME helpers
hcs.rs      synchronous wrappers over computecore.dll (HCS v2)
hcn.rs      synchronous wrappers over computenetwork.dll (HCN, the HNS v2 API)
network.rs  networks, per-container endpoints and namespaces, port mappings
run.rs      create, run, start: resolving image, isolation, command, volumes
supervisor.rs  `execute` supervises one run of a container (v2 document for
            process isolation, v1 for Hyper-V); Ctrl+C handling
process.rs  process parameters, console forwarding (run and exec), env, quoting
control.rs  stop, kill, exec, logs, inspect, delete (by container ID via vmcompute)
shim.rs     detached supervisor: `execute` with output to the container log
stdio.rs    one blocking thread per stream; raw VT console for ttys
system.rs   system prune
lock.rs     file locks from share modes (supervisor, store, import)
state.rs    container state in %ProgramData%\winner\containers\<id>\state.json
```

A container run is always supervised by exactly one process holding the
only compute system handle (`ShouldTerminateOnLastHandleClosed`): the
foreground `run`, or a detached `winner shim <id>` that `start` / `run -d`
spawn and then wait on until the state leaves `created`. The supervisor
records `running` and the exit code; if it dies, vmcompute tears the
container down and the next state read reconciles the record.

There is no daemon. `vmcompute` (the system HCS service) owns container
lifetimes; winner keeps its own state on disk.

On-disk layout under `%ProgramData%\winner`:

```
blobs/<algo>/<hex>          downloaded layer blobs
layers/<algo>-<hex>/        imported read-only layers
images/<escaped-ref>.json   image records
containers/<id>/            state.json, scratch/, log, supervisor.lock
tmp/import-*/               staging of non-base layer imports
tmp/importing-<algo>-<hex>  marker: that layer's import has not finished
store.lock                  pulls (shared) vs. garbage collection (exclusive)
import.lock                 one layer import at a time
```

## System resources: ownership and cleanup

Some resources live outside winner's directory and outlive the process that
created them: the mounted scratch layer (vmcompute) and a container's HNS
endpoint and namespace. Only the compute system itself is tied to its
handle. Without a daemon, nobody is guaranteed to clean up after a killed
supervisor, so these follow three rules instead of being recorded in
state.json:

1. **Derivable IDs.** Every object's ID follows from the container ID
   (`NameToGuid("winner/endpoint/<id>")`, `.../namespace/<id>`, and the
   scratch directory path), so it can always be found again. state.json only
   records what the user asked for (network name, ports).
2. **Idempotent create and release.** A supervisor releases leftovers of its
   container before creating them again (unmount the scratch layer, delete
   the endpoint and namespace). Releasing something that is not there
   succeeds.
3. **Sweeping.** Before connecting a container, `network::sweep` enumerates
   HNS endpoints named `winner-<32 hex>` whose ID matches the derived one and
   releases those whose container nobody holds the lock of.

Who may touch a container's resources is decided by `supervisor.lock`
(`src/lock.rs`): it is held, without read or write sharing, by

- the supervisor (`run` in the foreground, or `winner shim`) for as long as
  it runs, including releasing the network and scratch layer after the exit
  is recorded;
- `create`, from before state.json is written until the caller starts the
  container (so `system prune` cannot delete a container being created);
- `system prune` and `network::sweep` while they release a container's
  resources. Checking "no supervisor" and then releasing would race a
  supervisor starting in between; taking the lock makes it atomic.

Opening the file fails with a sharing violation exactly while someone holds
it, and Windows drops the lock with the process. Unlike `status` in
state.json it cannot go stale. `stop` and `kill` wait until the lock is
free, not just until the status says exited, so a following `start` or
`delete` does not collide with the supervisor's cleanup; a shim that finds
the lock taken waits a few seconds for that cleanup before giving up.
`start` and `delete` of an exited container also wait for the lock (up to
30s), since a container can exit on its own: the supervisor then still shuts
the compute system down and releases network and scratch, and `logs
--follow` has already returned. They reload the state once they have it.

Image records are named after the reference with every byte other than
`[a-z0-9.-]` written as `_xx`. The earlier scheme mapped all of them to `_`,
so `a/b:c` and `a_b:c` (or tags differing in case, on NTFS) shared a file.

The namespace is deleted before the endpoint: a leftover endpoint is found
again by name, a namespace has no name.

Layers have the same problem in another shape: a pull imports layers before
it writes the image record that references them, so garbage collection
running meanwhile would see them as unused. `store.lock` is a reader-writer
lock (`store::Lock`) built from share modes: pulls open it sharing read and write (several
pulls coexist), garbage collection opens it sharing nothing. Collection
skips its turn while a pull runs; a pull waits while collection runs. With
the exclusive lock held, `tmp/import-*` directories are leftovers of
interrupted imports and are removed too.

A layer directory is complete only if no `tmp/importing-*` marker names it:
the marker is written before the import and removed after, so a pull killed
mid-import leaves it behind and the next pull (or garbage collection)
destroys the half-written layer instead of taking it as done. The marker
lives outside the layer directory because vmcompute owns that layout.
`import.lock` serializes imports across processes (two pulls sharing a
layer would write the same directory); downloads go to a per-process
`.partial-<pid>` file and are renamed into place.

Garbage collection gives up if any image record or container state cannot
be read: the layers it references would look unused. Image records and
state files are written to a temp file and renamed, so a crash cannot leave
a truncated one.

`system prune` applies all of this at once: containers without a live
supervisor, the network objects `sweep` finds, networks winner created that
no remaining container names, unreferenced layers and blobs, and with
`--all` images no remaining container uses.

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
| Container / process | `computecore` (HCS v2 JSON; v1 JSON for Hyper-V, see below) |

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

Non-base layers follow hcsshim's `legacyLayerWriter`: entries are staged in
`%ProgramData%\winner\tmp\import-*` in the transport format vmcompute
`ImportLayer(info, layerPath, stagingPath, parents topmost first)` consumes:

- `Files\...`: a plain file holding the attributes (u32 LE) followed by the
  raw backup stream; directories also get a `<name>.$wcidirs$` file in that
  format. Directory reparse points are staged without the placeholder
  directory (recreated just before import) so nothing lands inside them.
- `Hives\...`: restored with `BackupWrite`, security processing off.
- Staged files carry the entry's times with the attributes cleared.

`ImportLayer` cannot express whiteouts or hard links, so afterwards
(`legacyLayerWriterWrapper.Close`): whiteouts are removed from the new layer,
hard links are created (target in this layer if it was added here, otherwise
the topmost parent that has it), and directory times are re-applied because
both steps bump them. UtilityVM content in non-base layers is not supported
yet (only needed for Hyper-V isolation).

## Hyper-V isolation

A Hyper-V isolated container runs in a utility VM booted from the base
layer's `UtilityVM/` directory (prepared at import by `ProcessUtilityImage`:
`SystemTemplate.vhdx` and friends). There are two ways to drive one:

- **HCS v2** (what hcsshim and containerd do today): the caller creates the
  utility VM as its own compute system, adds the layers and scratch to it
  over SCSI / VSMB, and creates the container inside it by talking to the
  guest compute service (GCS) over a hvsocket bridge with its own protocol.
  That is most of hcsshim's `internal/uvm` and `internal/gcs`.
- **HCS v1** (what Docker's `libcontainerd/local` still does): one
  `ContainerConfig` document with `HvPartition: true`, and vmcompute boots
  and manages the VM itself. Same `HcsCreateComputeSystem` /
  `HcsCreateProcess` calls as process isolation, just a different document.

winner uses v1 (`spec::ContainerV1`). The document differs from the process
isolated one in a few places:

- `LayerFolderPath` is the scratch directory from `CreateSandboxLayer`. It is
  **not** mounted on the host; vmcompute attaches `sandbox.vhdx` to the VM.
- `Layers` is the same list as `Storage.Layers` (topmost first, ID =
  `NameToGuid(dir name)`), but with the keys spelled `ID` / `Path`.
- `HvRuntime.ImagePath` is the `UtilityVM` directory of the topmost layer
  that has one (only base layers do).
- Networking is `EndpointList: [endpoint id]` instead of a namespace: the
  endpoint is created the same way (derived ID, port mappings), vmcompute
  adds it to the VM as a NIC. No namespace is created; `release` deleting a
  missing namespace is a no-op. `--network host` is refused, since the VM
  has its own network stack.
- `TerminateOnLastHandleClosed` is the v1 name of
  `ShouldTerminateOnLastHandleClosed`.

Processes use the same `HcsCreateProcess` document (v1 and v2 share the
field names), and `ConsoleSize` modification, shutdown and terminate work
the same way, so `exec`, `stop`, `kill` and console resize need no changes.

**Never pull the power on a utility VM during its first run.** Terminating
the compute system (`HcsTerminateComputeSystem`) at any point of the first
run of a fresh scratch layer leaves one the VM never boots from again: the
next start fails with `0x80370111` (communication protocol error) when
`IgnoreFlushesDuringBoot` is set, and with flushes on it hangs in
`HcsStartComputeSystem` forever. Later runs survive it, but there is no
reason to rely on that. So:

- After the main process exits, the supervisor shuts the compute system down
  (`HcsShutDownComputeSystem`, 30s) and terminates only if that fails, as
  Docker does. Terminating right away also raced `stop`: the shutdown it
  requested was cut off by the supervisor's terminate.
- `kill` and Ctrl+C end the main process (`HcsTerminateProcess`, by the PID
  the supervisor records in state.json) instead of the compute system; the
  supervisor then shuts it down. Terminating the system stays the fallback.
- `IgnoreFlushesDuringBoot` is set only on the first start (`started` in
  state.json), as Docker does; it saves about 0.2s of a 1.5s start.
- Every HCS operation is waited on for at most 4 minutes (hcsshim's
  default), so a VM that cannot boot fails the start instead of hanging it.
  Only waiting for a process to exit is unbounded.

A supervisor killed during the first run still loses the container (the
last handle closes and vmcompute terminates the system).

Isolation is picked at `create`: process isolation when the host can run the
image build (`host::process_compatible`), else Hyper-V. An image newer than
the host cannot run at all, and asking for process isolation of an
incompatible image fails early instead of at start.

## Networking

HNS objects (all created through HCN):

| Object | winner's use |
|---|---|
| Network | `winner` NAT network, created on first use; user networks via `network create` |
| Endpoint | one per running container, carries IP, MAC, DNS and `PortMapping` policies |
| Namespace | one per running container, holds its endpoint |

A process-isolated container joins a namespace through
`Container.Networking.Namespace` in the HCS document (as hcsshim does for
schema v2): create the endpoint on the network, create the namespace, add the
endpoint (`HcnModifyNamespace` with
`{"ResourceType":"Endpoint","RequestType":"Add","Settings":{"EndpointId":"..."}}`),
then create the compute system. Everything is released after the compute
system is terminated (`Terminating` guard in `supervisor::execute`).

`--network host` uses the host's default namespace (`"Type": "HostDefault"`,
`NamespaceId` 1 in an HCN query; `IsDefault: true` in `hnsdiag -d`) instead: the container sees the host's
adapters and binds host ports directly. Nothing is created or released.

Details that matter:

- HCN returns strings allocated with `CoTaskMemAlloc` (HCS uses `LocalAlloc`).
- The namespace request key is `EndpointId`. With `ID` it fails with
  `HCN_E_ENDPOINT_NOT_FOUND` (0x803B0002), which looks like an ID problem.
- `HcnCreateEndpoint` / `HcnCreateNetwork` / `HcnCreateNamespace` keep the
  GUID the caller passes; that is what makes derived IDs work. Two winner
  processes creating the default network at once collide on the same ID
  instead of creating two networks.
- HCN reports GUIDs in either case; compare parsed GUIDs, not strings.
- A NAT network created without `Ipams` gets a free subnet picked by HNS
  (e.g. `172.24.176.0/20`), and comes back with `Flags: 8`
  (non-persistent): it is gone after a reboot, hence on-demand creation.
- Query results omit `Type` for NAT networks; `hnsdiag list networks` shows
  it.
- HNS fills the endpoint's DNS servers from the host; nothing to configure.
- Published ports are reachable through `localhost`, the host's own
  addresses and the container IP (checked on 26100). Containers on the same
  network reach each other by IP.
- `hnsdiag list networks|endpoints|namespaces [-d]` (elevated) shows what
  HNS holds; useful to spot leaks.

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
- **Console size**: with a tty, the host console size is polled every
  200 ms and changes are sent as `HcsModifyProcess`
  `{"Operation":"ConsoleSize","ConsoleSize":{"Height":..,"Width":..}}`
  (hcsshim `ResizeConsole`). Polling, because the stdin thread already
  consumes the console input, window-size events included. nanoserver has no
  `mode` command to check the size from inside.
- **`cmd /c` quoting**: cmd does not parse its command line with the MSVCRT
  rules, so an argument ending in a backslash (`"... dir C:\"`) arrives with
  the backslash doubled. Pass such commands as separate arguments.

- **`ArgsEscaped`**: Windows image configs may set `ArgsEscaped: true`, in
  which case Entrypoint/Cmd hold an already escaped command line. Those
  image-provided parts are joined verbatim; only user-supplied arguments are
  quoted.

- **Stale "running" records**: the compute system is created with
  `ShouldTerminateOnLastHandleClosed`, so a killed CLI takes its container
  down with it but leaves `status: running` on disk (and the scratch layer
  mounted). Reading container state reconciles it with
  `HcsEnumerateComputeSystems` (needs elevation; skipped otherwise), and
  `delete` unmounts before destroying the scratch layer. The reconciled
  status is never written back: while a container runs, its supervisor is
  the only writer of state.json (`create` and `start` write it only before a
  supervisor exists), and a read-modify-write from `list` / `inspect` once
  wiped the error a failing shim had just recorded.
- **Restart after a killed supervisor**: the scratch layer is still
  activated, and `ActivateLayer` fails with a sharing violation (0x80070020).
  `execute` unmounts first; the supervisor lock guarantees no live run uses it.
- **When a container counts as running**: only after `HcsCreateProcess`
  succeeds. Marking it right after the compute system starts made `start`
  report success for a process that then failed to launch.
- **Concurrent delete**: `delete --force` can remove a container directory
  while its `run` process is still finishing. Only `create` makes the
  directory; `ContainerState::save` drops the write when it is gone, so a
  late write (or a racing `start`) cannot resurrect a half-deleted container.
- **Ctrl+C in the foreground**: by default it kills winner, and with it the
  container (last handle closed), but skips recording the exit, releasing
  the network and scratch layer, and `--remove`. `run` installs a console
  control handler that ends the container's main process instead (see Hyper-V
  isolation for why not the whole container) and lets `execute`
  finish; the exit code is `0xC000013A` as for any Ctrl+C'd program (HCS
  itself reports `0xC0000001` for a process of a terminated container). With
  a tty, Ctrl+C is input for the container and only Ctrl+Break reaches the
  handler. `exec` installs the same handler but ends only its own process:
  without it the exec'd process kept running in the container after winner
  died. Testing it: a process started with `Start-Process` may have Ctrl+C
  ignored, and events generated after `AttachConsole` to another process's
  console never arrived; the e2e case starts winner from a helper in the
  same console and sends Ctrl+Break. Nano Server has no `tasklist`, but
  process-isolated container processes are visible on the host
  (`Win32_Process`).

- **Handle inheritance when spawning the shim**: `CreateProcess` runs with
  inheritance on, so the shim would inherit winner's own stdio when it is an
  inheritable pipe (a script capturing `run -d` output) and the caller would
  wait until the container exits. `spawn_shim` marks the standard handles
  non-inheritable first. It also asks for `CREATE_BREAKAWAY_FROM_JOB` in case
  the terminal or sudo kills its job on exit.
- **HCN "not found"**: there is no single error code for it (each kind has
  its own, some none), so `hcn::get` opens the object and, only if that
  fails, enumerates to check whether it still exists: gone means `None`,
  anything else is an error. Treating every open failure as "not found" hid
  real failures (an unreachable HNS looked like "no endpoint").
- **Image user**: Windows images set `User` (nanoserver: `ContainerUser`).
  It goes into `ProcessParameters.User`; without it processes run as
  `ContainerAdministrator`.

## Debugging

- Run the built exe elevated rather than `sudo cargo run`, so `target/`
  is not populated with files owned by an elevated token.
- `scripts/e2e.ps1` runs every end-to-end check under one elevation
  (`sudo pwsh -NoProfile -File scripts/e2e.ps1 [-Full] [-Only <case>]`).
- `WINNER_KEEP_FAILED=1` keeps a partially imported layer on disk instead of
  destroying it (use `sudo --preserve-env`).
- hcsshim (`internal/wclayer`, `internal/layers/wcow_mount.go`,
  `pkg/ociwclayer`) and go-winio (`backuptar`, `backup.go`, `reparse.go`,
  `ea.go`) are the reference implementations.
