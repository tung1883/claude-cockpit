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
$agentType = if ($p -and $p.agent_type) { " ($($p.agent_type))" } else { "" }
. "$PSScriptRoot\claude-settings.ps1"
$settings = Get-NotifySettings

if ($settings.Terminal) {
    Add-Type -AssemblyName System.Windows.Forms
    [System.Media.SystemSounds]::Asterisk.Play()
    Write-Host "`a" -NoNewline
    $Host.UI.RawUI.WindowTitle = "🔔 $dir - subagent done$agentType"
}

if ($settings.Toast) {
    . "$PSScriptRoot\claude-toast.ps1"
    Show-ClaudeToast -Title "Subagent done$agentType" -Body $dir
}
