# Claude Multi Cockpit

An intentionally small, local Claude Code companion combining:

- `CLAUDE_CONFIG_DIR` account isolation from `claude-multi`;
- explicit transcript handoff between profiles;
- a compact two-line statusline inspired by Claudefy;
- no API proxy, telemetry, credential parsing, or dependency install.

## Install

Requirements: Node.js 18+ and Claude Code already installed.

```powershell
npm link
claude-cockpit add personal
claude-cockpit add work
```

Re-run `npm link` after upgrading so the new `ccpit` alias is installed.

After profiles are created, the master command is simply:

```powershell
claude-cockpit   # or the short alias: ccpit
```

It opens a colored account picker and launches Claude with the selected isolated
profile. The picker is a loop: when you exit Claude (`/exit`, or Ctrl+C twice)
you return to the cockpit menu instead of the shell, so switching accounts never
means retyping the command. Press `q` or `Esc` in the menu to leave entirely.

Navigate with Up/Down (or `k`/`j`), `Enter` to launch, `a` to add an account,
`i` to import an existing login, `d` to delete the selected profile (type its
name to confirm), `r` to refresh the session counts, `q`/`Esc` to quit.

To reuse an account you are already logged into on this machine, import its
config instead of logging in again — this keeps its sessions, plugins, skills,
and settings:

```powershell
ccpit import main            # copies ~/.claude + ~/.claude.json into profile "main"
ccpit import main <dir>      # or copy from a specific config dir
```

Close any running Claude Code before importing so the copy is consistent.

Delete a profile from the command line with an explicit confirmation flag:

```powershell
ccpit delete main --yes
```

### Sharing plugins, skills, and MCP servers between profiles

Profiles are isolated, but you can copy this state from one into another with
`sync` (menu key `s`, or the command):

```powershell
ccpit sync main work            # copy plugins + skills + MCP servers
ccpit sync main work plugins    # just one of: plugins | skills | mcp | all
```

It copies the `plugins/` and `skills/` directories and merges the relevant
`settings.json` keys (`enabledPlugins`, `extraKnownMarketplaces`, `mcpServers`,
`enabledMcpjsonServers`) plus `.claude.json` `mcpServers` into the target. It is
additive — nothing in the target is removed. Restart Claude Code in the target
profile afterward. Run it again whenever you add a plugin or MCP server you want
everywhere.

### Do profiles share anything?

No — each profile is a separate `CLAUDE_CONFIG_DIR`, so **user memory
(`<profile>/CLAUDE.md` and `<profile>/memory/`), credentials, sessions,
plugins, skills, MCP servers, and settings are per-profile**. Importing copies
them once; they diverge afterward.

The only shared state is whatever lives inside a project you open — a repo's
own `./CLAUDE.md` and `./.claude/` are read from the working directory, not the
config dir, so every profile sees the same project-level memory when working in
that repo.

Non-interactive dashboard and operational views are also available:

```powershell
claude-cockpit dashboard
claude-cockpit sessions work
claude-cockpit open work
claude-cockpit sessions --all
```

Adding an account creates its isolated profile; authenticate it with `/login` when Claude opens.

Account rotation is currently manual. Use `handoff` to continue a transcript under another account. The statusline reports quota data, but it does not interrupt and restart a running interactive Claude process automatically when a limit is reached.

Each profile has its own credentials, sessions, plugins, and settings under:

```text
%USERPROFILE%\.claude-multi-cockpit\profiles\<name>
```

Authenticate each one independently:

```powershell
claude-cockpit login personal
# inside Claude: /login

claude-cockpit login work
# inside Claude: /login
```

Run them in separate terminals:

```powershell
claude-cockpit run personal
claude-cockpit run work
```

Install the statusline into a profile:

```powershell
claude-cockpit install-statusline work
```

This backs up an existing `settings.json`, adds Claude Code's `statusLine` command, and uses the profile-specific status renderer. Restart Claude Code after installing it.

## Session handoff

Profiles intentionally do not share sessions. To continue a session under another account:

```powershell
claude-cockpit handoff <session-id> personal work
claude-cockpit run work --resume <session-id>
```

The handoff copies the JSONL transcript locally. The next Claude turn will resend that history as context, so the copy itself costs no tokens but the resumed turn does consume context/quota.

## Statusline

Claude Code statusline commands receive JSON on stdin. Configure the `statusLine` command in the profile’s `settings.json` to invoke `src/statusline.js`, for example on Windows:

```json
{
  "statusLine": {
    "type": "command",
    "command": "node C:/path/to/claude-multi-cockpit/src/statusline.js"
  }
}
```

The output matches Claude's own UI vibe — one warm orange accent, everything
else quiet — with a leading `✻` mark and `·` separators:

```text
✻ my-project · main · 12± · 3d · Opus 4.7 · 01:33 GMT+7 46m
  ctx 84% · 5h 24% 06:33 · 7d 17% · $8.42 · +616 -215 · 164.3k
```

Meter percentages are green under 50%, yellow under 80%, red above.

It reads project, branch, changes, commit age, Node version, model, session timing, context, cost, quota fields, token counts, and Git diff stats when those fields are present. Unknown fields degrade to `--`; it never fails the Claude session because a status value is unavailable.

Set the clock timezone with `CLAUDE_COCKPIT_TZ`, defaulting to `Asia/Bangkok`.

Color themes are selectable with `CLAUDE_COCKPIT_THEME`: `default`, `ocean`, `dracula`, `nord`, or `mono`.

```powershell
$env:CLAUDE_COCKPIT_THEME = 'dracula'
node src/cli.js run work
```

Set it before starting Claude so the statusline process inherits the theme. Set `NO_COLOR=1` to disable ANSI colors.

## Design boundaries

This project does not mutate Claude’s live credential files and does not automatically bypass usage limits. Automatic rotation can be added above the explicit `handoff` primitive, but it should be used only with accounts the operator owns and in accordance with Anthropic’s terms.
