//! Режим drun — поиск и запуск .desktop-приложений через IPC-демон.

use super::{exec_detached, SearchMode, SearchResult};
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
            Err(e) => {
                SearchResult::Error(format!("IPC error: {e:#}"))
            }
        }
    }

    fn execute(&self, entry: &AppEntry, _config: &Config) {
        let exec = &entry.exec;
        exec_detached(exec);
    }
}
