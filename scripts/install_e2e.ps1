# End-to-end check of the installer on Windows.
#
# Stages a copy of the release binaries in a temporary folder (marked as a
# download, the way a browser would), registers a throwaway scheduled job
# from there, installs from the copy with `bkengine install --desktop-shortcut`,
# checks the program folder, both shortcuts (including the AppUserModelID),
# the Settings > Apps entry and the re-pointed task, then uninstalls with
# `bkengine uninstall --yes` and checks that everything is gone and the job
# file was kept.
#
# Refuses to run on a machine that has a real install or real scheduled jobs:
# the uninstall step would remove them.
#
#   cargo build --release --workspace
#   powershell -ExecutionPolicy Bypass -File scripts\install_e2e.ps1     # asks for elevation
#
# Prints PASS/FAIL lines; the full log is also left at -Log.
param(
    [string]$Root = (Split-Path $PSScriptRoot -Parent),
    [string]$Log = (Join-Path $env:TEMP 'bku-install-e2e.log'),
    [switch]$Elevated
)
$ErrorActionPreference = 'Continue'

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Remove-Item $Log -ErrorAction SilentlyContinue
    Start-Process -FilePath powershell.exe -Verb RunAs -Wait -ArgumentList @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $PSCommandPath, '-Root', $Root, '-Log', $Log, '-Elevated')
    if (Test-Path $Log) { Get-Content $Log } else { Write-Host 'no log was written (was the elevation prompt declined?)' }
    exit
}

function Say($m) { $m | Out-File -FilePath $Log -Append -Encoding utf8 }
function Check($desc, [bool]$ok) { Say ("{0}  {1}" -f $(if ($ok) { 'PASS' } else { 'FAIL' }), $desc) }
'' | Out-File $Log -Encoding utf8

$rel = Join-Path $Root 'target\release'
$work = Join-Path $env:TEMP 'bku-install-e2e'
$src = Join-Path $work 'src'
$pf = Join-Path $env:ProgramFiles 'BackupUtility'
$startMenu = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\BackupUtility.lnk'
$desktop = Join-Path $env:PUBLIC 'Desktop\BackupUtility.lnk'
$regKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\BackupUtility'
$jobName = 'bku-install-e2e'
$taskName = "BackupUtility\$jobName"
$version = (Select-String -Path (Join-Path $Root 'Cargo.toml') -Pattern '^version = "([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value

try {
    Say '=== preconditions'
    if (-not (Test-Path (Join-Path $rel 'bkgui.exe'))) { throw "no release build in $rel; run cargo build --release --workspace first" }
    if (Test-Path $pf) { throw "$pf exists: this machine has a real install, and the round-trip would remove it" }
    $real = schtasks /Query /FO LIST 2>$null | Select-String -Pattern 'TaskName:\s+\\BackupUtility\\'
    if ($real) { throw "this machine has real BackupUtility scheduled tasks, and the uninstall step would remove them" }

    Say '=== stage a source copy, marked as a download'
    if (Test-Path $work) { Remove-Item $work -Recurse -Force }
    New-Item -ItemType Directory -Force (Join-Path $work 'srcA'), (Join-Path $work 'dstB'), $src | Out-Null
    Copy-Item (Join-Path $rel 'bkengine.exe'), (Join-Path $rel 'bkgui.exe') $src
    Copy-Item (Join-Path $Root 'LICENSE') $src
    'hello' | Set-Content (Join-Path $work 'srcA\file.txt')
    foreach ($b in 'bkengine.exe', 'bkgui.exe') { Set-Content -Path (Join-Path $src $b) -Stream Zone.Identifier -Value "[ZoneTransfer]`r`nZoneId=3" }
    Check 'source carries Zone.Identifier' ($null -ne (Get-Item (Join-Path $src 'bkgui.exe') -Stream Zone.Identifier -ErrorAction SilentlyContinue))

    Say '=== a scheduled job pointing at the source copy'
    & (Join-Path $src 'bkengine.exe') jobs add --name $jobName --source (Join-Path $work 'srcA') --dest (Join-Path $work 'dstB') --every daily --at 03:00 2>&1 | Out-File $Log -Append
    & (Join-Path $src 'bkengine.exe') schedule install --job $jobName 2>&1 | Out-File $Log -Append
    $xml = schtasks /Query /TN $taskName /XML 2>&1 | Out-String
    Check 'task registered with the source engine' ($xml -like "*$src\bkengine.exe*")
    $v = & (Join-Path $src 'bkengine.exe') version 2>&1 | Out-String
    Check 'source copy says it is not the installed one' ($v -like '*not the installed one*')

    Say '=== bkengine install --desktop-shortcut, from the source copy'
    & (Join-Path $src 'bkengine.exe') install --desktop-shortcut 2>&1 | Out-File $Log -Append
    Check 'installed bkgui.exe' (Test-Path (Join-Path $pf 'bkgui.exe'))
    Check 'installed bkengine.exe' (Test-Path (Join-Path $pf 'bkengine.exe'))
    Check 'installed LICENSE' (Test-Path (Join-Path $pf 'LICENSE'))
    Check 'no scratch dir left' (-not (Test-Path (Join-Path $pf '.bku-install')))
    Check 'Zone.Identifier stripped from the installed copy' ($null -eq (Get-Item (Join-Path $pf 'bkgui.exe') -Stream Zone.Identifier -ErrorAction SilentlyContinue))
    Check 'Start Menu shortcut' (Test-Path $startMenu)
    Check 'desktop shortcut' (Test-Path $desktop)
    $lnk = (New-Object -ComObject WScript.Shell).CreateShortcut($startMenu)
    Check 'shortcut targets the installed gui' ($lnk.TargetPath -eq (Join-Path $pf 'bkgui.exe'))
    $folder = (New-Object -ComObject Shell.Application).NameSpace((Split-Path $startMenu))
    $aumid = $folder.ParseName((Split-Path $startMenu -Leaf)).ExtendedProperty('System.AppUserModel.ID')
    Check 'shortcut carries the AppUserModelID' ($aumid -eq 'LittleGroove.BackupUtility')
    Check 'uninstall registry key' (Test-Path $regKey)
    $r = Get-ItemProperty $regKey -ErrorAction SilentlyContinue
    Check "DisplayVersion is $version" ($r.DisplayVersion -eq $version)
    Check 'UninstallString runs the installed gui' ($r.UninstallString -eq "`"$pf\bkgui.exe`" --uninstall")
    $xml = schtasks /Query /TN $taskName /XML 2>&1 | Out-String
    Check 'task re-pointed at the installed engine' ($xml -like "*$pf\bkengine.exe*")
    $v = & (Join-Path $pf 'bkengine.exe') version 2>&1 | Out-String
    Check 'installed copy says it is the installed one' ($v -like '*this is the installed copy*')

    Say '=== install again from home (refresh only)'
    & (Join-Path $pf 'bkengine.exe') install 2>&1 | Out-File $Log -Append
    Check 'still installed' (Test-Path (Join-Path $pf 'bkgui.exe'))

    Say '=== bkengine uninstall --yes, from the installed copy'
    & (Join-Path $pf 'bkengine.exe') uninstall --yes 2>&1 | Out-File $Log -Append
    Start-Sleep -Seconds 3
    Check 'program folder removed' (-not (Test-Path $pf))
    Check 'Start Menu shortcut removed' (-not (Test-Path $startMenu))
    Check 'desktop shortcut removed' (-not (Test-Path $desktop))
    Check 'registry key removed' (-not (Test-Path $regKey))
    schtasks /Query /TN $taskName 2>&1 | Out-Null
    Check 'scheduled task removed' ($LASTEXITCODE -ne 0)
    Check 'job file kept' (Test-Path (Join-Path $env:ProgramData "BackupUtility\jobs\$jobName.json"))
    $left = @(Get-ChildItem $env:TEMP -Filter '.bkengine*' -Force -ErrorAction SilentlyContinue)
    Check 'no self-delete leftovers in TEMP' ($left.Count -eq 0)
}
catch { Say ('ERROR: ' + $_.Exception.Message) }
finally {
    Say '=== cleanup'
    if (Test-Path (Join-Path $src 'bkengine.exe')) {
        & (Join-Path $src 'bkengine.exe') jobs remove --job $jobName 2>&1 | Out-File $Log -Append
    }
    schtasks /Delete /TN $taskName /F 2>&1 | Out-Null
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
    Say '=== done'
}
