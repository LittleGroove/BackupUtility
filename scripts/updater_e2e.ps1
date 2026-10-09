# End-to-end check of the self-updater on Windows, against the real GitHub release.
#
# Builds an engine that believes it is version 0.0.9, puts it in a scratch folder
# next to a dummy bkgui.exe, runs `bkengine update --yes` there, and checks that
# both binaries were replaced by the newest published release. Cargo.toml is
# restored afterwards. Needs a published release to exist.
#
#   powershell -ExecutionPolicy Bypass -File scripts\updater_e2e.ps1
$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $root
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

$saved = Get-Content Cargo.toml -Raw
$lowered = $saved -replace '(?m)^version = "\d+\.\d+\.\d+"', 'version = "0.0.9"'
Set-Content Cargo.toml -Value $lowered -NoNewline -Encoding utf8
try {
    Write-Host "=== building a 0.0.9 engine"
    cargo build -p bkengine 2>&1 | Select-String -Pattern "^error" | ForEach-Object { $_.Line }
    $scratch = Join-Path $env:TEMP "bku-updater-e2e"
    Remove-Item -Recurse -Force $scratch -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $scratch | Out-Null
    Copy-Item target\debug\bkengine.exe (Join-Path $scratch "bkengine.exe")
    Set-Content (Join-Path $scratch "bkgui.exe") -Value "dummy" -NoNewline
    $exe = Join-Path $scratch "bkengine.exe"

    Write-Host "=== before:"; & $exe version | Select-Object -First 1
    Write-Host "=== update --check"; & $exe update --check
    Write-Host "=== update --yes"; & $exe update --yes
    Write-Host "exit: $LASTEXITCODE"
    Start-Sleep -Seconds 2
    Write-Host "=== after:"
    Get-ChildItem $scratch | Select-Object Name, Length | Format-Table -AutoSize
    & $exe version | Select-Object -First 1
    Write-Host ("bkgui.exe FileVersion: " + (Get-Item (Join-Path $scratch "bkgui.exe")).VersionInfo.FileVersion)
    Write-Host "=== a second check should find nothing newer and sweep leftovers"
    & $exe update --check
    Get-ChildItem $scratch | Select-Object Name | Format-Table -AutoSize
} finally {
    Set-Content Cargo.toml -Value $saved -NoNewline -Encoding utf8
    git checkout -- Cargo.toml Cargo.lock 2>$null
    cargo build -p bkengine 2>&1 | Select-String -Pattern "^error" | ForEach-Object { $_.Line }
    Write-Host ("restored; uncommitted changes: " + (git status --short | Measure-Object).Count)
}
