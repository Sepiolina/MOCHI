# Smoke-check the NSIS installer (plan D7, D6, O11): silent per-user install, the files
# and the shell registration exist, then silent uninstall removes them all, twice: once
# with the default (the foreign-format opt-in is OFF) and once with /ASSOCIATE_FOREIGN.
# Usage: smoke-nsis.ps1 path\to\setup.exe
# Note: Windows Server runner; client-only behaviour (WebView2 bootstrap on a machine
# without it, Explorer's menu display and its locked-DLL behaviour on update) is on the release checklist (docs/dev/packaging.md).
param([Parameter(Mandatory = $true)][string]$Setup)
$ErrorActionPreference = "Stop"
function Fail($m) { Write-Error "SMOKE FAIL: $m"; exit 1 }

$dir = Join-Path $env:LOCALAPPDATA "MOCHI"
$classes = [Microsoft.Win32.Registry]::CurrentUser

# The default value of HKCU\Software\Classes\<sub>, or $null if the key is absent.
# (The registry API, not the HKCU: drive, because the key "*" is a wildcard there.)
function Get-ClassDefault($sub) {
  $k = $classes.OpenSubKey("Software\Classes\$sub")
  if (-not $k) { return $null }
  try { return [string]$k.GetValue("") } finally { $k.Close() }
}
function Get-ClassValue($sub, $name) {
  $k = $classes.OpenSubKey("Software\Classes\$sub")
  if (-not $k) { return $null }
  try { return [string]$k.GetValue($name) } finally { $k.Close() }
}
function Test-ClassKey($sub) {
  $k = $classes.OpenSubKey("Software\Classes\$sub")
  if ($k) { $k.Close(); return $true }
  return $false
}
function Test-OpenWith($ext) {
  $k = $classes.OpenSubKey("Software\Classes\.$ext\OpenWithProgids")
  if (-not $k) { return $false }
  try { return ($k.GetValueNames() -contains "MOCHI.Foreign") } finally { $k.Close() }
}

# Every key and value this installer may add (plan D6; packaging/nsis/hooks.nsh).
# Owner decision 2026-10-20: a flat Explorer menu. The shell extension DLL (CLSID below)
# serves "Add to archive..." and Add to "<Name>.mochi" on every file and folder; three
# static verbs sit on .mochi files. The cascade "MochiMenu" and the flat verbs of 0.1.0
# must be gone (an update removes them, an uninstall leaves none).
$clsid = "{508772cd-d924-4b90-8b87-37fd5d593932}"
$verbKeys = @(
  "CLSID\$clsid",
  "*\shellex\ContextMenuHandlers\MOCHI",
  "Directory\shellex\ContextMenuHandlers\MOCHI",
  "Directory\Background\shellex\ContextMenuHandlers\MOCHI",
  "SystemFileAssociations\.mochi\shell\MochiOpen",
  "SystemFileAssociations\.mochi\shell\MochiExtractHere",
  "SystemFileAssociations\.mochi\shell\MochiExtractToFolder"
)
$legacyKeys = @(
  "SystemFileAssociations\.mochi\shell\MochiMenu",
  "*\shell\MochiMenu",
  "Directory\shell\MochiMenu",
  "*\shell\MochiAdd",
  "Directory\shell\MochiAdd"
)
$foreignExts = "zip", "7z", "rar", "tar", "tgz", "gz"

$installKey = "HKCU:\Software\The MOCHI Project contributors\MOCHI"

# The user's Path exactly as stored (unexpanded), or $null if there is none.
function Get-UserPath {
  $k = $classes.OpenSubKey("Environment")
  try { return $k.GetValue("Path", $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames) } finally { $k.Close() }
}
function Set-UserPath($value) {
  $k = $classes.OpenSubKey("Environment", $true)
  try {
    if ($null -eq $value) { $k.DeleteValue("Path", $false) }
    else { $k.SetValue("Path", $value, [Microsoft.Win32.RegistryValueKind]::ExpandString) }
  } finally { $k.Close() }
}
$cliDir = Join-Path $dir "bin"
# The CLI's registered location (packaging/nsis/hooks.nsh, MOCHI_APP_PATHS).
$appPaths = "Software\Microsoft\Windows\CurrentVersion\App Paths\mochi.exe"
function Get-AppPath($name) {
  $k = $classes.OpenSubKey($appPaths)
  if (-not $k) { return $null }
  try { return [string]$k.GetValue($name) } finally { $k.Close() }
}
$cli = Join-Path $cliDir "mochi.exe"
$shortPath = "C:\smoke-a;%USERPROFILE%\smoke-b"
# Longer than an NSIS string (1023 characters): ReadRegStr cannot read it whole, so the
# installer must leave it exactly as it is (packaging/nsis/hooks.nsh, MochiReadPath).
$longPath = (@("C:\smoke-x") + (1..60 | ForEach-Object { "C:\smoke\a\long\folder\name$_" })) -join ";"

function Assert-NothingRegistered($when) {
  foreach ($k in $verbKeys + $legacyKeys + @("MOCHI.Archive", "MOCHI.Foreign")) {
    if (Test-ClassKey $k) { Fail "$when : HKCU\Software\Classes\$k exists" }
  }
  if (Test-ClassKey ".mochi") { Fail "$when : HKCU\Software\Classes\.mochi exists" }
  foreach ($e in $foreignExts) {
    if (Test-OpenWith $e) { Fail "$when : MOCHI.Foreign still offered for .$e" }
  }
  if ($classes.OpenSubKey($appPaths)) { Fail "$when : HKCU\$appPaths exists" }
  # Tauri's install-location key; packaging/nsis/hooks.nsh removes it on uninstall.
  if (Test-Path $installKey) { Fail "$when : $installKey exists" }
}

function Run-Cycle([string[]]$SetupArgs, [bool]$ExpectForeign, [string]$StartPath = $shortPath) {
  Assert-NothingRegistered "before install"
  if (Test-Path $dir) { Fail "$dir exists before install" }
  Set-UserPath $StartPath
  $p = Start-Process -FilePath $Setup -ArgumentList $SetupArgs -Wait -PassThru
  if ($p.ExitCode -ne 0) { Fail "installer exit code $($p.ExitCode)" }

  Get-ChildItem $dir -Recurse | Select-Object -ExpandProperty FullName
  $exe = Get-ChildItem $dir -Filter *.exe | Where-Object { $_.Name -notmatch "^uninstall" -and $_.Name -ne "mochi-foreign-helper.exe" }
  if ($exe.Count -ne 1) { Fail "expected one app exe in $dir, found $($exe.Count)" }
  if ($exe.Length -lt 1MB) { Fail "$($exe.Name) is suspiciously small" }
  $uninst = Join-Path $dir "uninstall.exe"
  if (-not (Test-Path $uninst)) { Fail "no uninstall.exe (per-user install must be removable)" }
  $lic = Join-Path $dir "licenses\THIRD-PARTY-LICENSES.txt"
  if (-not (Test-Path $lic)) { Fail "third-party license list missing" }
  $text = Get-Content $lic -Raw
  foreach ($s in "Rust crates:", "npm packages (UI bundle):", "MIT License") {
    if (-not $text.Contains($s)) { Fail "license list lacks '$s'" }
  }
  foreach ($s in "UnRAR source code may be used in any software") {
    if (-not $text.Contains($s)) { Fail "license list lacks the RARLAB UnRAR notice" }
  }
  foreach ($c in "unrar_sys", "sevenz-rust2", "zip", "tar", "flate2") {
    if ($text -notmatch "Used by:[^\r\n]*(^|[ ,])$([regex]::Escape($c)) \d") { Fail "license list does not cover the helper's dependency $c" }
  }
  # F8: the user guide sits at the resource path the app resolves (help.rs GUIDE_RESOURCE,
  # "guide/index.html" under the install directory), with every page, offline and static.
  $guideIndex = Join-Path $dir "guide\index.html"
  if (-not (Test-Path $guideIndex)) { Fail "user guide missing at $guideIndex" }
  foreach ($page in "index", "not-a-backup", "getting-started", "encrypted", "verification", "tar-compatible", "recovery-record", "experimental") {
    $f = Join-Path $dir "guide\$page.html"
    if (-not (Test-Path $f) -or (Get-Item $f).Length -eq 0) { Fail "user guide page $page.html missing or empty" }
  }
  $guideText = Get-Content $guideIndex -Raw
  if (-not $guideText.Contains("This guide is written for the mochi command line.")) { Fail "user guide lacks its command-line scope banner" }
  if ($guideText -match "<script|src=|https?://") { Fail "user guide must be offline and static" }
  # D8 (T16): the foreign-format helper sits next to the app (where the app looks for it),
  # runs, and answers a protocol-v2 list of a real fixture.
  $helper = Join-Path $dir "mochi-foreign-helper.exe"
  if (-not (Test-Path $helper)) { Fail "mochi-foreign-helper.exe not installed next to the app" }
  if ((Get-Item $helper).Length -lt 500KB) { Fail "mochi-foreign-helper.exe is suspiciously small" }
  $fixture = Join-Path $PSScriptRoot "..\fixtures\foreign\basic_infozip.zip"
  $probe = Join-Path $PSScriptRoot "helper_probe.py"
  & python -I $probe $helper (Resolve-Path $fixture).Path --expect a.txt --expect sub/b.txt
  if ($LASTEXITCODE -ne 0) { Fail "the installed helper did not list the fixture" }
  # The mochi command-line program, alone in bin\ (never the install directory itself on
  # PATH: it holds uninstall.exe), runs, and that folder was appended to the user's Path,
  # unless the Path was too long to edit, which must then be untouched.
  if (-not (Test-Path $cli)) { Fail "bin\mochi.exe not installed" }
  $binFiles = @(Get-ChildItem $cliDir | Select-Object -ExpandProperty Name)
  if ($binFiles.Count -ne 1) { Fail "bin\ should hold only mochi.exe, holds: $($binFiles -join ', ')" }
  $ver = & $cli --version
  if ($LASTEXITCODE -ne 0 -or -not ($ver -join "`n").StartsWith("mochi ")) { Fail "bin\mochi.exe --version gave: $ver" }
  if ($text -notmatch "Used by:[^\r\n]*(^|[ ,])clap \d") { Fail "license list does not cover the CLI's dependency clap" }
  $registered = Get-AppPath ""
  $registeredDir = Get-AppPath "Path"
  if ($registered -ne $cli) { Fail "App Paths\mochi.exe default is '$registered', expected '$cli'" }
  if ($registeredDir -ne $cliDir) { Fail "App Paths\mochi.exe Path is '$registeredDir', expected '$cliDir'" }
  $path = Get-UserPath
  if ($StartPath -eq $longPath) {
    if ($path -ne $longPath) { Fail "a Path too long to edit was changed by the installer" }
  } elseif ($path -ne "$StartPath;$cliDir") { Fail "user Path after install is '$path', expected '$StartPath;$cliDir'" }
  # Tauri writes its install-location key (read again by the next install).
  if (-not (Test-Path $installKey)) { Fail "no $installKey after install" }
  # Per-user: the uninstall entry is under HKCU, nothing needed elevation.
  $key = Get-ChildItem "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall" |
    Where-Object { (Get-ItemProperty $_.PSPath).DisplayName -eq "MOCHI" }
  if (-not $key) { Fail "no HKCU uninstall entry for MOCHI" }

  # D6: the .mochi association, the Explorer verbs, and the opt-in.
  $exePath = $exe.FullName
  if ((Get-ClassDefault ".mochi") -ne "MOCHI.Archive") { Fail ".mochi is not associated with MOCHI.Archive" }
  $open = Get-ClassDefault "MOCHI.Archive\shell\open\command"
  if (-not $open -or -not $open.Contains($exePath)) { Fail "MOCHI.Archive open command does not run the installed app: $open" }
  # The flat static verbs on .mochi files, with their exact commands (no cascade).
  $mochiVerbs = [ordered]@{
    "MochiOpen"            = @("--open", "Open with MOCHI");
    "MochiExtractHere"     = @("--extract-here", "Extract here");
    "MochiExtractToFolder" = @("--extract-to-folder", "Extract to a new folder")
  }
  foreach ($v in $mochiVerbs.Keys) {
    $k = "SystemFileAssociations\.mochi\shell\$v"
    $text = Get-ClassDefault $k
    if ($text -ne $mochiVerbs[$v][1]) { Fail "menu entry $k has text '$text', expected '$($mochiVerbs[$v][1])'" }
    $icon = Get-ClassValue $k "Icon"
    if ($icon -ne "`"$exePath`",0") { Fail "menu entry $k has Icon '$icon', expected the MOCHI icon '`"$exePath`",0'" }
    $cmd = Get-ClassDefault "$k\command"
    $expect = "`"$exePath`" $($mochiVerbs[$v][0]) -- `"%1`""
    if ($cmd -ne $expect) { Fail "menu entry $k runs '$cmd', expected '$expect'" }
    if ($cmd -match "passphrase|password") { Fail "menu entry $k puts a secret option on the command line" }
  }
  $sk = $classes.OpenSubKey("Software\Classes\SystemFileAssociations\.mochi\shell")
  try {
    $names = @($sk.GetSubKeyNames())
    if ($names.Count -ne $mochiVerbs.Count) { Fail ".mochi has verbs [$($names -join ', ')], expected [$($mochiVerbs.Keys -join ', ')]" }
  } finally { $sk.Close() }

  # The Explorer shell extension (crates/mochi-shellext): one versioned DLL next to the app,
  # registered per user as an in-process COM server and as a context-menu handler for all
  # files and folders. Its two entries are drawn by the DLL, so no verb keys exist for them.
  $dlls = @(Get-ChildItem $dir -Filter "mochi-shellext-*.dll")
  if ($dlls.Count -ne 1) { Fail "expected one mochi-shellext-<version>.dll in $dir, found $($dlls.Count)" }
  $dll = $dlls[0].FullName
  if ($dlls[0].Length -lt 20KB) { Fail "$($dlls[0].Name) is suspiciously small" }
  $inproc = Get-ClassDefault "CLSID\$clsid\InprocServer32"
  if ($inproc -ne $dll) { Fail "InprocServer32 is '$inproc', expected '$dll'" }
  $ik = $classes.OpenSubKey("Software\Classes\CLSID\$clsid\InprocServer32")
  try { if ($ik.GetValue("ThreadingModel") -ne "Apartment") { Fail "ThreadingModel is not Apartment" } } finally { $ik.Close() }
  # "Directory\Background" is the empty space of an open folder (the Find entry only).
  foreach ($class in "*", "Directory", "Directory\Background") {
    $h = Get-ClassDefault "$class\shellex\ContextMenuHandlers\MOCHI"
    if ($h -ne $clsid) { Fail "$class has context-menu handler '$h', expected $clsid" }
  }
  # Per-user: nothing under HKLM (no elevation).
  if (Test-Path "HKLM:\Software\Classes\CLSID\$clsid") { Fail "the shell extension was registered machine-wide" }
  # The DLL loads and the class factory answers, in a child process that exits (so that the
  # uninstall below does not meet a locked file here; Explorer's locked-DLL case is covered
  # by the rename-aside in the hooks and by the release checklist).
  $probe = @"
`$h = [System.Runtime.InteropServices.NativeLibrary]::Load('$dll')
`$p = [IntPtr]::Zero
foreach (`$e in 'DllGetClassObject', 'DllCanUnloadNow') {
  if (-not [System.Runtime.InteropServices.NativeLibrary]::TryGetExport(`$h, `$e, [ref]`$p)) { exit 3 }
}
`$t = [type]::GetTypeFromCLSID([guid]'$clsid')
`$o = [Activator]::CreateInstance(`$t)
if (-not `$o) { exit 4 }
"@
  $pwsh = (Get-Process -Id $PID).Path
  & $pwsh -NoProfile -NonInteractive -Command $probe
  if ($LASTEXITCODE -ne 0) { Fail "the shell extension DLL did not load or create its COM class (exit $LASTEXITCODE)" }
  # Nothing of the old cascade next to it.
  foreach ($old in $legacyKeys) {
    if (Test-ClassKey $old) { Fail "legacy menu key $old is still registered" }
  }
  # The opt-in is OFF unless asked for, and never writes a type's default value.
  foreach ($e in $foreignExts) {
    if ((Test-OpenWith $e) -ne $ExpectForeign) { Fail ".$e offered = $(Test-OpenWith $e), expected $ExpectForeign" }
    $d = Get-ClassDefault ".$e"
    if ($d -and $d -eq "MOCHI.Foreign") { Fail ".$e default was taken over by MOCHI" }
  }
  if ($ExpectForeign) {
    $fc = Get-ClassDefault "MOCHI.Foreign\shell\open\command"
    if ($fc -ne "`"$exePath`" --open -- `"%1`"") { Fail "MOCHI.Foreign command is '$fc'" }
  } elseif (Test-ClassKey "MOCHI.Foreign") { Fail "MOCHI.Foreign exists though the opt-in was not asked for" }

  # NSIS uninstallers copy themselves to %TEMP% and return at once; /S plus
  # _?= keeps it in-process so -Wait works (the directory is then left empty).
  $u = Start-Process -FilePath $uninst -ArgumentList "/S", "_?=$dir" -Wait -PassThru
  if ($u.ExitCode -ne 0) { Fail "uninstaller exit code $($u.ExitCode)" }
  if (Test-Path (Join-Path $dir $exe.Name)) { Fail "app exe still present after uninstall" }
  if (Test-Path $helper) { Fail "mochi-foreign-helper.exe still present after uninstall" }
  if (Test-Path $cli) { Fail "bin\mochi.exe still present after uninstall" }
  $path = Get-UserPath
  if ($path -ne $StartPath) { Fail "user Path after uninstall is '$path', expected it restored to '$StartPath'" }
  if (Test-Path $guideIndex) { Fail "user guide still present after uninstall" }
  if (Test-Path $dll) { Fail "$dll still present after uninstall" }
  Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
  # Uninstall removes every key and association it added (plan D6).
  Assert-NothingRegistered "after uninstall"
}

# The user's own Path is put back however the checks end.
$savedPath = Get-UserPath
try {
  # A silent install takes the default: the foreign-format opt-in is off.
  Run-Cycle @("/S") $false
  # The explicit switch turns it on; uninstall still removes all of it.
  Run-Cycle @("/S", "/ASSOCIATE_FOREIGN") $true
  # A Path longer than an NSIS string: installed all the same, the Path never written.
  Run-Cycle @("/S") $false $longPath
} finally {
  Set-UserPath $savedPath
}
"NSIS smoke check passed"
