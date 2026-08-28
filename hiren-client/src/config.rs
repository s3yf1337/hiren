//! hiren-client configuration
//!
//! Reads `~/.config/hiren/config.toml` and `~/.config/hiren/theme.toml` / `~/.config/hiren/themes/<name>/theme.toml`.
//! New architecture: config is split into launcher config (behavior) and theme config (visuals).

use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

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
pub fn cmd_exists(cmd: &str) -> bool {
    std::process::Command::new("which").arg(cmd).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
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
        KeyBinding { key: "Ctrl+Return".into(), prefix: None, modifiers: Modifiers(Modifiers::CTRL), key_name: "return".into() },
    ]
}

// ---------------------------------------------------------------------------
// TOML deserialization structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct TomlConfig {
    #[serde(default)] ui: UiSection,
    #[serde(default)] bindings: Option<Vec<RawBinding>>,
    #[serde(default)] mode: Option<TomlModeSection>,
    #[serde(default)] terminal: Option<TomlTerminalSection>,
    #[serde(default)] window: Option<TomlWindowSection>,
    #[serde(default)] theme: Option<ThemeSection>,
}
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
        _ => 0.0,
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
            return Self::with_defaults_from_toml(TomlConfig::default());
        }
        let raw = match fs::read_to_string(&config_path) {
            Ok(s) => s,
            Err(e) => { eprintln!("[hiren-config] WARN cannot read {}: {}", config_path.display(), e); return LauncherConfig::default(); }
        };
        let toml_cfg: TomlConfig = match toml::from_str(&raw) {
            Ok(c) => c,
            Err(e) => { eprintln!("[hiren-config] WARN failed to parse {}: {}", config_path.display(), e); return LauncherConfig::default(); }
        };
        Self::with_defaults_from_toml(toml_cfg)
    }

    fn with_defaults_from_toml(t: TomlConfig) -> Self {
        let bindings: Vec<KeyBinding> = t.bindings.unwrap_or_default().into_iter().filter_map(|raw| {
            let (mods, name) = parse_key_combo(&raw.key)?;
            let prefix = raw.prefix.filter(|p| !p.is_empty());
            Some(KeyBinding { key: raw.key, prefix, modifiers: mods, key_name: name })
        }).collect();
        let bindings = if bindings.is_empty() { default_bindings() } else { bindings };

        let modes = t.mode.map(|m| ModeConfig {
            drun: m.drun.unwrap_or(true),
            run: m.run.unwrap_or(false),
            window: m.window.unwrap_or(false),
            calc: m.calc.unwrap_or(false),
        }).unwrap_or_default();

        let default_term = TerminalConfig::default();
        let terminal = t.terminal.map(|x| TerminalConfig {
            command: x.command.unwrap_or(default_term.command.clone()),
            exec_flag: x.exec_flag.unwrap_or(default_term.exec_flag.clone()),
        }).unwrap_or(default_term);

        let window = t.window.map(|w| WindowConfig { list_command: w.list_command, activate_command: w.activate_command }).unwrap_or_default();

        let theme_name = t.theme.and_then(|x| x.name).unwrap_or_else(|| "default".into());

        LauncherConfig {
            bindings,
            auto_close_timeout_secs: t.ui.auto_close_timeout_secs.unwrap_or(8),
            window_width: t.ui.width.unwrap_or(620),
            window_height: t.ui.height.unwrap_or(360),
            modes,
            terminal,
            window,
            text_align: parse_text_align(&t.ui.text_align.unwrap_or_default()),
            freq_weight: t.ui.freq_weight.unwrap_or(0.8),
            keyboard_mode: parse_keyboard_mode(t.ui.keyboard_mode.as_deref()),
            theme: theme_name,
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

    /// Resolve theme path: user theme dir or built-in fallback.
    pub fn theme_path(&self) -> PathBuf {
        // 1. User themes: ~/.config/hiren/themes/<name>/
        if let Some(cfg) = dirs::config_dir() {
            let user = cfg.join("hiren").join("themes").join(&self.theme);
            if user.exists() { return user; }
            // legacy single theme file: ~/.config/hiren/theme.toml
            let legacy = cfg.join("hiren").join("theme.toml");
            if legacy.exists() && self.theme == "default" {
                return cfg.join("hiren");
            }
        }
        // 2. Built-in themes next to binary or in share
        // Try exe dir
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let builtin = dir.join("themes").join(&self.theme);
                if builtin.exists() { return builtin; }
                // also try ../share/hiren/themes
                let share = dir.join("../share/hiren/themes").join(&self.theme);
                if share.exists() { return share; }
            }
        }
        // 3. Fallback to crate-relative themes (dev)
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("themes").join(&self.theme);
        if dev.exists() { return dev; }
        // 4. default built-in if requested theme missing
        let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("themes/default");
        fallback
    }
}

/// Find all available themes (built-in + user).
pub fn available_themes() -> Vec<String> {
    let mut set = std::collections::HashSet::new();
    // built-in
    let builtin_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("themes");
    if let Ok(rd) = fs::read_dir(&builtin_dir) {
        for e in rd.flatten() { if e.path().is_dir() { if let Some(n) = e.file_name().to_str() { set.insert(n.to_string()); } } }
    }
    // user
    if let Some(cfg) = dirs::config_dir() {
        let user_dir = cfg.join("hiren/themes");
        if let Ok(rd) = fs::read_dir(&user_dir) {
            for e in rd.flatten() { if e.path().is_dir() { if let Some(n) = e.file_name().to_str() { set.insert(n.to_string()); } } }
        }
    }
    let mut v: Vec<_> = set.into_iter().collect(); v.sort(); v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn parse_plain_return() { let (m, k) = parse_key_combo("Return").unwrap(); assert_eq!(m, Modifiers::NONE); assert_eq!(k, "return"); }
    #[test] fn parse_ctrl_return() { let (m, k) = parse_key_combo("Ctrl+Return").unwrap(); assert_eq!(m.0, Modifiers::CTRL); assert_eq!(k, "return"); }
    #[test] fn parse_shift_a() { let (m, k) = parse_key_combo("Shift+A").unwrap(); assert_eq!(m.0, Modifiers::SHIFT); assert_eq!(k, "a"); }
    #[test] fn parse_unknown_modifier_none() { assert!(parse_key_combo("Hyper+Return").is_none()); }
    #[test] fn parse_empty_none() { assert!(parse_key_combo("").is_none() || parse_key_combo("").unwrap().1.is_empty()); }
}
