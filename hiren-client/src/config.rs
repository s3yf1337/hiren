//! hiren-client configuration
//!
//! Reads `~/.config/hiren/config.toml` and `~/.config/hiren/theme.toml` / `~/.config/hiren/themes/<name>/theme.toml`.
//! New architecture: config is split into launcher config (behavior) and theme config (visuals).
//!
//! Parsing is deliberately tolerant: a broken section or unknown key produces a
//! warning and defaults for that section only — one typo must not discard the
//! whole config. Numeric values are validated/clamped so they cannot panic the
//! renderer later (e.g. a window size of 4000000000).

use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::fs;
use std::path::{Path, PathBuf};

/// Largest accepted window dimension. The renderer allocates a pixmap of this
/// size; absurd values (4e9) previously aborted the process.
pub const MAX_WINDOW_DIMENSION: i32 = 8192;
/// Largest accepted `freq_weight` (frecency boost multiplier).
pub const MAX_FREQ_WEIGHT: f64 = 10.0;

// ---------------------------------------------------------------------------
// Modifier & Key abstraction (no GTK dependency)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers(pub u8);
impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const CTRL: u8 = 1 << 0;
    pub const SHIFT: u8 = 1 << 1;
    pub const ALT: u8 = 1 << 2;
    pub const SUPER: u8 = 1 << 3;
    pub fn contains(self, flag: u8) -> bool { self.0 & flag != 0 }
    pub fn empty(self) -> bool { self.0 == 0 }
}

#[derive(Debug, Clone)]
pub struct KeyBinding {
    pub key: String,
    pub prefix: Option<String>,
    /// Modifiers bitmask (see Modifiers constants)
    pub modifiers: Modifiers,
    /// Normalized key name lowercased, e.g. "return", "a", "f1"
    pub key_name: String,
}

// ---------------------------------------------------------------------------
// Launcher behavior config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ModeConfig {
    pub drun: bool,
    pub run: bool,
    pub window: bool,
    pub calc: bool,
}
impl Default for ModeConfig {
    fn default() -> Self { Self { drun: true, run: false, window: false, calc: false } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardModeConfig { #[default] Exclusive, OnDemand }

#[derive(Debug, Clone)]
pub struct TerminalConfig {
    pub command: String,
    pub exec_flag: String,
}
impl Default for TerminalConfig {
    fn default() -> Self {
        let (c, f) = detect_terminal();
        Self { command: c, exec_flag: f }
    }
}
fn detect_terminal() -> (String, String) {
    if let Ok(term) = std::env::var("TERMINAL") { if !term.is_empty() { return (term, "-e".into()); } }
    for (cmd, flag) in [("foot","-e"),("alacritty","-e"),("kitty","--"),("wezterm","start"),("gnome-terminal","--"),("konsole","-e"),("xfce4-terminal","-e"),("xterm","-e")] {
        if cmd_exists(cmd) { return (cmd.into(), flag.into()); }
    }
    ("foot".into(), "-e".into())
}
/// Find an executable in `$PATH` without spawning `which` (cheaper, testable).
pub fn cmd_exists(cmd: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    cmd_exists_in_path(cmd, &path)
}

fn cmd_exists_in_path(cmd: &str, path_var: &str) -> bool {
    if cmd.is_empty() {
        return false;
    }
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(cmd);
        match fs::metadata(&candidate) {
            Ok(meta) if meta.is_file() => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if meta.permissions().mode() & 0o111 == 0 {
                        continue;
                    }
                }
                return true;
            }
            _ => {}
        }
    }
    false
}

#[derive(Debug, Clone)]
pub struct WindowConfig {
    pub list_command: Option<String>,
    pub activate_command: Option<String>,
}
impl Default for WindowConfig { fn default() -> Self { Self { list_command: None, activate_command: None } } }

#[derive(Debug, Clone)]
pub struct LauncherConfig {
    pub bindings: Vec<KeyBinding>,
    pub auto_close_timeout_secs: u64,
    pub window_width: i32,
    pub window_height: i32,
    pub modes: ModeConfig,
    pub terminal: TerminalConfig,
    pub window: WindowConfig,
    pub text_align: f32,
    pub freq_weight: f64,
    pub keyboard_mode: KeyboardModeConfig,
    /// Selected theme name (directory under ~/.config/hiren/themes/ or built-in)
    pub theme: String,
}
impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            bindings: default_bindings(),
            auto_close_timeout_secs: 8,
            window_width: 620,
            window_height: 360,
            modes: ModeConfig::default(),
            terminal: TerminalConfig::default(),
            window: WindowConfig::default(),
            text_align: 0.0,
            freq_weight: 0.8,
            keyboard_mode: KeyboardModeConfig::Exclusive,
            theme: "default".into(),
        }
    }
}

fn default_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding { key: "Return".into(), prefix: None, modifiers: Modifiers::NONE, key_name: "return".into() },
        // Ctrl+Return: без явного prefix запускает в терминале из `[terminal]`.
        KeyBinding { key: "Ctrl+Return".into(), prefix: None, modifiers: Modifiers(Modifiers::CTRL), key_name: "return".into() },
    ]
}

// ---------------------------------------------------------------------------
// TOML deserialization structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct TomlConfig {
    #[serde(default)] ui: UiSection,
    #[serde(default)] raw_bindings: Vec<RawBinding>,
    #[serde(default)] mode: TomlModeSection,
    #[serde(default)] terminal: TomlTerminalSection,
    #[serde(default)] window: TomlWindowSection,
    #[serde(default = "default_theme_name")] theme_name: String,
    /// true, если в файле был `[[bindings]]` (даже если ни один не распарсился)
    #[serde(skip)] bindings_present: bool,
}
fn default_theme_name() -> String { "default".into() }

#[derive(Debug, Deserialize)]
struct RawBinding { key: String, #[serde(default)] prefix: Option<String> }
#[derive(Debug, Clone, Deserialize, Default)]
struct UiSection {
    #[serde(default)] auto_close_timeout_secs: Option<u64>,
    #[serde(default)] width: Option<i32>,
    #[serde(default)] height: Option<i32>,
    #[serde(default)] text_align: Option<String>,
    #[serde(default)] freq_weight: Option<f64>,
    #[serde(default)] keyboard_mode: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Default)]
struct TomlModeSection { drun: Option<bool>, run: Option<bool>, window: Option<bool>, calc: Option<bool> }
#[derive(Debug, Clone, Deserialize, Default)]
struct TomlTerminalSection { command: Option<String>, exec_flag: Option<String> }
#[derive(Debug, Clone, Deserialize, Default)]
struct TomlWindowSection { list_command: Option<String>, activate_command: Option<String> }
#[derive(Debug, Clone, Deserialize, Default)]
struct ThemeSection { name: Option<String> }

// ---------------------------------------------------------------------------
// Theme / UI runtime config (visual)
// ---------------------------------------------------------------------------

/// Theme manifest - describes visual theme metadata and optionally
/// points to main UI definition file.
#[derive(Debug, Clone, Deserialize)]
pub struct ThemeManifest {
    pub name: String,
    #[serde(default)] pub description: String,
    /// Main UI file relative to theme dir (default: theme.toml)
    #[serde(default = "default_main")] pub main: String,
    #[serde(default)] pub author: String,
}

fn default_main() -> String { "theme.toml".into() }

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn parse_key_combo(s: &str) -> Option<(Modifiers, String)> {
    let parts: Vec<&str> = s.split('+').map(|p| p.trim()).collect();
    if parts.is_empty() { return None; }
    let key_name = parts.last()?.to_lowercase();
    let mut mods = Modifiers::NONE;
    for m in &parts[..parts.len()-1] {
        match m.to_lowercase().as_str() {
            "ctrl"|"control" => mods.0 |= Modifiers::CTRL,
            "shift" => mods.0 |= Modifiers::SHIFT,
            "alt" => mods.0 |= Modifiers::ALT,
            "super"|"win"|"mod4" => mods.0 |= Modifiers::SUPER,
            "meta" => mods.0 |= Modifiers::ALT, // treat meta as alt for now
            other => { eprintln!("[hiren-config] WARN unknown modifier '{}'", other); return None; }
        }
    }
    // validate key name somewhat
    if key_name.is_empty() { return None; }
    Some((mods, key_name))
}

fn parse_text_align(s: &str) -> f32 {
    match s.to_lowercase().trim() {
        "center"|"middle" => 0.5,
        "right" => 1.0,
        "left"|"start"|"" => 0.0,
        other => {
            eprintln!("[hiren-config] WARN unknown text_align '{other}', using 'left'");
            0.0
        }
    }
}

fn parse_keyboard_mode(s: Option<&str>) -> KeyboardModeConfig {
    match s {
        Some(v) => match v.to_lowercase().as_str() {
            "on_demand"|"ondemand" => KeyboardModeConfig::OnDemand,
            "exclusive" => KeyboardModeConfig::Exclusive,
            other => { eprintln!("[hiren-config] WARN unknown keyboard_mode '{}', using exclusive", other); KeyboardModeConfig::Exclusive }
        },
        None => KeyboardModeConfig::Exclusive,
    }
}

fn warn_unknown_keys(table: &toml::Table, allowed: &[&str], section: &str) {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            eprintln!(
                "[hiren-config] WARN unknown key `{key}` in {section} (ignored); known keys: {}",
                allowed.join(", ")
            );
        }
    }
}

fn section_table<'a>(root: &'a toml::Table, name: &str, allowed: &[&str]) -> Option<&'a toml::Table> {
    match root.get(name) {
        Some(toml::Value::Table(table)) => {
            warn_unknown_keys(table, allowed, name);
            Some(table)
        }
        Some(_) => {
            eprintln!("[hiren-config] WARN `{name}` must be a table, ignoring it");
            None
        }
        None => None,
    }
}

/// Parse one section tolerantly: on error warn and fall back to defaults.
fn parse_section<T: DeserializeOwned + Default>(
    root: &toml::Table,
    name: &str,
    allowed: &[&str],
) -> T {
    let Some(table) = section_table(root, name, allowed) else {
        return T::default();
    };
    match toml::Value::Table(table.clone()).try_into::<T>() {
        Ok(value) => value,
        Err(e) => {
            eprintln!("[hiren-config] WARN invalid [{name}] section ({e}); using defaults for it");
            T::default()
        }
    }
}

fn parse_bindings(root: &toml::Table) -> (Vec<RawBinding>, bool) {
    let Some(value) = root.get("bindings") else {
        return (Vec::new(), false);
    };
    let toml::Value::Array(items) = value else {
        eprintln!("[hiren-config] WARN `bindings` must be an array of tables, ignoring it");
        return (Vec::new(), false);
    };
    if items.is_empty() {
        // Явно пустой список — валидный способ остаться с дефолтами.
        return (Vec::new(), false);
    }
    let mut bindings = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let toml::Value::Table(table) = item else {
            eprintln!("[hiren-config] WARN binding #{index} is not a table, ignoring it");
            continue;
        };
        warn_unknown_keys(table, &["key", "prefix"], &format!("bindings[{index}]"));
        match item.clone().try_into::<RawBinding>() {
            Ok(binding) => bindings.push(binding),
            Err(e) => eprintln!("[hiren-config] WARN invalid binding #{index} ({e}), ignoring it"),
        }
    }
    (bindings, true)
}

fn clamp_dimension(value: Option<i32>, default: i32, field: &str) -> i32 {
    match value {
        None => default,
        Some(v) if (1..=MAX_WINDOW_DIMENSION).contains(&v) => v,
        Some(v) => {
            let clamped = v.clamp(1, MAX_WINDOW_DIMENSION);
            eprintln!(
                "[hiren-config] WARN {field} = {v} is out of range 1..={MAX_WINDOW_DIMENSION}; using {clamped}"
            );
            clamped
        }
    }
}

fn validate_freq_weight(value: Option<f64>) -> f64 {
    let default = 0.8;
    match value {
        None => default,
        Some(v) if !v.is_finite() => {
            eprintln!("[hiren-config] WARN freq_weight = {v} is not finite; using {default}");
            default
        }
        Some(v) => {
            let clamped = v.clamp(0.0, MAX_FREQ_WEIGHT);
            if clamped != v {
                eprintln!(
                    "[hiren-config] WARN freq_weight = {v} is out of range 0..={MAX_FREQ_WEIGHT}; using {clamped}"
                );
            }
            clamped
        }
    }
}

fn validate_auto_close(value: Option<u64>) -> u64 {
    value.unwrap_or(8)
}

// ---------------------------------------------------------------------------
// Theme discovery
// ---------------------------------------------------------------------------

/// Каталоги, из которых берутся темы (по убыванию приоритета).
///
/// Собираются из пользовательского конфига, каталога бинарника и
/// XDG_DATA_DIRS/hiren/themes; `CARGO_MANIFEST_DIR/themes` — только в dev-сборке.
/// Отдельная структура позволяет юнит-тестам подставить временные каталоги.
#[derive(Debug, Clone, Default)]
struct ThemeSearchPaths {
    /// $XDG_CONFIG_HOME (обычно ~/.config)
    config_base: Option<PathBuf>,
    /// $XDG_DATA_HOME (обычно ~/.local/share)
    data_home: Option<PathBuf>,
    /// $XDG_DATA_DIRS (по умолчанию /usr/local/share:/usr/share)
    data_dirs: Vec<PathBuf>,
    /// каталог, в котором лежит бинарник
    exe_dir: Option<PathBuf>,
    /// dev-only: hiren-client/themes
    dev_themes: Option<PathBuf>,
}

impl ThemeSearchPaths {
    fn from_env() -> Self {
        let data_dirs = std::env::var_os("XDG_DATA_DIRS")
            .map(|value| {
                std::env::split_paths(&value)
                    .filter(|p| !p.as_os_str().is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|dirs| !dirs.is_empty())
            .unwrap_or_else(|| {
                vec![PathBuf::from("/usr/local/share"), PathBuf::from("/usr/share")]
            });
        Self {
            config_base: dirs::config_dir(),
            data_home: dirs::data_dir(),
            data_dirs,
            exe_dir: std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(Path::to_path_buf)),
            dev_themes: cfg!(debug_assertions)
                .then(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("themes")),
        }
    }
}

/// Каталоги, внутри которых лежат каталоги тем `<name>/theme.toml`.
fn theme_roots_from(paths: &ThemeSearchPaths) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push = |root: PathBuf| {
        if !roots.iter().any(|existing| existing == &root) {
            roots.push(root);
        }
    };
    if let Some(config) = &paths.config_base {
        // 1. пользовательские темы
        push(config.join("hiren").join("themes"));
    }
    if let Some(exe_dir) = &paths.exe_dir {
        // 2. установленные рядом с бинарником / в share префикса
        push(exe_dir.join("themes"));
        if let Some(prefix) = exe_dir.parent() {
            push(prefix.join("share").join("hiren").join("themes"));
        }
    }
    if let Some(data_home) = &paths.data_home {
        // 3. XDG_DATA_HOME/hiren/themes
        push(data_home.join("hiren").join("themes"));
    }
    for dir in &paths.data_dirs {
        // 4. XDG_DATA_DIRS/hiren/themes (системная установка)
        push(dir.join("hiren").join("themes"));
    }
    if let Some(dev) = &paths.dev_themes {
        // 5. dev-дерево исходников (только debug-сборка)
        push(dev.clone());
    }
    roots
}

fn is_safe_theme_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

fn theme_dir_has_manifest(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    dir.join("theme.toml").is_file()
        || dir.join("main.toml").is_file()
        || dir.join("theme").join("main.toml").is_file()
}

fn legacy_theme_dir(config_base: Option<&Path>) -> Option<PathBuf> {
    let config_base = config_base?;
    let hiren_dir = config_base.join("hiren");
    hiren_dir.join("theme.toml").is_file().then_some(hiren_dir)
}

/// Все каталоги-корни тем в порядке приоритета.
pub fn theme_roots() -> Vec<PathBuf> {
    theme_roots_from(&ThemeSearchPaths::from_env())
}

fn find_theme_in(name: &str, roots: &[PathBuf], legacy: Option<&Path>) -> Option<PathBuf> {
    if !is_safe_theme_name(name) {
        return None;
    }
    for root in roots {
        let candidate = root.join(name);
        if theme_dir_has_manifest(&candidate) {
            return Some(candidate);
        }
    }
    // Старый формат: ~/.config/hiren/theme.toml (по умолчанию называется default).
    if name == "default" {
        return legacy.map(Path::to_path_buf);
    }
    None
}

/// Найти каталог темы `name` (пользовательские темы приоритетнее встроенных).
pub fn find_theme(name: &str) -> Option<PathBuf> {
    let paths = ThemeSearchPaths::from_env();
    let roots = theme_roots_from(&paths);
    let legacy = legacy_theme_dir(paths.config_base.as_deref());
    find_theme_in(name, &roots, legacy.as_deref())
}

/// Список (имя, путь) всех доступных тем: пользовательские, установленные и
/// встроенные; одноимённые записи не дублируются (побеждает приоритет).
pub fn available_theme_paths() -> Vec<(String, PathBuf)> {
    let paths = ThemeSearchPaths::from_env();
    let roots = theme_roots_from(&paths);
    let legacy = legacy_theme_dir(paths.config_base.as_deref());
    available_theme_paths_in(&roots, legacy.as_deref())
}

fn available_theme_paths_in(roots: &[PathBuf], legacy: Option<&Path>) -> Vec<(String, PathBuf)> {
    let mut seen = std::collections::HashSet::new();
    let mut themes: Vec<(String, PathBuf)> = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if !is_safe_theme_name(name) {
                continue;
            }
            if seen.insert(name.to_string()) {
                themes.push((name.to_string(), path));
            }
        }
    }
    if let Some(legacy_dir) = legacy {
        if seen.insert("default".to_string()) {
            themes.push(("default".into(), legacy_dir.to_path_buf()));
        }
    }
    themes.sort_by(|a, b| a.0.cmp(&b.0));
    themes
}

/// Find all available themes (built-in + user + installed).
pub fn available_themes() -> Vec<String> {
    available_theme_paths().into_iter().map(|(name, _)| name).collect()
}

/// Встроенная тема по умолчанию (для fallback, если запрошенной нет).
pub fn default_theme_dir() -> Option<PathBuf> {
    find_theme("default")
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl LauncherConfig {
    pub fn load() -> Self {
        let config_dir = match dirs::config_dir() {
            Some(d) => d.join("hiren"),
            None => { eprintln!("[hiren-config] WARN cannot determine config dir, using defaults"); return LauncherConfig::default(); }
        };
        let _ = fs::create_dir_all(&config_dir);
        let config_path = config_dir.join("config.toml");
        if !config_path.exists() {
            return Self::from_raw_toml("");
        }
        let raw = match fs::read_to_string(&config_path) {
            Ok(s) => s,
            Err(e) => { eprintln!("[hiren-config] WARN cannot read {}: {}", config_path.display(), e); return LauncherConfig::default(); }
        };
        Self::from_raw_toml(&raw)
    }

    /// Разобрать TOML-текст (используется `load` и юнит-тестами).
    fn from_raw_toml(raw: &str) -> Self {
        let toml_cfg = parse_toml_config(raw);
        Self::with_defaults_from_toml(toml_cfg)
    }

    fn with_defaults_from_toml(t: TomlConfig) -> Self {
        let bindings: Vec<KeyBinding> = t.raw_bindings.into_iter().filter_map(|raw| {
            let (mods, name) = match parse_key_combo(&raw.key) {
                Some(parsed) => parsed,
                None => {
                    eprintln!("[hiren-config] WARN invalid binding key `{}`, ignoring it", raw.key);
                    return None;
                }
            };
            let prefix = raw.prefix.filter(|p| {
                if p.trim().is_empty() {
                    eprintln!("[hiren-config] WARN empty binding prefix for `{}`, ignoring it", raw.key);
                    false
                } else {
                    true
                }
            });
            Some(KeyBinding { key: raw.key, prefix, modifiers: mods, key_name: name })
        }).collect();
        let bindings = if bindings.is_empty() {
            if t.bindings_present {
                eprintln!("[hiren-config] WARN no valid bindings, using default (Return / Ctrl+Return)");
            }
            default_bindings()
        } else {
            bindings
        };

        let modes = ModeConfig {
            drun: t.mode.drun.unwrap_or(true),
            run: t.mode.run.unwrap_or(false),
            window: t.mode.window.unwrap_or(false),
            calc: t.mode.calc.unwrap_or(false),
        };

        let default_term = TerminalConfig::default();
        let terminal = TerminalConfig {
            command: t.terminal.command.unwrap_or(default_term.command),
            exec_flag: t.terminal.exec_flag.unwrap_or(default_term.exec_flag),
        };
        if terminal.command.trim().is_empty() {
            eprintln!("[hiren-config] WARN [terminal] command is empty; Ctrl+Enter will report an error");
        }

        let window = WindowConfig {
            list_command: t.window.list_command,
            activate_command: t.window.activate_command,
        };

        LauncherConfig {
            bindings,
            auto_close_timeout_secs: validate_auto_close(t.ui.auto_close_timeout_secs),
            window_width: clamp_dimension(t.ui.width, 620, "ui.width"),
            window_height: clamp_dimension(t.ui.height, 360, "ui.height"),
            modes,
            terminal,
            window,
            text_align: parse_text_align(t.ui.text_align.as_deref().unwrap_or_default()),
            freq_weight: validate_freq_weight(t.ui.freq_weight),
            keyboard_mode: parse_keyboard_mode(t.ui.keyboard_mode.as_deref()),
            theme: t.theme_name,
        }
    }

    pub fn active_modes(&self) -> Vec<hiren_shared::AppMode> {
        let mut v = Vec::new();
        if self.modes.drun { v.push(hiren_shared::AppMode::Drun); }
        if self.modes.run { v.push(hiren_shared::AppMode::Run); }
        if self.modes.window { v.push(hiren_shared::AppMode::Window); }
        if self.modes.calc { v.push(hiren_shared::AppMode::Calc); }
        v
    }
}

/// Разобрать config.toml по секциям: ошибка в одной секции не отбрасывает
/// остальные, неизвестные ключи выводятся предупреждением.
fn parse_toml_config(raw: &str) -> TomlConfig {
    if raw.trim().is_empty() {
        return TomlConfig::default();
    }
    let value: toml::Value = match raw.parse() {
        Ok(value) => value,
        Err(e) => {
            eprintln!("[hiren-config] WARN failed to parse config.toml: {e}; using defaults");
            return TomlConfig::default();
        }
    };
    let Some(root) = value.as_table() else {
        eprintln!("[hiren-config] WARN config.toml is not a table; using defaults");
        return TomlConfig::default();
    };
    warn_unknown_keys(
        root,
        &["ui", "bindings", "mode", "terminal", "window", "theme"],
        "config.toml",
    );

    let ui: UiSection = parse_section(
        root,
        "ui",
        &[
            "auto_close_timeout_secs",
            "width",
            "height",
            "text_align",
            "freq_weight",
            "keyboard_mode",
        ],
    );
    let mode: TomlModeSection = parse_section(root, "mode", &["drun", "run", "window", "calc"]);
    let terminal: TomlTerminalSection =
        parse_section(root, "terminal", &["command", "exec_flag"]);
    let window_cfg: TomlWindowSection =
        parse_section(root, "window", &["list_command", "activate_command"]);
    let theme: ThemeSection = parse_section(root, "theme", &["name"]);
    let (raw_bindings, bindings_present) = parse_bindings(root);

    TomlConfig {
        ui,
        raw_bindings,
        mode,
        terminal,
        window: window_cfg,
        theme_name: theme.name.unwrap_or_else(default_theme_name),
        bindings_present,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    // -----------------------------------------------------------------
    // Key combos / small parsers
    // -----------------------------------------------------------------

    #[test] fn parse_plain_return() { let (m, k) = parse_key_combo("Return").unwrap(); assert_eq!(m, Modifiers::NONE); assert_eq!(k, "return"); }
    #[test] fn parse_ctrl_return() { let (m, k) = parse_key_combo("Ctrl+Return").unwrap(); assert_eq!(m.0, Modifiers::CTRL); assert_eq!(k, "return"); }
    #[test] fn parse_shift_a() { let (m, k) = parse_key_combo("Shift+A").unwrap(); assert_eq!(m.0, Modifiers::SHIFT); assert_eq!(k, "a"); }
    #[test] fn parse_unknown_modifier_none() { assert!(parse_key_combo("Hyper+Return").is_none()); }
    #[test] fn parse_empty_none() { assert!(parse_key_combo("").is_none() || parse_key_combo("").unwrap().1.is_empty()); }

    #[test]
    fn text_align_unknown_falls_back_to_left() {
        assert_eq!(parse_text_align("center"), 0.5);
        assert_eq!(parse_text_align("right"), 1.0);
        assert_eq!(parse_text_align("justify"), 0.0);
    }

    // -----------------------------------------------------------------
    // Tolerant config parsing
    // -----------------------------------------------------------------

    #[test]
    fn default_config_has_terminal_prefix_binding() {
        let config = LauncherConfig::default();
        let ctrl_return = config
            .bindings
            .iter()
            .find(|b| b.key_name == "return" && b.modifiers.contains(Modifiers::CTRL))
            .expect("Ctrl+Return binding");
        assert_eq!(ctrl_return.prefix, None);
        // Prefix выводится из [terminal] в frontend::resolve_launch_mode.
        assert!(!config.terminal.command.is_empty());
    }

    #[test]
    fn valid_config_parses_all_sections() {
        let raw = r#"
[ui]
width = 800
height = 500
auto_close_timeout_secs = 3
text_align = "center"
freq_weight = 0.5
keyboard_mode = "on_demand"

[mode]
drun = true
run = true
calc = true

[terminal]
command = "kitty"
exec_flag = "--"

[window]
list_command = "my-list"
activate_command = "my-activate {id}"

[theme]
name = "atlus"

[[bindings]]
key = "Return"

[[bindings]]
key = "Ctrl+Return"
prefix = "env A=B"
"#;
        let config = LauncherConfig::from_raw_toml(raw);
        assert_eq!(config.window_width, 800);
        assert_eq!(config.window_height, 500);
        assert_eq!(config.auto_close_timeout_secs, 3);
        assert_eq!(config.text_align, 0.5);
        assert_eq!(config.freq_weight, 0.5);
        assert_eq!(config.keyboard_mode, KeyboardModeConfig::OnDemand);
        assert!(config.modes.run && config.modes.calc);
        assert_eq!(config.terminal.command, "kitty");
        assert_eq!(config.terminal.exec_flag, "--");
        assert_eq!(config.window.list_command.as_deref(), Some("my-list"));
        assert_eq!(config.theme, "atlus");
        assert_eq!(config.bindings.len(), 2);
        assert_eq!(config.bindings[1].prefix.as_deref(), Some("env A=B"));
    }

    #[test]
    fn broken_ui_section_does_not_discard_other_sections() {
        // width — строка вместо числа: секция [ui] падает, но [terminal],
        // [[bindings]] и [mode] должны остаться рабочими.
        let raw = r#"
[ui]
width = "wide"

[terminal]
command = "alacritty"
exec_flag = "-e"

[mode]
run = true

[[bindings]]
key = "Ctrl+Space"
"#;
        let config = LauncherConfig::from_raw_toml(raw);
        assert_eq!(config.terminal.command, "alacritty");
        assert_eq!(config.terminal.exec_flag, "-e");
        assert!(config.modes.run);
        assert_eq!(config.bindings.len(), 1);
        assert_eq!(config.bindings[0].key_name, "space");
        // [ui] упала целиком — размеры по умолчанию.
        assert_eq!(config.window_width, 620);
        assert_eq!(config.window_height, 360);
    }

    #[test]
    fn unknown_keys_are_ignored_without_losing_known_values() {
        let raw = r#"
unknown_root = 1

[ui]
width = 700
nonsense = true
"#;
        let config = LauncherConfig::from_raw_toml(raw);
        assert_eq!(config.window_width, 700);
    }

    #[test]
    fn invalid_toml_falls_back_to_defaults() {
        let config = LauncherConfig::from_raw_toml("this is not = [valid toml");
        assert_eq!(config.window_width, 620);
        assert_eq!(config.bindings.len(), LauncherConfig::default().bindings.len());
    }

    #[test]
    fn empty_bindings_list_keeps_defaults() {
        let config = LauncherConfig::from_raw_toml("bindings = []\n");
        assert_eq!(config.bindings.len(), LauncherConfig::default().bindings.len());
    }

    #[test]
    fn invalid_binding_is_skipped_and_defaults_used_when_all_invalid() {
        let raw = r#"
[[bindings]]
key = "Hyper+Return"

[[bindings]]
key = 42
"#;
        let config = LauncherConfig::from_raw_toml(raw);
        assert_eq!(config.bindings.len(), LauncherConfig::default().bindings.len());
    }

    #[test]
    fn binding_with_unknown_key_is_skipped_but_others_kept() {
        let raw = r#"
[[bindings]]
key = "Return"
typ =

[[bindings]]
key = "Ctrl+Return"
"#;
        // Второй биндинг валиден (лучше, чем полностью отбросить список).
        let config = LauncherConfig::from_raw_toml(raw);
        assert!(config.bindings.iter().any(|b| b.modifiers.contains(Modifiers::CTRL)));
    }

    // -----------------------------------------------------------------
    // Validation / clamping (prevents renderer panic)
    // -----------------------------------------------------------------

    #[test]
    fn huge_window_dimensions_are_clamped() {
        let raw = "[ui]\nwidth = 4000000000\nheight = 0\n";
        // 4000000000 не влезает в i32 → [ui] падает → дефолты (ранее --width
        // 4000000000 давал panic при аллокации pixmap).
        let config = LauncherConfig::from_raw_toml(raw);
        assert_eq!(config.window_width, 620);
        assert_eq!(config.window_height, 360);

        let raw = "[ui]\nwidth = 9000\nheight = 0\n";
        let config = LauncherConfig::from_raw_toml(raw);
        assert_eq!(config.window_width, MAX_WINDOW_DIMENSION);
        assert_eq!(config.window_height, 1);
    }

    #[test]
    fn freq_weight_is_clamped_and_rejects_non_finite() {
        let raw = "[ui]\nfreq_weight = 42.0\n";
        assert_eq!(LauncherConfig::from_raw_toml(raw).freq_weight, MAX_FREQ_WEIGHT);

        let raw = "[ui]\nfreq_weight = -5.0\n";
        assert_eq!(LauncherConfig::from_raw_toml(raw).freq_weight, 0.0);

        let raw = "[ui]\nfreq_weight = nan\n";
        assert_eq!(LauncherConfig::from_raw_toml(raw).freq_weight, 0.8);

        let raw = "[ui]\nfreq_weight = inf\n";
        assert_eq!(LauncherConfig::from_raw_toml(raw).freq_weight, 0.8);
    }

    // -----------------------------------------------------------------
    // Theme discovery
    // -----------------------------------------------------------------

    fn write_theme(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("theme.toml"), "name = \"test\"\n").unwrap();
        dir.to_path_buf()
    }

    fn search_paths(
        config_base: &Path,
        data_home: &Path,
        data_dir: &Path,
        exe_dir: &Path,
        dev: Option<&Path>,
    ) -> ThemeSearchPaths {
        ThemeSearchPaths {
            config_base: Some(config_base.to_path_buf()),
            data_home: Some(data_home.to_path_buf()),
            data_dirs: vec![data_dir.to_path_buf()],
            exe_dir: Some(exe_dir.to_path_buf()),
            dev_themes: dev.map(Path::to_path_buf),
        }
    }

    #[test]
    fn theme_roots_priority_order() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = search_paths(
            tmp.path(),
            tmp.path(),
            tmp.path(),
            tmp.path(),
            Some(&tmp.path().join("dev")),
        );
        let roots = theme_roots_from(&paths);
        assert_eq!(roots[0], tmp.path().join("hiren/themes"));
        assert!(roots.contains(&tmp.path().join("themes")));
        assert!(roots.contains(&tmp.path().join("share/hiren/themes")));
        assert!(roots.contains(&tmp.path().join("hiren/themes"))); // data_home, dedup
        assert_eq!(roots.last().unwrap(), &tmp.path().join("dev"));
    }

    #[test]
    fn find_theme_prefers_user_then_install_then_dev() {
        let tmp = tempfile::tempdir().unwrap();
        let config_base = tmp.path().join("config");
        let data_home = tmp.path().join("data-home");
        let data_dir = tmp.path().join("data-dir");
        let exe_dir = tmp.path().join("prefix/bin");
        let dev = tmp.path().join("dev");
        let paths = search_paths(
            &config_base,
            &data_home,
            &data_dir,
            &exe_dir,
            Some(&dev),
        );
        let roots = theme_roots_from(&paths);

        // Встроенная (dev) тема находится.
        let dev_theme = write_theme(&dev.join("atlus"));
        assert_eq!(find_theme_in("atlus", &roots, None), Some(dev_theme.clone()));

        // Пользовательская тема перекрывает встроенную.
        let user_theme = write_theme(&config_base.join("hiren/themes/atlus"));
        assert_eq!(find_theme_in("atlus", &roots, None), Some(user_theme));

        // Установленная в prefix/share тоже находится.
        let share_theme = write_theme(&exe_dir.join("../share/hiren/themes/macos"));
        assert_eq!(find_theme_in("macos", &roots, None), Some(share_theme));

        // XDG_DATA_DIRS/hiren/themes.
        let sys_theme = write_theme(&data_dir.join("hiren/themes/layered"));
        assert_eq!(find_theme_in("layered", &roots, None), Some(sys_theme));
    }

    #[test]
    fn missing_theme_is_none_not_silent_default() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = search_paths(tmp.path(), tmp.path(), tmp.path(), tmp.path(), None);
        let roots = theme_roots_from(&paths);
        assert_eq!(find_theme_in("does-not-exist", &roots, None), None);
        // Каталог есть, но манифеста нет — тоже не тема.
        fs::create_dir_all(tmp.path().join("hiren/themes/empty")).unwrap();
        assert_eq!(find_theme_in("empty", &roots, None), None);
    }

    #[test]
    fn legacy_single_theme_file_serves_default_only() {
        let tmp = tempfile::tempdir().unwrap();
        let config_base = tmp.path().join("config");
        fs::create_dir_all(config_base.join("hiren")).unwrap();
        fs::write(config_base.join("hiren/theme.toml"), "name = \"legacy\"\n").unwrap();
        let paths = search_paths(
            &config_base,
            tmp.path(),
            tmp.path(),
            tmp.path(),
            None,
        );
        let roots = theme_roots_from(&paths);
        let legacy = legacy_theme_dir(Some(&config_base));
        assert_eq!(
            find_theme_in("default", &roots, legacy.as_deref()),
            Some(config_base.join("hiren"))
        );
        assert_eq!(find_theme_in("other", &roots, legacy.as_deref()), None);
    }

    #[test]
    fn theme_names_cannot_escape_search_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = search_paths(tmp.path(), tmp.path(), tmp.path(), tmp.path(), None);
        let roots = theme_roots_from(&paths);
        for name in ["", ".", "..", "../etc/passwd", "a/b", "a\\b"] {
            assert_eq!(find_theme_in(name, &roots, None), None, "name: {name:?}");
        }
        assert!(!is_safe_theme_name("../escape"));
        assert!(is_safe_theme_name("my-theme_1"));
    }

    #[test]
    fn available_themes_union_and_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let config_base = tmp.path().join("config");
        let data_dir = tmp.path().join("data");
        let user_theme = write_theme(&config_base.join("hiren/themes/custom"));
        let builtin = write_theme(&tmp.path().join("themes/default"));
        // Одноимённая тема в двух местах: пользовательская побеждает.
        write_theme(&data_dir.join("hiren/themes/custom"));
        let sys_theme = write_theme(&data_dir.join("hiren/themes/sys"));

        let paths = ThemeSearchPaths {
            config_base: Some(config_base.clone()),
            data_home: None,
            data_dirs: vec![data_dir],
            exe_dir: Some(tmp.path().to_path_buf()),
            dev_themes: None,
        };
        let roots = theme_roots_from(&paths);
        let themes = available_theme_paths_in(&roots, None);
        let names: Vec<&str> = themes.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["custom", "default", "sys"]);
        let custom = themes.iter().find(|(n, _)| n == "custom").unwrap();
        assert_eq!(custom.1, user_theme);
        assert!(themes.iter().any(|(n, p)| n == "default" && p == &builtin));
        assert!(themes.iter().any(|(n, p)| n == "sys" && p == &sys_theme));
    }

    #[test]
    fn available_themes_include_legacy_default() {
        let tmp = tempfile::tempdir().unwrap();
        let config_base = tmp.path().join("config");
        fs::create_dir_all(config_base.join("hiren")).unwrap();
        fs::write(config_base.join("hiren/theme.toml"), "name = \"legacy\"\n").unwrap();
        let paths = ThemeSearchPaths {
            config_base: Some(config_base.clone()),
            ..ThemeSearchPaths::default()
        };
        let roots = theme_roots_from(&paths);
        let legacy = legacy_theme_dir(Some(&config_base));
        let themes = available_theme_paths_in(&roots, legacy.as_deref());
        assert!(themes.iter().any(|(n, _)| n == "default"));
    }

    // -----------------------------------------------------------------
    // cmd_exists (no `which` subprocess)
    // -----------------------------------------------------------------

    #[test]
    fn cmd_exists_scans_path_with_executable_bit() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("my-tool");
        fs::write(&executable, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let not_executable = bin.join("data-file");
        fs::write(&not_executable, b"x").unwrap();
        fs::set_permissions(&not_executable, fs::Permissions::from_mode(0o644)).unwrap();

        let path_var = format!("/nonexistent:{}", bin.display());
        assert!(cmd_exists_in_path("my-tool", &path_var));
        assert!(!cmd_exists_in_path("data-file", &path_var));
        assert!(!cmd_exists_in_path("absent-tool", &path_var));
        assert!(!cmd_exists_in_path("", &path_var));
    }
}
