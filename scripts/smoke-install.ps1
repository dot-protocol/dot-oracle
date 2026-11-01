# The README's install and first minute on a clean Windows machine, as written there. Exits 0 only if every step passes:
# the installer, a new Oracle, a seat, ox doctor, and one signed memory read back. The release workflow runs it on a fresh
# Windows machine before anything is published (Blaze, 1 Nov 2026: "failing is failing").
$ErrorActionPreference = 'Stop'
function Run([string]$exe, [string[]]$argv) {
    & $exe @argv
    if ($LASTEXITCODE -ne 0) { throw "$exe $($argv -join ' ') exited $LASTEXITCODE" }
}
& (Join-Path $PSScriptRoot 'install.ps1')
$bin = if ($env:ORACLE_BIN_DIR) { $env:ORACLE_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'dotOracle\bin' }
$env:Path = "$bin;$env:Path"
$env:ORACLE_HOME = if ($env:SMOKE_HOME) { $env:SMOKE_HOME } else { Join-Path $env:USERPROFILE '.oracle-server' }
Run 'oracle' @('init', '--owner', 'smoke')
$serve = Start-Process -FilePath (Join-Path $bin 'oracle.exe') -ArgumentList 'serve' -PassThru -NoNewWindow `
    -RedirectStandardOutput (Join-Path $env:ORACLE_HOME 'serve.log') -RedirectStandardError (Join-Path $env:ORACLE_HOME 'serve.err')
try {
    $up = $false
    for ($i = 0; $i -lt 60; $i++) {
        try { Invoke-WebRequest -Uri 'http://127.0.0.1:8900/health' -UseBasicParsing -TimeoutSec 2 | Out-Null; $up = $true; break } catch { Start-Sleep 1 }
    }
    if (-not $up) { Get-Content (Join-Path $env:ORACLE_HOME 'serve.err') -ErrorAction SilentlyContinue; throw 'oracle serve did not answer /health in 60 s' }
    Run 'oracle' @('seat', 'add', 'agent')
    $keys = Join-Path $env:ORACLE_HOME 'keys'
    Run 'ox' @('setup', '--url', 'http://127.0.0.1:8900', '--handle', 'agent', '--token-file', (Join-Path $keys 'agent.token'), '--key', (Join-Path $keys 'agent.key'))
    Run 'ox' @('doctor')
    Run 'ox' @('say', 'smoke', 'the first memory on this machine')
    $read = & ox read smoke --limit 1 | Out-String
    if ($read -notmatch 'the first memory on this machine') { throw 'the memory was not read back' }
    Write-Host "SMOKE OK Windows $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)"
} finally {
    Stop-Process -Id $serve.Id -Force -ErrorAction SilentlyContinue
}
