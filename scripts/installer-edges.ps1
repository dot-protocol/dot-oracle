# The installer's edges on a clean Windows machine (plan.oracle.everyone J7; docs/OPEN-SOURCE-RELEASE.md §2), the same five
# cases as installer-edges.sh: a path with a space, a re-run that changes nothing, an Oracle home left untouched by a
# re-install, a tampered archive refused, and no route to the release failing fast and installing nothing.
# $env:INSTALLER_EDGES_CONTROL = '1' serves the untampered archive in the tampered case, so that case must fail.
# Usage: ./scripts/installer-edges.ps1 <dist-dir>   (oracle-x86_64-pc-windows-msvc.zip + .sha256)
param([Parameter(Mandatory = $true)][string]$Dist)
$ErrorActionPreference = 'Stop'
$installer = Join-Path $PSScriptRoot 'install.ps1'
$work = Join-Path ([IO.Path]::GetTempPath()) ("oracle-edges-" + [guid]::NewGuid())
$servers = @()
function Fail([string]$why) { throw "installer-edges: FAIL $why" }
function Sum([string]$p) { (Get-FileHash $p -Algorithm SHA256).Hash }
# install.ps1 throws on refusal; run it in a child PowerShell so a refusal is an exit code, as a user sees it.
function Install([string]$base, [string]$bin, [string]$log) {
    $env:ORACLE_DOWNLOAD_BASE = $base; $env:ORACLE_BIN_DIR = $bin
    & pwsh -NoProfile -File $installer *> $log
    return $LASTEXITCODE
}
function Serve([string]$dir, [int]$port) {
    $p = Start-Process python -ArgumentList '-m', 'http.server', "$port", '--bind', '127.0.0.1', '--directory', "`"$dir`"" -WindowStyle Hidden -PassThru
    for ($i = 0; $i -lt 50; $i++) {
        try { Invoke-WebRequest "http://127.0.0.1:$port/" -UseBasicParsing -TimeoutSec 1 | Out-Null; return $p } catch { Start-Sleep -Milliseconds 100 }
    }
    Fail "the test server on port $port did not start"
}
try {
    New-Item -ItemType Directory -Path "$work\good", "$work\bad" | Out-Null
    Copy-Item (Join-Path $Dist 'oracle-*.zip*') "$work\good\"
    Copy-Item "$work\good\*" "$work\bad\"
    if ($env:INSTALLER_EDGES_CONTROL -ne '1') {
        Get-ChildItem "$work\bad\oracle-*.zip" | ForEach-Object { [IO.File]::AppendAllText($_.FullName, 'x') }
    }
    $servers += Serve "$work\good" 8781
    $servers += Serve "$work\bad" 8782
    $good = 'http://127.0.0.1:8781'; $bad = 'http://127.0.0.1:8782'

    # space
    $bin = Join-Path $work 'home with space\bin'
    if ((Install $good $bin "$work\space.log") -ne 0) { Get-Content "$work\space.log"; Fail 'space: the install failed' }
    & (Join-Path $bin 'ox.exe') --version | Out-Null; if ($LASTEXITCODE -ne 0) { Fail 'space: the installed ox does not run' }
    & (Join-Path $bin 'oracle.exe') --version | Out-Null; if ($LASTEXITCODE -ne 0) { Fail 'space: the installed oracle does not run' }
    Write-Host "space: passed ($bin)"

    # rerun
    $before = (Sum "$bin\ox.exe") + (Sum "$bin\oracle.exe")
    if ((Install $good $bin "$work\rerun.log") -ne 0) { Get-Content "$work\rerun.log"; Fail 'rerun: the second install failed' }
    if ($before -ne (Sum "$bin\ox.exe") + (Sum "$bin\oracle.exe")) { Fail 'rerun: the second install changed the binaries' }
    Write-Host 'rerun: passed'

    # data
    $ohome = Join-Path $work 'home with space\.oracle-server'
    $env:ORACLE_HOME = $ohome
    & (Join-Path $bin 'oracle.exe') init --owner edges | Out-Null; if ($LASTEXITCODE -ne 0) { Fail 'data: init failed' }
    function Snap { Get-ChildItem $ohome -Recurse -File | Sort-Object FullName | ForEach-Object { "$(Sum $_.FullName) $($_.FullName)" } }
    $snapBefore = Snap
    if (-not $snapBefore) { Fail 'data: init wrote nothing to compare' }
    if ((Install $good $bin "$work\data.log") -ne 0) { Fail 'data: the re-install failed' }
    if (Compare-Object $snapBefore (Snap)) { Fail "data: re-installing changed the Oracle's home" }
    Write-Host "data: passed ($(@($snapBefore).Count) files unchanged)"

    # tampered
    if ((Install $bad $bin "$work\tampered.log") -eq 0) { Fail 'tampered: a changed archive was installed' }
    if (-not (Select-String -Path "$work\tampered.log" -Pattern 'checksum mismatch; nothing installed' -Quiet)) { Get-Content "$work\tampered.log"; Fail 'tampered: refused without saying why' }
    if ($before -ne (Sum "$bin\ox.exe") + (Sum "$bin\oracle.exe")) { Fail 'tampered: the refused install changed the binaries' }
    Write-Host 'tampered: passed (refused: checksum mismatch)'

    # offline: port 9 on loopback is closed
    $empty = Join-Path $work 'offline-bin'
    $sw = [Diagnostics.Stopwatch]::StartNew()
    if ((Install 'http://127.0.0.1:9' $empty "$work\offline.log") -eq 0) { Fail 'offline: the install claimed success with no route to the release' }
    $took = [int]$sw.Elapsed.TotalSeconds
    if (-not (Select-String -Path "$work\offline.log" -Pattern 'download failed: .*nothing installed' -Quiet)) { Get-Content "$work\offline.log"; Fail 'offline: failed without saying why' }
    if ((Test-Path "$empty\ox.exe") -or (Test-Path "$empty\oracle.exe")) { Fail 'offline: something was installed' }
    if ($took -gt 60) { Fail "offline: it took $took s to fail" }
    Write-Host "offline: passed ($took s, nothing installed)"

    Write-Host "INSTALLER EDGES OK Windows $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)"
} finally {
    $servers | ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
