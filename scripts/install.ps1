# Install the `oracle` and `ox` binaries for Windows from the latest dotOracle release.
#   irm https://github.com/dot-protocol/dot-oracle/releases/latest/download/install.ps1 | iex
# ORACLE_DOWNLOAD_BASE points it at another copy of the release files (the release workflow tests from one).
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$repo = if ($env:ORACLE_REPO) { $env:ORACLE_REPO } else { 'dot-protocol/dot-oracle' }
$base = if ($env:ORACLE_DOWNLOAD_BASE) { $env:ORACLE_DOWNLOAD_BASE.TrimEnd('/') } else { "https://github.com/$repo/releases/latest/download" }
$dest = if ($env:ORACLE_BIN_DIR) { $env:ORACLE_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'dotOracle\bin' }
$arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if ($arch -ne 'X64') { throw "dotOracle for Windows is built for x86_64; this machine is $arch" }
$url = "$base/oracle-x86_64-pc-windows-msvc.zip"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("dot-oracle-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "downloading $url"
    Invoke-WebRequest -Uri $url -OutFile "$tmp\oracle.zip" -UseBasicParsing -TimeoutSec 120
    Invoke-WebRequest -Uri "$url.sha256" -OutFile "$tmp\sum" -UseBasicParsing -TimeoutSec 60
    $want = ((Get-Content "$tmp\sum" -TotalCount 1).Trim() -split '\s+')[0].ToLower()
    if ($want -notmatch '^[0-9a-f]{64}$') { throw 'invalid checksum; nothing installed' }
    $have = (Get-FileHash "$tmp\oracle.zip" -Algorithm SHA256).Hash.ToLower()
    if ($want -ne $have) { throw 'checksum mismatch; nothing installed' }
    Expand-Archive "$tmp\oracle.zip" -DestinationPath "$tmp\x" -Force
    New-Item -ItemType Directory -Path $dest -Force | Out-Null
    Copy-Item "$tmp\x\oracle.exe", "$tmp\x\ox.exe" -Destination $dest -Force
    Write-Host "installed oracle and ox to $dest"
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not (($userPath -split ';') -contains $dest)) {
        [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ";$dest").TrimStart(';'), 'User')
        Write-Host "added $dest to your PATH; open a new terminal to use oracle and ox"
    }
    $env:Path = "$dest;$env:Path"
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
