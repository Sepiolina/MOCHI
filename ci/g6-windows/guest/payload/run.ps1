# Run phase: control experiments, then the real workload. The host marks the
# dm-log-writes log at every Mark call below.
. C:\g6\payload\common.ps1
try {
  Say 'RUN-START'
  $d = Get-G6Disk 'G6D000'
  if (-not $d) { throw 'data disk not found' }
  Online-G6Disk $d
  Start-Sleep 3
  Update-Disk -Number $d.Number -ErrorAction SilentlyContinue
  $part = Get-Partition -DiskNumber $d.Number | Where-Object { $_.Type -eq 'Basic' } | Select-Object -First 1
  if (-not $part) { throw 'no data partition' }
  if (-not $part.DriveLetter) {
    Set-Partition -DiskNumber $d.Number -PartitionNumber $part.PartitionNumber -NewDriveLetter E
    $letter = 'E'
  } else { $letter = [string]$part.DriveLetter }
  $root = "${letter}:\"
  if (-not (Test-Path $root)) { throw "$root is not mounted" }
  try { Add-MpPreference -ExclusionPath $root } catch { }
  Say ('VOLUME ' + (Get-Volume -DriveLetter $letter | Select-Object FileSystemType, Size | ConvertTo-Json -Compress))
  Start-Sleep 5

  Mark 'run:begin'

  # Controls (as in G7): data followed by FlushFileBuffers must be in the log
  # before the acknowledgement; data that was never flushed must be droppable.
  New-Item -ItemType Directory -Force "${root}ctl" | Out-Null
  foreach ($k in 1..3) {
    $b = New-Object byte[] 262144
    for ($i = 0; $i -lt $b.Length; $i++) { $b[$i] = (($i * 7) + $k) -band 255 }
    $sha = Sha256Bytes $b
    $fs = [System.IO.File]::Open("${root}ctl\synced_$k.bin", 'Create', 'Write', 'None')
    $fs.Write($b, 0, $b.Length); $fs.Flush($true); $fs.Close()
    Mark "ctl:synced:${k}:${sha}:$($b.Length)"
    $fs = [System.IO.File]::Open("${root}ctl\unsynced_$k.bin", 'Create', 'Write', 'None')
    $fs.Write($b, 0, $b.Length); $fs.Close()
    Mark "ctl:unsynced:${k}:${sha}:$($b.Length)"
  }

  # Namespace controls: what do the primitives the publication step can use
  # actually promise on this NTFS? Data is flushed first, so only the rename
  # is in question. plain: a rename with no flush (must be droppable, or the
  # harness cannot see a lost name); wt: MoveFileExW(MOVEFILE_WRITE_THROUGH),
  # what the G6a publication uses (must survive); dir: a rename followed by
  # FlushFileBuffers on the directory handle, what the link-then-unlink
  # publication's directory flush does (recorded).
  Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class G6Native {
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  public static extern bool MoveFileExW(string src, string dst, uint flags);
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)]
  public static extern SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr sa, uint disp, uint flags, IntPtr tmpl);
  [DllImport("kernel32.dll", SetLastError=true)]
  public static extern bool FlushFileBuffers(SafeFileHandle h);
}
"@
  foreach ($k in 1..3) {
    foreach ($v in 'plain', 'wt', 'dir') {
      $src = "${root}ctl\ns_${v}_${k}.src"; $dst = "${root}ctl\ns_${v}_${k}.dst"
      $b = New-Object byte[] 4096
      $fs = [System.IO.File]::Open($src, 'Create', 'Write', 'None')
      $fs.Write($b, 0, $b.Length); $fs.Flush($true); $fs.Close()
      if ($v -eq 'plain') { [System.IO.File]::Move($src, $dst) }
      elseif ($v -eq 'wt') {
        if (-not [G6Native]::MoveFileExW($src, $dst, 8)) { throw "MoveFileExW failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }
      } else {
        [System.IO.File]::Move($src, $dst)
        $h = [G6Native]::CreateFileW("${root}ctl", 0x40000000, 7, [IntPtr]::Zero, 3, 0x02000000, [IntPtr]::Zero)
        if ($h.IsInvalid) { throw 'cannot open the directory handle' }
        [void][G6Native]::FlushFileBuffers($h); $h.Close()
      }
      Mark "ctl:ns:${v}:${k}"
    }
  }

  Mark 'workload:begin'
  $wlArgs = @()
  if (Test-Path C:\g6\payload\wl-args.txt) {
    $wlArgs = @((Get-Content C:\g6\payload\wl-args.txt -Raw) -split '\s+' | Where-Object { $_ })
  }
  & C:\g6\payload\g6_winworkload.exe workload $root '\\.\COM1' @wlArgs *> C:\g6\wl.out
  $rc = $LASTEXITCODE
  Mark 'workload:end'
  Say "WORKLOAD-RC $rc"
  Get-Content C:\g6\wl.out -ErrorAction SilentlyContinue | ForEach-Object { Say "WL $_" }
  Say 'RUN-DONE'
} catch { Say ('RUN-ERROR ' + $_.Exception.Message) }
