# Dot-sourced by notify*.ps1 — raises a real modern Windows toast via the
# WinRT ToastNotificationManager API, under our own AppUserModelID (set up
# by claude-shortcut.ps1) so the sender name reads "Claude Cockpit" instead
# of borrowed PowerShell branding. No custom icon — every variant tried
# (single-frame, multi-resolution, hand-rolled and byte-verified correct)
# still showed a blue background plate behind it; that turned out to be
# how Windows renders the small icon for any unpackaged/non-MSIX toast
# sender, not something fixable from a shortcut + AUMID script. Left as
# whatever default Windows assigns rather than fighting it further.
#
# NotifyIcon.ShowBalloonTip (the original approach, before this file
# existed) is a legacy XP-era API that on current Windows can return
# success while never actually registering anything with the notification
# system — not even Notification History showed it. This WinRT path is
# the real one.
$script:Aumid = 'ClaudeCockpit.Notify2'
$script:ShortcutPath = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Claude Cockpit.lnk'

function Show-ClaudeToast {
    param([string]$Title, [string]$Body)
    [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
    [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom, ContentType = WindowsRuntime] | Out-Null
    . "$PSScriptRoot\claude-shortcut.ps1"
    $psExe = (Get-Process -Id $PID).Path
    Ensure-AumidShortcut -ShortcutPath $script:ShortcutPath -TargetPath $psExe -IconPath $null -Aumid $script:Aumid -Description 'Claude Cockpit notifications'

    $escape = { param($s) [System.Security.SecurityElement]::Escape($s) }
    # One combined line, project first — a ToastGeneric with two <text>s
    # always renders title-then-body on its own line each; a single
    # <text> is what collapses it to one line. "Claude Cockpit" isn't
    # repeated here since the toast's sender header already shows it.
    $line = "$Body - $Title"
    $xmlText = @"
<toast>
  <visual>
    <binding template="ToastGeneric">
      <text>$(& $escape $line)</text>
    </binding>
  </visual>
</toast>
"@
    $xml = New-Object Windows.Data.Xml.Dom.XmlDocument
    $xml.LoadXml($xmlText)
    $toast = New-Object Windows.UI.Notifications.ToastNotification $xml
    [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier($script:Aumid).Show($toast)
}
