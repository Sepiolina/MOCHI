# Shared by the payload scripts. COM1 is the only channel to the host (never
# the data disk). It is opened per call and closed again, because the
# workload executable opens it itself while it runs.
$ErrorActionPreference = 'Continue'

function Say([string]$m) {
  try {
    $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
    $p.Open(); $p.WriteLine(($m -replace "[\r\n]+", ' ')); $p.Close()
  } catch { }
}

# Ask the host to put a mark into the dm-log-writes log, and wait for it.
function Mark([string]$label) {
  $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
  $p.ReadTimeout = 120000
  $p.Open()
  try {
    $p.WriteLine("MARKREQ $label")
    while ($true) {
      $l = $p.ReadLine().Trim()
      if ($l -eq "MARKED $label") { break }
    }
  } finally { $p.Close() }
}

function Get-G6Disk([string]$serial) {
  $d = Get-Disk | Where-Object { $_.SerialNumber -and $_.SerialNumber.Trim() -like "*$serial*" } | Select-Object -First 1
  if (-not $d) {
    Say ('DISKS ' + (Get-Disk | Select-Object Number, SerialNumber, Size, IsOffline, IsBoot | ConvertTo-Json -Compress))
  }
  $d
}

function Online-G6Disk($d) {
  Set-Disk -Number $d.Number -IsOffline $false -ErrorAction Stop
  Set-Disk -Number $d.Number -IsReadOnly $false -ErrorAction SilentlyContinue
}

function Sha256Bytes([byte[]]$b) {
  $h = [System.Security.Cryptography.SHA256]::Create()
  ([System.BitConverter]::ToString($h.ComputeHash($b)) -replace '-', '').ToLower()
}
