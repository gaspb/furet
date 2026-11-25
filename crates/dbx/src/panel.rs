//! The herdr panel, carried by the binary so it always matches the engine it talks to.

use common::herdr::Mod;

macro_rules! files {
    ($($path:literal),* $(,)?) => { &[$(($path, include_str!(concat!("../panel/", $path)))),*] };
}

pub const PANEL: Mod = Mod {
    name: "dbx",
    owner: "dbx",
    files: files!(
        "mod.json",
        "main.js",
        "api.js",
        "edit.js",
        "form.js",
        "grid.js",
        "help.js",
        "lines.js",
        "menu.js",
        "nav.js",
        "finder.js",
        "ask.js",
        "copy.js",
        "drift.js",
        "inspector.js",
        "sql.js",
        "database.svg",
        "icons/postgres.svg",
        "icons/clickhouse.svg",
    ),
};

// A file added to panel/ has to be added to the list above too.
#[cfg(test)]
mod tests {
    #[test]
    fn carries_every_panel_file() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("panel");
        let mut on_disk: Vec<String> = Vec::new();
        for entry in walk(&dir) {
            let rel = entry
                .strip_prefix(&dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if !rel.ends_with(".d.ts") && rel != "jsconfig.json" && !rel.starts_with(".") {
                on_disk.push(rel);
            }
        }
        let carried: Vec<&str> = super::PANEL.files.iter().map(|(p, _)| *p).collect();
        for f in &on_disk {
            assert!(carried.contains(&f.as_str()), "panel/{f} isn't in PANEL");
        }
    }

    fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p))
            } else {
                out.push(p)
            }
        }
        out
    }
}
