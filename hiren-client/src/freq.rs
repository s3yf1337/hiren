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

/// История запусков: ключ = нормализованная команда запуска, значение = счётчик.
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

    /// Нормализовать exec в ключ истории.
    ///
    /// - Обычные команды (`/usr/bin/foo`, `foo`): берём basename → `foo`.
    ///   Это объединяет запуски по имени бинарника независимо от пути.
    /// - `env`-команды (`env VAR=val /usr/bin/foo ...`): берём полную строку
    ///   как есть. Так `vesktop` и `vesktop PROXY` (env) не делят один ключ —
    ///   PROXY не получает буст от реального vesktop.
    fn key_for(exec: &str) -> String {
        if exec.starts_with("env ") {
            return exec.to_string();
        }
        let first = exec.split_whitespace().next().unwrap_or(exec);
        std::path::Path::new(first)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| exec.to_string())
    }

    /// Найти счётчик запусков для exec.
    ///
    /// Ищет по нормализованному ключу — так и старые записи (с полным
    /// путём, напр. `/usr/bin/steam`), и новые (basename `steam`) матчатся
    /// друг с другом, а `vesktop` и `vesktop PROXY` (env) остаются разными.
    fn lookup(&self, exec: &str) -> u64 {
        let target = Self::key_for(exec);
        // Точное совпадение ключа (быстро).
        if let Some(c) = self.counts.get(&target) {
            return *c;
        }
        // Обратная нормализация: среди сохранённых ключей найти тот, чей
        // нормализованный вид совпадает с target (покрывает случай, когда
        // история хранит полный путь, а запрос пришёл как basename).
        for (k, v) in &self.counts {
            if Self::key_for(k) == target {
                return *v;
            }
        }
        0
    }

    /// Записать запуск приложения. Увеличивает счётчик и сохраняет на диск.
    pub fn record_launch(&mut self, exec: &str) {
        if exec.is_empty() {
            return;
        }
        let key = Self::key_for(exec);
        *self.counts.entry(key).or_insert(0) += 1;
        self.save();
    }

    /// Отсортировать список записей: сначала самые частые, потом остальные.
    /// Возвращает (частые, остальные).
    pub fn partition_by_freq(
        &self,
        entries: Vec<hiren_shared::AppEntry>,
    ) -> (Vec<hiren_shared::AppEntry>, Vec<hiren_shared::AppEntry>) {
        let mut frequent = Vec::new();
        let mut rest = Vec::new();

        for entry in entries {
            if self.lookup(&entry.exec) > 0 {
                frequent.push(entry);
            } else {
                rest.push(entry);
            }
        }

        // Сортируем частые по убыванию счётчика
        frequent.sort_by(|a, b| {
            let ca = self.lookup(&a.exec);
            let cb = self.lookup(&b.exec);
            cb.cmp(&ca)
        });

        (frequent, rest)
    }

    /// Получить «вес частоты» для записи — логарифмический буст, зависящий
    /// от количества прошлых запусков. Чем больше запусков, тем выше буст,
    /// но с убывающей отдачей (log), чтобы единичный запуск не выносил
    /// нерелевантное приложение на самый верх.
    ///
    /// Множитель подобран так, чтобы буст был сопоставим с разбросом
    /// fuzzy-score (~0..150 для имени), а не на порядок больше его:
    /// count=1 → ~35, count=10 → ~96, count=100 → ~230 (при weight=0.8).
    fn freq_boost(&self, exec: &str) -> i64 {
        let count = self.lookup(exec);
        if count == 0 {
            return 0;
        }
        // 50 * ln(1 + count) — мягкий, монотонно растущий буст.
        (50.0 * (1.0 + count as f64).ln()) as i64
    }

    /// Гибридная сортировка «релевантность + частота» (frecency).
    ///
    /// Итоговый score = `entry.score + freq_boost(exec) * weight`.
    /// Частота мягко смещает результат вверх, но не переворачивает его
    /// целиком: при большой разнице в релевантности побеждает более
    /// подходящий по запросу вариант (напр. `Steam` выше `AyuGram` при
    /// запросе "steam"), а при близкой релевантности решает частота
    /// (`vesktop` выше `vesktop PROXY`, `AyuGram` выше `AyuXCB`).
    ///
    /// `weight` — множитель буста частоты (0.0 = чистая релевантность,
    /// 1.0 = честный буст). Обычно ~0.8.
    pub fn sort_by_frecency(
        &self,
        mut entries: Vec<hiren_shared::AppEntry>,
        weight: f64,
    ) -> Vec<hiren_shared::AppEntry> {
        entries.sort_by(|a, b| {
            let sa = a.score + (self.freq_boost(&a.exec) as f64 * weight) as i64;
            let sb = b.score + (self.freq_boost(&b.exec) as f64 * weight) as i64;
            sb.cmp(&sa)
        });
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hiren_shared::AppEntry;
    use std::collections::HashMap;

    fn entry(exec: &str, score: i64) -> AppEntry {
        let mut e = AppEntry::run(exec.to_string(), exec.to_string(), exec.to_string());
        e.score = score;
        e
    }

    /// Построить FreqHistory только в памяти (без записи на диск),
    /// чтобы тесты не портили реальный ~/.config/hiren/history.json.
    fn freq_from(map: &[(&str, u64)]) -> FreqHistory {
        let counts: HashMap<String, u64> = map.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        FreqHistory { counts }
    }

    #[test]
    fn test_key_for_basename() {
        assert_eq!(FreqHistory::key_for("/usr/bin/steam"), "steam");
        assert_eq!(FreqHistory::key_for("vesktop"), "vesktop");
    }

    #[test]
    fn test_key_for_env_keeps_full() {
        // env-команды не схлопываются в basename — иначе vesktop и
        // vesktop PROXY делили бы один ключ.
        let e = "env ALL_PROXY=socks5h://127.0.0.1:2080 vesktop --ozone-platform-hint=auto";
        assert_eq!(FreqHistory::key_for(e), e);
    }

    #[test]
    fn test_lookup_finds_both_full_and_basename() {
        // Старые записи с полным путём должны матчиться с basename-запросом.
        let freq = freq_from(&[("/usr/bin/steam", 11)]);
        assert_eq!(freq.lookup("/usr/bin/steam"), 11);
        assert_eq!(freq.lookup("steam"), 11);
    }

    #[test]
    fn test_frecency_boosts_frequent() {
        let freq = freq_from(&[("rare-app", 1), ("frequent-app", 2)]);

        // Оба имеют одинаковый релевантный score, но frequent-app чаще.
        let entries = vec![entry("rare-app", 100), entry("frequent-app", 100)];
        let sorted = freq.sort_by_frecency(entries, 0.8);
        assert_eq!(sorted[0].exec, "frequent-app");
        assert_eq!(sorted[1].exec, "rare-app");
    }

    #[test]
    fn test_frecency_frequency_wins_when_relevance_close() {
        // При близкой релевантности частота решает: vesktop (count=10)
        // выше vesktop PROXY (count=0), хотя у PROXY чуть выше fuzzy-score.
        let freq = freq_from(&[("vesktop", 10)]);
        let entries = vec![
            entry("env ALL_PROXY=socks5h://127.0.0.1:2080 vesktop --ozone-platform-hint=auto", 91),
            entry("/usr/bin/vesktop", 89),
        ];
        let sorted = freq.sort_by_frecency(entries, 0.8);
        assert_eq!(sorted[0].exec, "/usr/bin/vesktop");
        assert_eq!(
            sorted[1].exec,
            "env ALL_PROXY=socks5h://127.0.0.1:2080 vesktop --ozone-platform-hint=auto"
        );
    }

    #[test]
    fn test_frecency_relevance_wins_when_far() {
        // При большой разнице в релевантности частота не переворачивает
        // результат: Steam (score 109) выше AyuGram (score 36, count=10).
        let freq = freq_from(&[("AyuGram", 10)]);
        let entries = vec![
            entry("env DESKTOPINTEGRATION=1 AyuGram --", 36),
            entry("/usr/bin/steam", 109),
        ];
        let sorted = freq.sort_by_frecency(entries, 0.8);
        assert_eq!(sorted[0].exec, "/usr/bin/steam");
        assert_eq!(sorted[1].exec, "env DESKTOPINTEGRATION=1 AyuGram --");
    }

    #[test]
    fn test_frecency_within_group_sorts_by_score() {
        // Оба запускавшиеся — внутри группы сортируем по score+boost.
        let freq = freq_from(&[("app-a", 10), ("app-b", 1)]);
        let entries = vec![entry("app-b", 100), entry("app-a", 90)];
        let sorted = freq.sort_by_frecency(entries, 0.8);
        // app-a чаще (boost выше) → должен обойти app-b при близком score.
        assert_eq!(sorted[0].exec, "app-a");
    }

    #[test]
    fn test_frecency_zero_weight_uses_score_within_group() {
        // Оба запускавшиеся, weight=0 → внутри группы только по score.
        let freq = freq_from(&[("frequent-app", 2), ("rare-app", 1)]);
        let entries = vec![entry("rare-app", 200), entry("frequent-app", 100)];
        let sorted = freq.sort_by_frecency(entries, 0.0);
        assert_eq!(sorted[0].exec, "rare-app");
        assert_eq!(sorted[1].exec, "frequent-app");
    }
}
