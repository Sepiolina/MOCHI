# Check phase: every data disk the host attached is a replay of the logged
# disk up to one point. Bring each online in turn (NTFS runs its own log
# recovery as it mounts), judge it, and take it offline again: the copies
# share a GPT disk GUID, so only one may be online at a time.
. C:\g6\payload\common.ps1
# Windows PowerShell 5.1 emits a JSON array as ONE object; the ForEach-Object
# unrolls it into one object per point.
$points = @(Get-Content C:\g6\payload\points.json -Raw | ConvertFrom-Json | ForEach-Object { $_ })
Say "CHECK-START $($points.Count)"
foreach ($pt in $points) {
  $res = [ordered]@{ id = $pt.id; serial = $pt.serial; kind = $pt.kind }
  $d = $null
  try {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $d = Get-G6Disk $pt.serial
    if (-not $d) { throw "disk $($pt.serial) not found" }
    Set-Disk -Number $d.Number -IsOffline $false -ErrorAction Stop
    Set-Disk -Number $d.Number -IsReadOnly $false -ErrorAction SilentlyContinue
    $part = $null; $vol = $null
    for ($i = 0; $i -lt 40; $i++) {
      Update-Disk -Number $d.Number -ErrorAction SilentlyContinue
      $part = Get-Partition -DiskNumber $d.Number -ErrorAction SilentlyContinue | Where-Object { $_.Type -eq 'Basic' } | Select-Object -First 1
      if ($part) {
        $vol = $part | Get-Volume -ErrorAction SilentlyContinue
        if ($vol -and $vol.FileSystemType -eq 'NTFS') { break }
      }
      Start-Sleep 1
    }
    $res.fs = if ($vol) { [string]$vol.FileSystemType } else { 'none' }
    if (-not $vol -or $vol.FileSystemType -ne 'NTFS') { $res.mount = 'failed'; throw 'volume did not mount as NTFS' }
    $assigned = $false
    if ($part.DriveLetter) { $letter = [string]$part.DriveLetter }
    else {
      Set-Partition -DiskNumber $d.Number -PartitionNumber $part.PartitionNumber -NewDriveLetter Z
      $letter = 'Z'; $assigned = $true
    }
    $root = "${letter}:\"
    for ($i = 0; $i -lt 20 -and -not (Test-Path $root); $i++) { Start-Sleep 1 }
    if (-not (Test-Path $root)) { $res.mount = 'failed'; throw "$root is not accessible" }
    $res.mount = 'ok'
    $res.dirty = ((fsutil dirty query "${letter}:") -join ' ')

    if ($pt.kind -like 'ctl*') {
      $facts = [ordered]@{}
      foreach ($rel in @($pt.probe)) {
        $p = Join-Path $root $rel
        if (Test-Path $p) {
          $facts[$rel] = [ordered]@{ exists = $true; size = (Get-Item $p).Length; sha256 = (Get-FileHash $p -Algorithm SHA256).Hash.ToLower() }
        } else { $facts[$rel] = [ordered]@{ exists = $false } }
      }
      $res.facts = $facts
    } else {
      $acks = "C:\g6\acks_$($pt.id).txt"
      Set-Content -Path $acks -Value @($pt.acks)
      $j = & C:\g6\payload\g6_winworkload.exe check $root $acks 2>&1
      $res.judge_rc = $LASTEXITCODE
      $res.judge = ($j | Out-String).Trim()
      $ver = [ordered]@{}
      foreach ($f in @(Get-ChildItem $root -Filter 'a*.mochi' -ErrorAction SilentlyContinue)) {
        $o = & C:\g6\payload\mochi.exe verify $f.FullName --json --no-local-history 2>&1
        $ver[$f.Name] = [ordered]@{ rc = $LASTEXITCODE; out = (($o | Out-String).Trim() -replace '\s+', ' ') }
        if ($ver[$f.Name].out.Length -gt 600) { $ver[$f.Name].out = $ver[$f.Name].out.Substring(0, 600) }
      }
      $res.verify = $ver
      $c = & chkdsk "${letter}:" /scan 2>&1
      $res.chkdsk_rc = $LASTEXITCODE
      $res.chkdsk_tail = (($c | Select-Object -Last 4 | Out-String).Trim() -replace '\s+', ' ')
    }
    if ($assigned) { Remove-PartitionAccessPath -DiskNumber $d.Number -PartitionNumber $part.PartitionNumber -AccessPath $root -ErrorAction SilentlyContinue }
    $res.elapsed_s = [math]::Round($sw.Elapsed.TotalSeconds, 1)
  } catch { $res.error = $_.Exception.Message }
  if ($d) { try { Set-Disk -Number $d.Number -IsOffline $true -ErrorAction Stop } catch { $res.offline_error = $_.Exception.Message } }
  Say ('RESULT ' + ($res | ConvertTo-Json -Compress -Depth 8))
}
Say 'CHECK-DONE'
