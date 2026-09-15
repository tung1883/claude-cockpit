# Dot-sourced by notify*.ps1 — reads the cockpit's own settings.json so the
# Notifications setting in cpit-dev's Settings screen actually controls
# these hook scripts too. Missing file or missing key both default to
# "toast", matching Settings' Rust-side NotifyMode::default().
function Get-NotifySettings {
    $path = Join-Path $env:USERPROFILE '.claude-multi-cockpit\settings.json'
    $mode = 'toast'
    try {
        $s = Get-Content $path -Raw | ConvertFrom-Json
        if ($s.notify_mode) { $mode = $s.notify_mode }
    } catch {}
    [PSCustomObject]@{ Toast = ($mode -eq 'toast'); Terminal = ($mode -eq 'terminal') }
}
