//! Режим window — переключение между окнами.
//!
//! Поддерживает автоопределение WM:
//! - Sway (через swaymsg)
//! - Hyprland (через hyprctl)
//! - X11 (через wmctrl)
//! - Кастомные команды из конфига

use super::{detect_window_commands, exec_detached, SearchMode, SearchResult};
use crate::config::Config;
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

#[derive(Debug, Clone, Copy, PartialEq)]
enum WmType {
    Sway,
    Hyprland,
    Wmctrl,
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

    /// Выполнить команду и вернуть stdout.
    /// Разбирает строку команды на бинарник + аргументы и запускает напрямую
    /// (без `sh -c`), поскольку управляющие команды — простые бинарники
    /// без shell-метасимволов.
    fn run_cmd(cmd: &str) -> Result<String, String> {
        let mut parts = cmd.split_whitespace();
        let bin = parts
            .next()
            .ok_or_else(|| format!("Empty command string"))?;
        let args: Vec<&str> = parts.collect();

        let output = Command::new(bin)
            .args(&args)
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
        match self.wm_type {
            WmType::Sway => self.get_sway_windows(),
            WmType::Hyprland => self.get_hyprland_windows(),
            WmType::Wmctrl => self.get_wmctrl_windows(),
        }
    }

    fn get_sway_windows(&self) -> Result<Vec<AppEntry>, String> {
        let json = Self::run_cmd(&self.list_cmd)?;
        let root: serde_json::Value =
            serde_json::from_str(&json).map_err(|e| format!("swaymsg JSON parse: {e}"))?;

        let mut windows = Vec::new();
        Self::extract_sway_nodes(&root, &mut windows);
        Ok(windows)
    }

    fn extract_sway_nodes(node: &serde_json::Value, windows: &mut Vec<AppEntry>) {
        if let Some(name) = node.get("name").and_then(|v| v.as_str()) {
            if let Some(id) = node.get("id").and_then(|v| v.as_i64()) {
                if !name.is_empty() && name != "root" {
                    // Отфильтровываем не-окна (workspace'ы и т.д.)
                    let node_type = node
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if node_type == "con" || node_type == "floating_con" {
                        let app_id = node
                            .get("app_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let display = if app_id.is_empty() {
                            name.to_string()
                        } else {
                            format!("{} — {}", app_id, name)
                        };
                        windows.push(AppEntry::window(
                            id.to_string(),
                            display,
                            id.to_string(),
                        ));
                    }
                }
            }
        }

        if let Some(nodes) = node.get("nodes").and_then(|v| v.as_array()) {
            for child in nodes {
                Self::extract_sway_nodes(child, windows);
            }
        }
        if let Some(nodes) = node.get("floating_nodes").and_then(|v| v.as_array()) {
            for child in nodes {
                Self::extract_sway_nodes(child, windows);
            }
        }
    }

    fn get_hyprland_windows(&self) -> Result<Vec<AppEntry>, String> {
        let json = Self::run_cmd(&self.list_cmd)?;
        let clients: Vec<serde_json::Value> =
            serde_json::from_str(&json).map_err(|e| format!("hyprctl JSON parse: {e}"))?;

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

                Some(AppEntry::window(
                    address.to_string(),
                    display,
                    address.to_string(),
                ))
            })
            .collect();

        Ok(windows)
    }

    fn get_wmctrl_windows(&self) -> Result<Vec<AppEntry>, String> {
        let output = Self::run_cmd(&self.list_cmd)?;
        let mut windows = Vec::new();

        for line in output.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // Формат wmctrl -l: 0x01600003  0  hostname  Window Title
            let parts: Vec<&str> = line.splitn(4, ' ').collect();
            if parts.len() < 4 {
                // Пробуем разбить по пробельным символам
                let parts_ws: Vec<&str> = line.split_whitespace().collect();
                if parts_ws.len() >= 4 {
                    let id = parts_ws[0].to_string();
                    let title = parts_ws[3..].join(" ");
                    if !title.is_empty() {
                        windows.push(AppEntry::window(id.clone(), title, id));
                    }
                }
                continue;
            }

            let id = parts[0].to_string();
            let title = parts[3].to_string();

            if !title.is_empty() && !title.starts_with("N/A") {
                windows.push(AppEntry::window(id.clone(), title, id));
            }
        }

        Ok(windows)
    }

    fn activate(&self, window_id: &str) {
        let cmd = self.activate_cmd.replace("{id}", window_id);
        eprintln!("[hiren-client] Activating window: {cmd}");
        // activate_cmd может содержать shell-метасимволы (кавычки в swaymsg),
        // поэтому здесь нужен sh -c
        exec_detached(&cmd);
    }
}

impl SearchMode for WindowMode {
    fn init(&mut self, config: &Config) {
        let (list_cmd, activate_cmd) = detect_window_commands(&config.window);
        self.list_cmd = list_cmd;
        self.activate_cmd = activate_cmd;

        // Определяем тип WM
        if std::env::var("SWAYSOCK").is_ok() {
            self.wm_type = WmType::Sway;
            eprintln!("[hiren-client] Window mode: detected Sway");
        } else if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
            self.wm_type = WmType::Hyprland;
            eprintln!("[hiren-client] Window mode: detected Hyprland");
        } else {
            self.wm_type = WmType::Wmctrl;
            eprintln!("[hiren-client] Window mode: using wmctrl (X11)");
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
            .filter_map(|w| {
                self.matcher
                    .fuzzy_match(&w.name, query)
                    .map(|score| (score, w))
            })
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

    fn execute(&self, entry: &AppEntry, _config: &Config) {
        self.activate(&entry.exec);
    }
}
