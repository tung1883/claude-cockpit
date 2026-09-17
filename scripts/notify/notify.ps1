try { $p = [Console]::In.ReadToEnd() | ConvertFrom-Json } catch {}
# The hook's own JSON payload carries the *actual* cwd for this exact
# invocation ("current working directory when the hook is invoked", per
# Claude Code's hook docs) — independently querying Get-Location/git in
# this script's own process instead used to show the wrong project when
# multiple sessions/panes were running, since this process's cwd doesn't
# necessarily match the session that actually fired the hook.
$hookCwd = if ($p -and $p.cwd) { $p.cwd } else { (Get-Location).Path }
$top = git -C $hookCwd rev-parse --show-toplevel 2>$null
$dir = if ($top) { Split-Path -Leaf $top } else { Split-Path -Leaf $hookCwd }
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
