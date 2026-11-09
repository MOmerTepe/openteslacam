<#
.SYNOPSIS
  Builds dist\OpenTeslaCam-Setup-<version>.exe: the setup program with the release app appended to it.

  powershell -ExecutionPolicy Bypass -File scripts\build-installer.ps1
#>
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

cargo build --release
if ($LASTEXITCODE) { exit $LASTEXITCODE }
cargo build --release -p openteslacam-setup
if ($LASTEXITCODE) { exit $LASTEXITCODE }

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
$stub = [IO.File]::ReadAllBytes((Resolve-Path "$target\release\openteslacam-setup.exe"))
$app = [IO.File]::ReadAllBytes((Resolve-Path "$target\release\openteslacam.exe"))

New-Item -ItemType Directory -Force dist | Out-Null
$out = Join-Path (Resolve-Path dist) "OpenTeslaCam-Setup-$version.exe"
$part = "$out.part"

# Layout read back by split_payload in installer\src\main.rs: stub, app, app length as u64 LE, magic.
$file = [IO.File]::Create($part)
try {
    $file.Write($stub, 0, $stub.Length)
    $file.Write($app, 0, $app.Length)
    $length = [BitConverter]::GetBytes([uint64]$app.Length)
    $file.Write($length, 0, 8)
    $magic = [Text.Encoding]::ASCII.GetBytes('OTCSETUP')
    $file.Write($magic, 0, 8)
} finally {
    $file.Dispose()
}
Move-Item -Force $part $out

$mb = [math]::Round((Get-Item $out).Length / 1MB, 1)
Write-Host "Built $out ($mb MB)"
