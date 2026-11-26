# Pack a Windows build of tablex into tablex-<version>-windows-<arch>.zip (<arch> is x86_64, the default, or aarch64):
#   tablex-<version>-windows-<arch>\tablex.exe, tablex-mcp.exe, LICENSE, licenses\, README.txt
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts\package-windows.ps1 -Version X.Y.Z -OutDir dist
#            [-Arch x86_64|aarch64] [-Binary target\dist\tablex-gpui.exe] [-Mcp target\dist\tablex-mcp.exe]
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [ValidateSet("x86_64", "aarch64")][string]$Arch = "x86_64",
    [string]$Binary = "target\dist\tablex-gpui.exe",
    [string]$Mcp = "target\dist\tablex-mcp.exe"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
foreach ($file in @($Binary, $Mcp, "LICENSE", "licenses\README.md")) {
    if (-not (Test-Path $file)) { throw "missing: $file" }
}

$name = "tablex-$Version-windows-$Arch"
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("tablex-pack-" + [System.Guid]::NewGuid().ToString("N"))
$stage = Join-Path $work $name
New-Item -ItemType Directory -Path $stage | Out-Null
try {
    Copy-Item $Binary (Join-Path $stage "tablex.exe")
    Copy-Item $Mcp (Join-Path $stage "tablex-mcp.exe")
    Copy-Item "LICENSE" (Join-Path $stage "LICENSE")
    Copy-Item "licenses" (Join-Path $stage "licenses") -Recurse
    $readme = @"
tablex $Version for Windows ($Arch)

Run tablex.exe. It needs Windows 10 or later and nothing else: the C runtime is part of the program,
so no Visual C++ Redistributable has to be installed.

tablex-mcp.exe is the MCP server that lets an AI assistant use your saved connections; the app can
also install and update it from Driver Manager. Database drivers are installed from the app
(Driver Manager); they are not in this archive.

The program is not code signed yet, so Windows SmartScreen may ask for a confirmation the first
time: choose More info, then Run anyway.

Licence: AGPL-3.0 (LICENSE); third-party texts are in the licenses folder.
"@
    # CRLF, as Notepad expects.
    [System.IO.File]::WriteAllText((Join-Path $stage "README.txt"), ($readme -replace "`r?`n", "`r`n"))

    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $zip = Join-Path (Resolve-Path $OutDir) "$name.zip"
    if (Test-Path $zip) { Remove-Item $zip }
    # One entry per file with forward slashes: Windows PowerShell 5.1's CreateFromDirectory writes backslashes,
    # which unzip and other tools do not take for a separator.
    Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::Open($zip, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        $prefix = (Resolve-Path $work).Path.TrimEnd("\") + "\"
        foreach ($file in Get-ChildItem -Path $stage -Recurse -File) {
            $entry = $file.FullName.Substring($prefix.Length).Replace("\", "/")
            [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $archive, $file.FullName, $entry, [System.IO.Compression.CompressionLevel]::Optimal)
        }
    }
    finally {
        $archive.Dispose()
    }
    Write-Output $zip
}
finally {
    Remove-Item -Recurse -Force $work
}
