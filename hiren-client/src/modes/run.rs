//! Режим run — поиск и запуск исполняемых файлов из $PATH.
//!
//! Сканирует директории из переменной окружения PATH,
//! кэширует список исполняемых файлов, выполняет fuzzy-поиск.

use super::{exec_detached, SearchMode, SearchResult};
use crate::config::Config;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use hiren_shared::AppEntry;
use std::collections::HashSet;

pub struct RunMode {
    executables: Vec<String>,
    matcher: SkimMatcherV2,
    is_initialized: bool,
}

impl RunMode {
    pub fn new() -> Self {
        Self {
            executables: Vec::new(),
            matcher: SkimMatcherV2::default(),
            is_initialized: false,
        }
    }

    /// Сканирует $PATH и собирает список исполняемых файлов.
    fn scan_path() -> Vec<String> {
        let path_var = std::env::var("PATH").unwrap_or_default();
        let mut seen = HashSet::new();
        let mut result = Vec::new();

        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        // Пропускаем файлы без права на исполнение
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            if let Ok(meta) = path.metadata() {
                                if meta.permissions().mode() & 0o111 == 0 {
                                    continue;
                                }
                            }
                        }
                        if seen.insert(name.to_string()) {
                            result.push(name.to_string());
                        }
                    }
                }
            }
        }

        result.sort();
        eprintln!(
            "[hiren-client] Run mode: scanned {} executables from $PATH",
            result.len()
        );
        result
    }
}

impl SearchMode for RunMode {
    fn init(&mut self, _config: &Config) {
        if !self.is_initialized {
            self.executables = Self::scan_path();
            self.is_initialized = true;
        }
    }

    fn search(&self, query: &str) -> SearchResult {
        let query = query.trim();

        if query.is_empty() {
            // Показываем все (или первые N)
            let entries: Vec<AppEntry> = self
                .executables
                .iter()
                .take(50)
                .map(|name| AppEntry::run(name.clone(), name.clone(), name.clone()))
                .collect();
            return SearchResult::Entries(entries);
        }

        let mut scored: Vec<(i64, AppEntry)> = self
            .executables
            .iter()
            .filter_map(|name| {
                self.matcher
                    .fuzzy_match(name, query)
                    .map(|score| (score, AppEntry::run(name.clone(), name.clone(), name.clone())))
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0));

        let entries: Vec<AppEntry> = scored
            .into_iter()
            .take(50)
            .map(|(_, entry)| entry)
            .collect();

        SearchResult::Entries(entries)
    }

    fn execute(&self, entry: &AppEntry, _config: &Config) {
        exec_detached(&entry.exec);
    }
}
