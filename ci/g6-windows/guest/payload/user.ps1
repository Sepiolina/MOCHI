# Installer smoke test, interactive half (docs/dev/release-runbook.md "Automated").
# Runs in the logged-on session of the image's throwaway account g6, started by
# the scheduled task smoke.ps1 registered. Two install/uninstall cycles:
#
#   A: a silent install (/S): the foreign-format opt-in is OFF (its default).
#      Checks the files, Start menu, association, verbs, the opt-in being off,
#      WebView2, and that uninstall leaves nothing.
#   B: /ASSOCIATE_FOREIGN. Launches the app, runs every Explorer
#      menu entry as Explorer does (a .mochi verb: its registered command with
#      %1 replaced; the two Add entries of the shell extension DLL: the app with
#      --add|--add-now -- PATH..., which the DLL cannot be right-clicked to do here; quick entries extract or create at once and replace nothing)
#      with a path containing spaces, non-ASCII letters and an ampersand, a second launch while the app runs, the opt-in "Open with"
#      handler, then uninstall and the leftovers check.
#
# Every check is recorded as one result: id, status (PASS, FAIL, ERROR, INFO,
# NOT-RUN), title, detail. A FAIL is an assertion that did not hold; an ERROR is
# the script itself breaking; INFO is an observation that is reported and never
# gates. Results go to C:\g6\state\results.jsonl, which smoke.ps1 replays to
# the host. Non-ASCII characters are built from code points (see smokelib.ps1).
. C:\g6\smoke\smokelib.ps1
New-Item -ItemType Directory -Force $GuestState | Out-Null
Set-Content "$GuestState\user-started" (Get-Date -Format o)
$ResultsFile = "$GuestState\results.jsonl"
$LogFile = "$GuestState\user.log"
Remove-Item $ResultsFile -ErrorAction SilentlyContinue

function Log([string]$m) {
  $line = "$(Get-Date -Format HH:mm:ss) $m"
  Add-Content $LogFile $line
  Send-Serial ('USER ' + $line)
}

function Rec([string]$id, [string]$status, [string]$title, $detail = '') {
  $o = [ordered]@{ id = $id; status = $status; title = $title; detail = ([string]$detail) }
  $j = ConvertTo-AsciiJson $o
  Add-Content $ResultsFile $j
  Log "RESULT $id $status $detail"
}

function Fail([string]$m) { throw "ASSERT: $m" }

# Run one check. The block returns a detail string (PASS), or a hashtable
# @{ status = 'INFO'|'NOT-RUN'|'PASS'; detail = '...' }, or throws (Fail = FAIL).
function Check([string]$id, [string]$title, [scriptblock]$block) {
  try {
    $r = @(& $block)
    $last = if ($r.Count -gt 0) { $r[$r.Count - 1] } else { $null }
    if ($last -is [hashtable]) { Rec $id $last.status $title $last.detail }
    else { Rec $id 'PASS' $title (($r | Out-String).Trim()) }
  } catch {
    $m = $_.Exception.Message
    if ($m.StartsWith('ASSERT: ')) { Rec $id 'FAIL' $title $m.Substring(8) }
    else { Rec $id 'ERROR' $title ($m + ' @ ' + $_.InvocationInfo.ScriptLineNumber) }
  }
}

function Shot([string]$name) {
  # Ask the host for a screendump of the VM; wait for its answer (up to 30 s).
  try {
    $p = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
    $p.ReadTimeout = 30000
    $p.Open()
    try {
      $p.WriteLine("SHOTREQ $name")
      while ($true) { if ($p.ReadLine().Trim() -eq "SHOTTED $name") { break } }
    } finally { $p.Close() }
  } catch { Log "screenshot $name not taken: $($_.Exception.Message)" }
}

# ---- native helpers (each compiled separately: a failure costs one check) -----------

$haveAssoc = $false
try {
  Add-Type -TypeDefinition @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class G6Assoc {
  [DllImport("shlwapi.dll", CharSet = CharSet.Unicode)]
  static extern int AssocQueryStringW(uint flags, int str, string assoc, string extra, StringBuilder sb, ref uint len);
  // what: 1 = command, 2 = executable, 3 = friendly document name, 4 = friendly app name.
  // Returns null when there is no association (the HRESULT is not an error for the caller).
  public static string Query(string assoc, int what, string verb) {
    uint len = 2048;
    StringBuilder sb = new StringBuilder((int)len);
    int hr = AssocQueryStringW(0, what, assoc, verb, sb, ref len);
    return hr == 0 ? sb.ToString() : null;
  }
}
"@
  $haveAssoc = $true
} catch { Log "G6Assoc did not compile: $($_.Exception.Message)" }

$haveEnum = $false
try {
  Add-Type -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class G6OpenWith {
  [ComImport, Guid("F04061AC-1659-4a3f-A954-775AA57FC083"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IAssocHandler {
    [PreserveSig] int GetName([MarshalAs(UnmanagedType.LPWStr)] out string name);
    [PreserveSig] int GetUIName([MarshalAs(UnmanagedType.LPWStr)] out string name);
    [PreserveSig] int GetIconLocation([MarshalAs(UnmanagedType.LPWStr)] out string path, out int index);
    [PreserveSig] int IsRecommended();
    [PreserveSig] int MakeDefault([MarshalAs(UnmanagedType.LPWStr)] string description);
    [PreserveSig] int Invoke([MarshalAs(UnmanagedType.IUnknown)] object dataObject);
    [PreserveSig] int CreateInvoker([MarshalAs(UnmanagedType.IUnknown)] object dataObject, [MarshalAs(UnmanagedType.IUnknown)] out object invoker);
  }
  [ComImport, Guid("973810ae-9599-4b88-9e4d-6ee98c9552da"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IEnumAssocHandlers {
    [PreserveSig] int Next(uint count, [MarshalAs(UnmanagedType.Interface)] out IAssocHandler handler, out uint fetched);
  }
  [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
  static extern int SHAssocEnumHandlers(string extension, int filter, out IEnumAssocHandlers handlers);
  // The programs Windows itself lists under "Open with" for an extension (filter 1 = recommended).
  public static string[] List(string extension, int filter) {
    IEnumAssocHandlers e;
    int hr = SHAssocEnumHandlers(extension, filter, out e);
    if (hr != 0) throw new Exception("SHAssocEnumHandlers failed: 0x" + hr.ToString("x8"));
    List<string> r = new List<string>();
    while (true) {
      IAssocHandler h; uint fetched;
      if (e.Next(1, out h, out fetched) != 0 || fetched == 0) break;
      string n = null, u = null;
      h.GetName(out n); h.GetUIName(out u);
      r.Add((n ?? "") + " | " + (u ?? ""));
    }
    return r.ToArray();
  }
}
"@
  $haveEnum = $true
} catch { Log "G6OpenWith did not compile: $($_.Exception.Message)" }

# ---- fixtures and constants -------------------------------------------------------------

# A folder and file name with spaces, non-ASCII letters (e/u/i/a/c with accents) and an ampersand.
$trickyDir = Join-Path $env:USERPROFILE ("Documents\T$([char]0x00E9)st & spaces $([char]0x00DC)n$([char]0x00EF)")
$trickyName = "$([char]0x00C4)rchive & $([char]0x00E7)o.mochi"
$trickyName2 = "Second $([char]0x00E9) & copy.mochi"
$InstallDir = Join-Path $env:LOCALAPPDATA 'MOCHI'
$Prep = Get-Content "$GuestState\prep.json" -Raw | ConvertFrom-Json
# The Explorer menu (owner decision 2026-10-20, flat: no "MOCHI" cascade). Add to archive... and
# Add to "<Name>.mochi" on every file and folder are drawn by the shell extension DLL
# (mochi-shellext, a classic IContextMenu handler: CLSID below, ContextMenuHandlers\MOCHI under
# * and Directory); only .mochi files carry static verbs. Each value: option, menu text.
$shellExtClsid = '{508772cd-d924-4b90-8b87-37fd5d593932}'
$mochiVerbs = [ordered]@{
  'MochiOpen'            = @('--open', 'Open with MOCHI')
  'MochiExtractHere'     = @('--extract-here', 'Extract here')
  'MochiExtractToFolder' = @('--extract-to-folder', 'Extract to a new folder')
}
# Callers keep their item ids; this maps them to the static verb key, or (the DLL's two entries,
# which have no registry command) to the option the DLL passes.
$itemVerbKey = @{ '10Open' = 'MochiOpen'; '20ExtractHere' = 'MochiExtractHere'; '30ExtractToFolder' = 'MochiExtractToFolder' }
$dllOptions = @{ '40Add' = '--add-now'; '50AddPanel' = '--add' }
$legacyMenuKeys = @('SystemFileAssociations\.mochi\shell\MochiMenu', '*\shell\MochiMenu', 'Directory\shell\MochiMenu',
                    '*\shell\MochiAdd', 'Directory\shell\MochiAdd')
$foreignExts = 'zip', '7z', 'rar', 'tar', 'tgz', 'gz'
$classesRoot = [Microsoft.Win32.Registry]::ClassesRoot
$userClasses = [Microsoft.Win32.Registry]::CurrentUser

function Get-ClassDefault($root, [string]$sub) {
  $k = $root.OpenSubKey($sub)
  if (-not $k) { return $null }
  try { return [string]$k.GetValue('') } finally { $k.Close() }
}
function Test-OpenWithValue([string]$ext) {
  $k = $userClasses.OpenSubKey("Software\Classes\.$ext\OpenWithProgids")
  if (-not $k) { return $false }
  try { return ($k.GetValueNames() -contains 'MOCHI.Foreign') } finally { $k.Close() }
}

function Get-AppExe {
  $x = @(Get-ChildItem $InstallDir -Filter *.exe -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -notmatch '^uninstall' -and $_.Name -ne 'mochi-foreign-helper.exe' })
  if ($x.Count -ne 1) { Fail "expected one app exe in $InstallDir, found $($x.Count)" }
  $x[0]
}

# Registry: key names, value names and data that mention MOCHI. OS usage traces
# (recent files, compatibility records) are told apart: they record that the app
# ran, are not made by the installer, and are not removed by an uninstaller.
$gapRx = 'The MOCHI Project contributors'
$traceRx = '\\Explorer\\(FileExts|RecentDocs|ComDlg32|StartPage|Search)|AppCompatFlags|Compatibility Assistant|\\MuiCache|\\FeatureUsage|\\BagMRU|\\Bags\\|UserAssist|\\Shell\\(BagMRU|Bags)|\\Windows\\Shell\\Associations|\\CurrentVersion\\Start\\TileProperties'
function Scan-Registry {
  $lines = New-Object System.Collections.Generic.List[string]
  $scans = @(
    @('HKCU', '/k'), @('HKCU\Software\Classes', '/v'), @('HKCU\Software\Classes', '/d'),
    @('HKLM\SOFTWARE', '/k'), @('HKLM\SOFTWARE\Classes', '/v')
  )
  foreach ($s in $scans) {
    $cur = ''
    foreach ($l in @(& reg.exe query $s[0] /f mochi $s[1] /s 2>$null)) {
      if ($l -match '^HK') { $cur = $l; if ($s[1] -eq '/k') { $lines.Add("key $l") } }
      elseif ($l -match 'REG_') { $lines.Add("$($s[1]) $cur :: $($l.Trim())") }
    }
  }
  @($lines | Sort-Object -Unique)
}

# After an uninstall Explorer's own "open with" history can still name the deleted exe;
# that is a usage trace, not a registration. Only a program that still exists counts.
function Get-LiveAssoc {
  if (-not $haveAssoc) { return $null }
  $q = [G6Assoc]::Query('.mochi', 2, 'open')
  # Without an association Windows answers with its generic "Open with" program
  # (OpenWith.exe); only a MOCHI program that still exists is a leftover.
  if ($q -and $q -match 'mochi' -and (Test-Path -LiteralPath $q)) { return $q }
  return $null
}
function Get-InstallKeyLeft {
  # Tauri's install-location bookkeeping key (not a class registration): the installer
  # writes it and the NSIS uninstall hook removes it. Reported by its own check.
  $k = 'HKCU:\SOFTWARE\The MOCHI Project contributors'
  if (-not (Test-Path $k)) { return $null }
  $vals = foreach ($c in @(Get-Item $k; Get-ChildItem $k -Recurse -ErrorAction SilentlyContinue)) {
    $p = Get-ItemProperty $c.PSPath
    $names = @($c.GetValueNames() | ForEach-Object { if ($_ -eq '') { '(Default)' } else { $_ } })
    "$($c.Name -replace '^HKEY_CURRENT_USER','HKCU') [" + (($names | ForEach-Object { $n = $_; $v = if ($n -eq '(Default)') { $c.GetValue('') } else { $c.GetValue($n) }; "$n=$v" }) -join '; ') + "]"
  }
  ($vals -join ' || ')
}
function Get-Shortcuts {
  $sh = New-Object -ComObject WScript.Shell
  $found = @()
  foreach ($dir in @([Environment]::GetFolderPath('Programs'), [Environment]::GetFolderPath('CommonPrograms'),
                     [Environment]::GetFolderPath('Desktop'), [Environment]::GetFolderPath('CommonDesktopDirectory'))) {
    if (-not $dir) { continue }
    foreach ($f in @(Get-ChildItem $dir -Recurse -Filter *.lnk -ErrorAction SilentlyContinue)) {
      $t = $sh.CreateShortcut($f.FullName).TargetPath
      if ($t -and $t.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase)) { $found += [pscustomobject]@{ Link = $f.FullName; Target = $t } }
    }
  }
  $found
}

# ---- app control --------------------------------------------------------------------------

function Get-InstallProcs { @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) -and $_.Name -notlike 'uninstall*' }) }
function Stop-App {
  Get-InstallProcs | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep 3
}
function Get-AppProcs { @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) -and $_.Name -ne 'mochi-foreign-helper' -and $_.Name -notlike 'uninstall*' }) }
function Wait-AppWindow([int]$seconds) {
  for ($i = 0; $i -lt $seconds; $i++) {
    $w = @(Get-AppProcs | Where-Object { $_.MainWindowHandle -ne 0 })
    if ($w.Count -gt 0) { return $w[0] }
    Start-Sleep 1
  }
  return $null
}
function Get-UiaText([int]$hwnd, [int]$timeoutSec = 60) {
  $job = Start-Job -ArgumentList $hwnd -ScriptBlock {
    param($h)
    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
    $names = New-Object System.Collections.Generic.List[string]
    for ($i = 0; $i -lt 5; $i++) {
      $root = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]$h)
      $all = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
      foreach ($el in $all) { $n = $el.Current.Name; if ($n) { $names.Add($n) } }
      if ($names.Count -gt 5) { break }
      Start-Sleep 3
    }
    $names | Select-Object -Unique
  }
  $r = $null
  if (Wait-Job $job -Timeout $timeoutSec) { $r = @(Receive-Job $job) } else { Stop-Job $job }
  Remove-Job $job -Force -ErrorAction SilentlyContinue
  $r
}
function Get-DirSnapshot([string]$dir) { @(Get-ChildItem $dir -Recurse -Force -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName + '|' + $(if ($_.PSIsContainer) { 'd' } else { $_.Length }) } | Sort-Object) }

# One menu entry, started the way Explorer (or the DLL) starts it. A .mochi verb: its registered
# command with %1 replaced by the path. Add to archive... / Add to "<Name>.mochi": the DLL runs
# "<app>" --add|--add-now -- PATH... in ONE process (it cannot be right-clicked on a Server guest),
# so this does the same with the same arguments. `$path` may be several paths.
# `$wait` seconds for a window, then the UI text. Returns what was seen and what changed on disk.
$script:uiaNotes = New-Object System.Collections.Generic.List[string]
function Start-MenuEntry([string[]]$path, [string]$class, [string]$item) {
  if ($dllOptions.ContainsKey($item)) {
    $argList = @($dllOptions[$item], '--') + @($path | ForEach-Object { '"' + $_ + '"' })
    Start-Process -FilePath (Get-AppExe).FullName -ArgumentList $argList
    return
  }
  $cmd = Get-ClassDefault $classesRoot "$class\shell\$($itemVerbKey[$item])\command"
  if (-not $cmd) { Fail "no command registered for $class / $item" }
  $line = $cmd.Replace('%1', $path[0])
  Start-Process -FilePath (Get-AppExe).FullName -ArgumentList ($line.Substring($line.IndexOf('" ') + 2))
}
function Invoke-MenuEntry([string]$path, [string]$class, [string]$item, [string]$expectText, [int]$settle = 6) {
  Stop-App
  $dir = Split-Path $path
  $before = Get-DirSnapshot $dir
  Start-MenuEntry $path $class $item
  $w = Wait-AppWindow 45
  if (-not $w) { Fail "menu entry $item on '$path': no MOCHI window within 45 s" }
  Start-Sleep $settle
  $names = Get-UiaText $w.MainWindowHandle
  $seen = 'UI text not readable'
  if ($names -and $names.Count -gt 0) {
    $hit = @($names | Where-Object { $_ -like "*$expectText*" })
    $seen = if ($hit.Count -gt 0) { "UI shows '$expectText'" } else { "UI readable ($($names.Count) names) but '$expectText' not among them" }
  }
  $script:uiaNotes.Add("${item}: $seen")
  $after = Get-DirSnapshot $dir
  [pscustomobject]@{ Window = $w; Seen = $seen; Added = @($after | Where-Object { $before -notcontains $_ }); Removed = @($before | Where-Object { $after -notcontains $_ }) }
}

# ---- install and uninstall --------------------------------------------------------------------

function Install-Mochi([string[]]$setupArgs) {
  $t0 = Get-Date
  $p = Start-Process -FilePath "$GuestSmoke\mochi-setup.exe" -ArgumentList $setupArgs -PassThru
  if (-not $p.WaitForExit(900000)) { Stop-Process -Id $p.Id -Force; Fail "setup did not finish in 15 minutes (arguments: $setupArgs)" }
  Start-Sleep 3
  if ($p.ExitCode -ne 0) { Fail "setup exit code $($p.ExitCode)" }
  "exit 0 after $([int]((Get-Date) - $t0).TotalSeconds) s"
}
function Uninstall-Mochi {
  $u = Join-Path $InstallDir 'uninstall.exe'
  if (-not (Test-Path $u)) { Fail 'no uninstall.exe to run' }
  # As a user would: the uninstaller copies itself to %TEMP% and returns at once, so
  # wait for the directory to go (an app still running would hold it).
  $p = Start-Process -FilePath $u -ArgumentList '/S' -PassThru
  [void]$p.WaitForExit(120000)
  for ($i = 0; $i -lt 90 -and (Test-Path $InstallDir); $i++) { Start-Sleep 2 }
  # The folder can go before the uninstaller has finished with shortcuts and registry
  # keys: wait for its temp copy (%TEMP%\~nsu*.tmp\Au_.exe) to exit.
  for ($i = 0; $i -lt 60; $i++) {
    $left = @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path -like '*\~nsu*' })
    if ($left.Count -eq 0) { break }
    Start-Sleep 2
  }
  Start-Sleep 5
  "uninstaller exit $($p.ExitCode); $InstallDir " + $(if (Test-Path $InstallDir) { 'STILL EXISTS: ' + ((Get-ChildItem $InstallDir -Recurse -Force | ForEach-Object { $_.FullName }) -join '; ') } else { 'gone' })
}

function Assert-FilesInstalled {
  $exe = Get-AppExe
  if ($exe.Length -lt 1MB) { Fail "$($exe.Name) is suspiciously small" }
  foreach ($f in 'uninstall.exe', 'mochi-foreign-helper.exe', 'licenses\THIRD-PARTY-LICENSES.txt') {
    if (-not (Test-Path (Join-Path $InstallDir $f))) { Fail "$f missing from $InstallDir" }
  }
  "$($exe.Name) ($([int]($exe.Length / 1MB)) MB), helper, uninstall.exe, licenses"
}
function Assert-Registrations([bool]$foreign) {
  $exe = (Get-AppExe).FullName
  $notes = @()
  if ((Get-ClassDefault $classesRoot '.mochi') -ne 'MOCHI.Archive') { Fail ".mochi (as Windows sees it) is not associated with MOCHI.Archive" }
  $open = Get-ClassDefault $classesRoot 'MOCHI.Archive\shell\open\command'
  if (-not $open -or -not $open.Contains($exe)) { Fail "MOCHI.Archive open command does not run the installed app: $open" }
  $type = Get-ClassDefault $classesRoot 'MOCHI.Archive'
  $notes += "type text '$type'"
  if ($haveAssoc) {
    $q = [G6Assoc]::Query('.mochi', 2, 'open')
    if (-not $q -or $q -ne $exe) { Fail "AssocQueryString(.mochi) is '$q', expected $exe" }
    $notes += 'AssocQueryString resolves .mochi to the installed app'
  }
  # Flat static verbs on .mochi files (no cascade).
  $mochiShell = 'SystemFileAssociations\.mochi\shell'
  foreach ($v in $mochiVerbs.Keys) {
    $k = "$mochiShell\$v"
    $text = Get-ClassDefault $classesRoot $k
    if ($text -ne $mochiVerbs[$v][1]) { Fail "entry $k has menu text '$text', expected '$($mochiVerbs[$v][1])'" }
    $cmd = Get-ClassDefault $classesRoot "$k\command"
    $expect = '"' + $exe + '" ' + $mochiVerbs[$v][0] + ' -- "%1"'
    if ($cmd -ne $expect) { Fail "entry $k runs '$cmd', expected '$expect'" }
    if ($cmd -match 'passphrase|password') { Fail "entry $k puts a secret option on the command line" }
  }
  $sk = $userClasses.OpenSubKey("Software\Classes\$mochiShell")
  try { $names = @($sk.GetSubKeyNames()) } finally { $sk.Close() }
  if ($names.Count -ne $mochiVerbs.Count) { Fail ".mochi has verbs [$($names -join ', ')], expected [$($mochiVerbs.Keys -join ', ')]" }
  # The shell extension: a versioned DLL next to the app, an in-process COM server, and the
  # context-menu handler under * and Directory, all per user.
  $dlls = @(Get-ChildItem $InstallDir -Filter 'mochi-shellext-*.dll' -ErrorAction SilentlyContinue)
  if ($dlls.Count -ne 1) { Fail "expected one mochi-shellext-<version>.dll in $InstallDir, found $($dlls.Count)" }
  $inproc = Get-ClassDefault $userClasses "Software\Classes\CLSID\$shellExtClsid\InprocServer32"
  if ($inproc -ne $dlls[0].FullName) { Fail "InprocServer32 is '$inproc', expected '$($dlls[0].FullName)'" }
  foreach ($class in '*', 'Directory', 'Directory\Background') {
    $h = Get-ClassDefault $userClasses "Software\Classes\$class\shellex\ContextMenuHandlers\MOCHI"
    if ($h -ne $shellExtClsid) { Fail "$class has context-menu handler '$h', expected $shellExtClsid" }
  }
  foreach ($old in $legacyMenuKeys) {
    if ($userClasses.OpenSubKey("Software\Classes\$old")) { Fail "old menu key still registered: $old" }
  }
  $notes += "3 flat .mochi verbs with expected text and commands; shell extension $($dlls[0].Name) registered for *, Directory and Directory\Background; no MochiMenu cascade"
  $notes -join '; '
}

# ======================================================================================
function Main {
  Log 'user.ps1 started'
  for ($i = 0; $i -lt 90 -and -not (Get-Process explorer -ErrorAction SilentlyContinue); $i++) { Start-Sleep 2 }
  Start-Sleep 15
  $zipDefaultBefore = Get-ClassDefault $classesRoot '.zip'
  Check 'W-ENV' 'Guest facts' {
    $wf = Get-WindowsFacts
    @{ status = 'INFO'; detail = "$($wf.family) $($wf.edition) $($wf.display_version) build $($wf.build); WebView2 at start: $($Prep.webview2_at_start); before install: $($Prep.webview2_before_install); removed by the test: $($Prep.webview2_removed); network: $($Prep.network); bootstrapper link reachable: $($Prep.bootstrapper_reachable); user $env:USERNAME, interactive session $((Get-Process -Id $PID).SessionId)" }
  }
  Shot 'desktop-before'

  # ---------------- cycle A: silent install, opt-in off ------------------------------------------
  $regBeforeA = Scan-Registry
  Check 'A-CLEAN-START' 'Nothing of MOCHI is present before the first install' {
    if (Test-Path $InstallDir) { Fail "$InstallDir exists before install" }
    if (Get-ClassDefault $classesRoot '.mochi') { Fail '.mochi already has a program' }
    if (@(Get-Shortcuts).Count) { Fail 'a MOCHI shortcut exists before install' }
    'no install dir, no .mochi association, no shortcut'
  }
  Check 'A-INSTALL' 'Silent per-user install (setup /S)' { Install-Mochi @('/S') }
  Check 'A-FILES' 'App, foreign-format helper, uninstaller and license list are installed' { Assert-FilesInstalled }
  Check 'A-UNINSTALL-ENTRY' 'Per-user "Apps" entry exists (HKCU Uninstall key)' {
    $k = Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' -ErrorAction SilentlyContinue |
      Where-Object { (Get-ItemProperty $_.PSPath).DisplayName -eq 'MOCHI' }
    if (-not $k) { Fail 'no HKCU uninstall entry named MOCHI' }
    $v = Get-ItemProperty $k.PSPath
    "DisplayName MOCHI, DisplayVersion $($v.DisplayVersion), Publisher $($v.Publisher)"
  }
  Check 'A-START-MENU' 'A Start-menu shortcut points at the installed app' {
    $sm = @(Get-Shortcuts | Where-Object { $_.Link -like '*\Start Menu\Programs\*' })
    if ($sm.Count -eq 0) { Fail 'no .lnk under the per-user or common Start Menu\Programs resolves to the installed app' }
    ($sm | ForEach-Object { $_.Link }) -join '; '
  }
  Check 'A-ASSOC-VERBS' '.mochi association, the flat .mochi verbs and the Explorer shell extension are registered (as Windows sees them)' { Assert-Registrations $false }
  Check 'A-FOREIGN-OFF' 'The foreign-format "Open with" opt-in is off after a silent install' {
    $notes = @()
    foreach ($x in $foreignExts) {
      if (Test-OpenWithValue $x) { Fail "MOCHI.Foreign is offered for .$x" }
      if ((Get-ClassDefault $classesRoot ".$x") -eq 'MOCHI.Foreign') { Fail ".$x default taken over" }
    }
    if (Get-ClassDefault $userClasses 'Software\Classes\MOCHI.Foreign') { Fail 'MOCHI.Foreign key exists' }
    $notes += 'no MOCHI.Foreign key or value'
    if ($haveEnum) {
      try {
        $h = [G6OpenWith]::List('.zip', 1)
        if (@($h | Where-Object { $_ -match 'mochi' }).Count -gt 0) { Fail "Windows lists MOCHI under Open with for .zip: $($h -join ' ; ')" }
        $notes += "Windows' own Open-with list for .zip has no MOCHI ($($h.Count) programs)"
      } catch [System.Management.Automation.RuntimeException] { if ($_.Exception.Message.StartsWith('ASSERT: ')) { throw }; $notes += "Open-with list not enumerable: $($_.Exception.Message)" }
    } else { $notes += 'Open-with list not enumerated (helper did not compile)' }
    $notes -join '; '
  }
  Check 'A-WEBVIEW2' 'WebView2 is present after the install (the bootstrapper ran if it was missing)' {
    $after = Get-WebView2Version
    $before = $Prep.webview2_before_install
    if ($after -and -not $before) { return @{ status = 'PASS'; detail = "WebView2 was absent before the install; the bootstrapper installed $after" } }
    if ($after -and $before) { return @{ status = 'INFO'; detail = "WebView2 $before was already present, so the bootstrapper was NOT exercised; this stays on the manual list (runbook step: clean-VM snapshot)" } }
    if (-not $Prep.bootstrapper_reachable) { return @{ status = 'NOT-RUN'; detail = 'WebView2 is absent and the VM could not reach the bootstrapper download; nothing can be concluded' } }
    Fail 'WebView2 was absent, the download was reachable, and it is still absent after the install'
  }
  Shot 'installed-A'
  Check 'A-UNINSTALL' 'Uninstall removes the install directory, shortcuts and the Apps entry' {
    $r = Uninstall-Mochi
    if (Test-Path $InstallDir) { Fail $r }
    if (@(Get-Shortcuts).Count) { Fail "a shortcut to MOCHI remains: $((Get-Shortcuts | ForEach-Object { $_.Link }) -join '; ')" }
    $k = Get-ChildItem 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall' -ErrorAction SilentlyContinue |
      Where-Object { (Get-ItemProperty $_.PSPath).DisplayName -eq 'MOCHI' }
    if ($k) { Fail 'the HKCU uninstall entry remains' }
    $r
  }
  Check 'A-LEFTOVERS' 'After uninstall: no Mochi*, MOCHI.Archive or MOCHI.Foreign key or value; .mochi has no program' {
    $after = Scan-Registry
    $new = @($after | Where-Object { $regBeforeA -notcontains $_ })
    $ours = @($new | Where-Object { $_ -notmatch $traceRx -and $_ -notmatch $gapRx })
    $trace = @($new | Where-Object { $_ -match $traceRx })
    if ($ours.Count -gt 0) { Fail ("registry entries remain: " + ($ours -join ' || ')) }
    if (Get-ClassDefault $classesRoot '.mochi') { Fail '.mochi still has a program' }
    $live = Get-LiveAssoc
    if ($live) { Fail "AssocQueryString still resolves .mochi to an existing program: $live" }
    if (Get-ClassDefault $classesRoot 'MOCHI.Archive') { Fail 'MOCHI.Archive still resolves' }
    if ($trace.Count -gt 0) { @{ status = 'PASS'; detail = "none left by the installer; $($trace.Count) OS usage trace(s) ignored: $($trace -join ' || ')" } }
    else { 'none' }
  }

  Check 'A-INSTALL-KEY' 'After uninstall: the install-location key under HKCU\Software is gone' {
    $left = Get-InstallKeyLeft
    if (-not $left) { return 'gone' }
    Fail "the uninstaller leaves: $left (Tauri writes it at install; packaging/nsis/hooks.nsh removes it on uninstall)"
  }
  Remove-Item 'HKCU:\SOFTWARE\The MOCHI Project contributors' -Recurse -Force -ErrorAction SilentlyContinue

  # ---------------- cycle B: /ASSOCIATE_FOREIGN, launch, verbs, uninstall ---------------------------
  New-Item -ItemType Directory -Force $trickyDir | Out-Null
  Copy-Item "$GuestSmoke\fx-valid.mochi" (Join-Path $trickyDir $trickyName) -Force
  Copy-Item "$GuestSmoke\fx-valid.mochi" (Join-Path $trickyDir $trickyName2) -Force
  Copy-Item "$GuestSmoke\fx-basic.zip" (Join-Path $trickyDir "Zip & $([char]0x00E7).zip") -Force
  $plain = Join-Path $trickyDir "Note $([char]0x00E9) & more.txt"
  Set-Content $plain 'text'
  $sub = Join-Path $trickyDir "Folder & $([char]0x00FC)"
  New-Item -ItemType Directory -Force $sub | Out-Null
  Set-Content (Join-Path $sub 'inner.txt') 'inner'
  $archive = Join-Path $trickyDir $trickyName
  $archive2 = Join-Path $trickyDir $trickyName2

  $regBeforeB = Scan-Registry
  Check 'B-INSTALL' 'Silent install with /ASSOCIATE_FOREIGN' { Install-Mochi @('/S', '/ASSOCIATE_FOREIGN') }
  Check 'B-ASSOC-VERBS' '.mochi association, the flat .mochi verbs and the Explorer shell extension are registered again' { Assert-Registrations $true }
  Check 'B-FOREIGN-ON' 'The opt-in adds MOCHI under "Open with" for ZIP, 7z, RAR, TAR, TGZ, GZ and takes over no default' {
    $exe = (Get-AppExe).FullName
    foreach ($x in $foreignExts) {
      if (-not (Test-OpenWithValue $x)) { Fail ".$x does not offer MOCHI.Foreign" }
      if ((Get-ClassDefault $classesRoot ".$x") -eq 'MOCHI.Foreign') { Fail ".$x default was taken over by MOCHI" }
    }
    $fc = Get-ClassDefault $classesRoot 'MOCHI.Foreign\shell\open\command'
    if ($fc -ne ('"' + $exe + '" --open -- "%1"')) { Fail "MOCHI.Foreign command is '$fc'" }
    $zipNow = Get-ClassDefault $classesRoot '.zip'
    if ($zipNow -ne $zipDefaultBefore) { Fail ".zip default changed from '$zipDefaultBefore' to '$zipNow'" }
    "6 extensions offered; .zip default still '$zipNow'"
  }
  Check 'B-OPENWITH-ZIP' "Windows' own Open-with list for .zip now includes MOCHI" {
    if (-not $haveEnum) { return @{ status = 'NOT-RUN'; detail = 'enumeration helper did not compile' } }
    $h = [G6OpenWith]::List('.zip', 1)
    if (@($h | Where-Object { $_ -match 'mochi' }).Count -eq 0) { Fail "MOCHI is not in the list: $($h -join ' ; ')" }
    ($h -join ' ; ')
  }
  Stop-App
  Check 'B-LAUNCH' 'The app starts: a window appears and stays up' {
    $exe = Get-AppExe
    Start-Process -FilePath $exe.FullName
    $w = Wait-AppWindow 60
    if (-not $w) { Fail 'no MOCHI window within 60 s' }
    Start-Sleep 15
    $w = Wait-AppWindow 5
    if (-not $w) { Fail 'the window closed again within 15 s' }
    $wv = @(Get-Process msedgewebview2 -ErrorAction SilentlyContinue).Count
    if ($wv -eq 0) { Fail 'no msedgewebview2.exe process: the web view did not start' }
    Shot 'app-running'
    "window '$($w.MainWindowTitle)' (pid $($w.Id)), $wv WebView2 process(es)"
  }
  Check 'B-UI-READABLE' 'The window content can be read through UI Automation (observation)' {
    $w = Wait-AppWindow 5
    if (-not $w) { return @{ status = 'NOT-RUN'; detail = 'no window' } }
    $names = Get-UiaText $w.MainWindowHandle
    if (-not $names -or $names.Count -eq 0) { return @{ status = 'INFO'; detail = 'UI Automation returned no names; verb checks cannot confirm the file name in the window' } }
    $has = @($names | Where-Object { $_ -match 'Open archive' }).Count -gt 0
    @{ status = 'INFO'; detail = "$($names.Count) names; has 'Open archive': $has; sample: $(($names | Select-Object -First 12) -join ' | ')" }
  }
  Stop-App

  Check 'B-DOUBLE-CLICK' 'Double-clicking a .mochi file (default verb) opens MOCHI' {
    Stop-App
    Start-Process -FilePath $archive
    $w = Wait-AppWindow 45
    if (-not $w) { Fail "no MOCHI window within 45 s after opening $archive" }
    Start-Sleep 5
    $names = Get-UiaText $w.MainWindowHandle
    $seen = if ($names) { if (@($names | Where-Object { $_ -like "*$trickyName*" }).Count -gt 0) { 'the window text includes the archive file name' } else { 'window readable, archive name not found in it' } } else { 'window text not readable' }
    "window opened; $seen"
  }
  Check 'B-SINGLE-INSTANCE' 'A second .mochi opened while MOCHI runs goes to the same window (no second window or process)' {
    $before = @(Get-AppProcs | Where-Object { $_.Name -notlike 'msedgewebview2*' })
    Start-Process -FilePath $archive2
    Start-Sleep 12
    $after = @(Get-AppProcs | Where-Object { $_.Name -notlike 'msedgewebview2*' })
    $win = @($after | Where-Object { $_.MainWindowHandle -ne 0 })
    if ($before.Count -ne 1) { Fail "expected one MOCHI process before the second open, found $($before.Count)" }
    if ($after.Count -ne 1) { Fail "expected one MOCHI process after the second open, found $($after.Count)" }
    if ($win.Count -ne 1) { Fail "expected one window after the second open, found $($win.Count)" }
    "1 process and 1 window before and after"
  }
  Stop-App

  $mochiClass = 'SystemFileAssociations\.mochi'
  Check 'B-VERB-OPEN' 'Menu entry "MOCHI > Open with MOCHI" on the path with spaces, non-ASCII letters and &' {
    $r = Invoke-MenuEntry $archive $mochiClass '10Open' $trickyName
    if ($r.Added.Count -or $r.Removed.Count) { Fail "the folder changed: added $($r.Added -join '; ') removed $($r.Removed -join '; ')" }
    "window up; nothing written; $($r.Seen)"
  }
  Check 'B-VERB-EXTRACT-HERE' 'Menu entry "Extract here": extracts at once into the archive folder, replaces nothing' {
    $r = Invoke-MenuEntry $archive $mochiClass '20ExtractHere' 'Extracted'
    if ($r.Removed.Count) { Fail "something was removed or changed: $($r.Removed -join '; ')" }
    $files = @($r.Added | Where-Object { $_ -notmatch '\|d$' })
    if ($files.Count -eq 0) { Fail 'no file was extracted into the archive folder' }
    # Everything it wrote is in the folder next to the archive: the golden archive holds docs/a.txt.
    if (-not (Test-Path (Join-Path (Split-Path $archive) 'docs\a.txt'))) { Fail 'docs\a.txt was not extracted' }
    "compact window; $($files.Count) file(s) written, none replaced; $($r.Seen)"
  }
  Check 'B-VERB-EXTRACT-HERE-AGAIN' 'Menu entry "Extract here" again: names exist now, so nothing is written (the extract plan opens in the main window)' {
    $a = Join-Path (Split-Path $archive) 'docs\a.txt'
    Set-Content $a 'mine, not the archive'
    $r = Invoke-MenuEntry $archive $mochiClass '20ExtractHere' 'Nothing was written'
    if ($r.Added.Count -or $r.Removed.Count) { Fail "the folder changed: added $($r.Added -join '; ') removed $($r.Removed -join '; ')" }
    if ((Get-Content $a -Raw).Trim() -ne 'mine, not the archive') { Fail 'an existing file was replaced' }
    "nothing written, file untouched; $($r.Seen)"
  }
  Check 'B-VERB-EXTRACT-TO-FOLDER' 'Menu entry "Extract to a new folder": a new folder named for the archive, never an existing one' {
    $stem = [IO.Path]::GetFileNameWithoutExtension($archive)
    $existing = Join-Path (Split-Path $archive) $stem
    New-Item -ItemType Directory -Force $existing | Out-Null
    Set-Content (Join-Path $existing 'keep.txt') 'mine'
    $r = Invoke-MenuEntry $archive $mochiClass '30ExtractToFolder' 'Extracted'
    if ($r.Removed.Count) { Fail "something was removed or changed: $($r.Removed -join '; ')" }
    if (@(Get-ChildItem $existing -Force).Count -ne 1) { Fail "the existing folder $existing was reused" }
    $fresh = Join-Path (Split-Path $archive) "$stem (2)"
    if (-not (Test-Path (Join-Path $fresh 'docs\a.txt'))) { Fail "docs\a.txt is not in the new folder $fresh" }
    "extracted into '$stem (2)'; the existing folder was left alone; $($r.Seen)"
  }
  Check 'B-VERB-ADD-FILE' 'Menu entry "Add to archive" on one file: creates <name>.mochi next to it at once' {
    $r = Invoke-MenuEntry $plain '*' '40Add' 'Archive created'
    $made = Join-Path (Split-Path $plain) ([IO.Path]::GetFileNameWithoutExtension($plain) + '.mochi')
    if (-not (Test-Path $made)) { Fail "$made was not created" }
    if ($r.Removed.Count) { Fail "something was removed or changed: $($r.Removed -join '; ')" }
    "compact window; created $([IO.Path]::GetFileName($made)); $($r.Seen)"
  }
  Check 'B-VERB-ADD-FILE-AGAIN' 'Menu entry "Add to archive" again: a new name, the first archive is not replaced' {
    $made = Join-Path (Split-Path $plain) ([IO.Path]::GetFileNameWithoutExtension($plain) + '.mochi')
    $len = (Get-Item $made).Length
    $r = Invoke-MenuEntry $plain '*' '40Add' 'Archive created'
    $second = Join-Path (Split-Path $plain) ([IO.Path]::GetFileNameWithoutExtension($plain) + ' (2).mochi')
    if (-not (Test-Path $second)) { Fail "$second was not created" }
    if ((Get-Item $made).Length -ne $len) { Fail 'the first archive was changed' }
    "created $([IO.Path]::GetFileName($second)); the first is untouched; $($r.Seen)"
  }
  Check 'B-VERB-ADD-FOLDER' 'Menu entry "Add to archive" on a folder: <folder>.mochi next to it' {
    $r = Invoke-MenuEntry $sub 'Directory' '40Add' 'Archive created'
    $made = Join-Path (Split-Path $sub) ([IO.Path]::GetFileName($sub) + '.mochi')
    if (-not (Test-Path $made)) { Fail "$made was not created" }
    "created $([IO.Path]::GetFileName($made)); $($r.Seen)"
  }
  Check 'B-VERB-ADD-PANEL' 'Menu entry "Add to archive...": the main window shows the add panel; nothing is written' {
    $r = Invoke-MenuEntry $sub 'Directory' '50AddPanel' ([IO.Path]::GetFileName($sub))
    if ($r.Added.Count -or $r.Removed.Count) { Fail "the folder changed: added $($r.Added -join '; ') removed $($r.Removed -join '; ')" }
    "main window up; nothing written; $($r.Seen)"
  }
  Check 'B-VERB-ADD-MULTI' 'Menu entry "Add to archive" on several files (one launch with all paths, as the shell extension does) makes ONE archive named for the folder' {
    Stop-App
    $multi = Join-Path $trickyDir 'Multi & set'
    New-Item -ItemType Directory -Force $multi | Out-Null
    $files = @('one.txt', 'two.txt', 'three.txt') | ForEach-Object { $f = Join-Path $multi $_; Set-Content $f $_; $f }
    Start-MenuEntry $files '*' '40Add'  # one launch with all three paths, as the DLL does
    $made = Join-Path $multi 'Multi & set.mochi'
    for ($i = 0; $i -lt 60 -and -not (Test-Path $made); $i++) { Start-Sleep 1 }
    if (-not (Test-Path $made)) { Fail "$made was not created within 60 s" }
    Start-Sleep 8
    $archives = @(Get-ChildItem $multi -Filter *.mochi)
    if ($archives.Count -ne 1) { Fail "expected one archive for 3 files, found $($archives.Count): $($archives.Name -join ', ')" }
    $procs = @(Get-AppProcs | Where-Object { $_.Name -notlike 'msedgewebview2*' })
    if ($procs.Count -gt 1) { Fail "expected at most one MOCHI process for 3 files, found $($procs.Count)" }
    "3 files, 1 launch, 1 archive ($($archives[0].Name)), $($procs.Count) process"
  }
  foreach ($x in @(@('zip', "Zip & $([char]0x00E7).zip", 'fx-basic.zip'), @('7z', "Seven & $([char]0x00E7).7z", 'fx-basic.7z'),
                   @('tar', "Tar & $([char]0x00E7).tar", 'fx-basic.tar'), @('tar.gz', "Tgz & $([char]0x00E7).tar.gz", 'fx-basic.tar.gz'))) {
    $ext = $x[0]; $name = $x[1]; $src = $x[2]
    Check "B-FOREIGN-OPEN-$($ext.ToUpper().Replace('.', ''))" "Foreign .${ext}: the opt-in handler opens it for browsing" {
      if (-not (Test-Path "$GuestSmoke\$src")) { return @{ status = 'NOT-RUN'; detail = "fixture $src not supplied" } }
      Stop-App
      $f = Join-Path $trickyDir $name
      Copy-Item "$GuestSmoke\$src" $f -Force
      # The command Explorer runs for "Open with" -> MOCHI: the ProgID's open command with %1 replaced.
      $cmd = Get-ClassDefault $classesRoot 'MOCHI.Foreign\shell\open\command'
      $line = $cmd.Replace('%1', $f)
      $p = Start-Process -FilePath (Get-AppExe).FullName -ArgumentList ($line.Substring($line.IndexOf('" ') + 2)) -PassThru
      $w = Wait-AppWindow 45
      if (-not $w) { Fail "no MOCHI window for $name" }
      Start-Sleep 6
      $names = Get-UiaText $w.MainWindowHandle
      $seen = if ($names) { if (@($names | Where-Object { $_ -match 'foreign|Other archive formats' }).Count -gt 0) { 'foreign-format wording seen in the window' } else { 'window readable, no foreign-format wording found' } } else { 'window text not readable' }
      "window up; $seen"
    }
  }
  Check 'B-VERB-UI-NOTES' 'What UI Automation saw after each menu entry (observation)' {
    @{ status = 'INFO'; detail = ($script:uiaNotes -join ' || ') }
  }
  Shot 'verbs-done'
  Stop-App

  Check 'B-UNINSTALL' 'Uninstall (opt-in installed): install directory gone, shortcuts and Apps entry gone' {
    $r = Uninstall-Mochi
    if (Test-Path $InstallDir) { Fail $r }
    if (@(Get-Shortcuts).Count) { Fail "a shortcut to MOCHI remains: $((Get-Shortcuts | ForEach-Object { $_.Link }) -join '; ')" }
    $r
  }
  Check 'B-LEFTOVERS' 'After uninstall: no Mochi*, MOCHI.Archive or MOCHI.Foreign key or value; no Open-with entry; .mochi has no program' {
    $after = Scan-Registry
    $new = @($after | Where-Object { $regBeforeB -notcontains $_ })
    $ours = @($new | Where-Object { $_ -notmatch $traceRx -and $_ -notmatch $gapRx })
    $trace = @($new | Where-Object { $_ -match $traceRx })
    if ($ours.Count -gt 0) { Fail ("registry entries remain: " + ($ours -join ' || ')) }
    foreach ($x in $foreignExts) { if (Test-OpenWithValue $x) { Fail "MOCHI.Foreign is still offered for .$x" } }
    if (Get-ClassDefault $classesRoot '.mochi') { Fail '.mochi still has a program' }
    $live = Get-LiveAssoc
    if ($live) { Fail "AssocQueryString still resolves .mochi to an existing program: $live" }
    if ($haveEnum) {
      $h = [G6OpenWith]::List('.zip', 1)
      if (@($h | Where-Object { $_ -match 'mochi' }).Count -gt 0) { Fail "Windows still lists MOCHI for .zip: $($h -join ' ; ')" }
    }
    $trailer = if ($trace.Count -gt 0) { "; $($trace.Count) OS usage trace(s) ignored (they record that the app ran): $($trace -join ' || ')" } else { '' }
    "none left by the installer$trailer"
  }
  Check 'B-INSTALL-KEY' 'After uninstall: the install-location key under HKCU\Software is gone' {
    $left = Get-InstallKeyLeft
    if (-not $left) { return 'gone' }
    Fail "the uninstaller leaves: $left (Tauri writes it at install; packaging/nsis/hooks.nsh removes it on uninstall)"
  }
  Check 'B-APPDATA' 'User data the running app created (observation: an uninstall keeps it)' {
    $dirs = @("$env:APPDATA\io.github.immerh8.mochi", "$env:LOCALAPPDATA\io.github.immerh8.mochi", "$env:LOCALAPPDATA\MOCHI") |
      Where-Object { Test-Path $_ }
    $d = if ($dirs) { ($dirs | ForEach-Object { "$_ ($((Get-ChildItem $_ -Recurse -Force -ErrorAction SilentlyContinue | Measure-Object).Count) items)" }) -join '; ' } else { 'none' }
    @{ status = 'INFO'; detail = "left after uninstall: $d" }
  }
  Shot 'uninstalled'
  Rec 'W-END' 'INFO' 'Script reached its end' ''
}
try { Main } catch { Rec 'W-SCRIPT' 'ERROR' 'The script stopped' ($_.Exception.Message + ' @ ' + $_.InvocationInfo.ScriptLineNumber) }
finally {
  Set-Content "$GuestState\user-done" (Get-Date -Format o)
  Log 'user.ps1 done'
}
