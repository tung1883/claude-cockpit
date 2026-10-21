# Run by cpit (rust/src/notify.rs) to raise one toast: powershell -File
# cpit-toast.ps1 -Title <title> -Body <text>. Reuses the AppUserModelID toast
# helper the hook scripts use.
param([string]$Title = '', [string]$Body = '')
. "$PSScriptRoot\claude-toast.ps1"
Show-ClaudeToast -Title $Title -Body $Body
