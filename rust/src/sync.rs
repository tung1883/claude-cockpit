// Copy plugins / skills / MCP servers between profiles. Ported from
// syncProfiles/profileInventory in src/cli.js.
use crate::{paths, profiles, ui};
use anyhow::{bail, Result};
use serde_json::{Map, Value};
use std::path::Path;

fn read_json(file: &Path) -> Result<Value> {
    if !file.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let raw = std::fs::read_to_string(file)?;
    if raw.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("Refusing to sync: {} is not valid JSON: {e}", file.display()))
}

fn write_json(file: &Path, value: &Value) -> Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, format!("{}\n", serde_json::to_string_pretty(value)?))?;
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> bool {
    if !src.exists() {
        return false;
    }
    let _ = std::fs::create_dir_all(dst);
    copy_dir_recursive(src, dst).is_ok()
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            // Windows blocks symlink creation without elevation; a plain
            // metadata-following copy sidesteps that entirely.
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub struct Inventory {
    pub plugins: Vec<String>,
    pub skills: Vec<String>,
    pub mcp: Vec<String>,
}

pub fn profile_inventory(name: &str) -> Inventory {
    let dir = paths::profile_dir(name);
    let settings = read_json(&dir.join("settings.json")).unwrap_or(Value::Null);
    let claude_json = read_json(&dir.join(".claude.json")).unwrap_or(Value::Null);
    let mut plugins: Vec<String> = settings
        .get("enabledPlugins")
        .and_then(|v| v.as_object())
        .map(|o| o.iter().filter(|(_, v)| v.as_bool().unwrap_or(true)).map(|(k, _)| k.clone()).collect())
        .unwrap_or_default();
    plugins.sort();
    let mut skills: Vec<String> = std::fs::read_dir(dir.join("skills"))
        .map(|e| e.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    skills.sort();
    let mut mcp: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(o) = claude_json.get("mcpServers").and_then(|v| v.as_object()) {
        mcp.extend(o.keys().cloned());
    }
    if let Some(o) = settings.get("mcpServers").and_then(|v| v.as_object()) {
        mcp.extend(o.keys().cloned());
    }
    Inventory { plugins, skills, mcp: mcp.into_iter().collect() }
}

/// What to copy for one kind: every available item, or a specific subset.
pub enum Selection {
    All,
    Some(Vec<String>),
}

fn pick<'a>(sel: &Selection, available: &'a [String]) -> Vec<&'a String> {
    match sel {
        Selection::All => available.iter().collect(),
        Selection::Some(names) => available.iter().filter(|a| names.contains(a)).collect(),
    }
}

#[derive(Default)]
pub struct SyncSpec {
    pub plugins: Option<Selection>,
    pub skills: Option<Selection>,
    pub mcp: Option<Selection>,
}

pub fn sync_profiles(from_name: &str, to_name: &str, spec: SyncSpec) -> Result<()> {
    let from = profiles::get_profile(from_name)?;
    let to = profiles::get_profile(to_name)?;
    if from.name == to.name {
        bail!("Source and target are the same profile.");
    }
    if spec.plugins.is_none() && spec.skills.is_none() && spec.mcp.is_none() {
        bail!("Nothing selected to sync.");
    }

    let inv = profile_inventory(&from.name);
    let src = paths::profile_dir(&from.name);
    let dst = paths::profile_dir(&to.name);
    let src_settings = src.join("settings.json");
    let dst_settings = dst.join("settings.json");

    if let Some(sel) = &spec.plugins {
        let ids = pick(sel, &inv.plugins);
        copy_dir(&src.join("plugins"), &dst.join("plugins"));
        let s = read_json(&src_settings)?;
        let mut d = read_json(&dst_settings)?;
        let d_obj = d.as_object_mut().unwrap();
        let mut enabled = d_obj.get("enabledPlugins").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let s_enabled = s.get("enabledPlugins").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        for id in &ids {
            let v = s_enabled.get(*id).cloned().unwrap_or(Value::Bool(true));
            enabled.insert((*id).clone(), v);
        }
        d_obj.insert("enabledPlugins".into(), Value::Object(enabled));
        let mut marketplaces = d_obj.get("extraKnownMarketplaces").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        if let Some(sm) = s.get("extraKnownMarketplaces").and_then(|v| v.as_object()) {
            marketplaces.extend(sm.clone());
        }
        d_obj.insert("extraKnownMarketplaces".into(), Value::Object(marketplaces));
        write_json(&dst_settings, &d)?;
        println!("{}", ui::color::green(&format!("Synced {} plugin{}", ids.len(), if ids.len() == 1 { "" } else { "s" })));
    }

    if let Some(sel) = &spec.skills {
        let names = pick(sel, &inv.skills);
        let mut n = 0;
        for name in &names {
            if copy_dir(&src.join("skills").join(name), &dst.join("skills").join(name)) {
                n += 1;
            }
        }
        println!("{}", ui::color::green(&format!("Synced {n} skill{}", if n == 1 { "" } else { "s" })));
    }

    if let Some(sel) = &spec.mcp {
        let names = pick(sel, &inv.mcp);
        let sc = read_json(&src.join(".claude.json"))?;
        let mut dc = read_json(&dst.join(".claude.json"))?;
        let s = read_json(&src_settings)?;
        let mut d = read_json(&dst_settings)?;
        let dc_obj = dc.as_object_mut().unwrap();
        let mut dc_mcp = dc_obj.get("mcpServers").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let sc_mcp = sc.get("mcpServers").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let d_obj = d.as_object_mut().unwrap();
        let mut d_mcp = d_obj.get("mcpServers").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let s_mcp = s.get("mcpServers").and_then(|v| v.as_object()).cloned().unwrap_or_default();
        for name in &names {
            if let Some(v) = sc_mcp.get(*name) {
                dc_mcp.insert((*name).clone(), v.clone());
            }
            if let Some(v) = s_mcp.get(*name) {
                d_mcp.insert((*name).clone(), v.clone());
            }
        }
        dc_obj.insert("mcpServers".into(), Value::Object(dc_mcp));
        d_obj.insert("mcpServers".into(), Value::Object(d_mcp));
        write_json(&dst.join(".claude.json"), &dc)?;
        write_json(&dst_settings, &d)?;
        println!("{}", ui::color::green(&format!("Synced {} MCP server{}", names.len(), if names.len() == 1 { "" } else { "s" })));
    }

    println!("{}", ui::color::dim("Restart Claude Code in the target profile to pick these up."));
    Ok(())
}
