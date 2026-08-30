//! Freedesktop icon lookup — reusable by any theme.
//!
//! `AppEntry.icon` is the raw `Icon=` value (name or path). Themes bind
//! `item_icon` / `launcher.selected_result.icon`, which resolve to a
//! filesystem path the `Image` node can load. Empty string = no file.
//!
//! PNG is preferred (tiny-skia). SVG is returned when no PNG exists; the
//! renderer rasterizes it if the `svg` feature of the image path is used.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Resolve an icon name or path to an existing file. Cached per process.
pub fn resolve(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return String::new();
    }
    if let Ok(mut guard) = CACHE.lock() {
        let cache = guard.get_or_insert_with(HashMap::new);
        if let Some(hit) = cache.get(name) {
            return hit.clone();
        }
        let found = lookup(name);
        cache.insert(name.to_string(), found.clone());
        found
    } else {
        lookup(name)
    }
}

fn lookup(name: &str) -> String {
    let path = Path::new(name);
    if path.is_absolute() && path.is_file() {
        return name.to_string();
    }

    let stem = name
        .trim_end_matches(".png")
        .trim_end_matches(".svg")
        .trim_end_matches(".xpm");

    let mut dirs = Vec::new();
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/share/icons"));
        dirs.push(home.join(".icons"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
        dirs.push(PathBuf::from(d).join("icons"));
        dirs.push(PathBuf::from(d).join("pixmaps"));
    }
    dirs.push(PathBuf::from("/usr/share/pixmaps"));
    dirs.push(PathBuf::from("/usr/share/icons"));

    let themes = ["hicolor", "Adwaita", "Papirus", "breeze", "elementary"];
    let sizes = ["48x48", "32x32", "64x64", "128x128", "24x24", "256x256", "scalable"];
    let exts_png = ["png"];
    let exts_svg = ["svg"];

    // Prefer raster at a usable size, then SVG.
    for ext in exts_png {
        if let Some(p) = find_named(&dirs, &themes, &sizes, stem, ext) {
            return p;
        }
    }
    for ext in exts_svg {
        if let Some(p) = find_named(&dirs, &themes, &sizes, stem, ext) {
            return p;
        }
    }
    String::new()
}

fn find_named(dirs: &[PathBuf], themes: &[&str], sizes: &[&str], stem: &str, ext: &str) -> Option<String> {
    let file = format!("{stem}.{ext}");
    for dir in dirs {
        // pixmaps / loose files
        let loose = dir.join(&file);
        if loose.is_file() {
            return Some(loose.to_string_lossy().into_owned());
        }
        for theme in themes {
            for size in sizes {
                for folder in ["apps", "places", "mimetypes", "devices", "actions"] {
                    let p = dir.join(theme).join(size).join(folder).join(&file);
                    if p.is_file() {
                        return Some(p.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_name_is_empty_path() {
        assert_eq!(resolve(""), "");
        assert_eq!(resolve("   "), "");
    }

    #[test]
    fn absolute_existing_file_passes_through() {
        let f = std::env::temp_dir().join("hiren-icon-test.png");
        std::fs::write(&f, b"not-a-real-png").ok();
        let got = resolve(f.to_str().unwrap());
        assert_eq!(got, f.to_string_lossy());
        let _ = std::fs::remove_file(&f);
    }
}
