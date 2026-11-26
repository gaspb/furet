//! `<tool> setup herdr`: give a tool its own name and icon in the herdr app, and install the
//! mods (panels, palette entries) it ships.

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

/// A mod's files, as the binary carries them: path in the mod's folder → content.
pub struct Mod<'a> {
    pub name: &'a str,
    /// Who installs it, written in the marker so updates know the folder is theirs.
    pub owner: &'a str,
    pub files: &'a [(&'a str, &'a str)],
}

const MARKER: &str = ".installed-by";
/// Left by herdr-mods' install.sh: those copies are replaced too.
const HERDR_MODS_MARKER: &str = ".herdr-mods";

#[derive(Debug, PartialEq, Eq)]
pub enum Installed {
    Written,
    Unchanged,
    /// A symlink (someone working on it) or a folder someone else made: left alone.
    LeftAlone(String),
    NoHerdr,
}

impl Mod<'_> {
    /// Identifies the files, so a folder is only rewritten when they changed.
    pub fn stamp(&self) -> String {
        let mut h: u64 = 0xcbf29ce484222325;
        for (path, text) in self.files {
            for b in path.bytes().chain([0]).chain(text.bytes()).chain([0]) {
                h = (h ^ b as u64).wrapping_mul(0x100000001b3);
            }
        }
        format!("{} {h:016x}", self.owner)
    }

    fn dir(&self) -> PathBuf {
        app_dir().join("mods").join(self.name)
    }

    /// Installs or refreshes the mod; herdr reloads it live.
    pub fn install(&self) -> Result<Installed> {
        if !app_dir().is_dir() {
            return Ok(Installed::NoHerdr);
        }
        let dir = self.dir();
        if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
            let to = fs::read_link(&dir).unwrap_or_default();
            if !dir.exists() {
                fs::remove_file(&dir)?;
                return self.install();
            }
            return Ok(Installed::LeftAlone(format!(
                "it links to {}",
                to.display()
            )));
        }
        if dir.exists() {
            let marker = fs::read_to_string(dir.join(MARKER)).ok();
            if marker.as_deref() == Some(self.stamp().as_str()) {
                return Ok(Installed::Unchanged);
            }
            let ours = marker.is_some_and(|m| m.split(' ').next() == Some(self.owner));
            if !ours && !dir.join(HERDR_MODS_MARKER).exists() {
                return Ok(Installed::LeftAlone(format!(
                    "{} wasn't installed by {}",
                    dir.display(),
                    self.owner
                )));
            }
        }
        let tmp = dir.with_extension("new~");
        let _ = fs::remove_dir_all(&tmp);
        for (path, text) in self.files {
            let f = tmp.join(path);
            fs::create_dir_all(f.parent().unwrap())?;
            fs::write(&f, text).with_context(|| format!("cannot write {}", f.display()))?;
        }
        fs::write(tmp.join(MARKER), self.stamp())?;
        let old = dir.with_extension("old~");
        let _ = fs::remove_dir_all(&old);
        if dir.exists() {
            fs::rename(&dir, &old)?;
        }
        fs::rename(&tmp, &dir)?;
        let _ = fs::remove_dir_all(&old);
        Ok(Installed::Written)
    }

    /// Refreshes the mod only where this tool installed it before (after an update).
    pub fn refresh(&self) -> Result<Installed> {
        let marker = fs::read_to_string(self.dir().join(MARKER)).ok();
        if !marker.is_some_and(|m| m.split(' ').next() == Some(self.owner)) {
            return Ok(Installed::Unchanged);
        }
        self.install()
    }
}

pub fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp~");
    fs::write(&tmp, content).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, path)?;
    Ok(())
}
