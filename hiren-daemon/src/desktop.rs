//! Легковесный парсер `.desktop`-файлов freedesktop.org.
//!
//! Извлекает поля `Name`, `Exec`, `Comment`, `Categories`, `Keywords`,
//! фильтрует скрытые записи (`NoDisplay=true`, `Hidden=true`) и очищает
//! `Exec` от field-кодов.

use hiren_shared::AppEntry;
use std::path::{Path, PathBuf};

/// Распарсить `.desktop`-файл. Возвращает `None`, если файл не является
/// Desktop Entry, скрыт, или не содержит обязательных полей.
pub fn parse_desktop_file(path: &Path) -> Option<AppEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    // Удаляем UTF-8 BOM, если он есть в начале файла
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);

    let mut in_desktop_entry = false;
    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut comment: Option<String> = None;
    let mut try_exec: Option<String> = None;
    let mut categories: Vec<String> = Vec::new();
    let mut keywords: Vec<String> = Vec::new();
    let mut no_display = false;
    let mut hidden = false;

    for line in content.lines() {
        let line = line.trim();

        // Пропускаем пустые строки и комментарии
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Заголовок секции
        if line.starts_with('[') && line.ends_with(']') {
            in_desktop_entry = line.eq_ignore_ascii_case("[Desktop Entry]");
            continue;
        }

        if !in_desktop_entry {
            continue;
        }

        // Ключ=Значение (значение может содержать '=')
        if let Some((key, value)) = line.split_once('=') {
            match key {
                // Берём только непереведённые Name / Comment
                "Name" => {
                    if name.is_none() {
                        name = Some(value.to_string());
                    }
                }
                "Exec" => {
                    exec = Some(clean_exec(value));
                }
                "TryExec" => {
                    try_exec = Some(value.to_string());
                }
                "Comment" => {
                    if comment.is_none() {
                        comment = Some(value.to_string());
                    }
                }
                "NoDisplay" if is_truthy(value) => no_display = true,
                "Hidden" if is_truthy(value) => hidden = true,
                "Type" if value != "Application" => return None,
                "Categories" => {
                    for cat in value.split(';') {
                        let cat = cat.trim();
                        if !cat.is_empty() {
                            categories.push(cat.to_string());
                        }
                    }
                }
                "Keywords" => {
                    for kw in value.split(';') {
                        let kw = kw.trim();
                        if !kw.is_empty() {
                            keywords.push(kw.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if no_display || hidden {
        return None;
    }

    // Проверка TryExec: если указан и бинарник не найден/не исполняем — скрываем запись
    if let Some(ref try_exec) = try_exec {
        if !try_exec_exists(try_exec) {
            log::debug!("TryExec check failed for '{}': hiding entry", try_exec);
            return None;
        }
    }

    let name = name?;
    let exec = exec?;
    let id = path.file_stem()?.to_str()?.to_string();

    // Склеиваем Categories + Keywords в единую строку для поиска
    let mut kw_combined: Vec<String> = Vec::new();
    kw_combined.extend(categories);
    kw_combined.extend(keywords);
    kw_combined.dedup();
    let keywords = kw_combined.join(" ");

    Some(AppEntry::drun(
        id,
        name,
        exec,
        comment.filter(|c| !c.is_empty()),
        keywords,
    ))
}

/// Проверяет, является ли строка булевым "истинным" значением
/// (case-insensitive: true, True, TRUE, 1).
fn is_truthy(s: &str) -> bool {
    matches!(s.to_lowercase().as_str(), "true" | "1")
}

/// Проверяет существование и исполнимость бинарника, указанного в TryExec.
/// Если значение содержит '/' — проверяется как абсолютный/относительный путь.
/// Иначе — ищется в директориях из переменной окружения `PATH`.
fn try_exec_exists(value: &str) -> bool {
    if value.contains('/') {
        return is_executable(Path::new(value));
    }
    // Поиск в PATH
    let path_var = match std::env::var("PATH") {
        Ok(p) => p,
        Err(_) => return false,
    };
    for dir in path_var.split(':') {
        let candidate = PathBuf::from(dir).join(value);
        if is_executable(&candidate) {
            return true;
        }
    }
    false
}

/// Проверяет, что файл существует и является исполняемым.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111) != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Удалить field-коды из строки Exec:
///   %f %F %u %U %d %D %n %N %i %c %k %v %m  →  удаляются
///   %%                                     →  заменяется на %
fn clean_exec(exec: &str) -> String {
    let mut result = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(&ch) = chars.peek() {
        chars.next(); // consume current
        if ch == '%' {
            if let Some(&next) = chars.peek() {
                if "fFuUdDnNickvm".contains(next) {
                    chars.next(); // skip field code
                    continue;
                }
                if next == '%' {
                    chars.next();
                    result.push('%');
                    continue;
                }
            }
        }
        result.push(ch);
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_desktop_with_categories() {
        let desktop_content = "\
[Desktop Entry]
Name=TestApp
Exec=/usr/bin/testapp %u
Type=Application
Categories=Audio;Music;Player;AudioVideo;
Keywords=music;streaming;
";
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let file_path = dir.path().join("testapp.desktop");
        std::fs::write(&file_path, desktop_content).expect("Failed to write temp .desktop");

        let entry = parse_desktop_file(&file_path);
        assert!(entry.is_some(), "Test .desktop should parse");
        let entry = entry.unwrap();
        assert_eq!(entry.name, "TestApp");
        // Categories + Keywords склеены в keywords
        assert!(!entry.keywords.is_empty(), "keywords should include categories");
        assert!(entry.keywords.contains("Music"), "keywords should contain 'Music': {}", entry.keywords);
        assert!(entry.keywords.contains("Audio"), "keywords should contain 'Audio': {}", entry.keywords);
        assert!(entry.keywords.contains("streaming"), "keywords should contain 'streaming': {}", entry.keywords);
        println!("testapp keywords: '{}'", entry.keywords);
    }

    #[test]
    fn test_clean_exec_strips_field_codes() {
        assert_eq!(clean_exec("firefox %u"), "firefox");
        assert_eq!(clean_exec("app %F %U"), "app");
        assert_eq!(clean_exec("/usr/bin/app %f"), "/usr/bin/app");
    }

    #[test]
    fn test_clean_exec_percent_escape() {
        assert_eq!(clean_exec("echo 100%%"), "echo 100%");
    }

    #[test]
    fn test_clean_exec_preserves_args() {
        assert_eq!(
            clean_exec("env VAR=val /usr/bin/app --flag"),
            "env VAR=val /usr/bin/app --flag"
        );
    }
}
