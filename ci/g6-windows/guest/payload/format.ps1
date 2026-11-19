# Format phase: the guest itself puts NTFS on the data disk. The result,
# cleanly dismounted, is the base image every replay starts from.
. C:\g6\payload\common.ps1
try {
  Say 'FORMAT-START'
  $d = Get-G6Disk 'G6D000'
  if (-not $d) { throw 'data disk not found' }
  Online-G6Disk $d
  Initialize-Disk -Number $d.Number -PartitionStyle GPT -ErrorAction Stop
  New-Partition -DiskNumber $d.Number -UseMaximumSize -DriveLetter E -ErrorAction Stop | Out-Null
  Format-Volume -DriveLetter E -FileSystem NTFS -NewFileSystemLabel G6DATA -Confirm:$false -ErrorAction Stop | Out-Null
  Say ('FORMATTED ' + (Get-Volume -DriveLetter E | Select-Object FileSystemType, FileSystemLabel, Size | ConvertTo-Json -Compress))
  Say ('DISKINFO ' + (Get-PhysicalDisk | Select-Object FriendlyName, BusType, MediaType, Size | ConvertTo-Json -Compress))
  Set-Disk -Number $d.Number -IsOffline $true
  Say 'FORMAT-DONE'
} catch { Say ('FORMAT-ERROR ' + $_.Exception.Message) }
