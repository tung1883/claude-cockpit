mod detail;
mod editor;
mod input;
mod inspect;
mod launch;
mod notes;
mod layout;
mod paths;
mod picker;
mod profiles;
mod quota;
mod session;
mod statusline;
mod sync;
mod handoff;
mod ui;

use anyhow::Result;
use picker::Action;
use std::io::IsTerminal;
use std::path::PathBuf;

fn usage() {
    println!(
        "{}\n\n{}\n  {}            Open the interactive cockpit (master command)\n  {}                     Short alias for the same thing\n\n{}\n  add <name>                    Create an isolated account profile\n  delete <name>                 Delete a profile and all its data\n  list                          List profiles\n",
        ui::banner(),
        ui::color::bold("Usage"),
        ui::color::orange("claude-cockpit"),
        ui::color::orange("ccpit"),
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

/// Session list formatted as `choose_from_list` options — id, session name
/// (if any), project, and age — newest first (`session_files` already sorts
/// that way).
fn session_list_options(profile: &str) -> Vec<picker::ListOption> {
    session::session_files(profile)
        .iter()
        .map(|s| {
            let d = session::session_details(s);
            let age = session::relative_age(Some(d.modified));
            let label = if !d.session_name.is_empty() { d.session_name.clone() } else { d.id.chars().take(8).collect::<String>() };
            let proj = if d.project_path.is_empty() { String::new() } else { format!("{} · ", d.project_path.replace('\\', "/")) };
            picker::ListOption { label, value: d.id.clone(), note: Some(format!("{proj}{age}")) }
        })
        .collect()
}

/// The shared tail of every handoff flow once a session is chosen: pick a
/// target profile, copy the transcript, offer to resume there immediately.
fn run_handoff(term: &mut ui::Terminal, profiles: &[profiles::Profile], mark: i64, from: &str, session_id: &str) -> Result<()> {
    let others: Vec<&profiles::Profile> = profiles.iter().filter(|p| p.name != from).collect();
    if others.is_empty() {
        ui::home_clear();
        println!("{}\n", ui::banner());
        println!("{}", ui::color::dim("Need a second profile to hand off into."));
        picker::prompt_line("Press Enter...")?;
        return Ok(());
    }
    let target_options: Vec<picker::ListOption> = others
        .iter()
        .map(|p| {
            let count = layout::account_stat(&p.name).count;
            let note = match quota::load(&p.name) {
                Some(q) => format!("{count} sessions · {:.0}% used", quota::usage(&q)),
                None => format!("{count} sessions"),
            };
            picker::ListOption { label: p.name.clone(), value: p.name.clone(), note: Some(note) }
        })
        .collect();
    let target = match picker::choose_from_list(profiles, mark, "Copy session into…", None, &target_options, None)? {
        picker::ListChoice::Picked(t) => t,
        _ => return Ok(()),
    };

    ui::home_clear();
    match handoff::copy_session(from, &target, session_id) {
        Ok(_) => {
            println!("{}", ui::color::green(&format!("Copied session {session_id} to '{target}'.")));
            let go = picker::prompt_line("Enter to resume it there now (anything else to skip): ")?;
            if matches!(go.as_deref(), Some("")) {
                let suspend = term.suspend();
                ui::clear_screen();
                println!(
                    "{} resuming as {} — exit Claude to return here\n",
                    ui::color::dim(ui::glyph::spark()),
                    ui::color::orange(&target),
                );
                launch::launch(&target, &["--resume".to_string(), session_id.to_string()])?;
                drop(suspend);
            }
        }
        Err(e) => {
            eprintln!("{}", ui::color::red(&format!("Error: {e}")));
            picker::prompt_line("Press Enter...")?;
        }
    }
    Ok(())
}

/// Read-only detail screen for one session — the "detail first" variant of
/// the overview's two session-row styles (see detail::session_rows).
/// Opens one project folder's session list; picking a session opens a small
/// action menu (resume here / hand off / view conversation) rather than
/// jumping straight into any one of them.
fn open_folder_sessions(term: &mut ui::Terminal, profiles: &[profiles::Profile], mark: i64, from: &str, group: &session::SessionGroup) -> Result<()> {
    let options: Vec<picker::ListOption> = group
        .sessions
        .iter()
        .map(|s| {
            let d = session::session_details(s);
            let label = if !d.session_name.is_empty() { d.session_name.clone() } else { d.id.chars().take(8).collect::<String>() };
            let age = session::relative_age(Some(d.modified));
            // A recap only exists once a session has compacted — rare for a
            // short one — so fall back to the last prompt, which is nearly
            // always there.
            let summary = if !d.recap.is_empty() { &d.recap } else { &d.last_prompt };
            let note = if summary.is_empty() {
                age
            } else {
                let flat: String = summary.split_whitespace().collect::<Vec<_>>().join(" ");
                // Age is padded so the "·" lands on the same column no
                // matter how many digits/characters the age runs to
                // ("7d ago" vs "10d ago" vs "just now").
                format!("{}·  {}", layout::pad_to(&age, 10), flat.chars().take(80).collect::<String>())
            };
            picker::ListOption { label, value: d.id.clone(), note: Some(note) }
        })
        .collect();
    let heading = format!("Sessions in {}…", ui::color::orange(&group.folder));
    let picker::ListChoice::Picked(id) = picker::choose_from_list(profiles, mark, &heading, Some("choose a session"), &options, None)? else { return Ok(()) };

    loop {
        let action_options = [
            picker::ListOption { label: "Resume here".into(), value: "resume".into(), note: Some(format!("continue it as '{from}'")) },
            picker::ListOption { label: "Hand off".into(), value: "handoff".into(), note: Some("copy it to another profile".into()) },
            picker::ListOption { label: "View conversation".into(), value: "view".into(), note: Some("read-only, no Claude launch".into()) },
        ];
        let choice = picker::choose_from_list(profiles, mark, "What do you want to do with it?", None, &action_options, None)?;
        let picker::ListChoice::Picked(action) = choice else { return Ok(()) };
        match action.as_str() {
            "resume" => {
                let suspend = term.suspend();
                ui::clear_screen();
                println!("{} resuming as {} — exit Claude to return here\n", ui::color::dim(ui::glyph::spark()), ui::color::orange(from));
                launch::launch(from, &["--resume".to_string(), id.clone()])?;
                drop(suspend);
                return Ok(());
            }
            "handoff" => {
                run_handoff(term, profiles, mark, from, &id)?;
                return Ok(());
            }
            "view" => {
                view_conversation(profiles, mark, group, &id)?;
                // Back to the action menu, not the whole folder — you
                // likely want to act on what you just read.
            }
            _ => return Ok(()),
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

/// Full folder list (beyond the overview's first 5), sorted by each
/// folder's most recent session — picking one drills into its sessions.
fn open_all_folders(term: &mut ui::Terminal, profiles: &[profiles::Profile], mark: i64, from: &str, groups: &[session::SessionGroup]) -> Result<()> {
    let options: Vec<picker::ListOption> = groups
        .iter()
        .map(|g| {
            let n = g.sessions.len();
            let latest_age = g.sessions.first().map(|s| session::relative_age(Some(s.modified))).unwrap_or_default();
            picker::ListOption { label: g.folder.clone(), value: g.folder.clone(), note: Some(format!("{n} session{}  ·  {latest_age}", if n == 1 { "" } else { "s" })) }
        })
        .collect();
    if let picker::ListChoice::Picked(folder) = picker::choose_from_list(profiles, mark, "All project folders…", None, &options, None)? {
        if let Some(group) = groups.iter().find(|g| g.folder == folder) {
            open_folder_sessions(term, profiles, mark, from, group)?;
        }
    }
    Ok(())
}

fn cockpit() -> Result<()> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return dashboard();
    }
    let mut term = ui::Terminal::enter()?;
    let mut cursor: usize = 0;

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
        let action = picker::pick_profile(&profiles, cursor)?;
        let mark = match &action {
            Action::Open(n) | Action::Detail(n) | Action::Sync(n) | Action::Handoff(n) | Action::Delete(n) => {
                profiles.iter().position(|p| &p.name == n)
            }
            _ => None,
        };
        if let Some(m) = mark {
            cursor = m;
        }

        match action {
            Action::Quit => return Ok(()),
            Action::Refresh => continue,
            Action::Shell => {
                let suspend = term.suspend();
                ui::clear_screen();
                println!("{} opening a shell here — exit it to return\n", ui::color::dim(ui::glyph::spark()));
                let cwd = std::env::current_dir()?;
                launch::open_shell(&cwd)?;
                drop(suspend);
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
                let suspend = term.suspend(); // give Claude the real terminal
                // Hard wipe, not the soft home_clear: the primary buffer keeps
                // scrolling across launches, so a soft clear would stack every
                // past "launching Claude as..." line right above this one.
                ui::clear_screen();
                println!(
                    "{} launching Claude as {} — exit Claude to return here\n",
                    ui::color::dim(ui::glyph::spark()),
                    ui::color::orange(&name),
                );
                launch::launch(&name, &[])?;
                drop(suspend); // restore alt screen + raw mode before the next repaint

                // Auto-handoff: the statusline hook cached the last quota
                // reading it saw while Claude was running (main.rs can't see
                // Claude's own output — stdio was inherited straight
                // through). If that reading says this profile just ran dry,
                // offer to copy the session that was just running onto
                // whichever other profile has the most headroom.
                if quota::load(&name).is_some_and(|q| quota::is_exhausted(&q)) {
                    let candidate = profiles
                        .iter()
                        .filter(|p| p.name != name)
                        .filter(|p| !quota::load(&p.name).is_some_and(|q| quota::is_exhausted(&q)))
                        .min_by(|a, b| {
                            let ua = quota::load(&a.name).map(|q| quota::usage(&q)).unwrap_or(0.0);
                            let ub = quota::load(&b.name).map(|q| quota::usage(&q)).unwrap_or(0.0);
                            ua.partial_cmp(&ub).unwrap()
                        })
                        .map(|p| p.name.clone());
                    let latest = session::session_files(&name).into_iter().next();
                    if let (Some(candidate), Some(latest)) = (candidate, latest) {
                        ui::home_clear();
                        println!("{}\n", ui::banner());
                        println!("{}", ui::color::yellow(&format!("'{name}' just hit its usage limit.")));
                        match handoff::copy_session(&name, &candidate, &latest.id) {
                            Ok(_) => {
                                println!("{}", ui::color::dim(&format!("Latest session copied to '{candidate}'.")));
                                let go = picker::prompt_line("Enter to resume it there now (anything else to skip): ")?;
                                if matches!(go.as_deref(), Some("")) {
                                    let suspend = term.suspend();
                                    ui::clear_screen();
                                    println!(
                                        "{} resuming as {} — exit Claude to return here\n",
                                        ui::color::dim(ui::glyph::spark()),
                                        ui::color::orange(&candidate),
                                    );
                                    launch::launch(&candidate, &["--resume".to_string(), latest.id.clone()])?;
                                    drop(suspend);
                                }
                            }
                            Err(e) => {
                                eprintln!("{}", ui::color::red(&format!("Handoff failed: {e}")));
                                picker::prompt_line("Press Enter...")?;
                            }
                        }
                    }
                }
            }
            Action::Detail(name) => {
                let snap = inspect::build_inspection(&paths::profile_dir(&name));
                let groups = session::grouped_sessions(&name);
                let m = mark.map(|m| m as i64).unwrap_or(-1);
                let mut at: Option<String> = None;
                loop {
                    let (header, rows) = detail::profile_overview(&snap, &name, &profiles, m, &groups);
                    let res = picker::scroll_screen(&header, &rows, at.as_deref(), None)?;
                    let picker::ListChoice::Picked(pick) = res else { break };
                    at = Some(pick.clone());

                    if let Some(idx_str) = pick.strip_prefix("folder:") {
                        if let Some(group) = idx_str.parse::<usize>().ok().and_then(|i| groups.get(i)) {
                            open_folder_sessions(&mut term, &profiles, m, &name, group)?;
                        }
                        continue;
                    }
                    if pick == "sessions-more" {
                        open_all_folders(&mut term, &profiles, m, &name, &groups)?;
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
                // project-then-file drill-down.
                let mut others: Vec<PathBuf> = notes::known_project_roots().into_iter().filter(|p| p != &root).collect();
                others.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_string().to_lowercase()).unwrap_or_default());

                let stats: Vec<layout::Stat> = profiles.iter().map(|p| layout::account_stat(&p.name)).collect();
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
                group(&mut rows, &format!("{name} (this project)"), &root.join("TODO.md"), &root.join("PLAN.md"));
                group(&mut rows, "Master (every project)", &master_todo, &master_plan);
                for other in &others {
                    let oname = other.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    group(&mut rows, &oname, &other.join("TODO.md"), &other.join("PLAN.md"));
                }

                let choice = picker::scroll_screen(&header, &rows, None, Some('e'))?;
                let file = match choice {
                    picker::ListChoice::Back => continue,
                    picker::ListChoice::External(file) => {
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
                    picker::ListChoice::Picked(file) => file,
                };
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
            Action::Handoff(name) => {
                let options = session_list_options(&name);
                if options.is_empty() {
                    ui::home_clear();
                    println!("{}\n", ui::banner());
                    println!("{}", ui::color::dim(&format!("No sessions to hand off from '{name}'.")));
                    picker::prompt_line("Press Enter...")?;
                    continue;
                }
                let m = mark.map(|m| m as i64).unwrap_or(-1);
                let heading = format!("Hand off a session from {}…", ui::color::orange(&name));
                let session_id = match picker::choose_from_list(&profiles, m, &heading, Some("choose the session to copy"), &options, None)? {
                    picker::ListChoice::Picked(id) => id,
                    _ => continue,
                };
                run_handoff(&mut term, &profiles, m, &name, &session_id)?;
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
                println!("Launch it with: {}", ui::color::orange(&format!("ccpit {}", p.name)));
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
            handoff::copy_session(&from, &to, &session_id).map(|_| {
                println!("Copied session {session_id} to '{to}'. Run: claude-cockpit run {to} --resume {session_id}");
            })
        }
        Some("statusline") => {
            statusline::main();
            Ok(())
        }
        Some(other) => Err(anyhow::anyhow!("Unknown command '{other}'.")),
    };

    if let Err(error) = result {
        eprintln!("{}", ui::color::red(&format!("Error: {error}")));
        std::process::exit(1);
    }
    Ok(())
}
