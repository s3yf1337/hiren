//! Режим window — переключение между окнами.
//!
//! Поддерживает автоопределение WM:
//! - Sway (через swaymsg)
//! - Hyprland (через hyprctl)
//! - X11 (через wmctrl)
//! - Кастомные команды из конфига (тип WM выводится из команды листинга,
//!   неизвестный вывод парсится эвристически)

use super::{
    detect_window_commands, parse_exec_argv, spawn_monitored, LaunchErrors, LaunchPlan, SearchMode,
    SearchResult, WmType,
};
use crate::config::LauncherConfig as Config;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use hiren_shared::AppEntry;
use std::process::Command;

pub struct WindowMode {
    matcher: SkimMatcherV2,
    list_cmd: String,
    activate_cmd: String,
    wm_type: WmType,
}

impl WindowMode {
    pub fn new() -> Self {
        Self {
            matcher: SkimMatcherV2::default(),
            list_cmd: String::new(),
            activate_cmd: String::new(),
            wm_type: WmType::Wmctrl,
        }
    }

    /// Выполнить команду и вернуть stdout. Команда разбирается в argv тем же
    /// парсером, что и `Exec=` — без `sh -c` (команды листинга простые бинарники).
    fn run_cmd(cmd: &str) -> Result<String, String> {
        let argv = parse_exec_argv(cmd)?;
        let Some((bin, args)) = argv.split_first() else {
            return Err("Empty command string".into());
        };

        let output = Command::new(bin)
            .args(args)
            .output()
            .map_err(|e| format!("Failed to run '{cmd}': {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Command '{cmd}' failed: {stderr}"));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Получить список окон.
    fn get_windows(&self) -> Result<Vec<AppEntry>, String> {
        let output = Self::run_cmd(&self.list_cmd)?;
        parse_window_listing(self.wm_type, &output)
    }

}

/// Подставить id окна в argv-шаблон команды активации.
/// Сам id всегда попадает в один аргумент — shell-инъекция невозможна.
fn render_activate_argv(template: &str, window_id: &str) -> Result<Vec<String>, String> {
    let argv = parse_exec_argv(template)
        .map_err(|e| format!("invalid activate command `{template}`: {e}"))?;
    Ok(argv.iter().map(|arg| arg.replace("{id}", window_id)).collect())
}

// ---------------------------------------------------------------------------
// Parsers (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Разобрать вывод `wmctrl -l`.
///
/// Формат: `0x01600003  0  hostname  Window Title` — первые три колонки
/// (id, desktop, hostname), остаток строки — заголовок. Заголовок может
/// содержать пробелы; последовательности пробелов внутри заголовка
/// нормализуются в один пробел.
pub fn parse_wmctrl_listing(output: &str) -> Vec<AppEntry> {
    let mut windows = Vec::new();
    for line in output.lines() {
        let mut tokens = line.split_whitespace();
        let Some(id) = tokens.next() else { continue };
        if !id.starts_with("0x") {
            continue;
        }
        // desktop id и hostname не нужны, но их наличие проверяем.
        if tokens.next().is_none() || tokens.next().is_none() {
            continue;
        }
        let title = tokens.collect::<Vec<_>>().join(" ");
        if title.is_empty() || title == "N/A" {
            continue;
        }
        windows.push(AppEntry::window(id.to_string(), title, id.to_string()));
    }
    windows
}

/// Разобрать JSON `swaymsg -t get_tree`.
pub fn parse_sway_tree(json: &str) -> Result<Vec<AppEntry>, String> {
    let root: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("swaymsg JSON parse: {e}"))?;
    let mut windows = Vec::new();
    match &root {
        serde_json::Value::Array(nodes) => {
            for node in nodes {
                extract_sway_nodes(node, &mut windows);
            }
        }
        _ => extract_sway_nodes(&root, &mut windows),
    }
    Ok(windows)
}

fn extract_sway_nodes(node: &serde_json::Value, windows: &mut Vec<AppEntry>) {
    if let Some(name) = node.get("name").and_then(|v| v.as_str()) {
        if let Some(id) = node.get("id").and_then(|v| v.as_i64()) {
            if !name.is_empty() && name != "root" {
                // Отфильтровываем не-окна (workspace'ы и т.д.)
                let node_type = node.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if node_type == "con" || node_type == "floating_con" {
                    let app_id = node.get("app_id").and_then(|v| v.as_str()).unwrap_or("");
                    let display = if app_id.is_empty() {
                        name.to_string()
                    } else {
                        format!("{} — {}", app_id, name)
                    };
                    windows.push(AppEntry::window(id.to_string(), display, id.to_string()));
                }
            }
        }
    }

    for key in ["nodes", "floating_nodes"] {
        if let Some(nodes) = node.get(key).and_then(|v| v.as_array()) {
            for child in nodes {
                extract_sway_nodes(child, windows);
            }
        }
    }
}

/// Разобрать JSON `hyprctl clients -j`.
pub fn parse_hyprland_clients(json: &str) -> Result<Vec<AppEntry>, String> {
    let clients: Vec<serde_json::Value> =
        serde_json::from_str(json).map_err(|e| format!("hyprctl JSON parse: {e}"))?;

    let windows: Vec<AppEntry> = clients
        .into_iter()
        .filter_map(|c| {
            let address = c.get("address").and_then(|v| v.as_str())?;
            let title = c.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let class = c.get("class").and_then(|v| v.as_str()).unwrap_or("");

            let display = if title.is_empty() {
                class.to_string()
            } else if class.is_empty() {
                title.to_string()
            } else {
                format!("{} — {}", class, title)
            };

            if display.is_empty() {
                return None;
            }

            Some(AppEntry::window(address.to_string(), display, address.to_string()))
        })
        .collect();

    Ok(windows)
}

/// Эвристика для неизвестной кастомной команды листинга: JSON-объект → sway,
/// JSON-массив клиентов → hyprland, иначе построчный wmctrl-подобный формат.
pub fn parse_auto_listing(output: &str) -> Result<Vec<AppEntry>, String> {
    let trimmed = output.trim_start();
    if trimmed.starts_with('{') {
        return parse_sway_tree(output);
    }
    if trimmed.starts_with('[') {
        // Массив: либо контейнеры sway, либо клиенты hyprctl.
        if let Ok(windows) = parse_hyprland_clients(output) {
            if !windows.is_empty() || trimmed == "[]" {
                return Ok(windows);
            }
        }
        return parse_sway_tree(output);
    }
    Ok(parse_wmctrl_listing(output))
}

/// Разобрать вывод команды листинга согласно типу WM.
pub fn parse_window_listing(wm: WmType, output: &str) -> Result<Vec<AppEntry>, String> {
    match wm {
        WmType::Sway => parse_sway_tree(output),
        WmType::Hyprland => parse_hyprland_clients(output),
        WmType::Wmctrl => Ok(parse_wmctrl_listing(output)),
        WmType::Auto => parse_auto_listing(output),
    }
}

impl SearchMode for WindowMode {
    fn init(&mut self, config: &Config) {
        let commands = detect_window_commands(&config.window);
        self.list_cmd = commands.list;
        self.activate_cmd = commands.activate;
        self.wm_type = commands.wm;
        match self.wm_type {
            WmType::Sway => eprintln!("[hiren-client] Window mode: Sway ({})", self.list_cmd),
            WmType::Hyprland => {
                eprintln!("[hiren-client] Window mode: Hyprland ({})", self.list_cmd)
            }
            WmType::Wmctrl => eprintln!("[hiren-client] Window mode: wmctrl/X11"),
            WmType::Auto => eprintln!(
                "[hiren-client] Window mode: custom command, auto-detecting output ({})",
                self.list_cmd
            ),
        }
    }

    fn search(&self, query: &str) -> SearchResult {
        let query = query.trim();

        let windows = match self.get_windows() {
            Ok(w) => w,
            Err(e) => return SearchResult::Error(e),
        };

        if query.is_empty() {
            return SearchResult::Entries(windows);
        }

        let mut scored: Vec<(i64, AppEntry)> = windows
            .into_iter()
            .filter_map(|w| self.matcher.fuzzy_match(&w.name, query).map(|score| (score, w)))
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0));

        // Протаскиваем score в запись для гибридной сортировки на клиенте.
        let entries: Vec<AppEntry> = scored
            .into_iter()
            .map(|(score, mut w)| {
                w.score = score;
                w
            })
            .collect();
        SearchResult::Entries(entries)
    }

    fn execute(&self, entry: &AppEntry, config: &Config) {
        if let Err(e) = self.execute_with(entry, config, &super::new_launch_errors()) {
            log::warn!("hiren: cannot activate window {}: {e}", entry.name);
        }
    }

    fn execute_with(&self, entry: &AppEntry, _config: &Config, errors: &LaunchErrors) -> Result<(), String> {
        let argv = render_activate_argv(&self.activate_cmd, &entry.exec)?;
        eprintln!("[hiren-client] Activating window: {} ({})", entry.name, argv.join(" "));
        spawn_monitored(&LaunchPlan::Argv(argv), &entry.name, errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(entries: &[AppEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    // -----------------------------------------------------------------
    // wmctrl parsing
    // -----------------------------------------------------------------

    #[test]
    fn wmctrl_title_does_not_include_hostname() {
        // Репро бага: splitn(4,' ') давал title = "hostname  Window Title".
        let output = "0x01600003  0  myhost  Firefox — Mozilla\n\
                      0x01a00007 -1  myhost  Terminal — zsh\n";
        let windows = parse_wmctrl_listing(output);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name, "Firefox — Mozilla");
        assert_eq!(windows[0].exec, "0x01600003");
        assert_eq!(windows[1].name, "Terminal — zsh");
    }

    #[test]
    fn wmctrl_multiple_spaces_in_title_are_normalized() {
        let output = "0x01 0 host  Window   With    Spaces \n";
        let windows = parse_wmctrl_listing(output);
        assert_eq!(names(&windows), vec!["Window With Spaces"]);
    }

    #[test]
    fn wmctrl_skips_malformed_and_na_lines() {
        let output = "\n\
                      not a window line\n\
                      0x02 0 host N/A\n\
                      0x03 0 host\n\
                      0x04 0 host Real Title\n";
        let windows = parse_wmctrl_listing(output);
        assert_eq!(names(&windows), vec!["Real Title"]);
    }

    // -----------------------------------------------------------------
    // sway parsing
    // -----------------------------------------------------------------

    #[test]
    fn sway_tree_nested_containers_and_app_id() {
        let json = r#"{
            "id": 1, "name": "root", "type": "root",
            "nodes": [
                {"id": 2, "name": "1", "type": "workspace", "nodes": [
                    {"id": 42, "name": "Firefox", "type": "con", "app_id": "firefox"},
                    {"id": 43, "name": "float", "type": "floating_con", "app_id": ""}
                ]},
                {"id": 7, "name": "Sway", "type": "con", "app_id": "sway"}
            ]
        }"#;
        let windows = parse_sway_tree(json).unwrap();
        assert_eq!(names(&windows), vec!["firefox — Firefox", "float", "sway — Sway"]);
        assert_eq!(windows[0].exec, "42");
    }

    #[test]
    fn sway_tree_accepts_array_output() {
        let json = r#"[{"id": 5, "name": "x", "type": "con", "app_id": "app"}]"#;
        let windows = parse_sway_tree(json).unwrap();
        assert_eq!(names(&windows), vec!["app — x"]);
    }

    #[test]
    fn sway_invalid_json_is_error() {
        assert!(parse_sway_tree("not json").is_err());
    }

    // -----------------------------------------------------------------
    // hyprland parsing
    // -----------------------------------------------------------------

    #[test]
    fn hyprland_clients_class_and_title() {
        let json = r#"[
            {"address": "0x55aa", "class": "kitty", "title": "zsh"},
            {"address": "0x55bb", "class": "", "title": "Untitled"},
            {"address": "0x55cc", "class": "empty", "title": ""}
        ]"#;
        let windows = parse_hyprland_clients(json).unwrap();
        assert_eq!(names(&windows), vec!["kitty — zsh", "Untitled", "empty"]);
    }

    #[test]
    fn hyprland_invalid_json_is_error() {
        assert!(parse_hyprland_clients("{}").is_err());
    }

    // -----------------------------------------------------------------
    // Auto detection
    // -----------------------------------------------------------------

    #[test]
    fn auto_detects_json_object_as_sway() {
        let json = r#"{"id": 1, "name": "Sway", "type": "con", "app_id": "sway"}"#;
        let windows = parse_auto_listing(json).unwrap();
        assert_eq!(names(&windows), vec!["sway — Sway"]);
    }

    #[test]
    fn auto_detects_hyprland_array() {
        let json = r#"[{"address": "0x1", "class": "firefox", "title": "Mozilla"}]"#;
        let windows = parse_auto_listing(json).unwrap();
        assert_eq!(names(&windows), vec!["firefox — Mozilla"]);
    }

    #[test]
    fn auto_detects_wmctrl_lines() {
        let output = "0x01 0 host Some Title\n";
        let windows = parse_auto_listing(output).unwrap();
        assert_eq!(names(&windows), vec!["Some Title"]);
    }

    #[test]
    fn parse_window_listing_dispatches_by_type() {
        let wmctrl = "0x01 0 host Some Title\n";
        assert_eq!(parse_window_listing(WmType::Wmctrl, wmctrl).unwrap().len(), 1);
        assert_eq!(parse_window_listing(WmType::Auto, wmctrl).unwrap().len(), 1);
        assert!(parse_window_listing(WmType::Sway, wmctrl).is_err());
    }

    // -----------------------------------------------------------------
    // Activate command templating
    // -----------------------------------------------------------------

    #[test]
    fn activate_argv_replaces_id_without_shell() {
        let argv = render_activate_argv("swaymsg [con_id={id}] focus", "42").unwrap();
        assert_eq!(argv, vec!["swaymsg", "[con_id=42]", "focus"]);

        let argv = render_activate_argv("hyprctl dispatch focuswindow address:{id}", "0x55").unwrap();
        assert_eq!(argv, vec!["hyprctl", "dispatch", "focuswindow", "address:0x55"]);

        let argv = render_activate_argv("wmctrl -i -a {id}", "0x01").unwrap();
        assert_eq!(argv, vec!["wmctrl", "-i", "-a", "0x01"]);
    }

    #[test]
    fn activate_argv_keeps_shell_metacharacters_as_literal_arg() {
        // id окна приходит из вывода команды, но с argv он не может стать shell-командой.
        let argv = render_activate_argv("wmctrl -i -a {id}", "0x1; rm -rf /").unwrap();
        assert_eq!(argv.last().unwrap(), "0x1; rm -rf /");
    }

    #[test]
    fn activate_argv_rejects_invalid_template() {
        assert!(render_activate_argv("", "1").is_err());
        assert!(render_activate_argv("cmd \"unterminated", "1").is_err());
    }
}
