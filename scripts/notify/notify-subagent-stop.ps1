try { $p = [Console]::In.ReadToEnd() | ConvertFrom-Json } catch {}
$top = git rev-parse --show-toplevel 2>$null
$dir = if ($top) { Split-Path -Leaf $top } else { Split-Path -Leaf (Get-Location) }
. "$PSScriptRoot\claude-settings.ps1"
$settings = Get-NotifySettings

if ($settings.Terminal) {
    Add-Type -AssemblyName System.Windows.Forms
    [System.Media.SystemSounds]::Asterisk.Play()
    Write-Host "`a" -NoNewline
    $Host.UI.RawUI.WindowTitle = "🔔 $dir - subagent done"
}

if ($settings.Toast) {
    . "$PSScriptRoot\claude-toast.ps1"
    Show-ClaudeToast -Title 'Subagent done' -Body $dir
}
