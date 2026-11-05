//! Легковесный парсер `.desktop`-файлов freedesktop.org.
//!
//! Реализует подмножество Desktop Entry Specification, которое нужно
//! лаунчеру:
//! - секция `[Desktop Entry]` (остальные секции игнорируются);
//! - `Name`, локализованные `Name[locale]`/`Comment[locale]` с fallback-цепочкой;
//! - field-коды в `Exec` (`%c`, `%i`, `%k`, `%%`, deprecated-коды);
//! - `OnlyShowIn`/`NotShowIn` с учётом `$XDG_CURRENT_DESKTOP`;
//! - `Hidden` (tombstone), `NoDisplay`, `Terminal`, `TryExec`, `Type`;
//! - `Categories` + `Keywords` для поиска.
//!
//! Desktop-id назначает вызывающий код (демон): id — это путь файла
//! относительно каталога `applications/` с `/` → `-` (parser знает только
//! stem и используется в тестах).

use hiren_shared::AppEntry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Результат разбора одного `.desktop`-файла.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopFile {
    /// Валидная запись приложения.
    Entry(AppEntry),
    /// `Hidden=true` — запись удалена (tombstone). Перекрывает файл с тем же
    /// desktop-id из каталога с более низким приоритетом.
    Hidden,
    /// Файл есть, но показывать его не нужно: `NoDisplay=true`,
    /// неподходящий `OnlyShowIn`/`NotShowIn`, `Type != Application`,
    /// пустой `Exec` или не найден `TryExec`. Файл более высокого
    /// приоритета всё равно перекрывает запись с более низким.
    Skip,
}

/// Контекст разбора: локаль пользователя и список текущих DE.
#[derive(Debug, Clone, Default)]
pub struct ParseContext {
    /// `$XDG_CURRENT_DESKTOP`, разбитый по `:`.
    pub current_desktops: Vec<String>,
    /// Локаль для переводов (`LC_ALL`, затем `LC_MESSAGES`, затем `LANG`).
    pub locale: Option<String>,
}

impl ParseContext {
    /// Прочитать локаль и `XDG_CURRENT_DESKTOP` из окружения процесса.
    pub fn from_env() -> Self {
        Self {
            current_desktops: current_desktops_from_env(),
            locale: locale_from_env(),
        }
    }
}

fn current_desktops_from_env() -> Vec<String> {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|value| {
            value
                .split(':')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn locale_from_env() -> Option<String> {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Разобрать `.desktop`-файл с явным контекстом (для тестов и демона,
/// который читает контекст окружения один раз на пересборку кэша).
pub fn parse_desktop_file_with(path: &Path, ctx: &ParseContext) -> DesktopFile {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            log::debug!("Cannot read {}: {e}", path.display());
            return DesktopFile::Skip;
        }
    };
    // Удаляем UTF-8 BOM, если он есть в начале файла.
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    parse_desktop_entry(content, path, ctx)
}

/// Разбор содержимого `.desktop`-файла. `path` нужен для `%k` и `id`.
fn parse_desktop_entry(content: &str, path: &Path, ctx: &ParseContext) -> DesktopFile {
    let mut section_seen = false;
    let mut in_desktop_entry = false;

    // Нелокализованные значения хранятся под ключом "".
    let mut names: HashMap<String, String> = HashMap::new();
    let mut comments: HashMap<String, String> = HashMap::new();
    let mut exec_raw: Option<String> = None;
    let mut try_exec: Option<String> = None;
    let mut icon: Option<String> = None;
    let mut app_type: Option<String> = None;
    let mut categories: Vec<String> = Vec::new();
    let mut keywords: Vec<String> = Vec::new();
    let mut only_show_in: Vec<String> = Vec::new();
    let mut not_show_in: Vec<String> = Vec::new();
    let mut no_display = false;
    let mut hidden = false;
    let mut terminal = false;

    for raw_line in content.lines() {
        let line = raw_line.trim();

        // Пустые строки и комментарии.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Заголовок секции.
        if line.starts_with('[') && line.ends_with(']') {
            let section = &line[1..line.len() - 1].trim();
            in_desktop_entry = section.eq_ignore_ascii_case("Desktop Entry");
            section_seen |= in_desktop_entry;
            continue;
        }

        if !in_desktop_entry {
            continue;
        }

        // Ключ=Значение; пробелы вокруг '=' допустимы (спека их запрещает,
        // но реальные файлы их содержат — раньше такой ключ терялся целиком).
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let key = raw_key.trim();
        let value = raw_value.trim();
        let (base_key, locale_suffix) = split_localized_key(key);
        let locale_key = locale_suffix.unwrap_or("");

        match base_key {
            "Name" => {
                names.entry(locale_key.to_string()).or_insert_with(|| value.to_string());
            }
            "Comment" => {
                comments.entry(locale_key.to_string()).or_insert_with(|| value.to_string());
            }
            "Exec" => {
                if exec_raw.is_none() {
                    exec_raw = Some(value.to_string());
                }
            }
            "TryExec" => {
                if try_exec.is_none() && !value.is_empty() {
                    try_exec = Some(value.to_string());
                }
            }
            "Icon" => {
                if icon.is_none() && !value.is_empty() {
                    icon = Some(value.to_string());
                }
            }
            "Type" => {
                if app_type.is_none() {
                    app_type = Some(value.to_string());
                }
            }
            "Categories" => categories.extend(split_desktop_list(value)),
            "Keywords" => keywords.extend(split_desktop_list(value)),
            "OnlyShowIn" => only_show_in.extend(split_desktop_list(value)),
            "NotShowIn" => not_show_in.extend(split_desktop_list(value)),
            "NoDisplay" if is_truthy(value) => no_display = true,
            "Hidden" if is_truthy(value) => hidden = true,
            "Terminal" if is_truthy(value) => terminal = true,
            _ => {}
        }
    }

    if !section_seen {
        // Не desktop-файл: не tombstone, просто пропускаем.
        return DesktopFile::Skip;
    }

    // Hidden — это tombstone, он важнее валидности остальных полей.
    if hidden {
        return DesktopFile::Hidden;
    }
    if no_display {
        return DesktopFile::Skip;
    }

    // Type обязателен и должен быть Application.
    match app_type.as_deref() {
        Some("Application") => {}
        _ => return DesktopFile::Skip,
    }

    let Some(name) = select_localized(&names, ctx.locale.as_deref()) else {
        return DesktopFile::Skip;
    };

    let Some(exec_raw) = exec_raw else {
        return DesktopFile::Skip;
    };

    // TryExec: если указан и бинарник не найден/не исполняем — скрываем запись.
    if let Some(ref try_exec) = try_exec {
        if !try_exec_exists(try_exec) {
            log::debug!("TryExec check failed for '{try_exec}': hiding entry");
            return DesktopFile::Skip;
        }
    }

    // OnlyShowIn/NotShowIn: сравниваем с $XDG_CURRENT_DESKTOP.
    let current: HashSet<&str> = ctx.current_desktops.iter().map(String::as_str).collect();
    if not_show_in.iter().any(|desktop| current.contains(desktop.as_str())) {
        return DesktopFile::Skip;
    }
    if !only_show_in.is_empty()
        && !only_show_in
            .iter()
            .any(|desktop| current.contains(desktop.as_str()))
    {
        return DesktopFile::Skip;
    }

    let desktop_file_path = path.to_string_lossy();
    let exec = expand_exec(
        &exec_raw,
        &ExecFields {
            name: &name,
            icon: icon.as_deref(),
            desktop_file: &desktop_file_path,
        },
    );
    if exec.is_empty() {
        return DesktopFile::Skip;
    }

    let description = select_localized(&comments, ctx.locale.as_deref())
        .filter(|comment| !comment.is_empty());

    // Categories + Keywords в единую строку для поиска, без дублей.
    let mut search_terms = categories;
    search_terms.extend(keywords);
    let mut seen = HashSet::new();
    search_terms.retain(|term| seen.insert(term.clone()));
    let keywords = search_terms.join(" ");

    let id = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();

    let entry = AppEntry::drun(id, name, exec, description, keywords)
        .with_icon(icon.unwrap_or_default())
        .with_terminal(terminal);
    DesktopFile::Entry(entry)
}

/// `Key[locale]` → (`Key`, Some(`locale`)); `Key` → (`Key`, None).
fn split_localized_key(key: &str) -> (&str, Option<&str>) {
    if let Some(open) = key.find('[') {
        if key.ends_with(']') && open + 1 < key.len() {
            return (&key[..open], Some(&key[open + 1..key.len() - 1]));
        }
    }
    (key, None)
}

/// Выбрать локализованное значение: точное совпадение с локалью, затем
/// fallback-цепочка GLib (`lang_COUNTRY.ENCODING@MOD`, `lang_COUNTRY@MOD`,
/// `lang_COUNTRY`, `lang@MOD`, `lang`), затем нелокализованное значение.
fn select_localized(values: &HashMap<String, String>, locale: Option<&str>) -> Option<String> {
    if let Some(locale) = locale {
        for candidate in locale_candidates(locale) {
            if let Some(value) = values.get(&candidate) {
                return Some(value.clone());
            }
        }
    }
    values
        .get("")
        .cloned()
}

/// Кандидаты локали в порядке приоритета (см. GLib `g_get_language_names`).
fn locale_candidates(locale: &str) -> Vec<String> {
    let locale = locale.trim();
    if locale.is_empty() || locale == "C" || locale == "POSIX" {
        return Vec::new();
    }
    let (base, modifier) = match locale.split_once('@') {
        Some((base, modifier)) => (base, Some(modifier)),
        None => (locale, None),
    };
    let (base, encoding) = match base.split_once('.') {
        Some((base, encoding)) => (base, Some(encoding)),
        None => (base, None),
    };
    let (lang, country) = match base.split_once('_') {
        Some((lang, country)) => (lang, Some(country)),
        None => (base, None),
    };

    let mut candidates = Vec::new();
    if let Some(country) = country {
        if let (Some(encoding), Some(modifier)) = (encoding, modifier) {
            candidates.push(format!("{lang}_{country}.{encoding}@{modifier}"));
        }
        if let Some(modifier) = modifier {
            candidates.push(format!("{lang}_{country}@{modifier}"));
        }
        if let Some(encoding) = encoding {
            candidates.push(format!("{lang}_{country}.{encoding}"));
        }
        candidates.push(format!("{lang}_{country}"));
    }
    if let Some(modifier) = modifier {
        candidates.push(format!("{lang}@{modifier}"));
    }
    if let Some(encoding) = encoding {
        candidates.push(format!("{lang}.{encoding}"));
    }
    candidates.push(lang.to_string());
    candidates
}

/// Разбить значение-список `.desktop` (`a;b;c;`) на элементы.
fn split_desktop_list(value: &str) -> Vec<String> {
    value
        .split(';')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// Контекст подстановки field-кодов в `Exec`.
#[derive(Debug, Clone)]
pub struct ExecFields<'a> {
    /// `Name` (локализованное значение).
    pub name: &'a str,
    /// `Icon`, если задан.
    pub icon: Option<&'a str>,
    /// Путь к самому `.desktop`-файлу (для `%k`).
    pub desktop_file: &'a str,
}

/// Раскрыть field-коды `Exec` по Desktop Entry Specification:
///
/// | код | значение |
/// |-----|----------|
/// | `%c` | `Name` |
/// | `%i` | `--icon <Icon>`, если `Icon` задан (иначе ничего) |
/// | `%k` | путь к `.desktop`-файлу |
/// | `%%` | литеральный `%` |
/// | `%f %F %u %U` | удаляются (файлы/URL передаются демоном запуска) |
/// | `%d %D %n %N %v %m` | deprecated, удаляются |
/// | прочие `%x` | остаются как есть (спеку нарушать не должны) |
pub fn expand_exec(exec: &str, fields: &ExecFields) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut icon_expanded = false;
    let mut chars = exec.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let Some(code) = chars.next() else {
            // Одиночный '%' в конце — оставляем как есть.
            out.push('%');
            break;
        };
        match code {
            '%' => out.push('%'),
            'c' => out.push_str(fields.name),
            'k' => out.push_str(fields.desktop_file),
            'i' => {
                if !icon_expanded {
                    if let Some(icon) = fields.icon.filter(|icon| !icon.is_empty()) {
                        if !out.is_empty() && !out.ends_with(' ') {
                            out.push(' ');
                        }
                        out.push_str("--icon ");
                        out.push_str(icon);
                        icon_expanded = true;
                    }
                }
            }
            'f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm' => {}
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out.trim().to_string()
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

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "hiren-desktop-test-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Разобрать содержимое как `app.desktop` с заданным контекстом.
    fn parse(content: &str, ctx: &ParseContext) -> DesktopFile {
        let dir = temp_dir("parse");
        let path = dir.join("app.desktop");
        std::fs::write(&path, content).expect("write desktop file");
        let result = parse_desktop_file_with(&path, ctx);
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn parse_entry(content: &str, ctx: &ParseContext) -> AppEntry {
        match parse(content, ctx) {
            DesktopFile::Entry(entry) => entry,
            other => panic!("expected entry, got {other:?}"),
        }
    }

    fn no_locale() -> ParseContext {
        ParseContext {
            current_desktops: Vec::new(),
            locale: None,
        }
    }

    #[test]
    fn parses_minimal_application() {
        let entry = parse_entry(
            "\
[Desktop Entry]
Type=Application
Name=TestApp
Exec=/usr/bin/testapp %u
Comment=Test app
Icon=testapp
Categories=Audio;Music;Player;AudioVideo;
Keywords=music;streaming;
Terminal=true
",
            &no_locale(),
        );
        assert_eq!(entry.id, "app");
        assert_eq!(entry.name, "TestApp");
        assert_eq!(entry.exec, "/usr/bin/testapp");
        assert_eq!(entry.description.as_deref(), Some("Test app"));
        assert_eq!(entry.icon, "testapp");
        assert_eq!(entry.keywords, "Audio Music Player AudioVideo music streaming");
        assert!(entry.terminal);
        assert_eq!(entry.mode, hiren_shared::AppMode::Drun);
    }

    #[test]
    fn trims_spaces_around_equals() {
        let entry = parse_entry(
            "\
[Desktop Entry]
Type = Application
Name = Spaced App
Exec = /usr/bin/spaced --flag
Comment = spaced comment
Icon = spaced-icon
",
            &no_locale(),
        );
        assert_eq!(entry.name, "Spaced App");
        assert_eq!(entry.exec, "/usr/bin/spaced --flag");
        assert_eq!(entry.description.as_deref(), Some("spaced comment"));
        assert_eq!(entry.icon, "spaced-icon");
    }

    #[test]
    fn ignores_other_sections_and_comments() {
        let entry = parse_entry(
            "\
# comment
[Desktop Action new-window]
Name=Not The App
Exec=nope
[Desktop Entry]
Type=Application
Name=Real App
Exec=/usr/bin/real
",
            &no_locale(),
        );
        assert_eq!(entry.name, "Real App");
    }

    #[test]
    fn hidden_is_a_tombstone() {
        assert_eq!(
            parse(
                "[Desktop Entry]\nType=Application\nHidden=true\n",
                &no_locale()
            ),
            DesktopFile::Hidden
        );
    }

    #[test]
    fn no_display_and_wrong_type_are_skipped() {
        assert_eq!(
            parse(
                "[Desktop Entry]\nType=Application\nName=X\nExec=x\nNoDisplay=true\n",
                &no_locale()
            ),
            DesktopFile::Skip
        );
        assert_eq!(
            parse("[Desktop Entry]\nType=Link\nName=X\nExec=x\n", &no_locale()),
            DesktopFile::Skip
        );
        // Type отсутствует — по спеке невалидно.
        assert_eq!(
            parse("[Desktop Entry]\nName=X\nExec=x\n", &no_locale()),
            DesktopFile::Skip
        );
    }

    #[test]
    fn requires_name_and_exec() {
        assert_eq!(
            parse("[Desktop Entry]\nType=Application\nExec=x\n", &no_locale()),
            DesktopFile::Skip
        );
        assert_eq!(
            parse(
                "[Desktop Entry]\nType=Application\nName=X\nExec=%f %u\n",
                &no_locale()
            ),
            DesktopFile::Skip,
            "exec that becomes empty after field-code removal is invalid"
        );
    }

    #[test]
    fn localized_name_and_comment_fallback_chain() {
        let content = "\
[Desktop Entry]
Type=Application
Name=Plain Name
Name[ru]=Русское имя
Name[ru_RU]=Русское имя RU
Name[sr_YU@latin]=Srpski
Exec=/usr/bin/app
Comment=Plain comment
Comment[ru]=Русский комментарий
";
        // Точное совпадение с lang_COUNTRY.
        let ru_ru = ParseContext {
            current_desktops: Vec::new(),
            locale: Some("ru_RU.UTF-8".into()),
        };
        let entry = parse_entry(content, &ru_ru);
        assert_eq!(entry.name, "Русское имя RU");
        assert_eq!(entry.description.as_deref(), Some("Русский комментарий"));

        // Fallback на lang без country.
        let ru = ParseContext {
            current_desktops: Vec::new(),
            locale: Some("ru_UA.UTF-8".into()),
        };
        let entry = parse_entry(content, &ru);
        assert_eq!(entry.name, "Русское имя");

        // Fallback lang_COUNTRY@modifier.
        let sr = ParseContext {
            current_desktops: Vec::new(),
            locale: Some("sr_YU.UTF-8@latin".into()),
        };
        let entry = parse_entry(content, &sr);
        assert_eq!(entry.name, "Srpski");

        // Нет перевода — нелокализованное значение.
        let de = ParseContext {
            current_desktops: Vec::new(),
            locale: Some("de_DE.UTF-8".into()),
        };
        let entry = parse_entry(content, &de);
        assert_eq!(entry.name, "Plain Name");
        assert_eq!(entry.description.as_deref(), Some("Plain comment"));

        // C/POSIX — без переводов.
        let c_locale = ParseContext {
            current_desktops: Vec::new(),
            locale: Some("C".into()),
        };
        assert_eq!(parse_entry(content, &c_locale).name, "Plain Name");
    }

    #[test]
    fn only_show_in_and_not_show_in_filter_by_current_desktop() {
        let gnome = ParseContext {
            current_desktops: vec!["GNOME".into(), "Unity".into()],
            locale: None,
        };
        let base = "[Desktop Entry]\nType=Application\nName=X\nExec=x\n";

        let only_gnome = format!("{base}OnlyShowIn=GNOME;Unity;\n");
        assert!(matches!(
            parse(&only_gnome, &gnome),
            DesktopFile::Entry(_)
        ));

        let only_kde = format!("{base}OnlyShowIn=KDE;\n");
        assert_eq!(parse(&only_kde, &gnome), DesktopFile::Skip);

        let not_gnome = format!("{base}NotShowIn=GNOME;\n");
        assert_eq!(parse(&not_gnome, &gnome), DesktopFile::Skip);

        let not_kde = format!("{base}NotShowIn=KDE;\n");
        assert!(matches!(parse(&not_kde, &gnome), DesktopFile::Entry(_)));

        // OnlyShowIn при пустом XDG_CURRENT_DESKTOP — запись не показываем.
        let no_desktop = no_locale();
        assert_eq!(parse(&only_gnome, &no_desktop), DesktopFile::Skip);
    }

    #[test]
    fn find_field_codes_are_removed() {
        let fields = ExecFields {
            name: "App",
            icon: None,
            desktop_file: "/tmp/app.desktop",
        };
        let cases = [
            ("app %u", "app"),
            ("app %U", "app"),
            ("app %f", "app"),
            ("app %F", "app"),
            ("app %d %D %n %N %v %m", "app"),
            ("app %u --flag", "app  --flag"),
            ("/usr/bin/app %F %U", "/usr/bin/app"),
        ];
        for (input, expected) in cases {
            assert_eq!(expand_exec(input, &fields), expected, "input: {input}");
        }
    }

    #[test]
    fn special_field_codes_expand() {
        let fields = ExecFields {
            name: "My App",
            icon: Some("my-icon"),
            desktop_file: "/usr/share/applications/my.desktop",
        };
        assert_eq!(expand_exec("echo %c", &fields), "echo My App");
        assert_eq!(
            expand_exec("app %i %u", &fields),
            "app --icon my-icon"
        );
        assert_eq!(
            expand_exec("app %k", &fields),
            "app /usr/share/applications/my.desktop"
        );
        // %% → %, а не удаление.
        assert_eq!(expand_exec("echo 100%%", &fields), "echo 100%");
        // Неизвестный код остаётся как есть.
        assert_eq!(expand_exec("app %x", &fields), "app %x");
        // Одиночный % в конце строки сохраняется.
        assert_eq!(expand_exec("app 100%", &fields), "app 100%");
        // %i без Icon — ничего.
        let no_icon = ExecFields {
            name: "App",
            icon: None,
            desktop_file: "/tmp/app.desktop",
        };
        assert_eq!(expand_exec("app %i", &no_icon), "app");
        // Один %i на строку даже при повторе.
        assert_eq!(
            expand_exec("app %i %i", &fields),
            "app --icon my-icon"
        );
    }

    #[test]
    fn field_codes_do_not_touch_other_percent_prefixed_args() {
        // Аргументы вида --percent=50% не должны ломаться, если % не код.
        let fields = ExecFields {
            name: "App",
            icon: None,
            desktop_file: "/tmp/app.desktop",
        };
        assert_eq!(expand_exec("app --x=50%", &fields), "app --x=50%");
    }

    #[test]
    fn try_exec_missing_binary_hides_entry() {
        let dir = temp_dir("tryexec");
        let missing = dir.join("definitely-missing-binary");
        let content = format!(
            "[Desktop Entry]\nType=Application\nName=X\nExec=x\nTryExec={}\n",
            missing.display()
        );
        assert_eq!(parse(&content, &no_locale()), DesktopFile::Skip);

        // Существующий исполняемый файл — entry виден.
        let present = dir.join("present-binary");
        std::fs::write(&present, "#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&present, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let content = format!(
            "[Desktop Entry]\nType=Application\nName=X\nExec=x\nTryExec={}\n",
            present.display()
        );
        assert!(matches!(parse(&content, &no_locale()), DesktopFile::Entry(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminal_false_by_default() {
        let entry = parse_entry(
            "[Desktop Entry]\nType=Application\nName=X\nExec=x\n",
            &no_locale(),
        );
        assert!(!entry.terminal);
    }

    #[test]
    fn utf8_bom_and_crlf_are_tolerated() {
        let entry = parse_entry(
            "\u{feff}[Desktop Entry]\r\nType=Application\r\nName=Bom App\r\nExec=/usr/bin/bom\r\n",
            &no_locale(),
        );
        assert_eq!(entry.name, "Bom App");
        assert_eq!(entry.exec, "/usr/bin/bom");
    }

    #[test]
    fn split_localized_key_handles_plain_and_localized() {
        assert_eq!(split_localized_key("Name"), ("Name", None));
        assert_eq!(split_localized_key("Name[ru]"), ("Name", Some("ru")));
        assert_eq!(split_localized_key("Name[ru_RU@latin]"), ("Name", Some("ru_RU@latin")));
        assert_eq!(split_localized_key("Weird[key"), ("Weird[key", None));
    }

    #[test]
    fn locale_candidates_order() {
        assert_eq!(
            locale_candidates("sr_YU.UTF-8@latin"),
            vec![
                "sr_YU.UTF-8@latin",
                "sr_YU@latin",
                "sr_YU.UTF-8",
                "sr_YU",
                "sr@latin",
                "sr.UTF-8",
                "sr",
            ]
        );
        assert!(locale_candidates("C").is_empty());
        assert!(locale_candidates("POSIX").is_empty());
        assert!(locale_candidates("").is_empty());
    }
}
