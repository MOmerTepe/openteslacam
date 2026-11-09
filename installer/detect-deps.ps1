<#
.SYNOPSIS
  Detects what OpenTeslaCam Setup needs. Changes nothing.

.DESCRIPTION
  The setup program runs this first and offers to install what's missing. You can also run it yourself:

    powershell -ExecutionPolicy Bypass -File installer\detect-deps.ps1

  Only Windows is required to play clips. winget and git only matter for fetching Tesla's dashcam tools,
  which are optional. Exit code: 0 when everything required is present, 1 otherwise.

.PARAMETER InstallDir
  Where the app would be installed; Tesla's tools are looked for under <InstallDir>\vendor-dev\dashcam.

.PARAMETER Machine
  Print one tab-separated line per check for the setup program: id, status, detail, path.
  Status is ok, missing or fail.
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\OpenTeslaCam'),
    [switch]$Machine
)

function New-Check($Id, $Status, $Detail, $Path = '') {
    [pscustomobject]@{ Check = $Id; Status = $Status; Detail = "$Detail"; Path = "$Path" }
}

# A command only counts if it really runs: winget can exist as a dead App Execution Alias.
function Get-ToolVersion($Exe) {
    try {
        $out = & $Exe --version 2>$null
        if ($LASTEXITCODE -eq 0 -and $out) { return ($out | Select-Object -First 1).ToString().Trim() }
    } catch { }
    return $null
}

function Get-WindowsCheck {
    $v = [Environment]::OSVersion.Version
    $name = 'Windows'
    try { $name = (Get-CimInstance Win32_OperatingSystem -ErrorAction Stop).Caption.Trim() } catch { }
    $detail = "$name, build $($v.Build)"
    if ($v.Major -lt 10) { return New-Check 'windows' 'fail' "$detail. OpenTeslaCam needs Windows 10 or newer" }
    if (-not [Environment]::Is64BitOperatingSystem) { return New-Check 'windows' 'fail' "$detail. OpenTeslaCam needs 64-bit Windows" }
    New-Check 'windows' 'ok' $detail
}

function Get-WingetCheck {
    $cmd = Get-Command winget.exe -ErrorAction SilentlyContinue
    if ($cmd) {
        $version = Get-ToolVersion $cmd.Source
        if ($version) { return New-Check 'winget' 'ok' $version $cmd.Source }
    }
    New-Check 'winget' 'missing' 'not found'
}

function Get-GitCheck {
    # Also looks past PATH: a git that was installed a moment ago isn't on this process's PATH yet.
    $candidates = @()
    $cmd = Get-Command git.exe -ErrorAction SilentlyContinue
    if ($cmd) { $candidates += $cmd.Source }
    foreach ($key in 'HKLM:\SOFTWARE\GitForWindows', 'HKCU:\SOFTWARE\GitForWindows') {
        $root = (Get-ItemProperty -Path $key -ErrorAction SilentlyContinue).InstallPath
        if ($root) { $candidates += Join-Path $root 'cmd\git.exe' }
    }
    if ($env:ProgramFiles) { $candidates += Join-Path $env:ProgramFiles 'Git\cmd\git.exe' }
    if ($env:LOCALAPPDATA) { $candidates += Join-Path $env:LOCALAPPDATA 'Programs\Git\cmd\git.exe' }
    foreach ($exe in $candidates) {
        if (-not (Test-Path -LiteralPath $exe)) { continue }
        $version = Get-ToolVersion $exe
        if ($version) { return New-Check 'git' 'ok' $version $exe }
    }
    New-Check 'git' 'missing' 'not found'
}

function Get-TeslaCheck {
    $dir = Join-Path $InstallDir 'vendor-dev\dashcam'
    if (Test-Path -LiteralPath (Join-Path $dir 'sei_extractor.py')) { return New-Check 'tesla' 'ok' 'already installed' $dir }
    New-Check 'tesla' 'missing' 'not installed' $dir
}

$checks = @(Get-WindowsCheck; Get-WingetCheck; Get-GitCheck; Get-TeslaCheck)

if ($Machine) {
    try { [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false) } catch { }
    foreach ($c in $checks) { [Console]::Out.WriteLine(($c.Check, $c.Status, $c.Detail, $c.Path) -join "`t") }
} else {
    $checks | Format-Table Check, Status, Detail -AutoSize | Out-String | Write-Host
    Write-Host 'Only windows is required. winget and git are only used to fetch the optional Tesla dashcam tools;'
    Write-Host 'the setup program offers to install them.'
}

if (($checks | Where-Object { $_.Status -eq 'fail' })) { exit 1 }
exit 0
