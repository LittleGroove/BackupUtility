param(
    [Parameter(Mandatory)] [string]$EnginePath,
    [Parameter(Mandatory)] [string]$WorkDir,
    [Parameter(Mandatory)] [string]$LogPath
)

$ErrorActionPreference = 'Stop'
function Log($m) { Add-Content -Path $LogPath -Value $m; Write-Host $m }

# Wait for a disk's data partition to have a drive letter, assigning one if
# needed. Rides out the diskpart<->Storage-cmdlet propagation race.
function Wait-DriveLetter($diskNum) {
    for ($i = 0; $i -lt 12; $i++) {
        $p = Get-Partition -DiskNumber $diskNum -ErrorAction SilentlyContinue |
             Where-Object { $_.Size -gt 1MB } | Select-Object -First 1
        if ($p -and $p.DriveLetter) { return "$($p.DriveLetter)" }
        if ($p -and -not $p.DriveLetter) {
            try {
                Add-PartitionAccessPath -DiskNumber $diskNum -PartitionNumber $p.PartitionNumber `
                    -AssignDriveLetter -ErrorAction SilentlyContinue
            } catch {}
        }
        Start-Sleep -Milliseconds 600
    }
    return $null
}

# Run bkengine without letting its stderr (tracing/progress) trip
# ErrorActionPreference=Stop. Returns the process exit code; logs all output.
function Invoke-Engine([string[]]$EngineArgs) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $out = & $EnginePath @EngineArgs 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prev
    $out | ForEach-Object { Log $_ }
    return $code
}

Set-Content -Path $LogPath -Value "=== VHD clone test $(Get-Date -Format o) ==="

$srcVhd = Join-Path $WorkDir 'src.vhd'
$tgtVhd = Join-Path $WorkDir 'tgt.vhd'
$store  = Join-Path $WorkDir 'store'
$sizeMB = 512

$srcNum = $null; $tgtNum = $null
$result = 'FAIL'

# Detach our test VHDs if attached (from a prior aborted run) and delete the
# WorkDir. Dismount-DiskImage releases the file lock reliably; diskpart detach
# did not. Used both before creating and after finishing.
function Remove-TestVhds {
    foreach ($p in @($srcVhd, $tgtVhd)) {
        try {
            $di = Get-DiskImage -ImagePath $p -ErrorAction SilentlyContinue
            if ($di -and $di.Attached) { Dismount-DiskImage -ImagePath $p -ErrorAction SilentlyContinue | Out-Null }
        } catch {}
    }
    Start-Sleep -Milliseconds 700
    Remove-Item -Recurse -Force $WorkDir -ErrorAction SilentlyContinue
}

try {
    # Clear any leftovers from a prior aborted run (detach + delete), then start clean.
    Remove-TestVhds
    New-Item -ItemType Directory -Force $WorkDir | Out-Null

    # --- 1. Create+attach two raw VHDs via diskpart (create/attach only). ---
    Log "Creating VHDs ($sizeMB MB each) via diskpart..."
    $dp = @"
create vdisk file="$srcVhd" maximum=$sizeMB type=fixed
attach vdisk
create vdisk file="$tgtVhd" maximum=$sizeMB type=fixed
attach vdisk
exit
"@
    $dpOut = $dp | diskpart
    $dpOut | Where-Object { $_ -match '\S' } | ForEach-Object { Log "  [diskpart] $($_.Trim())" }
    if ($dpOut -match 'encountered an error') { throw "diskpart create failed (see log)" }
    Start-Sleep -Milliseconds 800

    # --- 2. Resolve each VHD to its disk, with HARD safety assertions ---
    $srcDisk = Get-DiskImage -ImagePath $srcVhd | Get-Disk
    $tgtDisk = Get-DiskImage -ImagePath $tgtVhd | Get-Disk
    $srcNum = $srcDisk.Number
    $tgtNum = $tgtDisk.Number
    Log "src.vhd -> disk $srcNum (style=$($srcDisk.PartitionStyle), $($srcDisk.BusType))"
    Log "tgt.vhd -> disk $tgtNum (style=$($tgtDisk.PartitionStyle), $($tgtDisk.BusType))"

    foreach ($d in @($srcDisk, $tgtDisk)) {
        if ($d.BusType -ne 'File Backed Virtual') {
            throw "SAFETY ABORT: disk $($d.Number) BusType '$($d.BusType)' is not a VHD."
        }
    }
    if ($srcDisk.Location -notlike '*src.vhd' -or $tgtDisk.Location -notlike '*tgt.vhd') {
        throw "SAFETY ABORT: disk/location mismatch."
    }

    # --- 3. Provision + format the SOURCE via Storage cmdlets, then payload. ---
    Log "Provisioning source disk $srcNum..."
    Set-Disk -Number $srcNum -IsOffline $false -ErrorAction SilentlyContinue
    Set-Disk -Number $srcNum -IsReadOnly $false -ErrorAction SilentlyContinue
    $srcDisk = Get-Disk -Number $srcNum
    if ($srcDisk.PartitionStyle -eq 'RAW') {
        Initialize-Disk -Number $srcNum -PartitionStyle MBR
        Log "  initialized disk $srcNum as MBR"
    }
    $part = New-Partition -DiskNumber $srcNum -UseMaximumSize -AssignDriveLetter
    Log "  created partition $($part.PartitionNumber), size $([math]::Round($part.Size/1MB)) MB, letter $($part.DriveLetter)"
    Format-Volume -Partition $part -FileSystem NTFS -NewFileSystemLabel 'BKSRC' -Confirm:$false -Force | Out-Null

    $sletter = Wait-DriveLetter $srcNum
    if (-not $sletter) { throw "source volume has no drive letter" }
    $sd = "${sletter}:"
    Log "Source volume at $sd"

    for ($i = 1; $i -le 25; $i++) {
        Set-Content -Path "$sd\file$i.txt" -Value ("payload-line " * 80 + "#$i")
    }
    $blob = New-Object byte[] (16MB)
    (New-Object System.Random(12345)).NextBytes($blob)
    [System.IO.File]::WriteAllBytes("$sd\blob.bin", $blob)

    $srcHashes = @{}
    Get-ChildItem "$sd\" -File | ForEach-Object {
        $srcHashes[$_.Name] = (Get-FileHash $_.FullName -Algorithm SHA256).Hash
    }
    Log "Wrote $($srcHashes.Count) payload files; flushing..."
    Write-VolumeCache -DriveLetter $sletter -ErrorAction SilentlyContinue

    # Offline source so its raw image is stable during backup (stands in for
    # the consistency VSS will later provide on a live volume).
    Set-Disk -Number $srcNum -IsOffline $true
    Log "Source disk offlined for a stable raw read."

    $srcDev = "\\.\PhysicalDrive$srcNum"
    $tgtDev = "\\.\PhysicalDrive$tgtNum"

    # --- 4. Back up the whole source PHYSICAL disk (MBR + partition + FS) ---
    Log "--- bkengine backup $srcDev ---"
    # --allow-torn: the engine refuses to image a live device without a VSS
    # snapshot, which is not implemented yet. Legitimate here because the
    # source VHD was taken offline above, so nothing is writing to it.
    if ((Invoke-Engine @('backup','--job','vhd','--source',$srcDev,'--store',$store,'--allow-torn')) -ne 0) {
        throw "backup failed"
    }

    # --- 5. Full clone onto the target disk (destructive, self-verified) ---
    Log "--- bkengine clone $tgtDev (full) ---"
    if ((Invoke-Engine @('clone','--job','vhd','--store',$store,'--target',$tgtDev,'--confirm')) -ne 0) {
        throw "clone failed"
    }

    # --- 6. Incremental re-clone: nothing changed => zero blocks written ---
    Log "--- bkengine clone $tgtDev (--delta, expect 0 written) ---"
    if ((Invoke-Engine @('clone','--job','vhd','--store',$store,'--target',$tgtDev,'--delta','--confirm')) -ne 0) {
        throw "incremental clone failed"
    }

    # --- 7. Filesystem-level proof: mount the CLONE, compare file hashes ---
    Log "Bringing cloned target online for filesystem verification..."
    "rescan" | diskpart | Out-Null
    Start-Sleep -Milliseconds 500
    Set-Disk -Number $tgtNum -IsReadOnly $false -ErrorAction SilentlyContinue
    Set-Disk -Number $tgtNum -IsOffline $false -ErrorAction SilentlyContinue
    "rescan" | diskpart | Out-Null
    Start-Sleep -Milliseconds 800

    $tletter = Wait-DriveLetter $tgtNum
    if (-not $tletter) { throw "cloned target volume has no drive letter" }
    $td = "${tletter}:"
    Log "Cloned target volume at $td"

    $mismatch = 0; $checked = 0
    foreach ($name in $srcHashes.Keys) {
        $th = (Get-FileHash "$td\$name" -Algorithm SHA256).Hash
        $checked++
        if ($th -ne $srcHashes[$name]) { $mismatch++; Log "  MISMATCH: $name" }
    }
    Log "Filesystem check: $checked/$($srcHashes.Count) files compared, $mismatch mismatches."
    if ($mismatch -eq 0 -and $checked -eq $srcHashes.Count) { $result = 'PASS' }
}
catch {
    Log "ERROR: $($_.Exception.Message)"
}
finally {
    Log "Cleaning up VHDs..."
    Remove-TestVhds
    Log "RESULT: $result"
}
