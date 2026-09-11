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

Navigate with Up/Down (or `k`/`j`), `Enter` to launch, `→` to inspect the
selected account, `a` to add an account, `i` to import an existing login, `d` to
delete the selected profile (type its name to confirm), `s` to sync, `r` to
refresh the session counts, `q`/`Esc` to quit.

`→` opens a read-only detail view: account identity (email, plan, org, token
expiry), activity and storage (startups, session count and size, disk usage),
and a list of every plugin, skill, and MCP server with its on-disk size and a
rough token estimate. `Enter` on any of those opens a per-item screen — version,
install date, commit, markdown/instruction size, contents for a plugin;
description and files for a skill; type and command for an MCP server. Token
figures are estimates (instruction bytes ÷ 4), not live tool budgets.

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

In the menu, `s` on a profile opens the sync flow: pick the target account, then
tick which kinds to copy. Press `→` on a kind to open its individual items
(specific plugins / skills / MCP servers) and pick a subset; `→ Go` runs it.

It is additive — nothing in the target is removed. Plugin enablement is merged
into `settings.json` (`enabledPlugins`, `extraKnownMarketplaces`), skills are
copied per-directory, MCP servers are merged by name into `settings.json` and
`.claude.json`. Restart Claude Code in the target profile afterward.

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

Every profile gets the cockpit statusline automatically — `add` and `import`
both wire it into that profile's `settings.json` (on import, only if the
imported config doesn't already have a `statusLine` of its own). To (re)install
it by hand, e.g. after editing settings.json directly:

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

## Project notes: TODO.md / PLAN.md

Every project you launch Claude into through `ccpit` (any of `run`/`login`/`ccpit <profile>`/picking an account from the menu) automatically gets:

- a `TODO.md` and `PLAN.md` at the project root (the git top-level if it's a repo, else the directory you ran `ccpit` from) — created once, from an empty skeleton, and never overwritten if they already exist;
- a `.gitignore` entry for both, added inside a `# ccpit` marker block so re-running never duplicates it. This happens whether or not the directory is a git repo. If either file was already tracked by git, cockpit prints a note to `git rm --cached` it yourself rather than doing that automatically;
- live, two-way sync with one global master `TODO.md`/`PLAN.md` for as long as that Claude session runs: edit the project's file and it's mirrored into that project's section of the master file; edit the project's section in the master file and it flows back down. Each project gets one section in the master file, keyed by its path (`<!-- ccpit:path=... -->`) so renaming a project's directory doesn't lose the link as long as the path itself is unchanged, and two projects with the same folder name never collide.

Conflict rule is last-write-wins: whichever side changed more recently (tracked by content hash, falling back to file mtime when neither side has prior sync history) overwrites the other. A reconciliation pass also runs once at the start and end of every session, to catch edits made while nothing was watching.

Master files default to `~/.claude-multi-cockpit/TODO.md` and `.../PLAN.md`; point them elsewhere with `CLAUDE_COCKPIT_NOTES_DIR`. Turn the whole feature off with `CLAUDE_COCKPIT_NOTES=0`.

If notes syncing ever seems to hang, set `CLAUDE_COCKPIT_NOTES_DEBUG=1` before reproducing it — every step (git calls, file ensure, reconcile, watch setup/teardown) gets a timestamped line in `~/.claude-multi-cockpit/notes-debug.log`. The last line before it hangs is the call that never returned.

**View/edit from the cockpit menu**: press `n`, pick a file, and `Enter` opens it in cockpit's own small built-in text editor right there in the terminal — no notepad, no external process. Arrow keys to move, type to insert, `Backspace`/`Delete`, `Ctrl+S` to save without leaving, `Esc` to save and exit, `Ctrl+C` to bail without saving. Press `e` instead of `Enter` if you'd rather open it in `$VISUAL`/`$EDITOR` (falls back to `notepad`/`nano`). Either way, closing it triggers an immediate two-way sync before you're back at the menu. Sync from the command line without opening anything: `ccpit notes [dir]`.

```powershell
$env:CLAUDE_COCKPIT_NOTES_DIR = 'C:\Users\you\notes'
$env:CLAUDE_COCKPIT_NOTES = '0'   # disable entirely
```

## Design boundaries

This project does not mutate Claude’s live credential files and does not automatically bypass usage limits. Automatic rotation can be added above the explicit `handoff` primitive, but it should be used only with accounts the operator owns and in accordance with Anthropic’s terms.
