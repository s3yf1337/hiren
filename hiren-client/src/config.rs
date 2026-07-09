//! Конфигурация клиента hiren.
//!
//! Читает `~/.config/hiren/config.toml` при загрузке.
//! При отсутствии или ошибке парсинга — возвращает значения по умолчанию.

use gtk4 as gtk;
use gtk::gdk::ModifierType;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Публичный тип конфигурации (то, что видят остальные модули)
// ---------------------------------------------------------------------------

/// Бинд клавиши для запуска приложений.
#[derive(Debug, Clone)]
pub struct KeyBinding {
    /// Человекочитаемая строка: "Ctrl+Return", "Shift+A", "Alt+F1"
    #[allow(dead_code)]
    pub key: String,
    /// Префикс команды (None = прямой запуск)
    pub prefix: Option<String>,
    /// Распарсенный модификатор (битовая маска)
    pub modifiers: ModifierType,
    /// Распарсенный keyval (raw u32)
    pub keyval: u32,
}

/// Включение/выключение режимов.
#[derive(Debug, Clone)]
pub struct ModeConfig {
    pub drun: bool,
    pub run: bool,
    pub window: bool,
    pub calc: bool,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            drun: true,
            run: false,
            window: false,
            calc: false,
        }
    }
}

/// Режим клавиатуры для layer-shell окна лаунчера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardModeConfig {
    /// Композитор отдаёт клавиатуру лаунчеру, пока он видим.
    /// Стандарт для лаунчеров: фокус ввода есть сразу при показе,
    /// даже под композиторами, которые не фокусируют OnDemand-слои (driftwm).
    #[default]
    Exclusive,
    /// Только по запросу/клику — композитор сам решает, давать ли фокус.
    /// Под части композиторов лаунчер может не ловить фокус автоматически.
    OnDemand,
}

/// Конфигурация терминала для режима run.
#[derive(Debug, Clone)]
pub struct TerminalConfig {
    /// Команда терминала (например "foot", "alacritty", "kitty").
    pub command: String,
    /// Флаг для запуска команды в терминале (например "-e" для foot/alacritty).
    pub exec_flag: String,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        // Автоопределение терминала
        let (command, exec_flag) = detect_terminal();
        Self {
            command,
            exec_flag,
        }
    }
}

fn detect_terminal() -> (String, String) {
    // Проверяем переменные окружения
    if let Ok(term) = std::env::var("TERMINAL") {
        if !term.is_empty() {
            return (term, "-e".into());
        }
    }
    // Пробуем популярные терминалы
    for (cmd, flag) in [
        ("foot", "-e"),
        ("alacritty", "-e"),
        ("kitty", "--"),
        ("wezterm", "start"),
        ("gnome-terminal", "--"),
        ("konsole", "-e"),
        ("xfce4-terminal", "-e"),
        ("xterm", "-e"),
        ("urxvt", "-e"),
        ("st", "-e"),
    ] {
        if cmd_exists(cmd) {
            return (cmd.into(), flag.into());
        }
    }
    ("foot".into(), "-e".into())
}

/// Проверяет, доступна ли команда в $PATH.
pub fn cmd_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Конфигурация для window mode.
#[derive(Debug, Clone)]
pub struct WindowConfig {
    /// Кастомная команда для получения списка окон.
    /// Если None — автоопределение по WM.
    pub list_command: Option<String>,
    /// Кастомная команда для активации окна.
    /// {id} заменяется на идентификатор окна.
    pub activate_command: Option<String>,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            list_command: None,
            activate_command: None,
        }
    }
}

/// Конфигурация запуска приложений.
#[derive(Debug, Clone)]
pub struct Config {
    /// Бинды запуска. key = модификаторы+клавиша, prefix = что подставить перед Exec.
    /// Проверяются по порядку — первый совпавший используется.
    pub bindings: Vec<KeyBinding>,
    /// Авто-закрытие через N секунд бездействия. 0 = отключено.
    pub auto_close_timeout_secs: u64,
    /// Ширина окна лаунчера в пикселях.
    pub window_width: i32,
    /// Высота окна лаунчера в пикселях.
    pub window_height: i32,
    /// Режимы лаунчера.
    pub modes: ModeConfig,
    /// Настройки терминала (зарезервировано для будущего использования).
    #[allow(dead_code)]
    pub terminal: TerminalConfig,
    /// Настройки window mode.
    pub window: WindowConfig,
    /// Выравнивание текста в поле ввода и результатах: 0.0 = слева, 0.5 = по центру.
    pub text_align: f32,
    /// Вес частоты запусков в гибридной сортировке поиска (0.0–1.0+).
    /// 0.0 = частота не учитывается, 1.0 = честный буст. 0.8 по умолчанию.
    pub freq_weight: f64,
    /// Режим клавиатуры layer-shell окна. По умолчанию `exclusive`.
    pub keyboard_mode: KeyboardModeConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bindings: Vec::new(),
            auto_close_timeout_secs: 8,
            window_width: 620,
            window_height: 360,
            modes: ModeConfig::default(),
            terminal: TerminalConfig::default(),
            window: WindowConfig::default(),
            text_align: 0.0,
            freq_weight: 0.8,
            keyboard_mode: KeyboardModeConfig::Exclusive,
        }
    }
}

// ---------------------------------------------------------------------------
// Внутренние структуры для десериализации TOML
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TomlConfig {
    #[serde(default)]
    ui: UiSection,
    #[serde(default)]
    bindings: Option<Vec<RawBinding>>,
    #[serde(default)]
    mode: Option<TomlModeSection>,
    #[serde(default)]
    terminal: Option<TomlTerminalSection>,
    #[serde(default)]
    window: Option<TomlWindowSection>,
}

/// Сырая версия бинда из TOML (до парсинга keyval).
#[derive(Debug, Deserialize)]
struct RawBinding {
    key: String,
    #[serde(default)]
    prefix: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct UiSection {
    #[serde(default)]
    auto_close_timeout_secs: Option<u64>,
    #[serde(default)]
    width: Option<i32>,
    #[serde(default)]
    height: Option<i32>,
    #[serde(default)]
    text_align: Option<String>,
    #[serde(default)]
    freq_weight: Option<f64>,
    #[serde(default)]
    keyboard_mode: Option<String>,
}

impl Default for UiSection {
    fn default() -> Self {
        Self {
            auto_close_timeout_secs: None,
            width: None,
            height: None,
            text_align: None,
            freq_weight: None,
            keyboard_mode: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TomlModeSection {
    #[serde(default)]
    drun: Option<bool>,
    #[serde(default)]
    run: Option<bool>,
    #[serde(default)]
    window: Option<bool>,
    #[serde(default)]
    calc: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct TomlTerminalSection {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    exec_flag: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TomlWindowSection {
    #[serde(default)]
    list_command: Option<String>,
    #[serde(default)]
    activate_command: Option<String>,
}

// ---------------------------------------------------------------------------
// Парсинг комбинаций клавиш
// ---------------------------------------------------------------------------

/// Парсит строку вида `"Ctrl+Shift+Return"` в `(модификаторы, keyval)`.
///
/// Последний токен (после разделения по `+`) считается именем клавиши,
/// все предыдущие — модификаторами (case-insensitive).
///
/// Возвращает `None`, если имя клавиши не удалось распознать.
fn parse_key_combo(s: &str) -> Option<(ModifierType, u32)> {
    let parts: Vec<&str> = s.split('+').map(|p| p.trim()).collect();
    if parts.is_empty() {
        eprintln!("[hiren-config] WARN: empty key binding string");
        return None;
    }

    let key_name = parts[parts.len() - 1];
    let mod_names = &parts[..parts.len() - 1];

    let mut modifiers = ModifierType::empty();

    for m in mod_names {
        match m.to_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= ModifierType::CONTROL_MASK,
            "shift" => modifiers |= ModifierType::SHIFT_MASK,
            "alt" => modifiers |= ModifierType::ALT_MASK,
            "super" | "win" | "mod4" => modifiers |= ModifierType::SUPER_MASK,
            "meta" => modifiers |= ModifierType::META_MASK,
            other => {
                eprintln!(
                    "[hiren-config] WARN: unknown modifier '{other}' in binding, skipping"
                );
                return None;
            }
        }
    }

    let keyval = match key_name.to_lowercase().as_str() {
        "return" | "enter" => 0xFF0D,
        "kp_enter" | "kpenter" => 0xFF8D,
        "escape" | "esc" => 0xFF1B,
        "tab" => 0xFF09,
        "iso_left_tab" => 0xFE20,
        "space" => 0x0020,
        "backspace" => 0xFF08,
        "delete" | "del" => 0xFFFF,
        "up" => 0xFF52,
        "down" => 0xFF54,
        "left" => 0xFF51,
        "right" => 0xFF53,
        "home" => 0xFF50,
        "end" => 0xFF57,
        "page_up" | "pageup" => 0xFF55,
        "page_down" | "pagedown" => 0xFF56,
        "minus" => 0x002D,
        "equal" => 0x003D,
        "bracketleft" => 0x005B,
        "bracketright" => 0x005D,
        "backslash" => 0x005C,
        "semicolon" => 0x003B,
        "apostrophe" | "quote" => 0x0027,
        "comma" => 0x002C,
        "period" => 0x002E,
        "slash" => 0x002F,
        "grave" => 0x0060,
        name => {
            // F1-F12
            if let Some(num) = name.strip_prefix('f') {
                if let Ok(n) = num.parse::<u32>() {
                    if (1..=12).contains(&n) {
                        return Some((modifiers, 0xFFBD + n));
                    }
                }
            }

            // Одиночная буква/цифра
            if name.len() == 1 {
                let ch = name.chars().next().unwrap();
                if ch.is_ascii_digit() {
                    return Some((modifiers, ch as u32));
                }
                if ch.is_ascii_alphabetic() {
                    return Some((modifiers, ch.to_ascii_uppercase() as u32));
                }
            }

            eprintln!(
                "[hiren-config] WARN: unknown key name '{name}' in binding"
            );
            return None;
        }
    };

    Some((modifiers, keyval))
}

// ---------------------------------------------------------------------------
// Загрузка
// ---------------------------------------------------------------------------

impl Config {
    /// Загружает конфигурацию из `~/.config/hiren/config.toml`.
    ///
    /// Если файла нет или он повреждён — возвращает `Config` со значениями
    /// по умолчанию и печатает предупреждение в stderr.
    pub fn load() -> Self {
        let config_dir = match dirs::config_dir() {
            Some(d) => d.join("hiren"),
            None => {
                eprintln!("[hiren-config] WARN: cannot determine config directory, using defaults");
                return Config::default();
            }
        };

        // Создаём директорию, если её ещё нет (молча)
        if let Err(e) = fs::create_dir_all(&config_dir) {
            eprintln!(
                "[hiren-config] WARN: failed to create config dir {}: {e}",
                config_dir.display()
            );
            return Config::default();
        }

        let config_path: PathBuf = config_dir.join("config.toml");

        // Если файла нет — тихо возвращаем умолчания
        if !config_path.exists() {
            return Config::with_default_bindings();
        }

        // Читаем и парсим
        let raw = match fs::read_to_string(&config_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "[hiren-config] WARN: cannot read {}: {e}",
                    config_path.display()
                );
                return Config::with_default_bindings();
            }
        };

        let toml_cfg: TomlConfig = match toml::from_str(&raw) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "[hiren-config] WARN: failed to parse {}: {e}",
                    config_path.display()
                );
                return Config::with_default_bindings();
            }
        };

        // Парсим бинды
        let bindings: Vec<KeyBinding> = toml_cfg
            .bindings
            .unwrap_or_default()
            .into_iter()
            .filter_map(|raw| {
                let (mods, kv) = parse_key_combo(&raw.key)?;
                // Нормализуем пустую строку в None — иначе даёт лишний пробел в
                // команде запуска (формат "{prefix} {exec}" при Some(""))
                let prefix = raw.prefix.filter(|p| !p.is_empty());
                Some(KeyBinding {
                    key: raw.key,
                    prefix,
                    modifiers: mods,
                    keyval: kv,
                })
            })
            .collect();

        // Если биндов нет — добавляем дефолтные (Enter, Ctrl+Enter)
        let bindings = if bindings.is_empty() {
            vec![
                KeyBinding {
                    key: "Return".into(),
                    prefix: None,
                    modifiers: ModifierType::empty(),
                    keyval: 0xFF0D,
                },
                KeyBinding {
                    key: "Ctrl+Return".into(),
                    prefix: None,
                    modifiers: ModifierType::CONTROL_MASK,
                    keyval: 0xFF0D,
                },
            ]
        } else {
            bindings
        };

        // Парсим режимы
        let modes = toml_cfg.mode.map(|m| ModeConfig {
            drun: m.drun.unwrap_or(true),
            run: m.run.unwrap_or(false),
            window: m.window.unwrap_or(false),
            calc: m.calc.unwrap_or(false),
        }).unwrap_or_else(|| {
            // Если секция [mode] не указана — включаем drun по умолчанию
            ModeConfig::default()
        });

        // Парсим терминал
        let default_terminal = TerminalConfig::default();
        let terminal = toml_cfg.terminal.map(|t| TerminalConfig {
            command: t.command.unwrap_or(default_terminal.command.clone()),
            exec_flag: t.exec_flag.unwrap_or(default_terminal.exec_flag.clone()),
        }).unwrap_or(default_terminal);

        // Парсим window
        let window = toml_cfg.window.map(|w| WindowConfig {
            list_command: w.list_command,
            activate_command: w.activate_command,
        }).unwrap_or_default();

        Config {
            bindings,
            auto_close_timeout_secs: toml_cfg.ui.auto_close_timeout_secs.unwrap_or(8),
            window_width: toml_cfg.ui.width.unwrap_or(620),
            window_height: toml_cfg.ui.height.unwrap_or(360),
            modes,
            terminal,
            window,
            text_align: parse_text_align(&toml_cfg.ui.text_align.unwrap_or_default()),
            freq_weight: toml_cfg.ui.freq_weight.unwrap_or(0.8),
            keyboard_mode: parse_keyboard_mode(toml_cfg.ui.keyboard_mode.as_deref()),
        }
    }

    /// Возвращает Config со значениями по умолчанию + дефолтные бинды.
    fn with_default_bindings() -> Self {
        Config {
            bindings: vec![
                KeyBinding {
                    key: "Return".into(),
                    prefix: None,
                    modifiers: ModifierType::empty(),
                    keyval: 0xFF0D,
                },
                KeyBinding {
                    key: "Ctrl+Return".into(),
                    prefix: None,
                    modifiers: ModifierType::CONTROL_MASK,
                    keyval: 0xFF0D,
                },
            ],
            auto_close_timeout_secs: 8,
            window_width: 620,
            window_height: 360,
            modes: ModeConfig::default(),
            terminal: TerminalConfig::default(),
            window: WindowConfig::default(),
            text_align: 0.0,
            freq_weight: 0.8,
            keyboard_mode: KeyboardModeConfig::Exclusive,
        }
    }

    /// Получить список активных режимов (те, что включены в конфиге).
    pub fn active_modes(&self) -> Vec<hiren_shared::AppMode> {
        let mut modes = Vec::new();
        if self.modes.drun { modes.push(hiren_shared::AppMode::Drun); }
        if self.modes.run { modes.push(hiren_shared::AppMode::Run); }
        if self.modes.window { modes.push(hiren_shared::AppMode::Window); }
        if self.modes.calc { modes.push(hiren_shared::AppMode::Calc); }
        modes
    }
}

// ---------------------------------------------------------------------------
// Хелперы
// ---------------------------------------------------------------------------

/// Парсит строку выравнивания текста в f32: "left"→0.0, "center"→0.5, "right"→1.0.
/// По умолчанию "left".
fn parse_text_align(s: &str) -> f32 {
    match s.to_lowercase().trim() {
        "center" | "middle" => 0.5,
        "right" => 1.0,
        _ => 0.0, // "left" и всё остальное
    }
}

/// Парсит режим клавиатуры layer-shell из строки конфига.
///
/// Допустимые значения: `"exclusive"` (по умолчанию) и `"on_demand"`.
/// Неизвестное/пустое значение → `Exclusive`.
fn parse_keyboard_mode(s: Option<&str>) -> KeyboardModeConfig {
    match s {
        Some(v) => match v.to_lowercase().as_str() {
            "on_demand" | "ondemand" => KeyboardModeConfig::OnDemand,
            "exclusive" => KeyboardModeConfig::Exclusive,
            other => {
                eprintln!(
                    "[hiren-config] WARN: unknown keyboard_mode '{other}', using exclusive"
                );
                KeyboardModeConfig::Exclusive
            }
        },
        None => KeyboardModeConfig::Exclusive,
    }
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_key_combo ---

    #[test]
    fn parse_plain_return() {
        let (mods, kv) = parse_key_combo("Return").unwrap();
        assert_eq!(mods, ModifierType::empty());
        assert_eq!(kv, 0xFF0D);
    }

    #[test]
    fn parse_plain_enter() {
        let (mods, kv) = parse_key_combo("Enter").unwrap();
        assert_eq!(mods, ModifierType::empty());
        assert_eq!(kv, 0xFF0D);
    }

    #[test]
    fn parse_ctrl_return() {
        let (mods, kv) = parse_key_combo("Ctrl+Return").unwrap();
        assert_eq!(mods, ModifierType::CONTROL_MASK);
        assert_eq!(kv, 0xFF0D);
    }

    #[test]
    fn parse_shift_a() {
        let (mods, kv) = parse_key_combo("Shift+A").unwrap();
        assert_eq!(mods, ModifierType::SHIFT_MASK);
        assert_eq!(kv, 0x0041); // 'A'
    }

    #[test]
    fn parse_lowercase_a() {
        let (mods, kv) = parse_key_combo("a").unwrap();
        assert_eq!(mods, ModifierType::empty());
        assert_eq!(kv, 0x0041); // uppercase keyval
    }

    #[test]
    fn parse_digit() {
        let (mods, kv) = parse_key_combo("Ctrl+Alt+3").unwrap();
        assert_eq!(
            mods,
            ModifierType::CONTROL_MASK | ModifierType::ALT_MASK
        );
        assert_eq!(kv, 0x0033); // '3'
    }

    #[test]
    fn parse_super_f1() {
        let (mods, kv) = parse_key_combo("Super+F1").unwrap();
        assert_eq!(mods, ModifierType::SUPER_MASK);
        assert_eq!(kv, 0xFFBE);
    }

    #[test]
    fn parse_f12() {
        let (mods, kv) = parse_key_combo("F12").unwrap();
        assert_eq!(mods, ModifierType::empty());
        assert_eq!(kv, 0xFFC9);
    }

    #[test]
    fn parse_escape() {
        let (mods, kv) = parse_key_combo("Escape").unwrap();
        assert_eq!(mods, ModifierType::empty());
        assert_eq!(kv, 0xFF1B);
    }

    #[test]
    fn parse_esc_short() {
        let (mods, kv) = parse_key_combo("Esc").unwrap();
        assert_eq!(mods, ModifierType::empty());
        assert_eq!(kv, 0xFF1B);
    }

    #[test]
    fn parse_arrows() {
        let (_, kv) = parse_key_combo("Up").unwrap();
        assert_eq!(kv, 0xFF52);
        let (_, kv) = parse_key_combo("Down").unwrap();
        assert_eq!(kv, 0xFF54);
        let (_, kv) = parse_key_combo("Left").unwrap();
        assert_eq!(kv, 0xFF51);
        let (_, kv) = parse_key_combo("Right").unwrap();
        assert_eq!(kv, 0xFF53);
    }

    #[test]
    fn parse_home_end() {
        let (_, kv) = parse_key_combo("Home").unwrap();
        assert_eq!(kv, 0xFF50);
        let (_, kv) = parse_key_combo("End").unwrap();
        assert_eq!(kv, 0xFF57);
    }

    #[test]
    fn parse_page_up_down() {
        let (_, kv) = parse_key_combo("Page_Up").unwrap();
        assert_eq!(kv, 0xFF55);
        let (_, kv) = parse_key_combo("PageUp").unwrap();
        assert_eq!(kv, 0xFF55);
        let (_, kv) = parse_key_combo("Page_Down").unwrap();
        assert_eq!(kv, 0xFF56);
        let (_, kv) = parse_key_combo("PageDown").unwrap();
        assert_eq!(kv, 0xFF56);
    }

    #[test]
    fn parse_special_chars() {
        assert_eq!(parse_key_combo("minus").unwrap().1, 0x002D);
        assert_eq!(parse_key_combo("equal").unwrap().1, 0x003D);
        assert_eq!(parse_key_combo("bracketleft").unwrap().1, 0x005B);
        assert_eq!(parse_key_combo("bracketright").unwrap().1, 0x005D);
        assert_eq!(parse_key_combo("backslash").unwrap().1, 0x005C);
        assert_eq!(parse_key_combo("semicolon").unwrap().1, 0x003B);
        assert_eq!(parse_key_combo("apostrophe").unwrap().1, 0x0027);
        assert_eq!(parse_key_combo("comma").unwrap().1, 0x002C);
        assert_eq!(parse_key_combo("period").unwrap().1, 0x002E);
        assert_eq!(parse_key_combo("slash").unwrap().1, 0x002F);
        assert_eq!(parse_key_combo("grave").unwrap().1, 0x0060);
    }

    #[test]
    fn parse_kp_enter() {
        let (_, kv) = parse_key_combo("KP_Enter").unwrap();
        assert_eq!(kv, 0xFF8D);
        let (_, kv) = parse_key_combo("KPEnter").unwrap();
        assert_eq!(kv, 0xFF8D);
    }

    #[test]
    fn parse_case_insensitive_modifiers() {
        let (mods, _) = parse_key_combo("ctrl+SHIFT+Alt+Return").unwrap();
        assert_eq!(
            mods,
            ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK | ModifierType::ALT_MASK
        );
    }

    #[test]
    fn parse_unknown_key_returns_none() {
        assert!(parse_key_combo("Ctrl+NonexistentKey").is_none());
    }

    #[test]
    fn parse_unknown_modifier_returns_none() {
        assert!(parse_key_combo("Hyper+Return").is_none());
    }

    #[test]
    fn parse_empty_string_returns_none() {
        assert!(parse_key_combo("").is_none());
    }

    #[test]
    fn parse_win_and_mod4_aliases() {
        let (mods1, _) = parse_key_combo("Win+Return").unwrap();
        assert_eq!(mods1, ModifierType::SUPER_MASK);
        let (mods2, _) = parse_key_combo("Mod4+Return").unwrap();
        assert_eq!(mods2, ModifierType::SUPER_MASK);
    }

    #[test]
    fn parse_meta() {
        let (mods, _) = parse_key_combo("Meta+Return").unwrap();
        assert_eq!(mods, ModifierType::META_MASK);
    }

    #[test]
    fn parse_control_alias() {
        let (mods, _) = parse_key_combo("Control+Return").unwrap();
        assert_eq!(mods, ModifierType::CONTROL_MASK);
    }

    // --- TomlConfig десериализация ---

    #[test]
    fn parse_valid_toml_with_bindings() {
        let toml_str = r#"
[[bindings]]
key = "Return"

[[bindings]]
key = "Ctrl+Return"
prefix = "proxychains"

[[bindings]]
key = "Shift+Return"
prefix = "foot --"
"#;
        let parsed: TomlConfig = toml::from_str(toml_str).unwrap();
        let bindings = parsed.bindings.unwrap();
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].key, "Return");
        assert!(bindings[0].prefix.is_none());
        assert_eq!(bindings[1].key, "Ctrl+Return");
        assert_eq!(bindings[1].prefix.as_deref(), Some("proxychains"));
        assert_eq!(bindings[2].key, "Shift+Return");
        assert_eq!(bindings[2].prefix.as_deref(), Some("foot --"));
    }

    #[test]
    fn parse_empty_toml_uses_defaults() {
        let parsed: TomlConfig = toml::from_str("").unwrap();
        assert!(parsed.bindings.is_none()); // None → default bindings applied later
    }

    #[test]
    fn parse_bindings_with_empty_prefix() {
        // TOML парсит пустую строку как Some("").
        // Нормализация в None происходит позже, внутри Config::load().
        let toml_str = r#"
[[bindings]]
key = "Return"
prefix = ""
"#;
        let parsed: TomlConfig = toml::from_str(toml_str).unwrap();
        let bindings = parsed.bindings.unwrap();
        assert_eq!(bindings[0].prefix.as_deref(), Some(""));
    }

    #[test]
    fn parse_mode_section() {
        let toml_str = r#"
[mode]
drun = true
run = true
window = false
calc = true

[terminal]
command = "alacritty"
exec_flag = "-e"
"#;
        let parsed: TomlConfig = toml::from_str(toml_str).unwrap();
        let modes = parsed.mode.unwrap();
        assert_eq!(modes.drun, Some(true));
        assert_eq!(modes.run, Some(true));
        assert_eq!(modes.window, Some(false));
        assert_eq!(modes.calc, Some(true));

        let terminal = parsed.terminal.unwrap();
        assert_eq!(terminal.command.as_deref(), Some("alacritty"));
        assert_eq!(terminal.exec_flag.as_deref(), Some("-e"));
    }
}
