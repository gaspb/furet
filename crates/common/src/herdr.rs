//! `<tool> setup herdr`: give a tool its own name and icon in the herdr app.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Program<'a> {
    /// Key in programs.json, and the icon's file name.
    pub id: &'a str,
    pub name: &'a str,
    pub icon_svg: &'a str,
    pub tile: &'a str,
    pub matches: &'a [&'a str],
}

fn app_dir() -> PathBuf {
    std::env::var_os("HERDR_APP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::scope::config_home().join("herdr-app"))
}

pub fn setup(p: &Program) -> Result<()> {
    let dir = app_dir();
    if !dir.is_dir() {
        bail!(
            "herdr app config not found at {} (is the herdr app installed?)",
            dir.display()
        );
    }

    let icon_rel = format!("my-icons/{}.svg", p.id);
    let icon = dir.join(&icon_rel);
    let icon_changed = fs::read_to_string(&icon).ok().as_deref() != Some(p.icon_svg);
    if icon_changed {
        fs::create_dir_all(icon.parent().unwrap())?;
        write_atomic(&icon, p.icon_svg)?;
        println!("wrote {}", icon.display());
    }

    let programs = dir.join("programs.json");
    let mut config: Value = match fs::read_to_string(&programs) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not valid JSON; fix it and run this again",
                programs.display()
            )
        })?,
        _ => json!({}),
    };
    let Some(root) = config.as_object_mut() else {
        bail!("{} should contain a JSON object", programs.display());
    };
    let Some(table) = root
        .entry("programs")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
        bail!("\"programs\" in {} should be an object", programs.display());
    };
    if table.contains_key(p.id) {
        println!(
            "\"{}\" is already in {}; left as is",
            p.id,
            programs.display()
        );
    } else {
        table.insert(
            p.id.into(),
            json!({
                "name": p.name,
                "icon": icon_rel,
                "tint": true,
                "tile": p.tile,
                "color": "#ffffff",
                "match": p.matches,
            }),
        );
        write_atomic(&programs, &(serde_json::to_string_pretty(&config)? + "\n"))?;
        println!(
            "added \"{}\" to {} (herdr applies it live)",
            p.id,
            programs.display()
        );
    }
    Ok(())
}

pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp~");
    fs::write(&tmp, content).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, path)?;
    Ok(())
}
