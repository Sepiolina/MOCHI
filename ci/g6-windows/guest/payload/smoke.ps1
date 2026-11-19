# Installer smoke test, SYSTEM half (docs/dev/release-runbook.md "Automated").
#
# The harness agent runs this at every boot, as SYSTEM, from the payload disc.
# Per-user installs, Explorer verbs and a visible window need an interactive
# session of a real user, so the test runs in two boots:
#
#   boot 1 (phase "prep"): record facts, optionally remove the WebView2 runtime
#     (so the installer's bootstrapper has something to do), turn on auto-logon
#     for the image's throwaway account g6, register a task that runs user.ps1
#     in g6's session at logon, and restart.
#   boot 2 (phase "wait"): g6 logs on, user.ps1 runs every check and writes
#     C:\g6\state\results.jsonl; this half waits for it, replays the results over
#     COM1 (the only channel to the host), and returns; the agent powers off.
#
# Differences from a real user's machine, written down in the runbook: the VM has
# no UAC consent prompt (so a per-machine WebView2 install does not stop for it),
# Defender and Windows Update are off (the image's quiet settings), and the
# account is the harness's throwaway administrator.
. C:\g6\payload\common.ps1
. C:\g6\payload\smokelib.ps1
New-Item -ItemType Directory -Force $GuestState | Out-Null
$phaseFile = "$GuestState\phase"
$phase = 'prep'
if (Test-Path $phaseFile) { $phase = (Get-Content $phaseFile | Select-Object -First 1).Trim() }
$opts = Get-Content C:\g6\payload\options.json -Raw | ConvertFrom-Json

function Fact([string]$k, $v) { Say ('SMOKE-FACT ' + (ConvertTo-AsciiJson @{ k = $k; v = $v })) }

if ($phase -eq 'prep') {
  Say 'SMOKE-PREP'
  try {
    $wf = Get-WindowsFacts
    foreach ($k in $wf.Keys) { Fact $k $wf[$k] }

    # Network: the VM has a user-mode (NAT) adapter for this test only.
    $net = $false
    for ($i = 0; $i -lt 45 -and -not $net; $i++) {
      try {
        $ip = @(Get-NetIPAddress -AddressFamily IPv4 -ErrorAction Stop | Where-Object { $_.IPAddress -notlike '169.254*' -and $_.IPAddress -ne '127.0.0.1' })
        if ($ip.Count -gt 0) { $net = $true }
      } catch { }
      if (-not $net) { Start-Sleep 2 }
    }
    Fact 'network_address' $net
    $code = $null
    if ($net) {
      [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
      try {
        $r = [Net.WebRequest]::Create('https://go.microsoft.com/fwlink/p/?LinkId=2124703')
        $r.Method = 'HEAD'; $r.AllowAutoRedirect = $false; $r.Timeout = 30000
        $resp = $r.GetResponse(); $code = [int]$resp.StatusCode; $resp.Close()
      } catch [Net.WebException] {
        if ($_.Exception.Response) { $code = [int]$_.Exception.Response.StatusCode } else { Fact 'bootstrapper_link_error' $_.Exception.Message }
      } catch { Fact 'bootstrapper_link_error' $_.Exception.Message }
    }
    Fact 'bootstrapper_link_http' $code
    $reach = ($code -ge 200 -and $code -lt 400)

    # WebView2 runtime: present on Windows 11, usually absent on an unpatched
    # Windows 10 22H2. Remove it when asked, so the bootstrapper is exercised.
    $before = Get-WebView2Version
    Fact 'webview2_found_at_start' $before
    $removed = $false
    if ($before -and $opts.remove_webview2) {
      foreach ($s in @(Get-ChildItem 'C:\Program Files (x86)\Microsoft\EdgeWebView\Application\*\Installer\setup.exe' -ErrorAction SilentlyContinue)) {
        try {
          $u = Start-Process -FilePath $s.FullName -ArgumentList '--uninstall', '--msedgewebview', '--system-level', '--verbose-logging', '--force-uninstall' -PassThru
          [void]$u.WaitForExit(300000)
          Fact 'webview2_uninstall_exit' $u.ExitCode
        } catch { Fact 'webview2_uninstall_error' $_.Exception.Message }
      }
      Start-Sleep 15
      $removed = -not (Get-WebView2Version)
    }
    $now = Get-WebView2Version
    Fact 'webview2_before_install' $now
    Fact 'webview2_removed' $removed
    [ordered]@{ webview2_at_start = $before; webview2_before_install = $now; webview2_removed = $removed
                network = $net; bootstrapper_reachable = $reach } |
      ConvertTo-Json | Set-Content "$GuestState\prep.json"

    # The task runs in the interactive session of g6. Copy the payload where the
    # agent (which deletes and recopies C:\g6\payload at every boot) cannot touch it.
    Remove-Item $GuestSmoke -Recurse -Force -ErrorAction SilentlyContinue
    Copy-Item C:\g6\payload $GuestSmoke -Recurse -Force
    New-Item -ItemType Directory -Force "$GuestState\shots" | Out-Null

    # Auto-logon for the throwaway account, and no UAC consent prompt (an
    # unattended per-machine WebView2 install would otherwise wait for a click).
    $wl = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty $wl AutoAdminLogon '1'
    Set-ItemProperty $wl DefaultUserName 'g6'
    Set-ItemProperty $wl DefaultPassword 'G6-crash-replay-VM-1'
    Set-ItemProperty $wl DefaultDomainName $env:COMPUTERNAME
    Remove-ItemProperty $wl AutoLogonCount -ErrorAction SilentlyContinue
    $sys = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
    Set-ItemProperty $sys ConsentPromptBehaviorAdmin 0 -Type DWord
    Set-ItemProperty $sys PromptOnSecureDesktop 0 -Type DWord

    $task = @{
      TaskName  = 'g6user'
      Action    = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\g6\smoke\user.ps1'
      Trigger   = New-ScheduledTaskTrigger -AtLogOn -User "$env:COMPUTERNAME\g6"
      Principal = New-ScheduledTaskPrincipal -UserId "$env:COMPUTERNAME\g6" -LogonType Interactive -RunLevel Limited
      Settings  = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Hours 2) -StartWhenAvailable
      Force     = $true
    }
    Register-ScheduledTask @task | Out-Null
    Set-Content $phaseFile 'wait'
    Say 'SMOKE-PREP-DONE'
  } catch {
    Say ('SMOKE-ABORT ' + $_.Exception.Message)
    return
  }
  Restart-Computer -Force
  # The agent powers the machine off when this script returns: stay here until
  # the restart takes the process down.
  Start-Sleep 900
  Say 'SMOKE-ABORT the restart did not happen'
  return
}

# phase "wait": g6 is logging on and user.ps1 runs in its session.
Say 'SMOKE-WAIT'
$waitStart = Get-Date
$deadline = $waitStart.AddMinutes(45)
$started = $false
while ((Get-Date) -lt $deadline -and -not (Test-Path "$GuestState\user-done")) {
  if (Test-Path "$GuestState\user-started") { $started = $true }
  if (-not $started -and ((Get-Date) - $waitStart).TotalMinutes -gt 8) {
    Say 'SMOKE-ABORT user.ps1 never started (auto-logon or the logon task did not run)'
    break
  }
  Start-Sleep 5
}
if (Test-Path "$GuestState\results.jsonl") {
  foreach ($l in @(Get-Content "$GuestState\results.jsonl")) { if ($l) { Send-Serial ('SMOKE-RESULT ' + $l) } }
}
if (Test-Path "$GuestState\user.log") {
  foreach ($l in @(Get-Content "$GuestState\user.log" | Select-Object -Last 60)) { Send-Serial ('USERLOG ' + $l) }
}
if (Test-Path "$GuestState\user-done") { Say 'SMOKE-DONE' } else { Say 'SMOKE-ABORT user.ps1 did not finish' }
