# End-to-end tests for winner. Needs an elevated shell, so run the whole
# suite under a single elevation:
#
#   sudo pwsh -NoProfile -File scripts/e2e.ps1 [-Full] [-Only <pattern>]
#
# -Full also runs the image deletion tests, which delete the .NET test image
# (about 225 MB to download again next time), and pulls an LTSC 2022 image
# to check that an image the host cannot run process-isolated gets hyper-v.
# Results are printed and written to target\e2e.log.

param(
    [switch]$Full,
    [string]$Only = '*',
    [string]$Winner = (Join-Path $PSScriptRoot '..\target\debug\winner.exe')
)

$ErrorActionPreference = 'Stop'
$Winner = (Resolve-Path $Winner).Path
$Nano = 'mcr.microsoft.com/windows/nanoserver:ltsc2025'
$Dotnet = 'mcr.microsoft.com/dotnet/runtime:10.0-nanoserver-ltsc2025'
# ASP.NET sample app, listens on port 8080.
$Web = 'mcr.microsoft.com/dotnet/samples:aspnetapp-nanoserver-ltsc2025'
$Log = Join-Path $PSScriptRoot '..\target\e2e.log'

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error 'run this script elevated: sudo pwsh -NoProfile -File scripts/e2e.ps1'
}

$script:Passed = 0
$script:Failed = [System.Collections.Generic.List[string]]::new()
$script:Lines = [System.Collections.Generic.List[string]]::new()

function Say([string]$Text, [string]$Color = 'Gray') {
    Write-Host $Text -ForegroundColor $Color
    $script:Lines.Add($Text)
}

# Run winner with a timeout. Stdin is redirected (and closed after $Stdin is
# written), so tty auto-detection always picks "no tty".
function W {
    param([string[]]$A, [string]$Stdin = '', [int]$Timeout = 120)
    $psi = [System.Diagnostics.ProcessStartInfo]::new($Winner)
    foreach ($x in $A) { $psi.ArgumentList.Add($x) }
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    # winner writes UTF-8 to pipes; .NET would otherwise decode with the
    # console code page (e.g. 936), mangling localized Windows error text.
    $psi.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $psi.StandardErrorEncoding = [Text.UTF8Encoding]::new($false)
    $p = [System.Diagnostics.Process]::Start($psi)
    $out = $p.StandardOutput.ReadToEndAsync()
    $err = $p.StandardError.ReadToEndAsync()
    if ($Stdin) { $p.StandardInput.Write($Stdin) }
    $p.StandardInput.Close()
    if (-not $p.WaitForExit($Timeout * 1000)) {
        $p.Kill($true)
        $p.WaitForExit()
        $code = 'timeout'
    } else {
        $p.WaitForExit()
        $code = $p.ExitCode
    }
    $r = [pscustomobject]@{ Code = $code; Out = $out.Result.TrimEnd(); Err = $err.Result.TrimEnd() }
    $script:Lines.Add("    > winner $($A -join ' ')  [exit $code]")
    foreach ($l in (($r.Out + "`n" + $r.Err).Trim() -split "`r?`n" | Select-Object -First 15)) { $script:Lines.Add("      $l") }
    $r
}

function Check([string]$What, [bool]$Ok, $Result = $null) {
    if ($Ok) {
        $script:Passed++
        Say "  ok   $What" 'Green'
    } else {
        $script:Failed.Add("$($script:Case): $What")
        Say "  FAIL $What" 'Red'
        if ($Result) {
            Say "       exit: $($Result.Code)" 'DarkGray'
            if ($Result.Out) { Say "       out:  $($Result.Out -replace "`r?`n", "`n             ")" 'DarkGray' }
            if ($Result.Err) { Say "       err:  $($Result.Err -replace "`r?`n", "`n             ")" 'DarkGray' }
        }
    }
}

function Case([string]$Name, [scriptblock]$Body) {
    if ($Name -notlike $Only) { return }
    $script:Case = $Name
    Say "== $Name" 'Cyan'
    try { & $Body } catch { Check "no exception ($_)" $false }
}

function State([string]$Container) {
    $r = W @('inspect', $Container)
    if ($r.Code -ne 0) { return $null }
    ($r.Out | ConvertFrom-Json)[0]
}

function WaitUntil([scriptblock]$Cond, [int]$Seconds = 30) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        if (& $Cond) { return $true }
        Start-Sleep -Milliseconds 500
    }
    $false
}

# Names of HNS endpoints winner created (winner-<container id>).
function Endpoints {
    @(hnsdiag list endpoints | Select-String -Pattern 'Name\s*:\s*(winner-[0-9a-f]{32})' | ForEach-Object { $_.Matches[0].Groups[1].Value })
}

function HttpStatus([string]$Url) {
    try { (Invoke-WebRequest -UseBasicParsing -TimeoutSec 5 $Url).StatusCode } catch { 0 }
}

function Cleanup {
    try {
        $r = W @('list', '--format', 'json')
        if ($r.Code -eq 0 -and $r.Out) {
            foreach ($c in ($r.Out | ConvertFrom-Json)) {
                if ($c.name -like 'e2e-*') { W @('delete', '--force', $c.id) | Out-Null }
            }
        }
        W @('network', 'delete', 'e2e-net') | Out-Null
    } catch {
        Say "cleanup failed: $_" 'Yellow'
    }
}

Say "winner e2e  $(Get-Date -Format s)  $Winner"
Cleanup

# --- basics ----------------------------------------------------------------

Case 'pull' {
    $r = W @('image', 'pull', $Nano) -Timeout 900
    Check 'image pull nanoserver' ($r.Code -eq 0) $r
    $r = W @('image', 'pull', 'nanoserver:ltsc2025')
    Check 'pull with a short name is refused' ($r.Code -ne 0 -and $r.Err -match 'registry') $r
}

Case 'run' {
    $r = W @('run', '--remove', $Nano, 'cmd', '/c', 'ver')
    Check 'prints the container OS version' ($r.Code -eq 0 -and $r.Out -match 'Microsoft Windows') $r
    $r = W @('run', '--remove', $Nano, 'cmd', '/c', 'exit 7')
    Check 'passes the exit code through' ($r.Code -eq 7) $r
    $r = W @('run', '--remove', $Nano) -Stdin "echo stdin-ok`r`n" -Timeout 60
    Check 'default command reads stdin and exits at EOF' ($r.Code -eq 0 -and $r.Out -match 'stdin-ok') $r
    $r = W @('run', '--remove', '--entrypoint', 'cmd.exe', $Nano, '/c', 'echo ep-ok')
    Check '--entrypoint replaces the command' ($r.Code -eq 0 -and $r.Out -match 'ep-ok') $r
    $r = W @('run', '--remove', '--pull', 'never', 'nanoserver:ltsc2025', 'cmd', '/c', 'echo short-ok')
    Check 'short name finds the local image' ($r.Code -eq 0 -and $r.Out -match 'short-ok') $r
    $r = W @('run', '--remove', 'example.invalid/nothing:1')
    Check 'winner failures exit 125' ($r.Code -eq 125) $r
}

Case 'pull-always' {
    $r = W @('run', '--remove', '--pull', 'always', 'nanoserver:ltsc2025', 'cmd', '/c', 'echo always-ok') -Timeout 900
    Check '--pull always with a short local name' ($r.Code -eq 0 -and $r.Out -match 'always-ok') $r
}

Case 'options' {
    $r = W @('run', '--remove', '-e', 'FOO=bar', '-w', 'C:\Windows', $Nano, 'cmd', '/c', 'echo [%FOO%] & cd')
    Check '--env and --workdir' ($r.Code -eq 0 -and $r.Out -match '\[bar\]' -and $r.Out -match '(?m)^C:\\Windows\s*$') $r
    $r = W @('run', '--remove', $Nano, 'cmd', '/c', 'echo user=%USERNAME%')
    Check 'image default user (ContainerUser)' ($r.Out -match 'user=ContainerUser') $r
    $r = W @('run', '--remove', '-u', 'ContainerAdministrator', $Nano, 'cmd', '/c', 'echo user=%USERNAME%')
    Check '--user overrides it' ($r.Out -match 'user=ContainerAdministrator') $r
}

Case 'volumes' {
    $dir = Join-Path ([IO.Path]::GetTempPath()) "winner-e2e-$PID"
    New-Item -ItemType Directory -Force $dir | Out-Null
    Set-Content (Join-Path $dir 'hello.txt') 'from-host'
    try {
        # Host ACLs still apply: the temp directory is only open to its owner
        # and administrators, so these run as ContainerAdministrator.
        $r = W @('run', '--remove', '-u', 'ContainerAdministrator', '-v', "${dir}:C:\data:ro", $Nano, 'cmd', '/c', 'type C:\data\hello.txt')
        Check 'read a mapped directory' ($r.Code -eq 0 -and $r.Out -match 'from-host') $r
        $r = W @('run', '--remove', '-u', 'ContainerAdministrator', '-v', "${dir}:C:\data:ro", $Nano, 'cmd', '/c', 'echo x> C:\data\ro.txt')
        Check 'read-only mapping refuses writes' ($r.Code -ne 0 -and -not (Test-Path (Join-Path $dir 'ro.txt'))) $r
        $r = W @('run', '--remove', '-v', "${dir}:C:\data:ro", $Nano, 'cmd', '/c', 'type C:\data\hello.txt')
        Check 'ContainerUser is subject to host ACLs' ($r.Code -ne 0 -and ($r.Out + $r.Err) -match 'denied') $r
        $r = W @('run', '--remove', '-u', 'ContainerAdministrator', '-v', "${dir}:C:\data", $Nano, 'cmd', '/c', 'echo from-container> C:\data\rw.txt')
        $written = Test-Path (Join-Path $dir 'rw.txt')
        Check 'read-write mapping reaches the host' ($r.Code -eq 0 -and $written) $r
        $r = W @('run', '--remove', '-v', 'C:\definitely\missing:C:\data', $Nano)
        Check 'missing host directory is rejected' ($r.Code -eq 125) $r
    } finally {
        Remove-Item -Recurse -Force $dir
    }
}

Case 'names' {
    $r = W @('run', '--name', 'e2e-named', $Nano, 'cmd', '/c', 'exit 3')
    Check 'named run' ($r.Code -eq 3) $r
    $r = W @('run', '--name', 'e2e-named', $Nano, 'cmd', '/c', 'ver')
    Check 'duplicate name is refused' ($r.Code -eq 125 -and $r.Err -match 'already in use') $r
    $s = State 'e2e-named'
    Check 'inspect by name shows exit code 3' ($s -and $s.status -eq 'exited' -and $s.exit_code -eq 3)
    $r = W @('list', '--format', 'json')
    Check 'list --format json includes it' ($r.Code -eq 0 -and (($r.Out | ConvertFrom-Json).name -contains 'e2e-named')) $r
    $r = W @('delete', $s.id.Substring(0, 8))
    Check 'delete by ID prefix' ($r.Code -eq 0) $r
    $r = W @('delete', 'e2e-named', 'e2e-nothing')
    Check 'delete of a missing container fails' ($r.Code -ne 0) $r
}

# --- detached lifecycle ----------------------------------------------------

Case 'detach' {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $r = W @('run', '-d', '--name', 'e2e-bg', $Nano, 'ping', '-n', '120', '127.0.0.1')
    Check 'run -d returns promptly with the ID' ($r.Code -eq 0 -and $r.Out -match '^[0-9a-f]{32}$' -and $sw.Elapsed.TotalSeconds -lt 60) $r
    Check 'it is running' ((State 'e2e-bg').status -eq 'running')
    Check 'logs show its output' (WaitUntil { (W @('logs', 'e2e-bg')).Out -match 'Reply from 127.0.0.1' })
    $r = W @('exec', 'e2e-bg', 'cmd', '/c', 'echo exec-ok')
    Check 'exec runs in it' ($r.Code -eq 0 -and $r.Out -match 'exec-ok') $r
    $r = W @('exec', 'e2e-bg', 'cmd', '/c', 'exit 5')
    Check 'exec passes the exit code through' ($r.Code -eq 5) $r
    $r = W @('delete', 'e2e-bg')
    Check 'delete refuses a running container' ($r.Code -ne 0 -and $r.Err -match 'running') $r
    $r = W @('stop', '--timeout', '5', 'e2e-bg') -Timeout 60
    Check 'stop' ($r.Code -eq 0) $r
    Check 'it is exited' ((State 'e2e-bg').status -eq 'exited')
    $r = W @('logs', '--follow', 'e2e-bg') -Timeout 15
    Check 'logs --follow returns for a stopped container' ($r.Code -eq 0) $r
    $r = W @('start', 'e2e-bg')
    Check 'start it again' ($r.Code -eq 0 -and (State 'e2e-bg').status -eq 'running') $r
    $r = W @('kill', 'e2e-bg')
    Check 'kill' ($r.Code -eq 0 -and (State 'e2e-bg').status -eq 'exited') $r
    $r = W @('start', 'e2e-bg')
    Check 'start right after kill (supervisor cleanup has finished)' ($r.Code -eq 0) $r
    W @('kill', 'e2e-bg') | Out-Null
    $r = W @('delete', 'e2e-bg')
    Check 'delete' ($r.Code -eq 0) $r
}

Case 'create-start' {
    $r = W @('create', '--name', 'e2e-created', $Nano, 'cmd', '/c', 'echo created-ok')
    Check 'create' ($r.Code -eq 0) $r
    Check 'status is created' ((State 'e2e-created').status -eq 'created')
    $r = W @('start', 'e2e-created')
    Check 'start' ($r.Code -eq 0) $r
    $r = W @('logs', '--follow', 'e2e-created') -Timeout 60
    Check 'logs --follow prints the output and ends' ($r.Code -eq 0 -and $r.Out -match 'created-ok') $r
    $s = State 'e2e-created'
    Check 'exited with code 0' ($s.status -eq 'exited' -and $s.exit_code -eq 0)
    $r = W @('delete', 'e2e-created')
    Check 'delete right after (waits for the supervisor to finish)' ($r.Code -eq 0) $r
}

Case 'detach-remove' {
    $r = W @('run', '-d', '--remove', '--name', 'e2e-gone', $Nano, 'cmd', '/c', 'echo bye')
    Check 'run -d --remove' ($r.Code -eq 0) $r
    Check 'container deletes itself after exiting' (WaitUntil { -not (State 'e2e-gone') })
}

Case 'start-failure' {
    # HCS accepts a missing working directory, so use a missing executable.
    $r = W @('create', '--name', 'e2e-bad', '--entrypoint', 'C:\does\not\exist.exe', $Nano)
    $r = W @('start', 'e2e-bad')
    Check 'start reports a failure to launch the process' ($r.Code -ne 0) $r
    $s = State 'e2e-bad'
    Check 'the error is kept in the state' ($s -and $s.error)
    $r = W @('run', '--remove', '--entrypoint', 'C:\does\not\exist.exe', $Nano)
    Check 'foreground run of a missing executable exits 125' ($r.Code -eq 125) $r
    W @('delete', '--force', 'e2e-bad') | Out-Null
}

Case 'orphan' {
    W @('run', '-d', '--name', 'e2e-orphan', $Nano, 'ping', '-n', '120', '127.0.0.1') | Out-Null
    $id = (State 'e2e-orphan').id
    $shim = Get-CimInstance Win32_Process -Filter "Name = 'winner.exe'" | Where-Object { $_.CommandLine -match "shim $id" }
    Check 'shim process found' ([bool]$shim)
    if ($shim) { Stop-Process -Id $shim.ProcessId -Force }
    Check 'killed supervisor is noticed as exited' (WaitUntil { (State 'e2e-orphan').status -eq 'exited' })
    Check 'its endpoint is left behind' ((Endpoints) -contains "winner-$id")
    $r = W @('start', 'e2e-orphan')
    Check 'it starts again (stale mount and endpoint replaced)' ($r.Code -eq 0 -and (State 'e2e-orphan').status -eq 'running') $r
    W @('kill', 'e2e-orphan') | Out-Null
    $r = W @('delete', 'e2e-orphan')
    Check 'delete without --force' ($r.Code -eq 0) $r
    Check 'no endpoint is left' (-not ((Endpoints) -contains "winner-$id"))
}

Case 'interrupt' {
    # A foreground run or exec needs a console of its own, so a helper pwsh
    # opens one, starts winner in it, and sends Ctrl+Break to that console
    # while swallowing the event itself. (Sending from outside with
    # AttachConsole never arrived.) Ctrl+Break because a process started this
    # way may have Ctrl+C ignored; winner handles both the same way.
    # Mode `run` waits for container e2e-int to run first, `exec` a moment.
    $helper = Join-Path ([IO.Path]::GetTempPath()) "winner-e2e-int-$PID.ps1"
    Set-Content $helper @'
param($Winner, $Mode)
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class Ctrl {
    public delegate bool Handler(uint e);
    static Handler swallow = e => true;
    [DllImport("kernel32.dll")] static extern bool SetConsoleCtrlHandler(Handler h, bool add);
    [DllImport("kernel32.dll")] public static extern bool GenerateConsoleCtrlEvent(uint e, uint group);
    public static void Swallow() { SetConsoleCtrlHandler(swallow, true); }
}
"@
$w = Start-Process -FilePath $Winner -ArgumentList $args -NoNewWindow -PassThru
if ($Mode -eq 'run') {
    $deadline = (Get-Date).AddSeconds(60)
    while ((Get-Date) -lt $deadline) {
        $s = & $Winner inspect e2e-int 2>$null | ConvertFrom-Json
        if ($s -and $s[0].status -eq 'running') { break }
        Start-Sleep -Milliseconds 500
    }
} else {
    Start-Sleep 5
}
[Ctrl]::Swallow()
if (-not [Ctrl]::GenerateConsoleCtrlEvent(1, 0)) { exit 3 }
if (-not $w.WaitForExit(30000)) { $w.Kill(); exit 4 }
exit $w.ExitCode
'@
    $pwsh = (Get-Process -Id $PID).Path
    function Interrupt([string]$Mode, [string[]]$A) {
        Start-Process -FilePath $pwsh -ArgumentList (@('-NoProfile', '-File', $helper, $Winner, $Mode) + $A) -WindowStyle Minimized -PassThru -Wait
    }
    # Nano Server has no tasklist, but process-isolated container processes
    # show up on the host; the exec'd one pings an address of its own.
    function Pings([string]$Address) {
        @(Get-CimInstance Win32_Process -Filter "Name = 'PING.EXE'" | Where-Object { $_.CommandLine -match [regex]::Escape($Address) }).Count
    }
    try {
        $h = Interrupt 'run' @('run', '--name', 'e2e-int', '--no-tty', $Nano, 'ping', '-n', '120', '127.0.0.1')
        $s = State 'e2e-int'
        Check 'run: winner exits with the Ctrl+C exit code (its handler ran)' ($h.ExitCode -eq -1073741510) ([pscustomobject]@{ Code = $h.ExitCode })
        Check 'run: the exit is recorded' ($s -and $s.status -eq 'exited' -and $s.exit_code -eq 3221225786)
        Check 'run: its endpoint is released' ($s -and -not ((Endpoints) -contains "winner-$($s.id)"))
        W @('delete', '--force', 'e2e-int') | Out-Null

        W @('run', '-d', '--name', 'e2e-int', $Nano, 'ping', '-n', '600', '127.0.0.2') | Out-Null
        $h = Interrupt 'exec' @('exec', '--no-tty', 'e2e-int', 'ping', '-n', '120', '127.0.0.3')
        Check 'exec: winner exits with the Ctrl+C exit code' ($h.ExitCode -eq -1073741510) ([pscustomobject]@{ Code = $h.ExitCode })
        Check 'exec: the container keeps running' ((State 'e2e-int').status -eq 'running')
        $main, $execd = (Pings '127.0.0.2'), (Pings '127.0.0.3')
        Check "exec: the exec'd process is ended, the main one is not" ($main -eq 1 -and $execd -eq 0) ([pscustomobject]@{ Main = $main; Execd = $execd })
    } finally {
        Remove-Item -Force $helper
        W @('delete', '--force', 'e2e-int') | Out-Null
    }
}

# --- networking ------------------------------------------------------------

Case 'network' {
    $r = W @('run', '--remove', $Nano, 'ipconfig')
    Check 'default network gives an IPv4 address' ($r.Code -eq 0 -and $r.Out -match 'IPv4 Address') $r
    $r = W @('network', 'list', '--format', 'json')
    Check 'the winner network exists' ($r.Code -eq 0 -and (($r.Out | ConvertFrom-Json).Name -contains 'winner')) $r
    $r = W @('run', '--remove', $Nano, 'curl.exe', '-sS', '-o', 'NUL', '-w', '%{http_code}', 'https://mcr.microsoft.com/v2/') -Timeout 60
    Check 'DNS and outbound HTTPS' ($r.Code -eq 0 -and $r.Out -match '^(200|401)$') $r
    $r = W @('run', '--remove', '--network', 'none', $Nano, 'ipconfig')
    Check '--network none has no adapter' ($r.Code -eq 0 -and $r.Out -notmatch 'IPv4 Address') $r
    $hostIp = (Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.IPAddress -ne '127.0.0.1' -and $_.AddressState -eq 'Preferred' } | Select-Object -First 1).IPAddress
    $r = W @('run', '--remove', '--network', 'host', $Nano, 'ipconfig')
    Check '--network host sees the host adapters' ($r.Code -eq 0 -and $r.Out -match [regex]::Escape($hostIp)) $r
    $r = W @('run', '--remove', '--network', 'host', $Nano, 'ipconfig')
    Check '--network host again (host namespace survives)' ($r.Code -eq 0 -and $r.Out -match [regex]::Escape($hostIp)) $r
    $r = W @('create', '--network', 'none', '-p', '18080:8080', $Nano)
    Check '--publish with --network none is refused' ($r.Code -ne 0) $r
    $r = W @('create', '--network', 'host', '-p', '18080:8080', $Nano)
    Check '--publish with --network host is refused' ($r.Code -ne 0) $r
    $r = W @('create', '-p', '18080', $Nano)
    Check 'malformed --publish is refused' ($r.Code -ne 0) $r
    $r = W @('create', '--network', 'e2e-missing', $Nano)
    Check 'unknown network is refused' ($r.Code -ne 0 -and $r.Err -match 'not found') $r
}

Case 'publish' {
    $r = W @('image', 'pull', $Web) -Timeout 1800
    Check 'pull the web sample' ($r.Code -eq 0) $r
    $r = W @('run', '-d', '--name', 'e2e-web', '-p', '18080:8080', $Web)
    Check 'run -d --publish' ($r.Code -eq 0) $r
    Check 'reachable on localhost' (WaitUntil { (HttpStatus 'http://localhost:18080/') -eq 200 } 60)
    $ip = (State 'e2e-web').endpoint.ip_address
    Check 'inspect shows the endpoint' ([bool]$ip)
    Check 'reachable on the container IP' ((HttpStatus "http://${ip}:8080/") -eq 200)
    $r = W @('list')
    Check 'list shows the ports' ($r.Out -match '18080->8080/tcp') $r
    $r = W @('run', '--remove', $Nano, 'curl.exe', '-sS', '-o', 'NUL', '-w', '%{http_code}', "http://${ip}:8080/")
    Check 'container to container' ($r.Out -match '^200$') $r
    $r = W @('run', '-d', '--name', 'e2e-web2', '-p', '18080:8080', $Web)
    Check 'a host port already published is refused' ($r.Code -ne 0) $r
    W @('delete', '--force', 'e2e-web2') | Out-Null
    $r = W @('delete', '--force', 'e2e-web')
    Check 'delete --force' ($r.Code -eq 0) $r
    Check 'no endpoint is left' ((Endpoints).Count -eq 0)
    Check 'port is released' ((HttpStatus 'http://localhost:18080/') -eq 0)
}

Case 'network-manage' {
    $r = W @('network', 'create', '--subnet', '172.31.250.0/24', 'e2e-net')
    Check 'network create' ($r.Code -eq 0) $r
    $r = W @('network', 'create', 'e2e-net')
    Check 'duplicate network name is refused' ($r.Code -ne 0) $r
    $r = W @('network', 'list')
    Check 'network list shows it as owned by winner' ($r.Out -match 'e2e-net\s+NAT\s+172\.31\.250\.0/24\s+winner') $r
    $r = W @('run', '--remove', '--network', 'e2e-net', $Nano, 'ipconfig')
    Check 'container gets an address in its subnet' ($r.Code -eq 0 -and $r.Out -match '172\.31\.250\.') $r
    W @('run', '-d', '--name', 'e2e-onnet', '--network', 'e2e-net', $Nano, 'ping', '-n', '120', '127.0.0.1') | Out-Null
    $r = W @('network', 'delete', 'e2e-net')
    Check 'network in use is not deleted' ($r.Code -ne 0 -and $r.Err -match 'in use') $r
    W @('delete', '--force', 'e2e-onnet') | Out-Null
    $r = W @('network', 'delete', 'e2e-net')
    Check 'network delete' ($r.Code -eq 0) $r
    $r = W @('network', 'list')
    Check 'it is gone' ($r.Out -notmatch 'e2e-net') $r
    $r = W @('network', 'create', 'host')
    Check 'reserved names are refused' ($r.Code -ne 0) $r
    if ((W @('network', 'list')).Out -match 'Default Switch') {
        $r = W @('network', 'delete', 'Default Switch')
        Check 'networks of other tools are not deleted' ($r.Code -ne 0 -and $r.Err -match 'not created by winner') $r
    }
}

# --- non-base layers -------------------------------------------------------

Case 'layers' {
    $r = W @('image', 'pull', $Dotnet) -Timeout 1800
    Check 'pull a multi-layer image' ($r.Code -eq 0) $r
    $r = W @('run', '--remove', $Dotnet, 'cmd', '/c', 'dotnet --list-runtimes')
    Check 'files from upper layers and PATH from their registry hives' ($r.Code -eq 0 -and $r.Out -match 'Microsoft\.NETCore\.App 10\.') $r
    $r = W @('run', '--remove', $Dotnet, 'cmd', '/c', 'echo user=%USERNAME% v=%DOTNET_VERSION%')
    Check 'image user and env' ($r.Out -match 'user=ContainerUser' -and $r.Out -match 'v=10\.') $r
}

# --- system prune ----------------------------------------------------------

Case 'prune' {
    # prune is not scoped to the tests: skip it rather than delete stopped
    # containers that belong to the user.
    $foreign = @(((W @('list', '--format', 'json')).Out | ConvertFrom-Json) | Where-Object { $_.status -ne 'running' -and $_.name -notlike 'e2e-*' })
    if ($foreign.Count -gt 0) {
        Say "  skip: $($foreign.Count) stopped containers not created by the tests" 'Yellow'
        return
    }
    W @('run', '-d', '--name', 'e2e-alive', $Nano, 'ping', '-n', '120', '127.0.0.1') | Out-Null
    W @('create', '--name', 'e2e-stopped', $Nano) | Out-Null
    W @('network', 'create', 'e2e-net') | Out-Null
    $tmp = Join-Path $env:ProgramData 'winner\tmp\import-e2e'
    New-Item -ItemType Directory -Force $tmp | Out-Null
    # An import killed halfway: the layer directory plus its marker.
    $halfLayer = Join-Path $env:ProgramData 'winner\layers\sha256-e2e0000000000000000000000000000000000000000000000000000000000000'
    $marker = Join-Path $env:ProgramData 'winner\tmp\importing-sha256-e2e0000000000000000000000000000000000000000000000000000000000000'
    New-Item -ItemType Directory -Force $halfLayer | Out-Null
    New-Item -ItemType File -Force $marker | Out-Null
    $r = W @('system', 'prune')
    Check 'refuses to run without a terminal or --force' ($r.Code -ne 0 -and $r.Err -match '--force') $r
    $r = W @('system', 'prune', '--force') -Timeout 300
    Check 'system prune --force' ($r.Code -eq 0 -and $r.Out -match 'reclaimed') $r
    Check 'stopped container deleted' (-not (State 'e2e-stopped'))
    Check 'running container kept' ((State 'e2e-alive').status -eq 'running')
    $nets = (W @('network', 'list')).Out
    Check 'unused network deleted, the one in use kept' ($nets -notmatch 'e2e-net' -and $nets -match '(?m)^winner\s') $nets
    Check 'leftover import staging deleted' (-not (Test-Path $tmp))
    Check 'half-imported layer and its marker deleted' (-not (Test-Path $halfLayer) -and -not (Test-Path $marker))
    $r = W @('image', 'list', '--format', 'json')
    Check 'images kept without --all' ((($r.Out | ConvertFrom-Json).reference) -contains $Nano) $r
    W @('delete', '--force', 'e2e-alive') | Out-Null
}

# --- hyper-v isolation -----------------------------------------------------

Case 'hyperv' {
    $hv = @('--isolation', 'hyperv')
    $r = W (@('run', '--remove') + $hv + @($Nano, 'cmd', '/c', 'ver')) -Timeout 300
    $build = ((W @('image', 'inspect', $Nano)).Out | ConvertFrom-Json)[0].os_version -replace '^(\d+\.\d+\.\d+).*', '$1'
    Check 'runs in a utility VM with the image kernel' ($r.Code -eq 0 -and $r.Out -match [regex]::Escape($build)) $r
    $r = W (@('run', '--name', 'e2e-hv') + $hv + @($Nano, 'cmd', '/c', 'exit 7')) -Timeout 300
    $s = State 'e2e-hv'
    Check 'passes the exit code through and records hyperv' ($r.Code -eq 7 -and $s.isolation -eq 'hyperv' -and $s.exit_code -eq 7) $r
    W @('delete', 'e2e-hv') | Out-Null

    $dir = Join-Path ([IO.Path]::GetTempPath()) "winner-e2e-hv-$PID"
    New-Item -ItemType Directory -Force $dir | Out-Null
    Set-Content (Join-Path $dir 'hello.txt') 'from-host'
    try {
        $r = W (@('run', '--remove', '-u', 'ContainerAdministrator', '-v', "${dir}:C:\data") + $hv + @($Nano, 'cmd', '/c', 'type C:\data\hello.txt & echo from-vm> C:\data\rw.txt')) -Timeout 300
        Check 'mapped directory in and out of the VM' ($r.Code -eq 0 -and $r.Out -match 'from-host' -and (Test-Path (Join-Path $dir 'rw.txt'))) $r
    } finally {
        Remove-Item -Recurse -Force $dir
    }

    $r = W (@('run', '--remove') + $hv + @($Nano, 'curl.exe', '-sS', '-o', 'NUL', '-w', '%{http_code}', 'https://mcr.microsoft.com/v2/')) -Timeout 300
    Check 'network: DNS and outbound HTTPS' ($r.Code -eq 0 -and $r.Out -match '^(200|401)$') $r
    $r = W (@('create', '--network', 'host') + $hv + @($Nano))
    Check '--network host is refused' ($r.Code -ne 0 -and $r.Err -match 'process isolation') $r

    # Terminating a utility VM during its first run used to leave a scratch
    # layer that never boots again; kill now ends the process instead.
    W (@('run', '-d', '--name', 'e2e-hv-kill') + $hv + @($Nano, 'ping', '-n', '600', '127.0.0.1')) -Timeout 300 | Out-Null
    W @('exec', '-u', 'ContainerAdministrator', 'e2e-hv-kill', 'cmd', '/c', 'echo kept> C:\kept.txt') | Out-Null
    $r = W @('kill', 'e2e-hv-kill') -Timeout 120
    Check 'kill during the first run' ($r.Code -eq 0 -and (State 'e2e-hv-kill').status -eq 'exited') $r
    $r = W @('start', 'e2e-hv-kill') -Timeout 300
    $kept = W @('exec', 'e2e-hv-kill', 'cmd', '/c', 'type C:\kept.txt')
    Check 'it starts again with its data' ($r.Code -eq 0 -and $kept.Out -match 'kept') $kept
    W @('delete', '--force', 'e2e-hv-kill') -Timeout 120 | Out-Null

    $r = W (@('run', '-d', '--name', 'e2e-hv-web', '-p', '18081:8080') + $hv + @($Web)) -Timeout 300
    Check 'run -d --publish' ($r.Code -eq 0) $r
    Check 'published port reachable on localhost' (WaitUntil { (HttpStatus 'http://localhost:18081/') -eq 200 } 120)
    $r = W @('exec', 'e2e-hv-web', 'cmd', '/c', 'echo exec-ok')
    Check 'exec runs in it' ($r.Code -eq 0 -and $r.Out -match 'exec-ok') $r
    $r = W @('stop', '--timeout', '10', 'e2e-hv-web') -Timeout 120
    Check 'stop' ($r.Code -eq 0 -and (State 'e2e-hv-web').status -eq 'exited') $r
    Check 'its endpoint is released' (-not ((Endpoints) -contains "winner-$((State 'e2e-hv-web').id)"))
    $r = W @('start', 'e2e-hv-web') -Timeout 300
    Check 'start it again' ($r.Code -eq 0 -and (WaitUntil { (HttpStatus 'http://localhost:18081/') -eq 200 } 120)) $r
    $id = (State 'e2e-hv-web').id
    $r = W @('delete', '--force', 'e2e-hv-web') -Timeout 120
    Check 'delete --force' ($r.Code -eq 0) $r
    Check 'no endpoint is left' ((Endpoints).Count -eq 0)
    Check 'no compute system is left' (-not (hcsdiag list 2>$null | Select-String -SimpleMatch $id))
}

if ($Full) {
    Case 'hyperv-auto' {
        $old = 'mcr.microsoft.com/windows/nanoserver:ltsc2022'
        $had = ((W @('image', 'list', '--format', 'json')).Out | ConvertFrom-Json).reference -contains $old
        $r = W @('run', '--remove', '--isolation', 'process', $old, 'cmd', '/c', 'ver') -Timeout 1800
        Check 'process isolation of an older LTSC image is refused' ($r.Code -eq 125 -and $r.Err -match 'hyperv') $r
        $r = W @('run', '--name', 'e2e-hv-auto', $old, 'cmd', '/c', 'ver') -Timeout 300
        Check 'it runs with hyper-v picked automatically' ($r.Code -eq 0 -and $r.Out -match '10\.0\.20348' -and (State 'e2e-hv-auto').isolation -eq 'hyperv') $r
        W @('delete', 'e2e-hv-auto') | Out-Null
        if (-not $had) { W @('image', 'delete', $old) | Out-Null }
    }

    Case 'image-delete' {
        W @('create', '--name', 'e2e-keep', $Dotnet, 'cmd', '/c', 'dotnet --list-runtimes') | Out-Null
        $r = W @('image', 'delete', $Dotnet)
        Check 'image in use is refused' ($r.Code -ne 0) $r
        $r = W @('image', 'delete', '--force', $Dotnet)
        Check 'image delete --force' ($r.Code -eq 0) $r
        $r = W @('start', 'e2e-keep')
        $logs = W @('logs', '--follow', 'e2e-keep') -Timeout 60
        Check 'its container still runs from the kept layers' ($r.Code -eq 0 -and $logs.Out -match 'Microsoft\.NETCore\.App') $logs
        $layers = (Get-ChildItem "$env:ProgramData\winner\layers").Count
        W @('delete', 'e2e-keep') | Out-Null
        $after = (Get-ChildItem "$env:ProgramData\winner\layers").Count
        Check 'deleting the last user collects its layers' ($after -lt $layers)
        $r = W @('image', 'list', '--format', 'json')
        Check 'nanoserver is untouched' ((($r.Out | ConvertFrom-Json).reference) -contains $Nano) $r
    }
}

Cleanup
Say ''
if ($script:Failed.Count -eq 0) {
    Say "all $($script:Passed) checks passed" 'Green'
} else {
    Say "$($script:Passed) passed, $($script:Failed.Count) failed:" 'Red'
    foreach ($f in $script:Failed) { Say "  $f" 'Red' }
}
$script:Lines | Set-Content -Encoding utf8 $Log
Write-Host "log: $((Resolve-Path $Log).Path)"
exit $script:Failed.Count
