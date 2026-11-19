# Installed in the image by provision.ps1 and run at every boot as SYSTEM.
# It carries no logic of the harness: it finds the payload disc the host
# attached (volume label G6PAYLOAD), copies it to C:\g6\payload, runs its
# entry.ps1, and powers off. Everything that changes between runs is in the
# payload, so the image can stay cached.
$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Force C:\g6 | Out-Null
function Say([string]$m) {
  try {
    $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
    $p.Open(); $p.WriteLine(($m -replace "[\r\n]+", ' ')); $p.Close()
  } catch { }
}
Say 'AGENT-UP'
$vol = $null
for ($i = 0; $i -lt 90 -and -not $vol; $i++) {
  $vol = Get-Volume | Where-Object { $_.FileSystemLabel -eq 'G6PAYLOAD' -and $_.DriveLetter } | Select-Object -First 1
  if (-not $vol) { Start-Sleep 2 }
}
if (-not $vol) { Say 'NO-PAYLOAD'; Stop-Computer -Force; exit }
Remove-Item C:\g6\payload -Recurse -Force -ErrorAction SilentlyContinue
Copy-Item ($vol.DriveLetter + ':\') C:\g6\payload -Recurse -Force
try {
  & powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\g6\payload\entry.ps1 *> C:\g6\entry.log
} catch { Say ('ENTRY-ERROR ' + $_.Exception.Message) }
Say 'AGENT-DONE'
Stop-Computer -Force
