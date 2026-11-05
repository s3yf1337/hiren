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

use crate::config::{self, LauncherConfig as Config, TerminalConfig, WindowConfig};
use hiren_shared::AppEntry;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Запись считается «упавшей при запуске», если она завершилась с ошибкой
/// быстрее этого порога. Долгий запуск, закончившийся позже, — обычная работа
/// приложения, и лаунчер уже закрыт.
pub const QUICK_EXIT_MS: u64 = 2_000;

/// Ошибки запуска, о которых нужно сообщить в UI (`LauncherState.error`).
///
/// Заполняется фоновым reaper-потоком, вычитывается `AppCore` в цикле кадра:
/// `Rc<ObservableState>` нельзя передать в поток (не `Send`), поэтому ошибки
/// передаются через `Arc<Mutex<_>>`.
pub type LaunchErrors = Arc<Mutex<Vec<String>>>;

pub fn new_launch_errors() -> LaunchErrors {
    Arc::new(Mutex::new(Vec::new()))
}

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

    /// Запустить выбранную запись. Ошибка возвращается для показа в UI.
    fn execute(
        &self,
        entry: &AppEntry,
        config: &Config,
        errors: &LaunchErrors,
    ) -> Result<(), String>;

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

// ---------------------------------------------------------------------------
// Exec parsing and launching
// ---------------------------------------------------------------------------

/// Разобрать команду (`Exec=` из .desktop, пользовательский префикс или
/// команду управления окнами) в argv без shell.
///
/// Правила как у POSIX-шелла (но без подстановок — freedesktop `Exec` не shell):
/// - пробелы/табы разделяют аргументы;
/// - `'...'` — литерал целиком;
/// - `"..."` — литерал, внутри работают escapes `\"`, `\\` и `\$`;
/// - `\` вне кавычек экранирует следующий символ (в т.ч. пробел).
///
/// Field-коды (`%f`, `%U`, …) удалены демоном; если они всё же встретятся,
/// они остаются обычным текстом аргумента — shell-интерпретации не происходит.
pub fn parse_exec_argv(command: &str) -> Result<Vec<String>, String> {
    let mut argv: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut chars = command.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                if has_token {
                    argv.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            '\'' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(e @ ('"' | '\\' | '$')) => current.push(e),
                            Some(other) => {
                                // Как shell: вне спецсимволов backslash остаётся.
                                current.push('\\');
                                current.push(other);
                            }
                            None => return Err("unterminated double quote".into()),
                        },
                        Some(c) => current.push(c),
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                has_token = true;
                match chars.next() {
                    Some(e) => current.push(e),
                    None => return Err("trailing backslash".into()),
                }
            }
            c => {
                has_token = true;
                current.push(c);
            }
        }
    }
    if has_token {
        argv.push(current);
    }
    if argv.is_empty() {
        return Err("empty command".into());
    }
    Ok(argv)
}

/// Как запускать запись после разворачивания всех префиксов.
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchPlan {
    /// Прямой запуск argv (без shell).
    Argv(Vec<String>),
    /// Пользовательский shell-префикс из конфига + argv записи, переданный
    /// позиционными аргументами (`sh -c '<prefix> "$@"' sh …`) — префикс может
    /// содержать shell-синтаксис, но сама команда приложения не переразбирается.
    ShellPrefix { prefix: String, argv: Vec<String> },
}

/// Построить argv терминальной обёртки: `command` (может содержать аргументы)
/// + `exec_flag` (может быть пустым, напр. gnome-terminal `--`) + argv записи.
fn terminal_argv(terminal: &TerminalConfig) -> Result<Vec<String>, String> {
    let mut argv = parse_exec_argv(&terminal.command)
        .map_err(|e| format!("invalid [terminal] command `{}`: {e}", terminal.command))?;
    if terminal.exec_flag.trim().is_empty() {
        return Ok(argv);
    }
    let flag = parse_exec_argv(&terminal.exec_flag)
        .map_err(|e| format!("invalid [terminal] exec_flag `{}`: {e}", terminal.exec_flag))?;
    argv.extend(flag);
    Ok(argv)
}

/// Построить план запуска записи.
///
/// Приоритет:
/// 1. явный пользовательский префикс из `[[bindings]] prefix` — shell-обёртка;
/// 2. терминал (`Ctrl+Enter` по умолчанию или `AppEntry.terminal` из daemon);
/// 3. прямой запуск argv (`Exec=` больше не прогоняется через `sh -c`).
pub fn plan_launch(
    exec: &str,
    terminal: Option<&TerminalConfig>,
    user_prefix: Option<&str>,
) -> Result<LaunchPlan, String> {
    let argv = parse_exec_argv(exec).map_err(|e| format!("invalid exec `{exec}`: {e}"))?;
    if let Some(prefix) = user_prefix.map(str::trim).filter(|p| !p.is_empty()) {
        return Ok(LaunchPlan::ShellPrefix { prefix: prefix.to_string(), argv });
    }
    if let Some(terminal) = terminal {
        let mut wrap = terminal_argv(terminal)?;
        wrap.extend(argv);
        return Ok(LaunchPlan::Argv(wrap));
    }
    Ok(LaunchPlan::Argv(argv))
}

/// Запросила ли запись запуск в терминале (`.desktop` `Terminal=true`).
///
/// Поле `AppEntry::terminal` добавляет ветка daemon/security; чтобы клиент
/// собирался и со старым `hiren-shared`, и после её мержа, значение читается
/// через serde (`#[serde(default)]` → отсутствующее поле означает `false`).
/// JSON-roundtrip происходит только при активации записи, не в поиске.
pub fn entry_requests_terminal(entry: &AppEntry) -> bool {
    serde_json::to_value(entry)
        .ok()
        .and_then(|value| value.get("terminal").and_then(|t| t.as_bool()))
        .unwrap_or(false)
}

/// Запустить argv напрямую, без shell. stdout/stderr → /dev/null, stdin → close.
fn spawn_argv(argv: &[String]) -> Result<Child, String> {
    let Some((program, args)) = argv.split_first() else {
        return Err("empty command".into());
    };
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn `{program}`: {e}"))
}

/// Shell-обёртка для пользовательского префикса. Сама команда приложения
/// передаётся как позиционные параметры и shell-интерпретации не подвергается.
fn spawn_shell_prefix(prefix: &str, argv: &[String]) -> Result<Child, String> {
    Command::new("sh")
        .arg("-c")
        .arg(format!("{prefix} \"$@\""))
        .arg("hiren-launch")
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn shell prefix `{prefix}`: {e}"))
}

/// Запустить процесс по плану. Возвращает `Child` для reaper-потока.
pub fn spawn_plan(plan: &LaunchPlan) -> Result<Child, String> {
    match plan {
        LaunchPlan::Argv(argv) => spawn_argv(argv),
        LaunchPlan::ShellPrefix { prefix, argv } => spawn_shell_prefix(prefix, argv),
    }
}

/// Запустить запись и повесить на неё reaper: быстрый ненулевой exit code
/// попадает в `errors`, откуда UI-поток забирает сообщение в `LauncherState.error`.
pub fn spawn_monitored(plan: &LaunchPlan, label: &str, errors: &LaunchErrors) -> Result<(), String> {
    let started = Instant::now();
    let child = spawn_plan(plan)?;
    let errors = Arc::clone(errors);
    let label = label.to_string();
    std::thread::spawn(move || reap_launch(child, label, started, errors));
    Ok(())
}

fn reap_launch(mut child: Child, label: String, started: Instant, errors: LaunchErrors) {
    let status = child.wait();
    let elapsed = started.elapsed();
    let message = match status {
        Ok(status) if !status.success() => {
            if elapsed <= Duration::from_millis(QUICK_EXIT_MS) {
                Some(format!("launch failed: {label} exited with {status}"))
            } else {
                log::debug!("hiren: {label} exited with {status} after {elapsed:?}");
                None
            }
        }
        Ok(_) => None,
        Err(e) => Some(format!("launch failed: cannot wait for {label}: {e}")),
    };
    if let Some(message) = message {
        log::warn!("hiren: {message}");
        if let Ok(mut queue) = errors.lock() {
            queue.push(message);
        }
    }
}

/// Запустить процесс без мониторинга ошибок в UI (команды активации окна).
pub fn spawn_quiet(plan: &LaunchPlan) -> Result<(), String> {
    let child = spawn_plan(plan)?;
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// Устаревший helper для внутренних команд, которым действительно нужен shell
/// (например, пайп в буфер обмена). Новый код запускает argv напрямую.
pub fn exec_detached(cmd: &str) {
    match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("[hiren-client] Failed to launch: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Clipboard (calc mode)
// ---------------------------------------------------------------------------

/// Запустить `argv`, подав `input` на stdin (для wl-copy/xclip), и дождаться
/// завершения в фоновом потоке. Ошибки — в `errors`.
pub fn spawn_with_stdin(argv: &[String], input: &str, errors: &LaunchErrors) -> Result<(), String> {
    let Some((program, args)) = argv.split_first() else {
        return Err("empty clipboard command".into());
    };
    let program = program.clone();
    let mut child = Command::new(&program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn `{program}`: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        if let Err(e) = stdin.write_all(input.as_bytes()) {
            log::warn!("hiren: cannot write clipboard input: {e}");
        }
        // stdin закрывается вместе с drop → команда видит EOF.
    }
    let errors = Arc::clone(errors);
    std::thread::spawn(move || {
        let status = child.wait();
        if let Ok(status) = status {
            if !status.success() {
                let msg = format!("clipboard tool `{program}` exited with {status}");
                log::warn!("hiren: {msg}");
                if let Ok(mut queue) = errors.lock() {
                    queue.push(msg);
                }
            }
        }
    });
    Ok(())
}

/// Команда копирования в буфер обмена: wl-copy на Wayland, xclip на X11.
pub fn clipboard_command() -> Option<Vec<String>> {
    for (bin, flag) in [("wl-copy", None), ("xclip", Some("-selection"))] {
        if config::cmd_exists(bin) {
            return Some(match flag {
                Some(flag) => vec![bin.to_string(), flag.to_string(), "clipboard".to_string()],
                None => vec![bin.to_string()],
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Window mode: команды и автоопределение WM
// ---------------------------------------------------------------------------

/// Обнаруженный оконный менеджер.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmType {
    Sway,
    Hyprland,
    Wmctrl,
    /// Команда листинга неизвестна — парсим вывод эвристически.
    Auto,
}

/// Команды листинга/активации окон и способ разбора их вывода.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowCommands {
    pub list: String,
    pub activate: String,
    pub wm: WmType,
}

/// Определить WM по имени бинарника команды листинга.
pub fn infer_wm(command: &str) -> Option<WmType> {
    let argv = parse_exec_argv(command).ok()?;
    let bin = argv.first()?;
    let name = std::path::Path::new(bin).file_name()?.to_string_lossy();
    match name.as_ref() {
        "swaymsg" => Some(WmType::Sway),
        "hyprctl" => Some(WmType::Hyprland),
        "wmctrl" => Some(WmType::Wmctrl),
        _ => None,
    }
}

fn wm_defaults(wm: WmType) -> (&'static str, &'static str) {
    match wm {
        WmType::Sway => ("swaymsg -t get_tree", "swaymsg [con_id={id}] focus"),
        WmType::Hyprland => ("hyprctl clients -j", "hyprctl dispatch focuswindow address:{id}"),
        _ => ("wmctrl -l", "wmctrl -i -a {id}"),
    }
}

fn detect_wm_from_env() -> WmType {
    if std::env::var("SWAYSOCK").is_ok() {
        WmType::Sway
    } else if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        WmType::Hyprland
    } else {
        WmType::Wmctrl
    }
}

/// Команды оконного режима с учётом пользовательских `[window]` настроек.
///
/// Кастомный `list_command` применяется отдельно от `activate_command` (раньше
/// частичная настройка молча игнорировалась), тип WM выводится из команды
/// листинга; для неизвестной команды — `Auto` (вывод парсится эвристически).
pub fn detect_window_commands(config: &WindowConfig) -> WindowCommands {
    let env_wm = detect_wm_from_env();
    let (env_list, env_activate) = wm_defaults(env_wm);

    let list = config.list_command.clone().unwrap_or_else(|| env_list.to_string());
    let inferred = infer_wm(&list);
    let wm = match (&config.list_command, inferred) {
        (Some(_), Some(wm)) => wm,
        (Some(_), None) => WmType::Auto,
        (None, _) => env_wm,
    };
    let activate = match &config.activate_command {
        Some(activate) => activate.clone(),
        None => match inferred {
            Some(wm) => wm_defaults(wm).1.to_string(),
            None if wm == WmType::Auto => {
                eprintln!(
                    "[hiren-client] WARN: cannot infer WM from list_command `{list}`; \
                     activate falls back to wmctrl — set [window] activate_command"
                );
                wm_defaults(WmType::Wmctrl).1.to_string()
            }
            None => env_activate.to_string(),
        },
    };
    WindowCommands { list, activate, wm }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TerminalConfig;

    fn terminal(command: &str, exec_flag: &str) -> TerminalConfig {
        TerminalConfig { command: command.into(), exec_flag: exec_flag.into() }
    }

    // -----------------------------------------------------------------
    // parse_exec_argv
    // -----------------------------------------------------------------

    #[test]
    fn parse_simple_command() {
        assert_eq!(parse_exec_argv("firefox").unwrap(), vec!["firefox"]);
        assert_eq!(
            parse_exec_argv("foot --class launcher").unwrap(),
            vec!["foot", "--class", "launcher"]
        );
    }

    #[test]
    fn parse_collapses_whitespace() {
        assert_eq!(
            parse_exec_argv("  env   FOO=1\tapp  ").unwrap(),
            vec!["env", "FOO=1", "app"]
        );
    }

    #[test]
    fn parse_double_quotes() {
        assert_eq!(
            parse_exec_argv(r#"firefox --new-window "https://example.com/a b""#).unwrap(),
            vec!["firefox", "--new-window", "https://example.com/a b"]
        );
    }

    #[test]
    fn parse_single_quotes_are_literal() {
        assert_eq!(
            parse_exec_argv(r#"printf '%s%s' '$HOME' "x""#).unwrap(),
            vec!["printf", "%s%s", "$HOME", "x"]
        );
    }

    #[test]
    fn parse_backslash_escapes() {
        assert_eq!(
            parse_exec_argv(r"my\ app --flag=a\ b").unwrap(),
            vec!["my app", "--flag=a b"]
        );
        assert_eq!(parse_exec_argv(r#""a\"b" "c\\d""#).unwrap(), vec!["a\"b", "c\\d"]);
    }

    #[test]
    fn parse_empty_segments_preserved() {
        assert_eq!(parse_exec_argv(r#"app "" 'ok'"#).unwrap(), vec!["app", "", "ok"]);
    }

    #[test]
    fn parse_rejects_empty_and_unterminated() {
        assert!(parse_exec_argv("").is_err());
        assert!(parse_exec_argv("   ").is_err());
        assert!(parse_exec_argv(r#"app "oops"#).is_err());
        assert!(parse_exec_argv("app 'oops").is_err());
        assert!(parse_exec_argv("app \\").is_err());
    }

    // -----------------------------------------------------------------
    // plan_launch
    // -----------------------------------------------------------------

    #[test]
    fn plan_direct_exec_is_not_shell_interpreted() {
        let plan = plan_launch("app --title '$HOME; rm -rf /'", None, None).unwrap();
        assert_eq!(
            plan,
            LaunchPlan::Argv(vec!["app".into(), "--title".into(), "$HOME; rm -rf /".into()])
        );
    }

    #[test]
    fn plan_terminal_wraps_with_exec_flag() {
        let term = terminal("foot", "-e");
        let plan = plan_launch("htop -d 5", Some(&term), None).unwrap();
        assert_eq!(
            plan,
            LaunchPlan::Argv(vec![
                "foot".into(),
                "-e".into(),
                "htop".into(),
                "-d".into(),
                "5".into()
            ])
        );
    }

    #[test]
    fn plan_terminal_supports_command_with_args_and_empty_flag() {
        let with_args = terminal("gnome-terminal --wait --", "");
        let plan = plan_launch("htop", Some(&with_args), None).unwrap();
        assert_eq!(
            plan,
            LaunchPlan::Argv(vec![
                "gnome-terminal".into(),
                "--wait".into(),
                "--".into(),
                "htop".into()
            ])
        );

        let empty_flag = terminal("wezterm start", "");
        let plan = plan_launch("htop", Some(&empty_flag), None).unwrap();
        assert_eq!(
            plan,
            LaunchPlan::Argv(vec!["wezterm".into(), "start".into(), "htop".into()])
        );
    }

    #[test]
    fn plan_terminal_rejects_invalid_configuration() {
        assert!(plan_launch("htop", Some(&terminal("", "-e")), None).is_err());
        assert!(plan_launch("htop", Some(&terminal("foot", "\"unterminated")), None).is_err());
    }

    #[test]
    fn plan_user_prefix_is_shell_wrapped_but_exec_is_not_parsed_by_shell() {
        let plan = plan_launch("app --name 'x y'", None, Some("env FOO=1")).unwrap();
        match plan {
            LaunchPlan::ShellPrefix { prefix, argv } => {
                assert_eq!(prefix, "env FOO=1");
                // Кавычки разобраны один раз парсером, а не шеллом.
                assert_eq!(argv, vec!["app", "--name", "x y"]);
            }
            other => panic!("unexpected plan: {other:?}"),
        }
    }

    #[test]
    fn plan_user_prefix_wins_over_terminal() {
        let term = terminal("foot", "-e");
        let plan = plan_launch("htop", Some(&term), Some("env A=B")).unwrap();
        assert!(matches!(plan, LaunchPlan::ShellPrefix { .. }));
    }

    #[test]
    fn plan_ignores_blank_user_prefix() {
        let plan = plan_launch("htop", None, Some("  ")).unwrap();
        assert_eq!(plan, LaunchPlan::Argv(vec!["htop".into()]));
    }

    // -----------------------------------------------------------------
    // Window commands / WM detection
    // -----------------------------------------------------------------

    fn window_config(list: Option<&str>, activate: Option<&str>) -> WindowConfig {
        WindowConfig {
            list_command: list.map(str::to_string),
            activate_command: activate.map(str::to_string),
        }
    }

    #[test]
    fn infer_wm_from_binary_name() {
        assert_eq!(infer_wm("swaymsg -t get_tree"), Some(WmType::Sway));
        assert_eq!(infer_wm("/usr/bin/hyprctl clients -j"), Some(WmType::Hyprland));
        assert_eq!(infer_wm("wmctrl -l"), Some(WmType::Wmctrl));
        assert_eq!(infer_wm("my-window-script --list"), None);
    }

    #[test]
    fn detect_custom_partial_list_is_used() {
        // Раньше кастомный list_command без activate молча игнорировался.
        let cmds = detect_window_commands(&window_config(Some("my-window-script --list"), None));
        assert_eq!(cmds.list, "my-window-script --list");
        assert_eq!(cmds.wm, WmType::Auto);
        assert!(!cmds.activate.is_empty());
    }

    #[test]
    fn detect_custom_activate_with_detected_list() {
        let cmds = detect_window_commands(&window_config(None, Some("my-script focus {id}")));
        assert_eq!(cmds.activate, "my-script focus {id}");
        // Без кастомного листинга WM берётся из окружения/дефолта.
        assert!(matches!(cmds.wm, WmType::Sway | WmType::Hyprland | WmType::Wmctrl));
    }

    #[test]
    fn detect_custom_sway_list_infers_sway_defaults() {
        let cmds = detect_window_commands(&window_config(Some("swaymsg -t get_tree -r"), None));
        assert_eq!(cmds.wm, WmType::Sway);
        assert_eq!(cmds.activate, "swaymsg [con_id={id}] focus");
    }

    #[test]
    fn detect_hyprland_list_infers_hypr_defaults() {
        let cmds = detect_window_commands(&window_config(Some("hyprctl clients -j"), None));
        assert_eq!(cmds.wm, WmType::Hyprland);
        assert_eq!(cmds.activate, "hyprctl dispatch focuswindow address:{id}");
    }

    // -----------------------------------------------------------------
    // Default commands are valid argv templates
    // -----------------------------------------------------------------

    #[test]
    fn default_window_commands_parse_as_argv() {
        for wm in [WmType::Sway, WmType::Hyprland, WmType::Wmctrl] {
            let (list, activate) = wm_defaults(wm);
            assert!(parse_exec_argv(list).is_ok(), "list: {list}");
            let argv = parse_exec_argv(activate).unwrap();
            assert!(argv.iter().any(|a| a.contains("{id}")), "activate: {activate}");
        }
    }

    #[test]
    fn clipboard_command_shape_is_argv_safe() {
        // Не зависит от установленных тулов: проверяем только, что при наличии
        // команда — это готовый argv (без shell-метасимволов).
        if let Some(argv) = clipboard_command() {
            assert!(!argv.is_empty());
            for arg in &argv {
                assert!(!arg.contains('|'), "shell pipe leaked into argv: {arg}");
            }
        }
    }

    // -----------------------------------------------------------------
    // AppEntry.terminal (daemon contract; field may not exist yet)
    // -----------------------------------------------------------------

    #[test]
    fn entry_terminal_defaults_to_false_without_field() {
        let entry = AppEntry::drun(
            "x".into(),
            "X".into(),
            "x".into(),
            None,
            String::new(),
        );
        assert!(!entry_requests_terminal(&entry));
    }

    #[test]
    fn entry_terminal_reads_true_and_false() {
        let base = AppEntry::drun("x".into(), "X".into(), "x".into(), None, String::new());

        let mut object = serde_json::to_value(&base).unwrap().as_object().unwrap().clone();
        object.insert("terminal".into(), serde_json::json!(true));
        let wants_terminal: AppEntry =
            serde_json::from_value(serde_json::Value::Object(object)).unwrap();
        assert!(entry_requests_terminal(&wants_terminal));

        let mut object = serde_json::to_value(&base).unwrap().as_object().unwrap().clone();
        object.insert("terminal".into(), serde_json::json!(false));
        let no_terminal: AppEntry =
            serde_json::from_value(serde_json::Value::Object(object)).unwrap();
        assert!(!entry_requests_terminal(&no_terminal));
    }

    // -----------------------------------------------------------------
    // Launch monitoring
    // -----------------------------------------------------------------

    #[test]
    fn quick_nonzero_exit_is_reported_as_launch_failure() {
        let errors = new_launch_errors();
        // /bin/false есть в любой Linux-системе: мгновенный exit 1.
        let plan = LaunchPlan::Argv(vec!["/bin/false".into()]);
        spawn_monitored(&plan, "false-test", &errors).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let message = errors.lock().unwrap().first().cloned();
            if let Some(message) = message {
                assert!(message.contains("launch failed"), "message: {message}");
                return;
            }
            assert!(Instant::now() < deadline, "reaper did not report the failure");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn shell_prefix_plan_spawns_through_sh() {
        let errors = new_launch_errors();
        let plan = LaunchPlan::ShellPrefix {
            prefix: "env HIREN_TEST=1".into(),
            argv: vec!["/bin/true".into()],
        };
        spawn_monitored(&plan, "true-test", &errors).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert!(errors.lock().unwrap().is_empty());
    }
}
