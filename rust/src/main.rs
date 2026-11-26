mod account_settings;
mod clipboard;
mod conpty;
mod detail;
mod dirpick;
mod editor;
mod input;
mod inspect;
mod launch;
mod notes;
mod notify;
mod osc7501;
mod layout;
mod panels;
mod paths;
mod open;
mod pane_picker;
mod picker;
mod profiles;
mod quota;
mod session;
mod sidebar;
mod settings;
mod settings_docs;
mod statusline;
mod pins;
mod plugin;
mod sessions;
mod sync;
mod toc;
mod transcript;
mod handoff;
mod ui;
mod workspace;
mod worktree;

use anyhow::Result;
use open::OpenRequest;
use picker::Action;
use std::io::IsTerminal;
use std::path::PathBuf;

fn usage() {
    println!(
        "{}\n\n{}\n  {}            Open the interactive cockpit (master command)\n  {}                     Short alias for the same thing\n\n{}\n  add <name>                    Create an isolated account profile\n  delete <name>                 Delete a profile and all its data\n  list                          List profiles\n",
        ui::banner(),
        ui::color::bold("Usage"),
        ui::color::orange("claude-cockpit"),
        ui::color::orange("cpit"),
        ui::color::bold("Commands"),
    );
}

fn show_sessions(name: Option<&str>) -> Result<()> {
    let targets: Vec<String> = match name {
        None | Some("--all") => profiles::list_profiles()?.into_iter().map(|p| p.name).collect(),
        Some(n) => vec![n.to_string()],
    };
    for profile in targets {
        profiles::get_profile(&profile)?;
        let sessions = session::session_files(&profile);
        println!("\n{} {} session{}", ui::color::bold(&format!("[{profile}]")), sessions.len(), if sessions.len() == 1 { "" } else { "s" });
        for raw in &sessions {
            let d = session::session_details(raw);
            let modified: chrono::DateTime<chrono::Local> = d.modified.into();
            println!("  {}  {}", ui::color::orange(&d.id), ui::color::dim(&modified.format("%Y-%m-%d %H:%M:%S").to_string()));
            if !d.project_path.is_empty() {
                println!("    {} {}", ui::color::dim("project:"), d.project_path.replace('\\', "/"));
            }
            if !d.session_name.is_empty() {
                println!("    {} {}", ui::color::dim("name:"), d.session_name);
            }
            if !d.recap.is_empty() {
                let flat: String = d.recap.split_whitespace().collect::<Vec<_>>().join(" ");
                println!("    {} {}", ui::color::dim("last recap:"), flat.chars().take(240).collect::<String>());
            }
            if !d.last_prompt.is_empty() {
                let flat: String = d.last_prompt.split_whitespace().collect::<Vec<_>>().join(" ");
                println!("    {} {}", ui::color::dim("last prompt:"), flat.chars().take(160).collect::<String>());
            }
        }
    }
    Ok(())
}

fn dashboard() -> Result<()> {
    let profiles = profiles::list_profiles()?;
    println!("{}\n", ui::banner());
    if profiles.is_empty() {
        println!("No profiles yet. Create one with: {}", ui::color::orange("claude-cockpit add personal"));
        return Ok(());
    }
    for p in &profiles {
        let stat = layout::account_stat(&p.name);
        println!(
            "  {}  {}sessions  {}",
            ui::color::bold(&format!("{:<16}", p.name)),
            ui::color::orange(&format!("{:>3} ", stat.count)),
            ui::color::dim(&stat.age),
        );
    }
    println!("\n{}", ui::color::dim("Commands: claude-cockpit | sessions <profile> | handoff <id> <from> <to>"));
    Ok(())
}

/// Screens not yet ported from the JS version land here — shown briefly so
/// `cargo run` is usable end-to-end for the parts that ARE done, without
/// silently pretending the feature exists.
fn not_yet_implemented(what: &str) -> Result<()> {
    ui::home_clear();
    println!("{}\n", ui::banner());
    println!("{} isn't in the Rust rewrite yet — coming in a follow-up pass.", what);
    println!("{}", ui::color::dim("Press Enter to go back..."));
    loop {
        if matches!(input::read_key()?.code, crossterm::event::KeyCode::Enter) {
            break;
        }
    }
    Ok(())
}

/// The shared tail of every handoff flow once a session is chosen: pick a
/// target profile, copy the transcript, offer to resume there immediately.
fn run_handoff(profiles: &[profiles::Profile], mark: i64, from: &str, session_id: &str) -> Result<Option<OpenRequest>> {
    let others: Vec<&profiles::Profile> = profiles.iter().filter(|p| p.name != from).collect();
    if others.is_empty() {
        ui::home_clear();
        println!("{}\n", ui::banner());
        println!("{}", ui::color::dim("Need a second profile to hand off into."));
        picker::prompt_line("Press Enter...")?;
        return Ok(None);
    }
    let Some(target) = pick_handoff_target(profiles, mark, from)? else { return Ok(None) };

    ui::home_clear();
    match handoff::copy_session(from, &target, session_id) {
        Ok((_, _, stripped)) => {
            println!("{}", ui::color::green(&format!("Copied session {session_id} to '{target}'{}.", strip_note(stripped))));
            let go = picker::prompt_line("Enter to resume it there now (anything else to skip): ")?;
            if matches!(go.as_deref(), Some("")) {
                return Ok(Some(OpenRequest::resume(&target, session_id)));
            }
        }
        Err(e) => {
            eprintln!("{}", ui::color::red(&format!("Error: {e}")));
            picker::prompt_line("Press Enter...")?;
        }
    }
    Ok(None)
}

/// Asks which account to copy a session of `from` into.
pub(crate) fn pick_handoff_target(profiles: &[profiles::Profile], mark: i64, from: &str) -> Result<Option<String>> {
    let others: Vec<&profiles::Profile> = profiles.iter().filter(|p| p.name != from).collect();
    // Most headroom first: a target with a known low usage ranks above one that is
    // nearly full, and one with no reading goes last.
    let mut ranked: Vec<(&profiles::Profile, Option<quota::Quota>)> = others.iter().map(|p| (*p, quota::load(&p.name))).collect();
    ranked.sort_by(|a, b| {
        let key = |q: &Option<quota::Quota>| q.as_ref().map(quota::usage).unwrap_or(f64::MAX);
        key(&a.1).partial_cmp(&key(&b.1)).unwrap_or(std::cmp::Ordering::Equal)
    });
    let target_options: Vec<picker::ListOption> = ranked
        .iter()
        .map(|(p, q)| {
            let count = layout::account_stat(&p.name).count;
            let count_label = layout::pad_to(&format!("{count} sessions"), 12);
            let note = match q {
                Some(q) => format!("{count_label}·  {}", quota::describe(q)),
                None => format!("{count_label}·  usage unknown"),
            };
            picker::ListOption { label: p.name.clone(), value: p.name.clone(), note: Some(note) }
        })
        .collect();
    Ok(match picker::choose_from_list(profiles, mark, "Copy session into…", Some("most headroom first"), &target_options, None)? {
        picker::ListChoice::Picked(t) => Some(t),
        _ => None,
    })
}

/// A small file browser over one of Claude Code's own auto-memory
/// directories (`<profile>/memory` or `<profile>/projects/<encoded>/memory`)
/// — these are written by Claude itself, not user-authored like CLAUDE.md,
/// so this just lists whatever's there (if anything) and lets you create a
/// new file, rather than assuming/enforcing Claude's own naming scheme.
fn browse_memory_dir(profiles: &[profiles::Profile], mark: i64, dir: &std::path::Path) -> Result<()> {
    let mut at: Option<String> = None;
    loop {
        let mut files: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().filter(|e| e.path().is_file()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
            .unwrap_or_default();
        files.sort();
        let mut options: Vec<picker::ListOption> = files.iter().map(|f| picker::ListOption { label: f.clone(), value: dir.join(f).to_string_lossy().to_string(), note: None }).collect();
        options.push(picker::ListOption { label: "+ New file".into(), value: "__new__".into(), note: None });
        let heading = format!("Memory (auto) — {}", ui::color::orange(&dir.display().to_string()));
        let choice = picker::choose_from_list_lazy(profiles, mark, &heading, Some("these are Claude's own files, not user-authored"), &options, at.as_deref(), None, None)?;
        let file = match choice {
            picker::ListChoice::Back => return Ok(()),
            picker::ListChoice::Picked(v) if v == "__new__" => {
                let Some(input) = picker::prompt_line("New memory file name (e.g. notes.md): ")? else { continue };
                let input = input.trim();
                if input.is_empty() {
                    continue;
                }
                let filename = if input.ends_with(".md") { input.to_string() } else { format!("{input}.md") };
                std::fs::create_dir_all(dir)?;
                dir.join(filename).to_string_lossy().to_string()
            }
            picker::ListChoice::Picked(v) => v,
            _ => continue,
        };
        at = Some(file.clone());
        if !std::path::Path::new(&file).exists() {
            let _ = std::fs::write(&file, "");
        }
        let path = std::path::PathBuf::from(&file);
        let title = path.file_name().map(|n| n.to_string_lossy().to_string());
        editor::edit_file(&path, profiles, mark, title.as_deref())?;
    }
}

/// Read-only detail screen for one session — the "detail first" variant of
/// the overview's two session-row styles (see detail::session_rows).
/// Opens one project folder's session list; picking a session opens a small
/// action menu (resume here / hand off / view conversation) rather than
/// jumping straight into any one of them.
/// Row label for a session: its terminal title, else a clipped first-resort
/// summary, else the short id.
/// Suffix for the hand-off confirmation when thinking blocks were removed.
fn strip_note(stripped: usize) -> String {
    if stripped == 0 {
        String::new()
    } else {
        format!(" ({stripped} thinking block{} stripped — they only work on the account that made them)", if stripped == 1 { "" } else { "s" })
    }
}

fn session_label(s: &session::SessionFile, title: &str, summary: &str) -> String {
    if !title.is_empty() {
        return title.to_string();
    }
    let flat: String = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    if !flat.is_empty() {
        return flat.chars().take(60).collect();
    }
    s.id.chars().take(8).collect()
}

pub(crate) fn open_folder_sessions(profiles: &[profiles::Profile], mark: i64, from: &str, group: &session::SessionGroup, preselect: Option<&str>) -> Result<Option<OpenRequest>> {
    // session_preview() reads/scans each transcript (a folder can hold ones
    // tens of MB each), so rows used to fill in lazily on cursor approach —
    // but that made the 3rd column visibly pop in as you scrolled. Resolving
    // every row up front instead, spread across a worker pool the same way
    // `grouped_sessions`' cwd scan is, so opening a folder with hundreds of
    // sessions doesn't serialize into one long stall.
    let previews: Vec<(String, String)> = {
        let mut out = vec![(String::new(), String::new()); group.sessions.len()];
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
        let chunk_size = group.sessions.len().div_ceil(workers).max(1);
        std::thread::scope(|scope| {
            let handles: Vec<_> = group
                .sessions
                .chunks(chunk_size)
                .enumerate()
                .map(|(chunk_idx, chunk)| {
                    let base = chunk_idx * chunk_size;
                    scope.spawn(move || (base, chunk.iter().map(|s| session::session_preview(&s.file)).collect::<Vec<_>>()))
                })
                .collect();
            for h in handles {
                let (base, results) = h.join().unwrap();
                for (i, r) in results.into_iter().enumerate() {
                    out[base + i] = r;
                }
            }
        });
        out
    };
    session::flush_preview_cache();
    let options: Vec<picker::ListOption> = group
        .sessions
        .iter()
        .zip(previews.iter())
        .map(|(s, (session_name, summary))| {
            let label = session_label(s, session_name, summary);
            picker::ListOption { label, value: s.id.clone(), note: Some(session::relative_age(Some(s.modified))) }
        })
        .collect();
    let heading = format!("Sessions in {}", ui::color::orange(&group.folder));
    // Outer loop: backing out of a session's action menu lands on this
    // sessions list, not the account screen above it.
    let mut direct = preselect.map(str::to_string);
    'list: loop {
    let id = match direct.take() {
        Some(id) => id,
        None => {
            let choice = picker::choose_from_list(profiles, mark, &heading, Some("choose a session"), &options, None)?;
            let picker::ListChoice::Picked(id) = choice else { return Ok(None) };
            id
        }
    };

    loop {
        // Claude resumes a session by id from any folder, so "here" just means the
        // folder the cockpit is in; the folder it started in is one entry away.
        let started = group.sessions.iter().find(|s| s.id == id).and_then(|s| sessions::started_folder(&s.file));
        let here = std::env::current_dir().map(|p| sessions::norm_path(&p.to_string_lossy())).unwrap_or_default();
        let started_dir = started.filter(|p| !p.eq_ignore_ascii_case(&here) && std::path::Path::new(p).is_dir());
        let mut action_options = vec![
            picker::ListOption { label: "Resume here".into(), value: "resume".into(), note: Some(format!("continue it as '{from}'")) },
            picker::ListOption { label: "Hand off".into(), value: "handoff".into(), note: Some("copy it to another profile".into()) },
            picker::ListOption { label: "View conversation".into(), value: "view".into(), note: Some("read-only, no Claude launch".into()) },
            picker::ListOption { label: "System prompt".into(), value: "prompt".into(), note: Some("appended only when resuming this session".into()) },
            picker::ListOption { label: "Rename".into(), value: "rename".into(), note: Some("set a custom title".into()) },
            picker::ListOption { label: "Clone here".into(), value: "clone".into(), note: Some("fork into a new independent session".into()) },
            picker::ListOption { label: "Edit transcript".into(), value: "edit".into(), note: Some("blocks, tool calls, search, delete, truncate".into()) },
            picker::ListOption { label: "Edit raw .jsonl".into(), value: "rawedit".into(), note: Some("every line as-is (risky)".into()) },
            picker::ListOption { label: "Delete".into(), value: "delete".into(), note: Some("remove this session permanently".into()) },
        ];
        if let Some(dir) = &started_dir {
            action_options.insert(1, picker::ListOption { label: "Resume in started folder".into(), value: "resume-start".into(), note: Some(format!("open Claude in {dir}")) });
        }
        let choice = picker::choose_from_list(profiles, mark, "What do you want to do with it?", None, &action_options, None)?;
        let picker::ListChoice::Picked(action) = choice else {
            // Opened straight onto one session: back leaves, no list to land on.
            if preselect.is_some() {
                return Ok(None);
            }
            continue 'list;
        };
        match action.as_str() {
            "resume" => return Ok(Some(OpenRequest::resume(from, &id))),
            "resume-start" => {
                if let Some(dir) = &started_dir {
                    let dir = PathBuf::from(dir.replace('/', std::path::MAIN_SEPARATOR_STR));
                    return Ok(Some(OpenRequest { dir: Some(dir), ..OpenRequest::resume(from, &id) }));
                }
            }
            "handoff" => return run_handoff(profiles, mark, from, &id),
            "view" => {
                view_conversation(profiles, mark, group, &id)?;
                // Back to the action menu, not the whole folder — you
                // likely want to act on what you just read.
            }
            "prompt" => {
                if let Some(file) = group.sessions.iter().find(|s| s.id == id) {
                    let path = session::system_prompt_sidecar(&file.file);
                    if !path.exists() {
                        let _ = std::fs::write(&path, "");
                    }
                    let title = format!("system prompt — {}", &id[..id.len().min(8)]);
                    editor::edit_file(&path, profiles, mark, Some(&title))?;
                }
            }
            "rename" => {
                let short = &id[..id.len().min(8)];
                if let Some(new_title) = picker::prompt_line(&format!("New title for {short} (empty to clear): "))? {
                    if let Some(file) = group.sessions.iter().find(|s| s.id == id) {
                        let entry = serde_json::json!({"type": "custom-title", "customTitle": new_title.trim()});
                        let _ = std::fs::OpenOptions::new()
                            .append(true)
                            .open(&file.file)
                            .and_then(|mut f| { use std::io::Write; writeln!(f, "{entry}") });
                    }
                    return Ok(None);
                }
            }
            "clone" => {
                if let Some(file) = group.sessions.iter().find(|s| s.id == id) {
                    let new_id = uuid::Uuid::new_v4().to_string();
                    let dest = file.file.with_file_name(format!("{new_id}.jsonl"));
                    match std::fs::copy(&file.file, &dest) {
                        Ok(_) => {
                            layout::invalidate_stats();
                            println!("{}", ui::color::green(&format!("Cloned to new session {}", &new_id[..8])));
                            let _ = std::io::stdin().read_line(&mut String::new());
                        }
                        Err(e) => {
                            println!("{}", ui::color::red(&format!("Clone failed: {e}")));
                            let _ = std::io::stdin().read_line(&mut String::new());
                        }
                    }
                    return Ok(None);
                }
            }
            "edit" => {
                if let Some(file) = group.sessions.iter().find(|s| s.id == id) {
                    let (name, summary) = session::session_preview(&file.file);
                    let title = session_label(file, &name, &summary);
                    transcript::run(profiles, mark, &file.file, &id, &title, from, None)?;
                    // fall through: stay on action menu
                }
            }
            "rawedit" => {
                if let Some(file) = group.sessions.iter().find(|s| s.id == id) {
                    let title = format!("transcript — {}", &id[..id.len().min(8)]);
                    editor::edit_file(&file.file, profiles, mark, Some(&title))?;
                }
            }
            "delete" => {
                if let Some(file) = group.sessions.iter().find(|s| s.id == id) {
                    let short = &id[..id.len().min(8)];
                    let n_folders = sessions::folder_count(&file.file);
                    let scope = if n_folders > 1 { format!(" (touched {n_folders} folders; removed from all)") } else { String::new() };
                    let confirm = picker::prompt_line(&format!("Type {short} to confirm delete{scope}: "))?;
                    if confirm.as_deref().map(str::trim) == Some(short) {
                        let profile_dir = paths::profile_dir(from);
                        session::delete_session(&profile_dir, file)?;
                        println!("{}", ui::color::green("Deleted."));
                        let _ = std::io::stdin().read_line(&mut String::new());
                        return Ok(None);
                    } else {
                        println!("{}", ui::color::dim("Cancelled."));
                        let _ = std::io::stdin().read_line(&mut String::new());
                    }
                }
            }
            _ => continue 'list,
        }
    }
    }
}

/// Read-only transcript viewer — no Claude launch, nothing sent anywhere.
fn view_conversation(profiles: &[profiles::Profile], mark: i64, group: &session::SessionGroup, session_id: &str) -> Result<()> {
    let Some(file) = group.sessions.iter().find(|s| s.id == session_id) else { return Ok(()) };
    let turns = session::read_transcript(&file.file);

    let stats: Vec<layout::Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
    let mut header = layout::account_lines(profiles, mark, &stats);
    header.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold("Conversation")));
    header.push(ui::color::dim(&format!("   {}", group.folder)));
    header.push(String::new());

    let mut rows: Vec<picker::ScrollRow> = Vec::new();
    if turns.is_empty() {
        rows.push(picker::ScrollRow::line(ui::color::dim("  (no readable message text in this transcript)")));
    }
    for turn in &turns {
        let label = if turn.role == "user" { ui::color::orange_bold("You") } else { ui::color::green("Claude") };
        rows.push(picker::ScrollRow::line(format!("  {label}")));
        for line in wrap_paragraphs(&turn.text, 90) {
            rows.push(picker::ScrollRow::info(format!("    {line}")));
        }
        rows.push(picker::ScrollRow::line(String::new()));
    }
    picker::scroll_screen(&header, &rows, None, None)?;
    Ok(())
}

/// Word-wraps `text` to `width`, wrapping each existing line separately so
/// paragraph/blank-line breaks in the original message survive.
fn wrap_paragraphs(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in raw_line.split_whitespace() {
            let candidate = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
            if candidate.chars().count() > width && !cur.is_empty() {
                out.push(cur);
                cur = word.to_string();
            } else {
                cur = candidate;
            }
        }
        out.push(cur);
    }
    out
}

/// The start-up question about the newest saved layout: true to restore it.
/// "Not now" keeps it for the home screen's `o`.
fn ask_restore(profiles: &[profiles::Profile], mark: i64, saved: &workspace::Saved) -> Result<bool> {
    let n = saved.workspace.panes.len();
    let heading = format!("Restore {n} pane{}?", if n == 1 { "" } else { "s" });
    let hint = format!("{} · {}", session::relative_age(Some(saved.modified)), workspace::describe(&saved.workspace));
    let option = |label: &str, value: &str, note: &str| picker::ListOption { label: label.into(), value: value.into(), note: Some(note.into()) };
    let options = [
        option("Yes", "yes", "reopen the panes"),
        option("Not now", "later", "keep it: o on the home screen"),
        option("Discard", "discard", "forget this layout (the sessions stay)"),
    ];
    Ok(match picker::choose_from_list(profiles, mark, &heading, Some(&hint), &options, None)? {
        picker::ListChoice::Picked(v) if v == "yes" => true,
        picker::ListChoice::Picked(v) if v == "discard" => {
            workspace::discard(&saved.path);
            false
        }
        _ => false,
    })
}

/// The saved layouts, newest first: Enter picks one, `d` discards it.
fn pick_layout(profiles: &[profiles::Profile], mark: i64) -> Result<Option<workspace::Saved>> {
    loop {
        let mut saved = workspace::list();
        if saved.is_empty() {
            return Ok(None);
        }
        let stats: Vec<layout::Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
        let mut header = layout::account_lines(profiles, mark, &stats);
        header.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold("Restore panes")));
        header.push(String::new());
        let width = ui::term_width().saturating_sub(6);
        let rows: Vec<picker::ScrollRow> = saved
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let text = format!("{:<10} {}", session::relative_age(Some(s.modified)), workspace::describe(&s.workspace));
                picker::ScrollRow::pick(text.chars().take(width).collect::<String>(), i.to_string())
            })
            .collect();
        match picker::scroll_screen_ext(&header, &rows, None, None, None, Some(('d', "discard")))? {
            picker::ListChoice::Picked(v) => return Ok(v.parse().ok().filter(|&i| i < saved.len()).map(|i: usize| saved.swap_remove(i))),
            picker::ListChoice::RowAction(_, v) => {
                if let Some(s) = v.parse::<usize>().ok().and_then(|i| saved.get(i)) {
                    workspace::discard(&s.path);
                }
            }
            _ => return Ok(None),
        }
    }
}

fn restore_layout(profiles: &[profiles::Profile], saved: &workspace::Saved, session: &mut Option<panels::Session>) -> Result<()> {
    match panels::restore(profiles, saved) {
        Ok(s) => *session = s,
        Err(e) => {
            ui::home_clear();
            eprintln!("{}", ui::color::red(&format!("Restore failed: {e}")));
            picker::prompt_line("Press Enter...")?;
        }
    }
    Ok(())
}

/// Opens what a menu asked for: a pane when sessions are wrapped (added to the
/// running ones, if any), else plain Claude. After plain Claude exits, an
/// account that hit its limit is offered a hand-off, which is opened in turn.
fn dispatch(term: &mut ui::Terminal, profiles: &[profiles::Profile], mark: i64, request: OpenRequest, session: &mut Option<panels::Session>) -> Result<()> {
    if let Some(requests) = open::home_pane() {
        if let Err(e) = open::send(&requests, &request) {
            ui::home_clear();
            eprintln!("{}", ui::color::red(&format!("Could not ask for a pane: {e}")));
            picker::prompt_line("Press Enter...")?;
        }
        return Ok(());
    }
    let mut next = Some(request);
    while let Some(request) = next.take() {
        if settings::session_wrapped() {
            // Wrapped session: its own PTY, composited so /shell, F7 add-pane
            // and Ctrl+B work. Cockpit keeps terminal ownership (no suspend).
            if let Err(e) = panels::open(profiles, session, &request) {
                ui::home_clear();
                eprintln!("{}", ui::color::red(&format!("Session error: {e}")));
                picker::prompt_line("Press Enter...")?;
            }
        } else {
            let suspend = term.suspend(); // give Claude the real terminal
            // Hard wipe, not the soft home_clear: the primary buffer keeps
            // scrolling across launches, so a soft clear would stack every
            // past "launching Claude as..." line above.
            ui::clear_screen();
            let verb = if request.resume.is_some() { "resuming" } else { "launching Claude" };
            let place = request.dir.as_ref().map(|d| format!(" in {}", d.display())).unwrap_or_default();
            println!("{} {verb} as {}{place} — exit Claude to return here\n", ui::color::dim(ui::glyph::spark()), ui::color::orange(&request.profile));
            launch::launch_in(&request.profile, &request.claude_args(), request.dir.as_deref())?;
            drop(suspend); // restore alt screen + raw mode before the next repaint
        }
        // Panes watch for limits themselves (see `panels.rs`).
        if !settings::session_wrapped() && session.is_none() {
            next = offer_limit_handoff(profiles, mark, &request.profile)?;
        }
    }
    Ok(())
}

/// Auto-handoff: the statusline hook cached the last quota reading it saw while
/// Claude was running (main.rs can't see Claude's own output — stdio was
/// inherited straight through). If that reading says `name` just ran dry, offer
/// to copy its latest session onto another account.
fn offer_limit_handoff(profiles: &[profiles::Profile], mark: i64, name: &str) -> Result<Option<OpenRequest>> {
    if !quota::load(name).is_some_and(|q| quota::is_exhausted(&q)) {
        return Ok(None);
    }
    let Some(latest) = session::session_files(name).into_iter().next() else { return Ok(None) };
    // Candidates ranked best-first (known usage data beats an untested profile
    // with no cache at all, then lowest usage wins) — but that ranking is just
    // the *default position* in a real picker, not an auto-pick. You see every
    // non-exhausted profile and choose.
    let mut candidates: Vec<&profiles::Profile> = profiles
        .iter()
        .filter(|p| p.name != name)
        .filter(|p| !quota::load(&p.name).is_some_and(|q| quota::is_exhausted(&q)))
        .collect();
    candidates.sort_by(|a, b| {
        let qa = quota::load(&a.name);
        let qb = quota::load(&b.name);
        let rank = |q: &Option<quota::Quota>| i32::from(q.is_none());
        rank(&qa).cmp(&rank(&qb)).then_with(|| {
            let ua = qa.as_ref().map(quota::usage).unwrap_or(0.0);
            let ub = qb.as_ref().map(quota::usage).unwrap_or(0.0);
            ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    if candidates.is_empty() {
        ui::home_clear();
        println!("{}\n", ui::banner());
        println!("{}", ui::color::yellow(&format!("'{name}' just hit its usage limit.")));
        println!("{}", ui::color::dim("No other profile has headroom to hand off to right now."));
        picker::prompt_line("Press Enter...")?;
        return Ok(None);
    }
    let target_options: Vec<picker::ListOption> = candidates
        .iter()
        .map(|p| {
            let count = layout::account_stat(&p.name).count;
            let count_label = layout::pad_to(&format!("{count} sessions"), 12);
            let note = match quota::load(&p.name) {
                Some(q) => format!("{count_label}·  {:.0}% used", quota::usage(&q)),
                None => format!("{count_label}·  usage unknown"),
            };
            picker::ListOption { label: p.name.clone(), value: p.name.clone(), note: Some(note) }
        })
        .collect();
    let heading = format!("{} just hit its usage limit — hand off to…", ui::color::orange(name));
    let picker::ListChoice::Picked(candidate) = picker::choose_from_list(profiles, mark, &heading, Some("pick where to continue, or back to skip"), &target_options, None)? else {
        return Ok(None);
    };
    ui::home_clear();
    match handoff::copy_session(name, &candidate, &latest.id) {
        Ok((_, _, stripped)) => {
            println!("{}", ui::color::green(&format!("Copied session {} to '{candidate}'{}.", latest.id, strip_note(stripped))));
            let go = picker::prompt_line("Enter to resume it there now (anything else to skip): ")?;
            if matches!(go.as_deref(), Some("")) {
                return Ok(Some(OpenRequest::resume(&candidate, &latest.id)));
            }
        }
        Err(e) => {
            eprintln!("{}", ui::color::red(&format!("Handoff failed: {e}")));
            picker::prompt_line("Press Enter...")?;
        }
    }
    Ok(None)
}

/// Detail-view data (plugin/skill/MCP scan + session grouping) keyed by
/// profile name, filled in by a background thread started at cockpit
/// startup so it's usually already sitting there by the time you press →
/// instead of scanning on demand. `Action::Detail` below still falls back
/// to computing it synchronously if you get there before the prefetch does
/// for that profile — never a regression, just usually a head start.
type DetailCache = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (inspect::Inspection, Vec<session::SessionGroup>)>>>;

fn prefetch_detail_cache(profiles: &[profiles::Profile]) -> DetailCache {
    let cache: DetailCache = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();
    let bg_cache = cache.clone();
    std::thread::spawn(move || {
        for name in names {
            let snap = inspect::build_inspection(&paths::profile_dir(&name));
            let groups = session::grouped_sessions(&name);
            if let Ok(mut c) = bg_cache.lock() {
                c.insert(name, (snap, groups));
            }
        }
    });
    cache
}

fn cockpit() -> Result<()> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return dashboard();
    }
    let mut term = ui::Terminal::enter()?;
    let mut cursor: usize = 0;
    let detail_cache = prefetch_detail_cache(&profiles::list_profiles().unwrap_or_default());
    // Keep the session index (folders touched, searchable text) up to date.
    sessions::start_background_index(profiles::list_profiles().unwrap_or_default().into_iter().map(|p| p.name).collect());
    // A session detached with Ctrl+B keeps running in the background; it lives
    // here so the cockpit can offer to resume it. Dropped (killing its panes)
    // when the cockpit exits.
    let mut session: Option<panels::Session> = None;
    // As a pane of another cockpit, this one only asks it to open things.
    let in_pane = open::home_pane().is_some();
    // A layout left by an earlier run is offered once, at the start.
    let mut offer_restore = !in_pane;

    loop {
        let profiles = profiles::list_profiles()?;
        if profiles.is_empty() {
            ui::home_clear();
            println!("{}\n", ui::banner());
            println!("No accounts yet.");
            println!("  1  create a fresh empty account");
            println!("  (import isn't in the Rust rewrite yet)");
            let Some(name) = picker::prompt_line("Account name (blank to quit): ")? else { return Ok(()) };
            let name = name.trim();
            if name.is_empty() {
                return Ok(());
            }
            match profiles::create_account(name) {
                Ok(p) => println!("{}", ui::color::green(&format!("Created '{}'.", p.name))),
                Err(e) => eprintln!("{}", ui::color::red(&format!("Error: {e}"))),
            }
            continue;
        }

        cursor = cursor.min(profiles.len() - 1);
        if std::mem::take(&mut offer_restore) {
            if let Some(saved) = workspace::list().into_iter().next() {
                if ask_restore(&profiles, cursor as i64, &saved)? {
                    restore_layout(&profiles, &saved, &mut session)?;
                    continue;
                }
            }
        }
        let has_saved = !in_pane && !workspace::list().is_empty();
        let action = picker::pick_profile(&profiles, cursor, session.is_some(), has_saved, in_pane)?;
        let mark = match &action {
            Action::Open(n)
            | Action::Detail(n)
            | Action::Sync(n)
            | Action::Delete(n)
            | Action::Memory(n)
            | Action::Settings(n) => profiles.iter().position(|p| &p.name == n),
            _ => None,
        };
        if let Some(m) = mark {
            cursor = m;
        }

        match action {
            Action::Quit => return Ok(()),
            Action::Sessions => {
                let m = mark.map(|m| m as i64).unwrap_or(-1);
                if let Some(request) = sessions::run(&profiles, m, sessions::Scope::All)? {
                    dispatch(&mut term, &profiles, m, request, &mut session)?;
                }
            }
            Action::Refresh => {
                layout::invalidate_stats();
                continue;
            }
            Action::Settings(_) => {
                picker::settings_menu(&profiles, cursor as i64)?;
            }
            Action::Shell => {
                let suspend = term.suspend();
                ui::clear_screen();
                println!("{} opening a shell here — exit it to return\n", ui::color::dim(ui::glyph::spark()));
                let cwd = std::env::current_dir()?;
                launch::open_shell(&cwd)?;
                drop(suspend);
            }
            Action::Panels => {
                // Unlike Shell/external-editor, cockpit stays the terminal
                // owner here (its own raw mode + alt screen already active)
                // — panels.rs draws the PTYs into it directly rather than
                // handing the console away, so no term.suspend(). A Ctrl+B
                // detach hands back a live Session we hold onto.
                if let Err(e) = panels::run_split(&profiles, &mut session) {
                    ui::home_clear();
                    eprintln!("{}", ui::color::red(&format!("Split view error: {e}")));
                    picker::prompt_line("Press Enter...")?;
                }
            }
            Action::Restore => {
                if session.is_some() {
                    ui::home_clear();
                    println!("{}\n", ui::banner());
                    println!("{}", ui::color::dim("Panes are still running: resume them (Ctrl+B) and quit them first."));
                    picker::prompt_line("Press Enter...")?;
                } else if let Some(saved) = pick_layout(&profiles, cursor as i64)? {
                    restore_layout(&profiles, &saved, &mut session)?;
                }
            }
            Action::ResumeSession => {
                if let Some(s) = session.take() {
                    match panels::resume(s, &profiles) {
                        Ok(s) => session = s,
                        Err(e) => {
                            ui::home_clear();
                            eprintln!("{}", ui::color::red(&format!("Split view error: {e}")));
                            picker::prompt_line("Press Enter...")?;
                        }
                    }
                }
            }
            Action::Memory(name) => {
                let m = mark.map(|m| m as i64).unwrap_or(-1);
                let cwd = std::env::current_dir()?;
                let project_root = notes::project_root(&cwd);
                let project_name = project_root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let mut others: Vec<PathBuf> = notes::known_project_roots().into_iter().filter(|p| p != &project_root).collect();
                others.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_string().to_lowercase()).unwrap_or_default());

                // "global:<path>" rows are profile-scoped (CLAUDE.md, the
                // profile-wide system prompt) — the only ones that make
                // sense to copy to another profile. "project:<path>" rows
                // live in the project directory itself, so every profile
                // launched from there already shares the same file; there's
                // nothing to copy for those.
                let stats: Vec<layout::Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
                let mut header = layout::account_lines(&profiles, m, &stats);
                header.push(format!("{} {}  {}", ui::color::orange(ui::glyph::back()), ui::color::orange(&name), ui::color::dim("— memory")));
                header.push(String::new());
                let mut rows: Vec<picker::ScrollRow> = Vec::new();
                rows.push(picker::ScrollRow::line(format!("  {}", ui::color::orange_bold("Global (profile-wide)"))));
                rows.push(picker::ScrollRow::pick("    CLAUDE.md", format!("global:{}", paths::profile_dir(&name).join("CLAUDE.md").to_string_lossy())));
                rows.push(picker::ScrollRow::pick(
                    "    System prompt",
                    format!("global:{}", paths::profile_dir(&name).join(".cockpit-system-prompt.md").to_string_lossy()),
                ));
                rows.push(picker::ScrollRow::pick("    Memory (auto)", format!("memdir:{}", paths::profile_dir(&name).join("memory").to_string_lossy())));
                rows.push(picker::ScrollRow::line(String::new()));
                // Claude Code names a project's own dir under <profile>/projects
                // by replacing ':' and '\' in its absolute path with '-' — this
                // computes the same encoding so a project's memory folder is
                // reachable even before any session has ever run there (which
                // is the only time that directory would otherwise get created).
                let encode_project_dir = |root: &std::path::Path| -> String { root.to_string_lossy().replace([':', '\\', '/'], "-") };
                let project_group = |rows: &mut Vec<picker::ScrollRow>, label: &str, root: &std::path::Path| {
                    rows.push(picker::ScrollRow::line(format!("  {}", ui::color::orange_bold(label))));
                    rows.push(picker::ScrollRow::pick("    CLAUDE.md", format!("project:{}", root.join("CLAUDE.md").to_string_lossy())));
                    rows.push(picker::ScrollRow::pick("    System prompt", format!("project:{}", root.join(".cockpit-system-prompt.md").to_string_lossy())));
                    let mem_dir = paths::profile_dir(&name).join("projects").join(encode_project_dir(root)).join("memory");
                    rows.push(picker::ScrollRow::pick("    Memory (auto)", format!("memdir:{}", mem_dir.to_string_lossy())));
                    rows.push(picker::ScrollRow::line(String::new()));
                };
                project_group(&mut rows, &format!("{project_name} (this project)"), &project_root);
                for other in &others {
                    let oname = other.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    project_group(&mut rows, &oname, other);
                }

                // Looped so returning from the action menu/editor lands back
                // on this list, not the main menu, and `at` keeps the
                // highlighted row put across that round trip.
                let mut at: Option<String> = None;
                'files: loop {
                    let choice = picker::scroll_screen(&header, &rows, at.as_deref(), None)?;
                    let picker::ListChoice::Picked(pick) = choice else { break };
                    at = Some(pick.clone());
                    if let Some(dir) = pick.strip_prefix("memdir:") {
                        browse_memory_dir(&profiles, m, std::path::Path::new(dir))?;
                        continue 'files;
                    }
                    let (is_global, file) = match pick.strip_prefix("global:") {
                        Some(f) => (true, f.to_string()),
                        None => (false, pick.strip_prefix("project:").unwrap_or(&pick).to_string()),
                    };
                    if !std::path::Path::new(&file).exists() {
                        let _ = std::fs::write(&file, "");
                    }

                    let _ = is_global; // no longer gates anything — copy works from any row now
                    let action_options = [
                        picker::ListOption { label: "Edit here".into(), value: "edit".into(), note: None },
                        picker::ListOption { label: "Edit in $EDITOR".into(), value: "external".into(), note: None },
                        picker::ListOption { label: "Copy…".into(), value: "copy".into(), note: Some("into another profile's global slot, or a project's".into()) },
                    ];
                    let path = std::path::PathBuf::from(&file);
                    let title = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    // Looped so returning from the editor (or a copy) lands
                    // back on this action menu — where you actually left
                    // off — rather than the file list a level up; only an
                    // explicit Back on the menu itself goes back to that.
                    loop {
                        let action = match picker::choose_from_list(&profiles, m, &format!("{title}…"), None, &action_options, None)? {
                            picker::ListChoice::Picked(a) => a,
                            _ => continue 'files,
                        };
                        match action.as_str() {
                            "edit" => {
                                editor::edit_file(&path, &profiles, m, Some(&title))?;
                            }
                            "external" => {
                                let suspend = term.suspend();
                                launch::open_in_editor(&path)?;
                                drop(suspend);
                            }
                            "copy" => {
                                // Fully expanded, no drill-down: every
                                // profile group (including this one — its
                                // Project row is a real, useful copy:
                                // promoting a profile-wide file into the
                                // current project's slot; only its own
                                // Global row can be a true self-copy, and
                                // that's already caught below) shows its
                                // Global and Project rows directly pickable
                                // right here. "Project" resolves to the same
                                // file no matter which group it's picked
                                // from — it isn't profile-owned.
                                let copy_stats: Vec<layout::Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
                                let mut copy_header = layout::account_lines(&profiles, m, &copy_stats);
                                copy_header.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold(&format!("Copy {title} into…"))));
                                copy_header.push(String::new());
                                let mut copy_rows: Vec<picker::ScrollRow> = Vec::new();
                                for p in profiles.iter() {
                                    let label = if p.name == name { format!("{} (this profile)", p.name) } else { p.name.clone() };
                                    copy_rows.push(picker::ScrollRow::line(format!("  {}", ui::color::orange_bold(&label))));
                                    copy_rows.push(picker::ScrollRow::pick("    Global", format!("global:{}", p.name)));
                                    copy_rows.push(picker::ScrollRow::pick("    Project", "project".to_string()));
                                    copy_rows.push(picker::ScrollRow::line(String::new()));
                                }
                                let copy_choice = picker::scroll_screen(&copy_header, &copy_rows, None, None)?;
                                let picker::ListChoice::Picked(copy_pick) = copy_choice else { continue };
                                let (dest, dest_desc) = if let Some(target_profile) = copy_pick.strip_prefix("global:") {
                                    (paths::profile_dir(target_profile).join(path.file_name().unwrap_or_default()), format!("profile '{target_profile}'"))
                                } else {
                                    (project_root.join(path.file_name().unwrap_or_default()), format!("project '{project_name}'"))
                                };
                                if dest == path {
                                    ui::home_clear();
                                    println!("{}", ui::color::dim("That's the same file — nothing to copy."));
                                    picker::prompt_line("Press Enter...")?;
                                    continue;
                                }
                                ui::home_clear();
                                match std::fs::copy(&path, &dest) {
                                    Ok(_) => println!("{}", ui::color::green(&format!("Copied {title} to {dest_desc}."))),
                                    Err(e) => eprintln!("{}", ui::color::red(&format!("Error: {e}"))),
                                }
                                picker::prompt_line("Press Enter...")?;
                            }
                            _ => {}
                        }
                    }
                }
            }
            Action::Add => {
                ui::home_clear();
                println!("{}\n", ui::banner());
                let Some(name) = picker::prompt_line("New account name: ")? else { continue };
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                match profiles::create_account(name) {
                    Ok(p) => {
                        let suspend = term.suspend();
                        ui::clear_screen();
                        println!("{}", ui::color::green(&format!("Created '{}'. Launching Claude — run /login inside it.", p.name)));
                        launch::launch(&p.name, &[])?;
                        drop(suspend);
                    }
                    Err(e) => {
                        eprintln!("{}", ui::color::red(&format!("Error: {e}")));
                        picker::prompt_line("Press Enter...")?;
                    }
                }
            }
            Action::Delete(name) => {
                ui::home_clear();
                println!("{}\n", ui::banner());
                println!("{}", ui::color::red(&format!("Everything under the '{name}' profile will be removed.")));
                let Some(confirm) = picker::prompt_line("Type the profile name to confirm: ")? else { continue };
                if confirm.trim() == name {
                    match profiles::remove_profile(&name) {
                        Ok(_) => continue,
                        Err(e) => {
                            eprintln!("{}", ui::color::red(&format!("Error: {e}")));
                            picker::prompt_line("Press Enter...")?;
                        }
                    }
                } else {
                    println!("{}", ui::color::dim("Cancelled."));
                    picker::prompt_line("Press Enter...")?;
                }
            }
            Action::Open(name) => {
                dispatch(&mut term, &profiles, mark.map(|m| m as i64).unwrap_or(-1), OpenRequest::fresh(&name), &mut session)?;
            }
            Action::Detail(name) => {
                let (snap, groups) = detail_cache
                    .lock()
                    .ok()
                    .and_then(|mut c| c.remove(&name))
                    .unwrap_or_else(|| (inspect::build_inspection(&paths::profile_dir(&name)), session::grouped_sessions(&name)));
                let m = mark.map(|m| m as i64).unwrap_or(-1);
                let mut at: Option<String> = None;
                loop {
                    let (header, rows) = detail::profile_overview(&snap, &name, &profiles, m, &groups);
                    let res = picker::scroll_screen(&header, &rows, at.as_deref(), None)?;
                    let picker::ListChoice::Picked(pick) = res else { break };
                    at = Some(pick.clone());

                    if pick.starts_with("folder:") || pick.starts_with("recent:") || pick == "sessions-more" {
                        if let Some(request) = sessions::run(&profiles, m, sessions::Scope::Account(name.clone()))? {
                            dispatch(&mut term, &profiles, m, request, &mut session)?;
                            break;
                        }
                        continue;
                    }
                    if pick == "plugin-more" || pick == "skill-more" || pick == "mcp-more" {
                        let (heading, opts) = match pick.as_str() {
                            "plugin-more" => ("All plugins…", detail::plugin_options(&snap)),
                            "skill-more" => ("All skills…", detail::skill_options(&snap)),
                            _ => ("All MCP servers…", detail::mcp_options(&snap)),
                        };
                        let options: Vec<picker::ListOption> =
                            opts.into_iter().map(|(label, value, note)| picker::ListOption { label, value, note: Some(note) }).collect();
                        if let picker::ListChoice::Picked(v) = picker::choose_from_list(&profiles, m, heading, None, &options, None)? {
                            let (h2, r2) = detail::item_detail(&snap, &v, &profiles, m);
                            picker::scroll_screen(&h2, &r2, None, None)?;
                        }
                        continue;
                    }

                    let (h2, r2) = detail::item_detail(&snap, &pick, &profiles, m);
                    picker::scroll_screen(&h2, &r2, None, None)?;
                }
            }
            Action::Notes => {
                let cwd = std::env::current_dir()?;
                let root = notes::project_root(&cwd);
                notes::ensure_project_notes(&root);
                let (master_todo, master_plan) = notes::master_paths();
                let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let m = mark.map(|m| m as i64).unwrap_or(-1);

                // One screen, every project — grouped under a heading each
                // rather than a flat "<project> — TODO.md" list, or a
                // project-then-file drill-down. Hidden ones (see
                // notes::hide) are filtered out here only — their files and
                // master section are untouched, so they resurface on their
                // own once a session opens there again.
                let stats: Vec<layout::Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();

                // Outer loop rebuilds the project list from scratch — needed
                // after a hide, since that removes a group from the screen
                // entirely. `continue 'screen` (fresh `at` of None) is what
                // gives "hide" the requested "back to top" landing; a plain
                // `continue` on the inner loop is what gives "no"/back its
                // "stay exactly where you were" landing.
                'screen: loop {
                    let mut others: Vec<PathBuf> =
                        notes::known_project_roots().into_iter().filter(|p| p != &root && !notes::is_hidden(p)).collect();
                    others.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_string().to_lowercase()).unwrap_or_default());
                    let this_hidden = notes::is_hidden(&root);

                    let mut header = layout::account_lines(&profiles, m, &stats);
                    header.push(format!("{} {}", ui::color::orange(ui::glyph::back()), ui::color::bold("Notes")));
                    header.push(ui::color::dim("   Enter edits it here"));
                    header.push(String::new());

                    let mut rows: Vec<picker::ScrollRow> = Vec::new();
                    let group = |rows: &mut Vec<picker::ScrollRow>, label: &str, todo: &std::path::Path, plan: &std::path::Path| {
                        rows.push(picker::ScrollRow::line(format!("  {}", ui::color::orange_bold(label))));
                        rows.push(picker::ScrollRow::pick("    TODO.md", todo.to_string_lossy().to_string()));
                        rows.push(picker::ScrollRow::pick("    PLAN.md", plan.to_string_lossy().to_string()));
                        rows.push(picker::ScrollRow::line(String::new()));
                    };
                    if !this_hidden {
                        group(&mut rows, &format!("{name} (this project)"), &root.join("TODO.md"), &root.join("PLAN.md"));
                    }
                    group(&mut rows, "Master (every project)", &master_todo, &master_plan);
                    for other in &others {
                        let oname = other.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        group(&mut rows, &oname, &other.join("TODO.md"), &other.join("PLAN.md"));
                    }

                    // Looped so declining/backing out of a confirm (Esc)
                    // lands back on this list at the same row, not the main
                    // menu — only an actual Back on the list itself, or
                    // confirming turn-off/hide, leaves this screen.
                    let mut at: Option<String> = None;
                    loop {
                        let choice = picker::scroll_screen_ext(&header, &rows, at.as_deref(), Some('e'), Some(('c', "turn off notes")), Some(('h', "hide project")))?;
                        let file = match choice {
                            picker::ListChoice::Back => break 'screen,
                            picker::ListChoice::Command('c') => {
                                let confirm_options = [
                                    picker::ListOption { label: "Yes, turn off Notes".into(), value: "yes".into(), note: None },
                                    picker::ListOption { label: "No, keep Notes".into(), value: "no".into(), note: None },
                                ];
                                if let picker::ListChoice::Picked(a) = picker::choose_from_list(
                                    &profiles,
                                    m,
                                    "Turn off Notes?",
                                    Some("Removes it from the main menu until re-enabled"),
                                    &confirm_options,
                                    None,
                                )? {
                                    if a == "yes" {
                                        settings::set_notes_enabled(false);
                                        break 'screen;
                                    }
                                }
                                continue;
                            }
                            picker::ListChoice::Command(_) => continue,
                            picker::ListChoice::External(file) => {
                                at = Some(file.clone());
                                let target_root = if file == master_todo.to_string_lossy() || file == master_plan.to_string_lossy() {
                                    root.clone()
                                } else {
                                    std::path::Path::new(&file).parent().map(PathBuf::from).unwrap_or_else(|| root.clone())
                                };
                                notes::ensure_project_notes(&target_root);
                                let suspend = term.suspend();
                                launch::open_in_editor(std::path::Path::new(&file))?;
                                drop(suspend);
                                let session = notes::Session::start(&target_root);
                                session.stop();
                                continue;
                            }
                            picker::ListChoice::RowAction('h', file) => {
                                // TODO.md/PLAN.md → its project root; Master's
                                // own rows aren't a project, so 'h' on those
                                // is a no-op rather than hiding `root`.
                                if file == master_todo.to_string_lossy() || file == master_plan.to_string_lossy() {
                                    at = Some(file);
                                    continue;
                                }
                                let hide_root = std::path::Path::new(&file).parent().map(PathBuf::from).unwrap_or_else(|| root.clone());
                                let hide_name = hide_root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                                at = Some(file);
                                let confirm_options = [
                                    picker::ListOption { label: "Yes, hide it".into(), value: "yes".into(), note: None },
                                    picker::ListOption { label: "No, keep it".into(), value: "no".into(), note: None },
                                ];
                                if let picker::ListChoice::Picked(a) = picker::choose_from_list(
                                    &profiles,
                                    m,
                                    &format!("Hide '{hide_name}' from Notes?"),
                                    Some("Its TODO.md/PLAN.md stay put — reappears once a session opens there again"),
                                    &confirm_options,
                                    None,
                                )? {
                                    if a == "yes" {
                                        notes::hide(&hide_root);
                                        continue 'screen;
                                    }
                                }
                                continue;
                            }
                            picker::ListChoice::RowAction(_, _) => continue,
                            picker::ListChoice::Picked(file) => file,
                        };
                        at = Some(file.clone());
                        let target_root = if file == master_todo.to_string_lossy() || file == master_plan.to_string_lossy() {
                            root.clone()
                        } else {
                            std::path::Path::new(&file).parent().map(PathBuf::from).unwrap_or_else(|| root.clone())
                        };
                        notes::ensure_project_notes(&target_root);
                        let path = std::path::PathBuf::from(&file);
                        let title = path.file_name().map(|n| n.to_string_lossy().to_string());
                        editor::edit_file(&path, &profiles, m, title.as_deref())?;
                        let session = notes::Session::start(&target_root);
                        session.stop();
                    }
                }
            }
            Action::Import => {
                ui::home_clear();
                println!("{}\n", ui::banner());
                let Some(name) = picker::prompt_line("Profile name for the imported login [main]: ")? else { continue };
                let name = if name.trim().is_empty() { "main".to_string() } else { name.trim().to_string() };
                match profiles::import_profile(&name, None) {
                    Ok(p) => println!("{}", ui::color::green(&format!("Imported into profile '{}'.", p.name))),
                    Err(e) => eprintln!("{}", ui::color::red(&format!("Error: {e}"))),
                }
                picker::prompt_line("Press Enter...")?;
            }
            Action::Sync(from_name) => {
                let others: Vec<&profiles::Profile> = profiles.iter().filter(|p| p.name != from_name).collect();
                if others.is_empty() {
                    ui::home_clear();
                    println!("{}\n", ui::banner());
                    println!("{}", ui::color::dim("Need a second profile to sync into."));
                    picker::prompt_line("Press Enter...")?;
                    continue;
                }
                let m = mark.map(|m| m as i64).unwrap_or(-1);
                let target_options: Vec<picker::ListOption> = others
                    .iter()
                    .map(|p| picker::ListOption { label: p.name.clone(), value: p.name.clone(), note: Some(format!("{} sessions", layout::account_stat(&p.name).count)) })
                    .collect();
                let heading = format!("Sync plugins / skills / MCP from {} into…", ui::color::orange(&from_name));
                let target = match picker::choose_from_list(&profiles, m, &heading, Some("choose the target account"), &target_options, None)? {
                    picker::ListChoice::Picked(t) => t,
                    _ => continue,
                };

                let inv = sync::profile_inventory(&from_name);
                let cats = [
                    picker::MultiOption { label: "Plugins".into(), value: "plugins".into(), items: inv.plugins.clone(), note: format!("{} available", inv.plugins.len()), checked: !inv.plugins.is_empty() },
                    picker::MultiOption { label: "Skills".into(), value: "skills".into(), items: inv.skills.clone(), note: format!("{} available", inv.skills.len()), checked: !inv.skills.is_empty() },
                    picker::MultiOption { label: "MCP servers".into(), value: "mcp".into(), items: inv.mcp.clone(), note: format!("{} available", inv.mcp.len()), checked: !inv.mcp.is_empty() },
                ];
                let mut state: Option<picker::MultiState> = None;
                let final_state = loop {
                    let heading = format!("Sync  {}  {}  {}", ui::color::orange(&from_name), ui::glyph::pointer(), ui::color::orange(&target));
                    let res = picker::choose_multi(&profiles, m, &heading, Some("Space to tick a kind · → to pick individual items · then → Go"), &cats, state.take())?;
                    match res {
                        picker::MultiResult::Back => break None,
                        picker::MultiResult::Go(s) => break Some(s),
                        picker::MultiResult::Drill(i, s) => {
                            let cat = &cats[i];
                            let pre: Vec<String> = s.sub[i].clone().unwrap_or_else(|| cat.items.clone());
                            let sub_options: Vec<picker::MultiOption> = cat
                                .items
                                .iter()
                                .map(|name| picker::MultiOption { label: name.clone(), value: name.clone(), items: vec![], note: String::new(), checked: pre.contains(name) })
                                .collect();
                            let sub_heading = format!("{}  —  copy which into {}?", cat.label, ui::color::orange(&target));
                            let sub_res = picker::choose_multi(&profiles, m, &sub_heading, Some("Space to tick · → Go when done"), &sub_options, None)?;
                            let mut s = s;
                            if let picker::MultiResult::Go(sub_state) = sub_res {
                                let picks: Vec<String> = cat.items.iter().zip(sub_state.checked.iter()).filter(|(_, &c)| c).map(|(n, _)| n.clone()).collect();
                                s.checked[i] = !picks.is_empty();
                                s.sub[i] = if picks.len() == cat.items.len() { None } else { Some(picks) };
                            }
                            state = Some(s);
                        }
                    }
                };
                let Some(final_state) = final_state else { continue };
                let mut spec = sync::SyncSpec::default();
                for (i, cat) in cats.iter().enumerate() {
                    if final_state.checked[i] {
                        let sel = match &final_state.sub[i] {
                            Some(list) => sync::Selection::Some(list.clone()),
                            None => sync::Selection::All,
                        };
                        match cat.value.as_str() {
                            "plugins" => spec.plugins = Some(sel),
                            "skills" => spec.skills = Some(sel),
                            _ => spec.mcp = Some(sel),
                        }
                    }
                }
                ui::home_clear();
                if spec.plugins.is_none() && spec.skills.is_none() && spec.mcp.is_none() {
                    println!("{}", ui::color::dim("Nothing selected."));
                } else if let Err(e) = sync::sync_profiles(&from_name, &target, spec) {
                    eprintln!("{}", ui::color::red(&format!("Error: {e}")));
                }
                picker::prompt_line("Press Enter to return to the menu...")?;
            }
        }
    }
}

fn main() -> Result<()> {
    // Ctrl+C is delivered to the whole console group, including us. While
    // we're reading our own keys (raw mode), Ctrl+C just arrives as a normal
    // key event and our own screens quit on it deliberately; while Claude or
    // an external editor is running, we want it to own Ctrl+C instead of it
    // taking the cockpit down too. A single global no-op handler covers both
    // cases for the whole process lifetime — simpler than the JS version,
    // which had to install/remove a listener around every single launch.
    let _ = ctrlc::set_handler(|| {});

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(|s| s.as_str());

    let result = match command {
        None => cockpit(),
        Some("--help") | Some("-h") | Some("help") => {
            usage();
            Ok(())
        }
        Some("dashboard") | Some("accounts") => dashboard(),
        Some("notes") => {
            let dir = args.get(1).map(PathBuf::from).unwrap_or(std::env::current_dir()?);
            let root = notes::project_root(&dir);
            let session = notes::Session::start(&root);
            let tracked = session.tracked.clone();
            session.stop();
            let (master_todo, _) = notes::master_paths();
            println!("Synced {} <-> {}", ui::color::orange(&root.display().to_string()), ui::color::dim(&master_todo.display().to_string()));
            if !tracked.is_empty() {
                println!(
                    "{}",
                    ui::color::dim(&format!(
                        "Note: {} already tracked by git — run 'git rm --cached {}' if you want them untracked.",
                        tracked.join(", "),
                        tracked.join(" "),
                    ))
                );
            }
            Ok(())
        }
        Some("add") => {
            let name = args.get(1).map(|s| s.as_str()).unwrap_or_default();
            profiles::create_account(name).map(|p| {
                println!("Created profile '{}' at {}", p.name, paths::profile_dir(&p.name).display());
            })
        }
        Some("import") => {
            let name = args.get(1).cloned().unwrap_or_default();
            let source = args.get(2).map(PathBuf::from);
            profiles::import_profile(&name, source.as_deref()).map(|p| {
                println!("{}", ui::color::green(&format!("Imported into profile '{}'.", p.name)));
                println!("{}", ui::color::dim("Close any running Claude Code first if the copy looks incomplete, then re-run."));
                println!("Launch it with: {}", ui::color::orange(&format!("cpit {}", p.name)));
            })
        }
        Some("sync") => {
            let from = args.get(1).cloned().unwrap_or_default();
            let to = args.get(2).cloned().unwrap_or_default();
            let what = args.get(3).map(|s| s.as_str()).unwrap_or("all");
            let spec_result: Result<sync::SyncSpec> = match what {
                "all" => Ok(sync::SyncSpec { plugins: Some(sync::Selection::All), skills: Some(sync::Selection::All), mcp: Some(sync::Selection::All) }),
                "plugins" => Ok(sync::SyncSpec { plugins: Some(sync::Selection::All), ..Default::default() }),
                "skills" => Ok(sync::SyncSpec { skills: Some(sync::Selection::All), ..Default::default() }),
                "mcp" => Ok(sync::SyncSpec { mcp: Some(sync::Selection::All), ..Default::default() }),
                other => Err(anyhow::anyhow!("what must be one of: plugins, skills, mcp, all (got '{other}')")),
            };
            spec_result.and_then(|spec| sync::sync_profiles(&from, &to, spec))
        }
        Some("list") => profiles::list_profiles().map(|list| {
            for p in list {
                println!("{}\t{}", p.name, paths::profile_dir(&p.name).display());
            }
        }),
        Some("delete") | Some("rm") => {
            let name = args.get(1).map(|s| s.as_str()).unwrap_or_default();
            if args.get(2).map(|s| s.as_str()) != Some("--yes") && args.get(2).map(|s| s.as_str()) != Some("-y") {
                Err(anyhow::anyhow!("Pass --yes to confirm: claude-cockpit delete {name} --yes"))
            } else {
                profiles::remove_profile(name).map(|(name, dir)| {
                    println!("Deleted profile '{name}' and {}", dir.display());
                })
            }
        }
        Some("run") | Some("login") => {
            let name = args.get(1).cloned().unwrap_or_default();
            profiles::get_profile(&name)?;
            let pass_args: Vec<String> = if command == Some("login") { vec![] } else { args[2..].to_vec() };
            launch::launch(&name, &pass_args).map(|code| std::process::exit(code))
        }
        Some("sessions") => show_sessions(args.get(1).map(|s| s.as_str())),
        Some("install-statusline") => {
            let name = args.get(1).map(|s| s.as_str()).unwrap_or_default();
            profiles::install_statusline(name, false, false).map(|_| ())
        }
        Some("handoff") => {
            let session_id = args.get(1).cloned().unwrap_or_default();
            let from = args.get(2).cloned().unwrap_or_default();
            let to = args.get(3).cloned().unwrap_or_default();
            profiles::get_profile(&from)?;
            profiles::get_profile(&to)?;
            handoff::copy_session(&from, &to, &session_id).map(|(_, _, stripped)| {
                println!("Copied session {session_id} to '{to}'{}. Run: claude-cockpit run {to} --resume {session_id}", strip_note(stripped));
            })
        }
        // The plugin's SessionStart hook (see workspace.rs).
        Some("session-track") => {
            workspace::track_from_stdin();
            Ok(())
        }
        Some("statusline") => {
            statusline::main();
            Ok(())
        }
        // Drive the split view directly with a fixed set of profiles, skipping
        // the picker — for headless testing of the N-pane compositor.
        Some("split-test") => {
            let names: Vec<String> = args[1..].to_vec();
            if names.is_empty() {
                return Err(anyhow::anyhow!("split-test needs at least 1 profile name"));
            }
            let profiles = profiles::list_profiles().unwrap_or_default();
            let _term = ui::Terminal::enter()?;
            panels::run_split_profiles(&profiles, &names).map(|_| ())
        }
        Some(other) => Err(anyhow::anyhow!("Unknown command '{other}'.")),
    };

    if let Err(error) = result {
        eprintln!("{}", ui::color::red(&format!("Error: {error}")));
        std::process::exit(1);
    }
    Ok(())
}
