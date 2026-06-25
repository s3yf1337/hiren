//! Модуль режимов лаунчера.
//!
//! Каждый режим реализует trait `SearchMode` — источник данных и логику выполнения.
//! - `Drun` — поиск .desktop-файлов через IPC-демон
//! - `Run` — поиск исполняемых файлов в $PATH
//! - `Window` — переключение окон (sway/hyprland/wmctrl)
//! - `Calc` — вычисление математических выражений

pub mod calc;
pub mod drun;
pub mod run;
pub mod window;

use crate::config::{self, Config, WindowConfig};
use hiren_shared::AppEntry;

/// Результат поиска в режиме.
pub enum SearchResult {
    /// Обычные записи для отображения в списке.
    Entries(Vec<AppEntry>),
    /// Ошибка с сообщением.
    Error(String),
}

/// Трейт режима поиска.
///
/// Режим отвечает за:
/// 1. Получение данных для заданного запроса
/// 2. Запуск/активацию выбранной записи
pub trait SearchMode {
    /// Выполнить поиск по запросу и вернуть результаты.
    fn search(&self, query: &str) -> SearchResult;

    /// Запустить выбранную запись.
    fn execute(&self, entry: &AppEntry, config: &Config);

    /// Инициализировать режим (загрузить кэш, открыть соединения и т.д.).
    fn init(&mut self, _config: &Config) {}
}

/// Создать стандартную реализацию для указанного режима.
pub fn create_mode(mode: hiren_shared::AppMode) -> Box<dyn SearchMode> {
    match mode {
        hiren_shared::AppMode::Drun => Box::new(drun::DrunMode::new()),
        hiren_shared::AppMode::Run => Box::new(run::RunMode::new()),
        hiren_shared::AppMode::Window => Box::new(window::WindowMode::new()),
        hiren_shared::AppMode::Calc => Box::new(calc::CalcMode::new()),
    }
}

/// Запустить команду как detached процесс (через `sh -c`).
pub fn exec_detached(cmd: &str) {
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {}
        Err(e) => eprintln!("[hiren-client] Failed to launch: {e}"),
    }
}

/// Получить список окон через автоопределение WM.
///
/// Возвращает команду для листинга и команду для активации (с {id} placeholder).
fn detect_window_commands(config: &WindowConfig) -> (String, String) {
    // Если заданы кастомные команды — используем их
    if let (Some(list), Some(activate)) = (&config.list_command, &config.activate_command) {
        return (list.clone(), activate.clone());
    }

    // Проверяем Sway
    if std::env::var("SWAYSOCK").is_ok() {
        return (
            "swaymsg -t get_tree".into(),
            "swaymsg \"[con_id={id}] focus\"".into(),
        );
    }

    // Проверяем Hyprland
    if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        return (
            "hyprctl clients -j".into(),
            "hyprctl dispatch focuswindow address:{id}".into(),
        );
    }

    // X11 через wmctrl
    if config::cmd_exists("wmctrl") {
        return (
            "wmctrl -l".into(),
            "wmctrl -i -a {id}".into(),
        );
    }

    // Fallback: пробуем wmctrl в любом случае
    ("wmctrl -l".into(), "wmctrl -i -a {id}".into())
}
