# Compile the Windows installer, tablex-<version>-windows-x86_64-setup.exe, from the portable zip that
# package-windows.ps1 made (the installer holds exactly the files of the zip):
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts\package-windows-installer.ps1 -Version X.Y.Z -OutDir dist
#            [-Iscc "C:\path\to\ISCC.exe"]
# The zip is <OutDir>\tablex-<version>-windows-x86_64.zip. ISCC.exe is the Inno Setup 6 compiler; it defaults to
# the one on the PATH or in the usual per-user and per-machine places.
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$Iscc = ""
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $Iscc) {
    $found = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($found) { $Iscc = $found.Source }
    else {
        foreach ($dir in @("${env:ProgramFiles(x86)}", "${env:ProgramFiles}", "${env:LOCALAPPDATA}\Programs")) {
            $candidate = Join-Path $dir "Inno Setup 6\ISCC.exe"
            if (Test-Path $candidate) { $Iscc = $candidate; break }
        }
    }
}
if (-not $Iscc -or -not (Test-Path $Iscc)) { throw "ISCC.exe (Inno Setup 6) not found: pass -Iscc" }

$name = "tablex-$Version-windows-x86_64"
$zip = Join-Path $OutDir "$name.zip"
if (-not (Test-Path $zip)) { throw "missing: $zip (run package-windows.ps1 first)" }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("tablex-setup-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $work | Out-Null
try {
    Expand-Archive -Path $zip -DestinationPath $work
    $out = (New-Item -ItemType Directory -Force -Path $OutDir).FullName
    & $Iscc /Q "/DAppVersion=$Version" "/DSourceDir=$(Join-Path $work $name)" `
        "/DIconFile=$(Join-Path $root 'apps\tablex-gpui\assets\AppIcon.ico')" "/O$out" "$root\scripts\windows\tablex.iss"
    if ($LASTEXITCODE -ne 0) { throw "ISCC failed ($LASTEXITCODE)" }
    $setup = Join-Path $out "$name-setup.exe"
    if (-not (Test-Path $setup)) { throw "missing: $setup" }
    Write-Output $setup
}
finally {
    Remove-Item -Recurse -Force $work
}
