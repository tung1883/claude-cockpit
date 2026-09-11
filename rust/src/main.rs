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
            Action::Open(n) | Action::Detail(n) | Action::Sync(n) | Action::Delete(n) => {
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
            }
            Action::Detail(name) => {
                let snap = inspect::build_inspection(&paths::profile_dir(&name));
                let m = mark.map(|m| m as i64).unwrap_or(-1);
                let mut at: Option<String> = None;
                loop {
                    let (header, rows) = detail::profile_overview(&snap, &name, &profiles, m);
                    let res = picker::scroll_screen(&header, &rows, at.as_deref())?;
                    let Some(pick) = res else { break };
                    at = Some(pick.clone());
                    let (h2, r2) = detail::item_detail(&snap, &pick, &profiles, m);
                    picker::scroll_screen(&h2, &r2, None)?;
                }
            }
            Action::Notes => {
                let cwd = std::env::current_dir()?;
                let root = notes::project_root(&cwd);
                notes::ensure_project_notes(&root);
                let (master_todo, master_plan) = notes::master_paths();
                let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let options = [
                    picker::ListOption { label: "TODO.md".into(), value: root.join("TODO.md").to_string_lossy().to_string(), note: Some("this project".into()) },
                    picker::ListOption { label: "PLAN.md".into(), value: root.join("PLAN.md").to_string_lossy().to_string(), note: Some("this project".into()) },
                    picker::ListOption { label: "Master TODO.md".into(), value: master_todo.to_string_lossy().to_string(), note: Some("every project".into()) },
                    picker::ListOption { label: "Master PLAN.md".into(), value: master_plan.to_string_lossy().to_string(), note: Some("every project".into()) },
                ];
                let heading = format!("Notes for {}", ui::color::orange(&name));
                let choice = picker::choose_from_list(
                    &profiles,
                    mark.map(|m| m as i64).unwrap_or(-1),
                    &heading,
                    Some("Enter edits it here — e for your $EDITOR instead"),
                    &options,
                    Some('e'),
                )?;
                match choice {
                    picker::ListChoice::Back => {}
                    picker::ListChoice::External(file) => {
                        let suspend = term.suspend();
                        launch::open_in_editor(std::path::Path::new(&file))?;
                        drop(suspend);
                        let session = notes::Session::start(&root);
                        session.stop();
                    }
                    picker::ListChoice::Picked(file) => {
                        let path = std::path::PathBuf::from(&file);
                        let title = path.file_name().map(|n| n.to_string_lossy().to_string());
                        editor::edit_file(&path, &profiles, mark.map(|m| m as i64).unwrap_or(-1), title.as_deref())?;
                        let session = notes::Session::start(&root);
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
