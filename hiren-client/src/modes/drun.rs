//! Режим drun — поиск и запуск .desktop-приложений через IPC-демон.

use super::{entry_requests_terminal, plan_launch, spawn_monitored, LaunchErrors, SearchMode, SearchResult};
use crate::config::LauncherConfig as Config;
use hiren_shared::AppEntry;

pub struct DrunMode;

impl DrunMode {
    pub fn new() -> Self {
        Self
    }
}

impl SearchMode for DrunMode {
    fn search(&self, query: &str) -> SearchResult {
        match crate::ipc::search_sync(query) {
            Ok(apps) => {
                // Помечаем все записи как Drun (на случай если демон ещё старый)
                let apps: Vec<AppEntry> = apps
                    .into_iter()
                    .map(|mut a| {
                        a.mode = hiren_shared::AppMode::Drun;
                        a
                    })
                    .collect();
                SearchResult::Entries(apps)
            }
            Err(e) => SearchResult::Error(format!("IPC error: {e:#}")),
        }
    }

    fn execute(&self, entry: &AppEntry, config: &Config, errors: &LaunchErrors) -> Result<(), String> {
        // Exec из .desktop запускается напрямую (без sh -c): field-коды уже
        // вырезаны демоном, а shell-интерпретация ломала бы кавычки/спецсимволы.
        let terminal = entry_requests_terminal(entry).then_some(&config.terminal);
        let plan = plan_launch(&entry.exec, terminal, None)?;
        spawn_monitored(&plan, &entry.name, errors)
    }
}
