# One-time move of ~/.claude-multi-cockpit to ~/.claude-cockpit. Leaves a
# junction at the old name so absolute paths stored inside the data keep working.
# Run from a plain PowerShell window with every claude and cpit process closed.
$ErrorActionPreference = 'Stop'
$old = Join-Path $env:USERPROFILE '.claude-multi-cockpit'
$new = Join-Path $env:USERPROFILE '.claude-cockpit'

$running = Get-Process claude, cpit, cpit-dev -ErrorAction SilentlyContinue
if ($running) {
    $running | Format-Table Id, ProcessName | Out-String | Write-Host
    throw 'Close the processes above, then run this script again.'
}
if (-not (Test-Path $old)) { throw "Nothing to move: $old does not exist." }
if ((Get-Item $old -Force).LinkType) { throw "Already migrated: $old is a link." }
if (Test-Path $new) { throw "$new already exists; merge or remove it first." }

# A same-volume rename: it either moves everything or fails without changes.
[System.IO.Directory]::Move($old, $new)
New-Item -ItemType Junction -Path $old -Target $new | Out-Null
Write-Host "Moved to $new (junction left at $old)."
