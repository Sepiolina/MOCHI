# Runs once, at the first logon of the installed image, from the answer-file
# disc (volume label G6UNATTEND). Installs the agent, makes the guest quiet,
# records the exact Windows build, and powers off. Progress goes to COM1.
$ErrorActionPreference = 'Continue'
function Say([string]$m) {
  try {
    $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
    $p.Open(); $p.WriteLine(($m -replace "[\r\n]+", ' ')); $p.Close()
  } catch { }
}
Say 'PROVISION-START'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
New-Item -ItemType Directory -Force C:\g6 | Out-Null
Copy-Item "$here\agent.ps1" C:\g6\agent.ps1 -Force

# New disks come up offline, so that a disk carrying a dirty NTFS volume is
# not mounted (and chkdsk'd at boot) before the harness asks for it.
try { 'san policy=OfflineAll' | diskpart | Out-Null } catch { Say "SAN-ERROR $_" }

# Keep the guest quiet; failures here only cost noise, never correctness.
$quiet = {
  powercfg /h off
  powercfg /change standby-timeout-ac 0
  powercfg /change monitor-timeout-ac 0
  foreach ($s in 'wuauserv', 'UsoSvc', 'WSearch', 'SysMain', 'DiagTrack') {
    Stop-Service $s -Force -ErrorAction SilentlyContinue
    Set-Service $s -StartupType Disabled -ErrorAction SilentlyContinue
  }
  New-Item -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows Defender' -Force | Out-Null
  Set-ItemProperty 'HKLM:\SOFTWARE\Policies\Microsoft\Windows Defender' DisableAntiSpyware 1 -Type DWord
  Set-MpPreference -DisableRealtimeMonitoring $true
  reg add 'HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Schedule\Maintenance' /v MaintenanceDisabled /t REG_DWORD /d 1 /f
  bcdedit /set '{current}' recoveryenabled No
}
try { & $quiet | Out-Null } catch { Say "QUIET-ERROR $_" }

$task = @{
  TaskName  = 'g6agent'
  Action    = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\g6\agent.ps1'
  Trigger   = New-ScheduledTaskTrigger -AtStartup
  Principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -RunLevel Highest
  Force     = $true
}
try { Register-ScheduledTask @task | Out-Null; Say 'AGENT-REGISTERED' } catch { Say "TASK-ERROR $_" }

# The exact build under test, for the evidence.
try {
  $cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
  $ntfs = (fsutil fsinfo ntfsinfo C:) -join ';'
  $info = [ordered]@{
    ProductName = $cv.ProductName; EditionID = $cv.EditionID; DisplayVersion = $cv.DisplayVersion
    CurrentBuild = $cv.CurrentBuild; UBR = $cv.UBR; BuildLabEx = $cv.BuildLabEx
    Ntfs = $ntfs
  }
  Say ('BUILD ' + ($info | ConvertTo-Json -Compress))
} catch { Say "BUILD-ERROR $_" }

# Let first-boot servicing settle before the image is frozen.
Start-Sleep 120
Say 'PROVISIONED'
Stop-Computer -Force
