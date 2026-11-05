//! Частотная история запусков.
//!
//! Хранится в `~/.config/hiren/history.json` как `{ "exec_command": count }`.
//! Пишется при каждом запуске приложения (асинхронно, атомарной заменой
//! файла через temp + rename + fsync), загружается при старте лаунчера.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Максимум записей в истории: старые/редкие записи отбрасываются, чтобы файл
/// не рос неограниченно (и загрузка оставалась дешёвой).
const MAX_ENTRIES: usize = 512;
/// Потолок счётчика — защищает от переполнения при экзотических конфигах.
const MAX_COUNT: u64 = 1_000_000;

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
        match history_path() {
            Some(path) => Self::load_from_path(&path),
            None => Self::default(),
        }
    }

    /// Загрузить историю из явного пути (используется и в тестах).
    /// Битый/нечитаемый файл — предупреждение и пустая история: лаунчер не
    /// должен падать или молча терять всю историю при повреждении.
    pub fn load_from_path(path: &Path) -> Self {
        let data = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                log::warn!("hiren: cannot read freq history {}: {e}", path.display());
                return Self::default();
            }
        };

        match serde_json::from_str::<HashMap<String, u64>>(&data) {
            Ok(counts) => {
                let mut hist = Self { counts };
                hist.prune();
                hist
            }
            Err(e) => {
                log::warn!(
                    "hiren: freq history {} is corrupt, starting empty: {e}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Записать историю на диск (best-effort, вызывается из фонового потока).
    pub fn save(&self) {
        let Some(path) = history_path() else {
            return;
        };
        if let Err(e) = self.save_to_path(&path) {
            log::warn!("hiren: cannot save freq history {}: {e}", path.display());
        }
    }

    /// Атомарно записать историю в явный путь (temp + fsync + rename).
    ///
    /// Запись чужого процесса/частичная запись не может испортить основной
    /// файл: читатель видит либо старую, либо новую полную версию.
    pub fn save_to_path(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string(&self.bounded()).map_err(std::io::Error::other)?;

        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "history.json".into());
        let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));

        let write_result = (|| -> std::io::Result<()> {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.flush()?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(e) = write_result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        // fsync каталога: гарантия, что rename переживёт падение питания.
        // На платформах, где нельзя открыть каталог, молча пропускаем.
        if let Some(parent) = path.parent() {
            if let Ok(dir) = fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
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

    /// Увеличить счётчик и асинхронно сохранить историю.
    ///
    /// Запись на диск не блокирует UI-поток (история маленькая, но fsync
    /// может занимать десятки миллисекунд на медленном диске).
    pub fn record_launch(&mut self, exec: &str) {
        if exec.is_empty() {
            return;
        }
        self.bump(exec);
        let snapshot = self.bounded();
        std::thread::spawn(move || match history_path() {
            Some(path) => {
                let hist = FreqHistory { counts: snapshot };
                if let Err(e) = hist.save_to_path(&path) {
                    log::warn!("hiren: cannot save freq history {}: {e}", path.display());
                }
            }
            None => log::warn!("hiren: cannot determine config dir for freq history"),
        });
    }

    /// Увеличить счётчик в памяти (без записи на диск).
    fn bump(&mut self, exec: &str) {
        if exec.is_empty() {
            return;
        }
        let key = Self::key_for(exec);
        let count = self.counts.entry(key).or_insert(0);
        *count = count.saturating_add(1).min(MAX_COUNT);
    }

    /// Привести историю к записываемому виду: обрезать счётчики и число
    /// записей (самые частые записи сохраняются, редкие отбрасываются).
    fn prune(&mut self) {
        for count in self.counts.values_mut() {
            *count = (*count).min(MAX_COUNT);
        }
        if self.counts.len() <= MAX_ENTRIES {
            return;
        }
        let mut entries: Vec<(String, u64)> = self.counts.drain().collect();
        // Счётчик по убыванию, затем ключ — детерминированный порядок.
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        entries.truncate(MAX_ENTRIES);
        self.counts = entries.into_iter().collect();
    }

    fn bounded(&self) -> HashMap<String, u64> {
        let mut hist = self.clone();
        hist.prune();
        hist.counts
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

    // -----------------------------------------------------------------
    // Persistence: atomic writes, corrupt input, bounded size
    // -----------------------------------------------------------------

    #[test]
    fn test_load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let hist = FreqHistory::load_from_path(&dir.path().join("nope.json"));
        assert!(hist.counts.is_empty());
    }

    #[test]
    fn test_load_corrupt_is_empty_and_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        fs::write(&path, "{ this is not json").unwrap();
        assert!(FreqHistory::load_from_path(&path).counts.is_empty());
    }

    #[test]
    fn test_save_is_atomic_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let hist = freq_from(&[("firefox", 4), ("/usr/bin/foot", 2)]);
        hist.save_to_path(&path).unwrap();
        assert!(path.exists());
        // Никаких временных файлов не остаётся.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");

        let loaded = FreqHistory::load_from_path(&path);
        assert_eq!(loaded.lookup("firefox"), 4);
        assert_eq!(loaded.lookup("foot"), 2);

        // Повторная запись заменяет файл целиком.
        let hist2 = freq_from(&[("firefox", 5)]);
        hist2.save_to_path(&path).unwrap();
        let loaded2 = FreqHistory::load_from_path(&path);
        assert_eq!(loaded2.lookup("firefox"), 5);
        assert_eq!(loaded2.lookup("foot"), 0);
    }

    #[test]
    fn test_save_bounds_entries_and_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let mut counts: HashMap<String, u64> = HashMap::new();
        for i in 0..(MAX_ENTRIES + 100) {
            counts.insert(format!("app{i}"), i as u64 + 1);
        }
        counts.insert("overflow".into(), MAX_COUNT + 50);
        let hist = FreqHistory { counts };
        hist.save_to_path(&path).unwrap();

        let loaded = FreqHistory::load_from_path(&path);
        assert!(loaded.counts.len() <= MAX_ENTRIES, "entries not pruned");
        assert_eq!(loaded.lookup("overflow"), MAX_COUNT);
        // Самые частые записи сохранены.
        assert_eq!(loaded.lookup(&format!("app{}", MAX_ENTRIES + 99)), (MAX_ENTRIES + 99) as u64);
    }

    #[test]
    fn test_bump_caps_count() {
        let mut hist = FreqHistory::default();
        hist.counts.insert("app".into(), MAX_COUNT);
        hist.bump("app");
        assert_eq!(hist.lookup("app"), MAX_COUNT);
    }

    #[test]
    fn test_bump_ignores_empty_exec() {
        let mut hist = FreqHistory::default();
        hist.bump("");
        assert!(hist.counts.is_empty());
    }

    #[test]
    fn test_save_to_unwritable_parent_does_not_panic() {
        // Родитель — файл, а не каталог: create_dir_all падает, но клиент
        // должен просто залогировать ошибку.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("sub").join("history.json");
        assert!(FreqHistory::default().save_to_path(&path).is_err());
    }
}
