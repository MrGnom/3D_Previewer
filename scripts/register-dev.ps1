<#
.SYNOPSIS
  Builds the Explorer extension and registers it for the current user from a dev staging
  folder (%LOCALAPPDATA%\BabylonViewerDev), mirroring the installed layout:
    shell\babylon_shell.dll
    shell\occt.wasm         (OpenCascade, for STEP/IGES/BREP thumbnails)
    viewer\index.html ...
.PARAMETER Unregister
  Removes the registration.
.PARAMETER NoBuild
  Stages the existing build outputs without rebuilding.
#>
param([switch]$Unregister, [switch]$NoBuild)
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot -Parent
$stage = Join-Path $env:LOCALAPPDATA 'BabylonViewerDev'
$dll = Join-Path $stage 'shell\babylon_shell.dll'

function Invoke-Regsvr([string[]]$regArgs) {
    $p = Start-Process -FilePath "$env:WINDIR\System32\regsvr32.exe" -ArgumentList $regArgs -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw "regsvr32 $($regArgs -join ' ') failed with exit code $($p.ExitCode)" }
}

if ($Unregister) {
    if (Test-Path $dll) { Invoke-Regsvr @('/s', '/u', "`"$dll`"") }
    Write-Host 'Unregistered the Babylon Viewer preview handler and thumbnail provider.'
    return
}

if (-not $NoBuild) {
    npm run build --prefix (Join-Path $root 'viewer')
    if ($LASTEXITCODE -ne 0) { throw 'viewer build failed' }
    node (Join-Path $root 'scripts\build-occt.mjs')
    if ($LASTEXITCODE -ne 0) { throw 'OpenCascade kernel build failed' }
    cargo build --release -p babylon-shell --manifest-path (Join-Path $root 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw 'shell build failed' }
}

# prevhost.exe keeps the preview handler loaded; restart it so the new DLL is picked up.
Get-Process prevhost -ErrorAction SilentlyContinue | Stop-Process -Force

New-Item -ItemType Directory -Force (Join-Path $stage 'shell') | Out-Null
if (Test-Path $dll) {
    # A loaded DLL can't be overwritten but can be renamed out of the way.
    $old = "$dll.$([DateTime]::Now.Ticks).old"
    Move-Item $dll $old
}
Get-ChildItem (Join-Path $stage 'shell') -Filter '*.old' | Remove-Item -ErrorAction SilentlyContinue
Copy-Item (Join-Path $root 'target\release\babylon_shell.dll') $dll
Copy-Item (Join-Path $root 'target\occt\occt.wasm') (Join-Path $stage 'shell\occt.wasm') -Force

$viewer = Join-Path $stage 'viewer'
if (Test-Path $viewer) { Remove-Item -Recurse -Force $viewer }
Copy-Item -Recurse (Join-Path $root 'viewer\dist') $viewer

Invoke-Regsvr @('/s', "`"$dll`"")
Write-Host "Registered $dll"
Write-Host 'Open a folder with 3D files in Explorer; toggle the Preview pane with Alt+P.'
Write-Host 'Thumbnails already cached by Explorer may need a refresh (or Disk Cleanup > Thumbnails).'
