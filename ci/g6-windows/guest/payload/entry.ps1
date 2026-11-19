# The agent (in the image) runs this from the payload disc. It runs the phase
# script named in mode.txt and reports anything that escapes it over COM1, so
# a failure in a phase script is visible on the host instead of being a
# silent power-off.
. C:\g6\payload\common.ps1
try {
  $mode = (Get-Content C:\g6\payload\mode.txt -ErrorAction Stop | Select-Object -First 1).Trim()
  Say "ENTRY $mode"
  & "C:\g6\payload\$mode.ps1"
} catch {
  Say ('ENTRY-ERROR ' + ($_ | Out-String))
}
foreach ($e in @($Error | Select-Object -First 8)) { Say ('PSERR ' + ($e | Out-String)) }
Say 'ENTRY-END'
