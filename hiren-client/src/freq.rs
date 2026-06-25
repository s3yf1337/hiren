//! Частотная история запусков.
//!
//! Хранится в `~/.config/hiren/history.json` как `{ "exec_command": count }`.
//! Пишется при каждом запуске приложения, загружается при старте лаунчера.

use std::collections::HashMap;
use std::path::PathBuf;

/// Путь к файлу истории.
fn history_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("hiren").join("history.json"))
}

/// История запусков: ключ = команда запуска (exec), значение = счётчик.
#[derive(Debug, Clone, Default)]
pub struct FreqHistory {
    counts: HashMap<String, u64>,
}

impl FreqHistory {
    /// Загрузить историю с диска. Если файла нет — возвращает пустую.
    pub fn load() -> Self {
        let Some(path) = history_path() else {
            return Self::default();
        };

        let data = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };

        match serde_json::from_str::<HashMap<String, u64>>(&data) {
            Ok(counts) => Self { counts },
            Err(_) => Self::default(),
        }
    }

    /// Записать историю на диск.
    pub fn save(&self) {
        let Some(path) = history_path() else {
            return;
        };

        // Создаём директорию если нет
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match serde_json::to_string_pretty(&self.counts) {
            Ok(json) => {
                let _ = std::fs::write(&path, &json);
            }
            Err(_) => {}
        }
    }

    /// Записать запуск приложения. Увеличивает счётчик и сохраняет на диск.
    pub fn record_launch(&mut self, exec: &str) {
        if exec.is_empty() {
            return;
        }
        *self.counts.entry(exec.to_string()).or_insert(0) += 1;
        self.save();
    }

    /// Отсортировать список записей: сначала самые частые, потом остальные.
    /// Возвращает (частые, остальные).
    pub fn partition_by_freq(&self, entries: Vec<hiren_shared::AppEntry>) -> (Vec<hiren_shared::AppEntry>, Vec<hiren_shared::AppEntry>) {
        let mut frequent = Vec::new();
        let mut rest = Vec::new();

        for entry in entries {
            if self.counts.contains_key(&entry.exec) {
                frequent.push(entry);
            } else {
                rest.push(entry);
            }
        }

        // Сортируем частые по убыванию счётчика
        frequent.sort_by(|a, b| {
            let ca = self.counts.get(&a.exec).copied().unwrap_or(0);
            let cb = self.counts.get(&b.exec).copied().unwrap_or(0);
            cb.cmp(&ca)
        });

        (frequent, rest)
    }
}
