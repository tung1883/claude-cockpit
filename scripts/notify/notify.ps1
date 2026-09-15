try { $p = [Console]::In.ReadToEnd() | ConvertFrom-Json } catch {}
$top = git rev-parse --show-toplevel 2>$null
$dir = if ($top) { Split-Path -Leaf $top } else { Split-Path -Leaf (Get-Location) }
. "$PSScriptRoot\claude-settings.ps1"
$settings = Get-NotifySettings

if ($settings.Terminal) {
    # SystemSounds plays the actual Windows notification chime (native
    # .wav asset) instead of a synthesized square-wave beep.
    Add-Type -AssemblyName System.Windows.Forms
    [System.Media.SystemSounds]::Exclamation.Play()
    # BEL flashes the tab's taskbar icon on terminals that honor it
    # (Windows Terminal does); the title change puts a persistent 🔔
    # marker in the tab header itself, visible even without that flash.
    Write-Host "`a" -NoNewline
    $Host.UI.RawUI.WindowTitle = "🔔 $dir - needs input"
}

if ($settings.Toast) {
    . "$PSScriptRoot\claude-toast.ps1"
    Show-ClaudeToast -Title 'Needs input' -Body $dir
}
