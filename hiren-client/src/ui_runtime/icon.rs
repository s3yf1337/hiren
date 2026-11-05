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
use std::time::{Duration, Instant};

struct CacheEntry {
    value: String,
    /// Negative results expire: apps installed after the launcher started (or
    /// an icon theme being installed) must become visible without a restart.
    expires: Option<Instant>,
}

static CACHE: Mutex<Option<HashMap<String, CacheEntry>>> = Mutex::new(None);
/// Discovered icon-theme names, scanned once per process.
static THEMES: Mutex<Option<Vec<String>>> = Mutex::new(None);

const NEGATIVE_TTL: Duration = Duration::from_secs(15);
const CACHE_MAX_ENTRIES: usize = 4096;
/// Well-known icon themes, tried before any discovered ones.
const KNOWN_THEMES: &[&str] = &[
    "hicolor", "Adwaita", "Yaru", "Papirus", "Papirus-Dark", "breeze", "elementary", "Tela",
];

/// Resolve an icon name or path to an existing file. Cached per process;
/// misses are re-tried after a short TTL.
pub fn resolve(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return String::new();
    }
    if let Ok(mut guard) = CACHE.lock() {
        let cache = guard.get_or_insert_with(HashMap::new);
        if let Some(hit) = cache.get(name) {
            if hit.expires.is_none_or(|e| Instant::now() < e) {
                return hit.value.clone();
            }
        }
        let found = lookup(name);
        if cache.len() >= CACHE_MAX_ENTRIES {
            cache.clear();
        }
        let expires = found.is_empty().then(|| Instant::now() + NEGATIVE_TTL);
        cache.insert(
            name.to_string(),
            CacheEntry { value: found.clone(), expires },
        );
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

    let dirs = icon_dirs();
    let themes = icon_themes(&dirs);
    let theme_refs: Vec<&str> = themes.iter().map(|s| s.as_str()).collect();
    let sizes = ["48x48", "64x64", "32x32", "128x128", "24x24", "256x256", "scalable"];
    let exts_png = ["png"];
    let exts_svg = ["svg"];

    // Prefer raster at a usable size, then SVG.
    for ext in exts_png {
        if let Some(p) = find_named(&dirs, &theme_refs, &sizes, stem, ext) {
            return p;
        }
    }
    for ext in exts_svg {
        if let Some(p) = find_named(&dirs, &theme_refs, &sizes, stem, ext) {
            return p;
        }
    }
    String::new()
}

/// Icon search roots, honouring `XDG_DATA_HOME` (was ignored, so per-user
/// themes never resolved) and the standard XDG + legacy locations.
fn icon_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        dirs.push(data_home.join("icons"));
    } else if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/share/icons"));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".icons"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
        dirs.push(PathBuf::from(d).join("icons"));
        dirs.push(PathBuf::from(d).join("pixmaps"));
    }
    dirs.push(PathBuf::from("/usr/share/pixmaps"));
    dirs.push(PathBuf::from("/usr/share/icons"));
    dirs
}

/// Icon-theme names to probe: well-known ones first, then anything discovered
/// as a subdirectory of an icon root (`Papirus-Dark`, `Yaru`, custom themes).
/// Scanned once per process (idle cost is one read_dir per root).
fn icon_themes(dirs: &[PathBuf]) -> Vec<String> {
    if let Ok(guard) = THEMES.lock() {
        if let Some(hit) = guard.as_ref() {
            return hit.clone();
        }
    }
    let mut themes: Vec<String> = KNOWN_THEMES.iter().map(|s| s.to_string()).collect();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        for entry in rd.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            if !themes.iter().any(|t| t == &name) {
                themes.push(name);
            }
        }
    }
    if let Ok(mut guard) = THEMES.lock() {
        *guard = Some(themes.clone());
    }
    themes
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
        let dir = test_dir("absolute");
        let f = dir.join("hiren-icon-test.png");
        std::fs::write(&f, b"not-a-real-png").ok();
        let got = resolve(f.to_str().unwrap());
        assert_eq!(got, f.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_dir(tag: &str) -> PathBuf {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("hiren-icon-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test dir");
        dir
    }

    #[test]
    fn discovers_non_hardcoded_theme_and_finds_icon() {
        // A theme that is not in the old hardcoded list (e.g. Papirus-Dark)
        // must still be probed; XDG_DATA_HOME must be searched.
        let root = test_dir("xdg");
        let icons = root.join("icons");
        let app_dir = icons.join("MyCustomTheme").join("48x48").join("apps");
        std::fs::create_dir_all(&app_dir).expect("app dir");
        let file = app_dir.join("my-custom-app.png");
        std::fs::write(&file, b"png").expect("icon file");

        let themes = icon_themes(&[icons.clone()]);
        assert!(themes.iter().any(|t| t == "MyCustomTheme"), "themes: {themes:?}");
        let found = find_named(
            &[icons.clone()],
            &themes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            &["48x48"],
            "my-custom-app",
            "png",
        );
        assert_eq!(found, Some(file.to_string_lossy().into_owned()));
        let _ = std::fs::remove_dir_all(&root);
    }
}
