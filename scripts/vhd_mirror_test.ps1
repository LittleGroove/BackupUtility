# Elevated end-to-end test for the FILE MIRROR and the LAYOUT pass.
#
# Proves, against two disposable VHDs formatted with real NTFS:
#   1. a mirror reproduces a tree exactly (hash-compared file by file)
#   2. a second run copies nothing
#   3. `verify --deep` agrees the copy is faithful
#   4. the layout pass raises physical-layout similarity between the drives
#   5. and — the one that matters — moving clusters does NOT corrupt data
#   6. delete mode removes what the source dropped
#
# Safety: it only ever touches disks whose BusType is 'File Backed Virtual',
# asserted before anything is written. It cannot write a physical drive.
#
# Run elevated:
#   powershell -ExecutionPolicy Bypass -File scripts\vhd_mirror_test.ps1 `
#     -EnginePath .\target\debug\bkengine.exe -WorkDir $env:TEMP\bkmirror `
#     -LogPath $env:TEMP\bkmirror_result.log

param(
    [Parameter(Mandatory)] [string]$EnginePath,
    [Parameter(Mandatory)] [string]$WorkDir,
    [Parameter(Mandatory)] [string]$LogPath
)

$ErrorActionPreference = 'Stop'
function Log($m) { Add-Content -Path $LogPath -Value $m; Write-Host $m }

# Run bkengine without letting its stderr (tracing/progress) trip
# ErrorActionPreference=Stop. Returns the exit code; the output is logged and
# kept in $script:LastEngineOutput so callers can parse the report.
$script:LastEngineOutput = @()
function Invoke-Engine([string[]]$EngineArgs) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $out = & $EnginePath @EngineArgs 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prev
    $script:LastEngineOutput = $out
    $out | ForEach-Object { Log "    | $_" }
    return $code
}

# Ride out the diskpart <-> Storage-cmdlet propagation race.
function Wait-DriveLetter($diskNum) {
    for ($i = 0; $i -lt 12; $i++) {
        $p = Get-Partition -DiskNumber $diskNum -ErrorAction SilentlyContinue |
             Where-Object { $_.Size -gt 1MB } | Select-Object -First 1
        if ($p -and $p.DriveLetter) { return "$($p.DriveLetter)" }
        Start-Sleep -Milliseconds 600
    }
    return $null
}

$srcVhd = Join-Path $WorkDir 'msrc.vhd'
$tgtVhd = Join-Path $WorkDir 'mtgt.vhd'
$sizeMB = 512

function Remove-TestVhds {
    foreach ($p in @($srcVhd, $tgtVhd)) {
        try {
            $di = Get-DiskImage -ImagePath $p -ErrorAction SilentlyContinue
            if ($di -and $di.Attached) {
                Dismount-DiskImage -ImagePath $p -ErrorAction SilentlyContinue | Out-Null
            }
        } catch {}
    }
    Start-Sleep -Milliseconds 700
    Remove-Item -Recurse -Force $WorkDir -ErrorAction SilentlyContinue
}

# Partition and format an attached VHD. Returns its drive letter.
function Initialize-TestDisk($num, $label) {
    Set-Disk -Number $num -IsOffline $false -ErrorAction SilentlyContinue
    Set-Disk -Number $num -IsReadOnly $false -ErrorAction SilentlyContinue
    if ((Get-Disk -Number $num).PartitionStyle -eq 'RAW') {
        Initialize-Disk -Number $num -PartitionStyle MBR
    }
    $part = New-Partition -DiskNumber $num -UseMaximumSize -AssignDriveLetter
    Format-Volume -Partition $part -FileSystem NTFS -NewFileSystemLabel $label `
        -Confirm:$false -Force | Out-Null
    $letter = Wait-DriveLetter $num
    if (-not $letter) { throw "disk $num never got a drive letter" }
    return $letter
}

# Hash every file under a root, as relativepath -> hash.
function Get-TreeHashes([string]$root) {
    $map = @{}
    Get-ChildItem -Path $root -Recurse -File -Force -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -notmatch '\\\.bku-' } |
        ForEach-Object {
            $rel = $_.FullName.Substring($root.Length).TrimStart('\')
            $map[$rel] = (Get-FileHash -Path $_.FullName -Algorithm SHA256).Hash
        }
    return $map
}

function Compare-Trees($a, $b, [string]$what) {
    $missing = @($a.Keys | Where-Object { -not $b.ContainsKey($_) })
    $extra   = @($b.Keys | Where-Object { -not $a.ContainsKey($_) })
    $differ  = @($a.Keys | Where-Object { $b.ContainsKey($_) -and $a[$_] -ne $b[$_] })
    if ($missing.Count -or $extra.Count -or $differ.Count) {
        Log "  FAIL ${what}: $($missing.Count) missing, $($extra.Count) extra, $($differ.Count) differing"
        $missing | Select-Object -First 5 | ForEach-Object { Log "    missing: $_" }
        $differ  | Select-Object -First 5 | ForEach-Object { Log "    differs: $_" }
        return $false
    }
    Log "  OK ${what}: $($a.Count) files identical"
    return $true
}

# Pull "layout similarity: 62.0% -> 97.0%" out of the engine's output.
function Get-Similarity($lines) {
    foreach ($l in $lines) {
        if ($l -match 'layout similarity:\s*([\d.]+)%\s*->\s*([\d.]+)%') {
            return @([double]$Matches[1], [double]$Matches[2])
        }
    }
    return $null
}

Set-Content -Path $LogPath -Value "=== VHD mirror + layout test $(Get-Date -Format o) ==="
$result = 'FAIL'
$failures = 0

try {
    Remove-TestVhds
    New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

    # --- Create and attach two raw VHDs -----------------------------------
    Log "Creating two disposable VHDs ($sizeMB MB each)..."
    $dp = @"
create vdisk file="$srcVhd" maximum=$sizeMB type=fixed
attach vdisk
create vdisk file="$tgtVhd" maximum=$sizeMB type=fixed
attach vdisk
exit
"@
    $dpOut = $dp | diskpart
    if ($dpOut -match 'encountered an error') {
        $dpOut | Where-Object { $_ -match '\S' } | ForEach-Object { Log "  [diskpart] $($_.Trim())" }
        throw "diskpart create failed"
    }
    Start-Sleep -Milliseconds 800

    # --- Resolve to disks, with hard safety assertions --------------------
    $srcDisk = Get-DiskImage -ImagePath $srcVhd | Get-Disk
    $tgtDisk = Get-DiskImage -ImagePath $tgtVhd | Get-Disk
    foreach ($d in @($srcDisk, $tgtDisk)) {
        if ($d.BusType -ne 'File Backed Virtual') {
            throw "SAFETY ABORT: disk $($d.Number) BusType '$($d.BusType)' is not a VHD."
        }
    }
    if ($srcDisk.Location -notlike '*msrc.vhd' -or $tgtDisk.Location -notlike '*mtgt.vhd') {
        throw "SAFETY ABORT: disk/location mismatch."
    }
    Log "  msrc.vhd -> disk $($srcDisk.Number), mtgt.vhd -> disk $($tgtDisk.Number)"

    $srcLetter = Initialize-TestDisk $srcDisk.Number 'BKSRC'
    $tgtLetter = Initialize-TestDisk $tgtDisk.Number 'BKTGT'
    Log "  source volume ${srcLetter}:  target volume ${tgtLetter}:"

    $srcRoot = "${srcLetter}:\data"
    $tgtRoot = "${tgtLetter}:\data"
    $state   = Join-Path $WorkDir 'state'
    New-Item -ItemType Directory -Force -Path $srcRoot, $state | Out-Null

    # --- Build a source tree with files big enough to occupy real clusters -
    Log "Writing the source tree..."
    New-Item -ItemType Directory -Force -Path "$srcRoot\docs", "$srcRoot\media" | Out-Null
    $rng = New-Object System.Random 1234
    foreach ($spec in @(
        @{ p = "$srcRoot\media\one.bin";   mb = 12 },
        @{ p = "$srcRoot\media\two.bin";   mb = 16 },
        @{ p = "$srcRoot\media\three.bin"; mb = 10 },
        @{ p = "$srcRoot\docs\notes.bin";  mb = 6  },
        @{ p = "$srcRoot\top.bin";         mb = 8  }
    )) {
        $bytes = New-Object byte[] ($spec.mb * 1MB)
        $rng.NextBytes($bytes)
        [System.IO.File]::WriteAllBytes($spec.p, $bytes)
    }
    Set-Content -Path "$srcRoot\readme.txt" -Value 'small file, lives in the MFT'
    Write-VolumeCache -DriveLetter $srcLetter -ErrorAction SilentlyContinue
    Log "  wrote $(@(Get-ChildItem $srcRoot -Recurse -File).Count) files"

    # --- Fragment the target's free space ---------------------------------
    # Without this both volumes would lay the files out identically and the
    # layout pass would have nothing to prove.
    Log "Fragmenting the target volume's free space..."
    $filler = "${tgtLetter}:\filler"
    New-Item -ItemType Directory -Force -Path $filler | Out-Null
    for ($i = 0; $i -lt 40; $i++) {
        $bytes = New-Object byte[] (1MB)
        $rng.NextBytes($bytes)
        [System.IO.File]::WriteAllBytes("$filler\f$i.bin", $bytes)
    }
    for ($i = 0; $i -lt 40; $i += 2) { Remove-Item "$filler\f$i.bin" -Force }
    Write-VolumeCache -DriveLetter $tgtLetter -ErrorAction SilentlyContinue
    Log "  left 20 one-megabyte holes"

    # --- 1. Mirror --------------------------------------------------------
    Log "TEST 1: mirror $srcRoot -> $tgtRoot"
    $code = Invoke-Engine @('mirror', '--source', $srcRoot, '--dest', $tgtRoot,
                            '--state-root', $state, '--name', 'vhdtest', '--verify')
    if ($code -ne 0) { Log "  FAIL: engine exited $code"; $failures++ }
    $srcHashes = Get-TreeHashes $srcRoot
    $tgtHashes = Get-TreeHashes $tgtRoot
    if (-not (Compare-Trees $srcHashes $tgtHashes 'mirror reproduced the tree')) { $failures++ }

    # --- 2. Second run must move nothing ----------------------------------
    Log "TEST 2: a second run should copy nothing"
    $null = Invoke-Engine @('mirror', '--source', $srcRoot, '--dest', $tgtRoot,
                            '--state-root', $state, '--name', 'vhdtest')
    if ($script:LastEngineOutput | Where-Object { $_ -match 'copied\s+0 B in 0 files' }) {
        Log '  OK: nothing re-copied'
    } else {
        Log '  FAIL: expected a no-op run'; $failures++
    }

    # --- 3. Deep verification ---------------------------------------------
    Log "TEST 3: verify --deep"
    $code = Invoke-Engine @('verify', '--source', $srcRoot, '--dest', $tgtRoot, '--deep')
    if ($code -ne 0) { Log "  FAIL: deep verification did not agree"; $failures++ }
    else { Log '  OK: the copy hashes identical' }

    # --- 4. Layout similarity before, apply, then after -------------------
    Log "TEST 4: layout alignment"
    $null = Invoke-Engine @('layout', '--source', $srcRoot, '--dest', $tgtRoot)
    $before = Get-Similarity $script:LastEngineOutput
    if (-not $before) { Log '  FAIL: could not read the similarity report'; $failures++ }
    else { Log "  similarity before: $($before[0])%" }

    $code = Invoke-Engine @('layout', '--source', $srcRoot, '--dest', $tgtRoot, '--apply')
    if ($code -ne 0) { Log "  FAIL: layout --apply exited $code"; $failures++ }
    $after = Get-Similarity $script:LastEngineOutput
    if (-not $after) { Log '  FAIL: could not read the post-apply report'; $failures++ }
    else {
        Log "  similarity: $($after[0])% -> $($after[1])%"
        if ($after[1] -gt $after[0]) {
            Log "  OK: layout similarity improved"
        } else {
            Log "  FAIL: similarity did not improve"; $failures++
        }
    }

    # --- 5. THE CRITICAL CHECK: moving clusters must not corrupt data -----
    Log 'TEST 5: data integrity after moving clusters'
    $tgtAfter = Get-TreeHashes $tgtRoot
    if (-not (Compare-Trees $srcHashes $tgtAfter 'contents survived the cluster moves')) { $failures++ }
    $code = Invoke-Engine @('verify', '--source', $srcRoot, '--dest', $tgtRoot, '--deep')
    if ($code -ne 0) { Log '  FAIL: deep verification failed after the moves'; $failures++ }
    else { Log '  OK: still hashes identical after relocation' }

    # --- 6. Delete mode ---------------------------------------------------
    Log 'TEST 6: delete mode removes what the source dropped'
    Remove-Item "$srcRoot\media\three.bin" -Force
    Remove-Item "$srcRoot\docs" -Recurse -Force
    $code = Invoke-Engine @('mirror', '--source', $srcRoot, '--dest', $tgtRoot,
                            '--state-root', $state, '--name', 'vhdtest', '--mode', 'delete')
    if ($code -ne 0) { Log "  FAIL: engine exited $code"; $failures++ }
    if (Test-Path "$tgtRoot\media\three.bin") { Log '  FAIL: deleted file survived'; $failures++ }
    if (Test-Path "$tgtRoot\docs") { Log '  FAIL: deleted directory survived'; $failures++ }
    if (-not (Compare-Trees (Get-TreeHashes $srcRoot) (Get-TreeHashes $tgtRoot) `
              'delete mode left the trees identical')) { $failures++ }

    if ($failures -eq 0) { $result = 'PASS' }
}
catch {
    Log "EXCEPTION: $_"
    Log $_.ScriptStackTrace
    $failures++
}
finally {
    Log 'Cleaning up...'
    Remove-TestVhds
    Log "RESULT: $result ($failures failures)"
}
