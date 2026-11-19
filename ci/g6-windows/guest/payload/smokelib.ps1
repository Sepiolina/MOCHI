# Shared by smoke.ps1 (SYSTEM half) and user.ps1 (interactive half) of the installer
# smoke test (docs/dev/release-runbook.md, "Automated"). Dot-sourced; defines functions only.
# Do not put non-ASCII characters in these scripts: Windows PowerShell 5.1 reads a
# BOM-less file as ANSI. Build them from code points ([char]0x00E9) instead.

$ErrorActionPreference = 'Continue'
$GuestState = 'C:\g6\state'
$GuestSmoke = 'C:\g6\smoke'

# One JSON line, ASCII only (the serial line is ASCII and a non-ASCII character
# would turn into '?'). Characters above 0x7e become \uXXXX escapes.
function ConvertTo-AsciiJson($o) {
  $j = ConvertTo-Json -Compress -Depth 8 -InputObject $o
  [regex]::Replace($j, '[^\x20-\x7e]', { param($m) '\u{0:x4}' -f [int][char]$m.Value })
}

# COM1 is opened per call (the SYSTEM half also uses it). A busy port is retried.
function Send-Serial([string]$line) {
  for ($i = 0; $i -lt 8; $i++) {
    try {
      $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
      $p.Open(); $p.WriteLine(($line -replace "[\r\n]+", ' ')); $p.Close()
      return
    } catch { Start-Sleep -Milliseconds 400 }
  }
}

function Get-WebView2Version {
  $guid = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
  foreach ($p in "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\$guid",
                 "HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\$guid",
                 "HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\$guid") {
    $v = (Get-ItemProperty $p -ErrorAction SilentlyContinue).pv
    if ($v -and $v -ne '0.0.0.0') { return "$v" }
  }
  return $null
}

function Get-WindowsFacts {
  $cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
  $build = [int]$cv.CurrentBuild
  [ordered]@{
    family = $(if ($build -ge 22000) { 'Windows 11' } else { 'Windows 10' })
    edition = $cv.EditionID; display_version = $cv.DisplayVersion
    build = "$($cv.CurrentBuild).$($cv.UBR)"
  }
}
