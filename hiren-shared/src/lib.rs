//! hiren-shared — общие типы данных и протокол IPC для hiren-daemon и hiren-client.
//!
//! Формат сообщений в сокете: [4 байта LE-длина][JSON-тело]
//!
//! Безопасность сокета: путь по умолчанию лежит в `$XDG_RUNTIME_DIR`
//! (приватный каталог пользователя), файл сокета создаётся с правами 0600,
//! обе стороны проверяют `SO_PEERCRED` (uid собеседника). См. [`socket_path`]
//! и [`verify_peer_uid`].

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Типы данных
// ---------------------------------------------------------------------------

/// Режим работы лаунчера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash, Default)]
pub enum AppMode {
    /// Поиск и запуск .desktop-приложений (drun).
    #[default]
    Drun,
    /// Запуск исполняемых файлов из $PATH.
    Run,
    /// Переключение окон (EWMH / wlroots / Hyprland).
    Window,
    /// Калькулятор: вычисление выражений.
    Calc,
}

/// Запись о приложении, извлечённая из `.desktop`-файла.
/// Также используется для режимов run/window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppEntry {
    /// Идентификатор (desktop-id: путь от каталога `applications/`,
    /// где `/` заменён на `-`, без суффикса `.desktop`).
    pub id: String,
    /// Человекочитаемое название (`Name=`).
    pub name: String,
    /// Команда запуска (`Exec=`), очищенная от field-кодов.
    pub exec: String,
    /// Описание (`Comment=`), если указано.
    pub description: Option<String>,
    /// Режим, к которому относится запись.
    #[serde(default)]
    pub mode: AppMode,
    /// Ключевые слова для поиска: Categories + Keywords из .desktop,
    /// разделённые пробелами. Для не-drun записей — пустая строка.
    #[serde(default)]
    pub keywords: String,
    /// Предвычисленная строка для поиска (name + keywords).
    /// Заполняется при построении кэша, чтобы избежать format!() на каждый запрос.
    #[serde(default)]
    pub search_text: String,
    /// Релевантность результата поиска (fuzzy-скор, >=0).
    /// Заполняется источником (демон/drun, run, window) при поиске.
    /// Для записей без поиска (пустой запрос) — 0.
    /// Используется клиентом для гибридной сортировки «релевантность × частота».
    #[serde(default)]
    pub score: i64,
    /// Имя или путь иконки из `.desktop` (`Icon=`). Пусто, если поля нет.
    /// Клиент резолвит это в файл через freedesktop lookup.
    #[serde(default)]
    pub icon: String,
    /// `Terminal=true` из `.desktop`: приложение должно запускаться
    /// в терминале (клиент оборачивает `exec` в настроенный терминал-emulator).
    #[serde(default)]
    pub terminal: bool,
}

impl AppEntry {
    /// Создать запись для режима drun (существующее поведение).
    pub fn drun(id: String, name: String, exec: String, description: Option<String>, keywords: String) -> Self {
        let search_text = if keywords.is_empty() {
            name.clone()
        } else {
            format!("{} {}", name, keywords)
        };
        Self {
            id,
            name,
            exec,
            description,
            mode: AppMode::Drun,
            keywords,
            search_text,
            score: 0,
            icon: String::new(),
            terminal: false,
        }
    }

    /// Создать запись для режима run.
    pub fn run(id: String, name: String, exec: String) -> Self {
        let search_text = name.clone();
        Self {
            id,
            name,
            exec,
            description: None,
            mode: AppMode::Run,
            keywords: String::new(),
            search_text,
            score: 0,
            icon: String::new(),
            terminal: false,
        }
    }

    /// Создать запись для режима window.
    pub fn window(id: String, name: String, exec: String) -> Self {
        let search_text = name.clone();
        Self {
            id,
            name,
            exec,
            description: None,
            mode: AppMode::Window,
            keywords: String::new(),
            search_text,
            score: 0,
            icon: String::new(),
            terminal: false,
        }
    }

    /// Создать запись-результат для calc.
    pub fn calc_result(expression: String, result: String) -> Self {
        let name = format!("{} = {}", expression, result);
        let search_text = name.clone();
        Self {
            id: String::new(),
            name,
            exec: result,
            description: None,
            mode: AppMode::Calc,
            keywords: String::new(),
            search_text,
            score: 0,
            icon: String::new(),
            terminal: false,
        }
    }

    /// Создать запись с ошибкой для calc.
    pub fn calc_error(msg: String) -> Self {
        let name = format!("Error: {msg}");
        let search_text = name.clone();
        Self {
            id: String::new(),
            name,
            exec: String::new(),
            description: None,
            mode: AppMode::Calc,
            keywords: String::new(),
            search_text,
            score: 0,
            icon: String::new(),
            terminal: false,
        }
    }

    /// Attach a desktop `Icon=` name or path.
    pub fn with_icon(mut self, icon: String) -> Self {
        self.icon = icon;
        self
    }

    /// Пометить запись как требующую терминала (`Terminal=true`).
    pub fn with_terminal(mut self, terminal: bool) -> Self {
        self.terminal = terminal;
        self
    }
}

// ---------------------------------------------------------------------------
// Путь к сокету
// ---------------------------------------------------------------------------

/// Имя файла сокета в `$XDG_RUNTIME_DIR`.
pub const SOCKET_FILE_NAME: &str = "hiren.socket";

/// Переменная окружения, которая полностью переопределяет путь сокета.
/// Используется тестами и изолированными экземплярами демона.
pub const SOCKET_PATH_ENV: &str = "HIREN_SOCKET_PATH";

/// Путь к UNIX-сокету IPC.
///
/// Порядок приоритета:
/// 1. `$HIREN_SOCKET_PATH` — явный override (тесты, изоляция);
/// 2. `$XDG_RUNTIME_DIR/hiren.socket` — штатное место: приватный каталог
///    пользователя, недоступный другим uid;
/// 3. `$TMPDIR/hiren-$UID.socket`;
/// 4. `/tmp/hiren-$UID.socket`.
///
/// Fallback-ветки включают uid в имя, чтобы разные пользователи не
/// сталкивались на общем `/tmp`. Предсказуемый путь в `/tmp` безопасен
/// только вместе с проверкой владельца файла и `SO_PEERCRED` (см.
/// [`verify_socket_file_owner`], [`verify_peer_uid`]).
pub fn socket_path() -> PathBuf {
    resolve_socket_path(
        non_empty_env(SOCKET_PATH_ENV),
        non_empty_env("XDG_RUNTIME_DIR"),
        non_empty_env("TMPDIR"),
        current_uid(),
    )
}

fn non_empty_env(key: &str) -> Option<OsString> {
    std::env::var_os(key).filter(|value| !value.is_empty())
}

/// Чистое ядро [`socket_path`] — без чтения окружения, чтобы политику пути
/// можно было покрыть тестами без гонок за переменные процесса.
fn resolve_socket_path(
    override_path: Option<OsString>,
    runtime_dir: Option<OsString>,
    tmpdir: Option<OsString>,
    uid: Option<u32>,
) -> PathBuf {
    if let Some(path) = override_path {
        return PathBuf::from(path);
    }
    if let Some(dir) = runtime_dir {
        return PathBuf::from(dir).join(SOCKET_FILE_NAME);
    }
    let file_name = match uid {
        Some(uid) => format!("hiren-{uid}.socket"),
        // Платформа без geteuid: pid хотя бы исключает коллизию между
        // одновременно работающими процессами.
        None => format!("hiren-{}.socket", std::process::id()),
    };
    match tmpdir {
        Some(dir) => PathBuf::from(dir).join(file_name),
        None => PathBuf::from("/tmp").join(file_name),
    }
}

/// Создать родительский каталог сокета, если его ещё нет.
///
/// Существующий каталог не изменяется. Созданный — получает права 0700:
/// сокет не должен быть доступен другим пользователям.
pub fn ensure_socket_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    match std::fs::metadata(parent) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("{} exists but is not a directory", parent.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Проверить существующий файл сокета через `lstat` (без следования
/// симлинкам): он должен быть UNIX-сокетом и принадлежать текущему uid.
///
/// Возвращает `Ok(())`, если пути не существует (stale-сокета нет).
/// Иначе ошибка: это защита от подмены/чужого сокета в предсказуемом
/// каталоге (`/tmp`).
#[cfg(unix)]
pub fn verify_socket_file_owner(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists but is not a UNIX socket; refusing to replace it",
                        path.display()
                    ),
                ));
            }
            if let Some(uid) = current_uid() {
                if meta.uid() != uid {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "{} is owned by uid {} (expected {uid}); refusing to use it",
                            path.display(),
                            meta.uid()
                        ),
                    ));
                }
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
pub fn verify_socket_file_owner(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Выставить права 0600 на созданный сокет (bind создаёт файл с umask,
/// обычно 0755 — это лишние права для IPC).
pub fn tighten_socket_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Проверка uid
// ---------------------------------------------------------------------------

/// Эффективный uid текущего процесса.
///
/// На Linux реализовано через libc `geteuid` (std уже линкует libc, поэтому
/// отдельная зависимость не нужна); на прочих платформах — `None`.
#[cfg(target_os = "linux")]
pub fn current_uid() -> Option<u32> {
    Some(sys::euid())
}

#[cfg(not(target_os = "linux"))]
pub fn current_uid() -> Option<u32> {
    None
}

/// Проверить через `SO_PEERCRED`, что процесс на другом конце UNIX-сокета
/// работает под тем же uid.
///
/// Демон вызывает это для каждого принятого соединения (не обслуживаем
/// чужих клиентов), клиент — после подключения (не разговариваем с чужим
/// демоном, даже если сокет лежит в предсказуемом `/tmp`).
#[cfg(target_os = "linux")]
pub fn verify_peer_uid(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    let peer = sys::peer_uid(fd)?;
    match current_uid() {
        Some(own) if peer == own => Ok(()),
        Some(own) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("peer uid {peer} != our uid {own}"),
        )),
        // Недостижимо на Linux (geteuid не падает), но fail-open здесь хуже
        // fail-closed не бывает: uid не узнали — проверка невозможна.
        None => Ok(()),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn verify_peer_uid(_fd: std::os::fd::RawFd) -> std::io::Result<()> {
    // SO_PEERCRED — Linux-specific. На других платформах полагаемся на
    // права 0600 и приватный каталог сокета.
    Ok(())
}

#[cfg(target_os = "linux")]
mod sys {
    use std::io;
    use std::os::raw::{c_int, c_void};

    // Константы и типы из <sys/socket.h>/<unistd.h> (Linux, glibc и musl
    // совместимы: uid_t = u32, pid_t = i32).
    const SOL_SOCKET: c_int = 1;
    const SO_PEERCRED: c_int = 17;

    #[repr(C)]
    struct UCred {
        pid: i32,
        uid: u32,
        gid: u32,
    }

    extern "C" {
        fn getsockopt(
            fd: c_int,
            level: c_int,
            optname: c_int,
            optval: *mut c_void,
            optlen: *mut u32,
        ) -> c_int;
        fn geteuid() -> u32;
    }

    /// `geteuid()` не может вернуть ошибку.
    pub(super) fn euid() -> u32 {
        // SAFETY: geteuid — чистый libc-вызов без аргументов.
        unsafe { geteuid() }
    }

    /// `SO_PEERCRED` для подключённого UNIX-сокета.
    pub(super) fn peer_uid(fd: c_int) -> io::Result<u32> {
        let mut cred = UCred {
            pid: 0,
            uid: u32::MAX,
            gid: 0,
        };
        let mut len = std::mem::size_of::<UCred>() as u32;
        // SAFETY: cred — валидная выровненная структура нужного размера,
        // len соответствует её размеру; fd — дескриптор сокета вызывающего.
        // Невалидный fd даёт EBADF, а не UB.
        let rc = unsafe {
            getsockopt(
                fd,
                SOL_SOCKET,
                SO_PEERCRED,
                &mut cred as *mut UCred as *mut c_void,
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
}

// ---------------------------------------------------------------------------
// Протокол IPC
// ---------------------------------------------------------------------------

/// Версия протокола IPC.
///
/// Клиент передаёт её в [`IPCMessage::RequestSearch`]; демон отвергает
/// запросы с другой версией через [`IPCMessage::Error`], а не пытается
/// угадать раскладку.
pub const PROTOCOL_VERSION: u32 = 1;

/// Максимальный размер тела фрейма — 256 КиБ.
///
/// Ограничение защищает обе стороны от выделения памяти под произвольную
/// длину, присланную собеседником: длина проверяется до аллокации.
pub const MAX_FRAME_SIZE: usize = 256 * 1024;

/// Максимальная длина строки поиска в символах. Более длинные запросы
/// демон отвергает с [`IPCMessage::Error`].
pub const MAX_QUERY_LEN: usize = 1024;

/// Сообщения протокола IPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IPCMessage {
    /// Запрос от клиента: версия протокола и строка поиска.
    RequestSearch {
        /// Версия протокола клиента (см. [`PROTOCOL_VERSION`]).
        version: u32,
        /// Строка поиска.
        query: String,
    },
    /// Ответ от демона: отфильтрованный и отсортированный список приложений.
    ResponseApps(Vec<AppEntry>),
    /// Ошибка обработки запроса; клиент показывает её пользователю
    /// (попадает в `LauncherState.error`).
    Error(String),
}

impl IPCMessage {
    /// Собрать запрос поиска с текущей [`PROTOCOL_VERSION`].
    pub fn request_search(query: impl Into<String>) -> Self {
        Self::RequestSearch {
            version: PROTOCOL_VERSION,
            query: query.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Сериализация / десериализация фреймов
// ---------------------------------------------------------------------------

/// Упаковать сообщение в готовый к отправке фрейм:
/// `[u32 LE длина JSON] [JSON байты]`.
///
/// Ошибка, если тело больше [`MAX_FRAME_SIZE`] (длину нельзя записать
/// в u32 без переполнения).
pub fn encode_frame(msg: &IPCMessage) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(msg).map_err(|e| format!("Serialization error: {e}"))?;
    if json.len() > MAX_FRAME_SIZE {
        return Err(format!(
            "Frame too large: {} bytes (max {MAX_FRAME_SIZE})",
            json.len()
        ));
    }
    // Проверка выше гарантирует, что длина влезает в u32; try_from оставлен
    // как явная гарантия отсутствия усечения.
    let len = u32::try_from(json.len())
        .map_err(|_| format!("Frame length {} does not fit in u32", json.len()))?;
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// Распарсить тело фрейма (после отрезания 4-байтового префикса длины).
pub fn decode_frame(data: &[u8]) -> Result<IPCMessage, String> {
    if data.len() > MAX_FRAME_SIZE {
        return Err(format!(
            "Frame too large: {} bytes (max {MAX_FRAME_SIZE})",
            data.len()
        ));
    }
    serde_json::from_slice(data).map_err(|e| format!("Deserialization error: {e}"))
}

/// Прочитать 4-байтовый LE-префикс длины из сырого буфера.
#[inline]
pub fn read_frame_length(buf: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*buf)
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
            "hiren-shared-test-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn socket_path_override_wins_over_everything() {
        let path = resolve_socket_path(
            Some(OsString::from("/run/user/1000/custom.sock")),
            Some(OsString::from("/run/user/1000")),
            Some(OsString::from("/tmp")),
            Some(1000),
        );
        assert_eq!(path, PathBuf::from("/run/user/1000/custom.sock"));
    }

    #[test]
    fn socket_path_prefers_xdg_runtime_dir() {
        let path = resolve_socket_path(
            None,
            Some(OsString::from("/run/user/1000")),
            Some(OsString::from("/var/tmp")),
            Some(1000),
        );
        assert_eq!(path, PathBuf::from("/run/user/1000/hiren.socket"));
    }

    #[test]
    fn socket_path_falls_back_to_tmpdir_with_uid() {
        let path = resolve_socket_path(None, None, Some(OsString::from("/var/tmp")), Some(1000));
        assert_eq!(path, PathBuf::from("/var/tmp/hiren-1000.socket"));
    }

    #[test]
    fn socket_path_falls_back_to_tmp_with_uid() {
        let path = resolve_socket_path(None, None, None, Some(1000));
        assert_eq!(path, PathBuf::from("/tmp/hiren-1000.socket"));
    }

    #[test]
    fn socket_path_honors_override_env() {
        let dir = temp_dir("override-env");
        let want = dir.join("explicit.sock");
        // SAFETY: единственный тест, который трогает HIREN_SOCKET_PATH;
        // остальные тесты вызывают resolve_socket_path напрямую.
        std::env::set_var(SOCKET_PATH_ENV, &want);
        let got = socket_path();
        std::env::remove_var(SOCKET_PATH_ENV);
        assert_eq!(got, want);
    }

    #[test]
    fn current_uid_is_available_on_linux() {
        #[cfg(target_os = "linux")]
        {
            let uid = current_uid().expect("geteuid must be available on Linux");
            assert_ne!(uid, u32::MAX, "uid must be a real value");
        }
    }

    #[test]
    fn frame_roundtrip_request() {
        let msg = IPCMessage::request_search("fire fox");
        let frame = encode_frame(&msg).expect("encode");
        assert_eq!(&frame[..4], &(frame.len() as u32 - 4).to_le_bytes());
        assert_eq!(decode_frame(&frame[4..]).expect("decode"), msg);
    }

    #[test]
    fn frame_roundtrip_response_and_error() {
        let entries = vec![
            AppEntry::drun(
                "firefox".into(),
                "Firefox".into(),
                "/usr/bin/firefox".into(),
                Some("Web browser".into()),
                "Network WebBrowser".into(),
            )
            .with_icon("firefox".into())
            .with_terminal(true),
            AppEntry::run("ls".into(), "ls".into(), "/usr/bin/ls".into()),
        ];
        for msg in [
            IPCMessage::ResponseApps(entries),
            IPCMessage::ResponseApps(Vec::new()),
            IPCMessage::Error("daemon is busy".into()),
        ] {
            let frame = encode_frame(&msg).expect("encode");
            assert_eq!(decode_frame(&frame[4..]).expect("decode"), msg);
        }
    }

    #[test]
    fn frame_roundtrip_preserves_terminal_flag_and_defaults() {
        // AppEntry, сериализованный старым демоном без поля terminal,
        // должен десериализоваться с terminal=false.
        let json = br#"{"id":"x","name":"X","exec":"x","description":null,"icon":""}"#;
        let entry: AppEntry = serde_json::from_slice(json).expect("legacy entry");
        assert!(!entry.terminal);
        let encoded = serde_json::to_vec(&entry).expect("serialize");
        let back: AppEntry = serde_json::from_slice(&encoded).expect("roundtrip");
        assert!(!back.terminal);
        let with_terminal = AppEntry::drun("t".into(), "T".into(), "t".into(), None, String::new())
            .with_terminal(true);
        assert!(with_terminal.terminal);
    }

    #[test]
    fn encode_frame_rejects_oversized_body() {
        // ~400 КиБ JSON заметно больше лимита 256 КиБ.
        let big_name = "x".repeat(2000);
        let entries: Vec<AppEntry> = (0..400)
            .map(|i| AppEntry::run(format!("id{i}"), big_name.clone(), big_name.clone()))
            .collect();
        let err = encode_frame(&IPCMessage::ResponseApps(entries)).expect_err("must reject");
        assert!(err.contains("too large"), "unexpected error: {err}");
    }

    #[test]
    fn decode_rejects_oversized_and_empty_body() {
        let oversized = vec![b'x'; MAX_FRAME_SIZE + 1];
        assert!(decode_frame(&oversized).is_err());
        assert!(decode_frame(&[]).is_err());
    }

    #[test]
    fn max_frame_size_is_256_kib() {
        assert_eq!(MAX_FRAME_SIZE, 256 * 1024);
    }

    #[test]
    fn ensure_socket_dir_creates_private_directory() {
        let root = temp_dir("ensure-dir");
        let nested = root.join("a").join("b");
        let socket = nested.join("hiren.socket");
        ensure_socket_dir(&socket).expect("create dir");
        assert!(nested.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&nested).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_socket_dir_keeps_existing_directory() {
        let root = temp_dir("keep-dir");
        ensure_socket_dir(&root.join("hiren.socket")).expect("existing dir is fine");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn verify_socket_file_owner_accepts_own_socket_only() {
        use std::os::unix::net::UnixListener;

        let dir = temp_dir("owner-check");
        let socket = dir.join("hiren.socket");

        // Нет файла — ок (stale-сокета нет).
        verify_socket_file_owner(&socket).expect("absent is fine");

        // Свой сокет — ок.
        let listener = UnixListener::bind(&socket).expect("bind");
        verify_socket_file_owner(&socket).expect("own socket");
        drop(listener);

        // Обычный файл на месте сокета — отказ.
        std::fs::remove_file(&socket).expect("remove socket");
        std::fs::write(&socket, b"not a socket").expect("write file");
        assert!(verify_socket_file_owner(&socket).is_err());

        // Симлинк на сокет — отказ (lstat не следует по ссылке).
        let target = dir.join("real.sock");
        let listener = UnixListener::bind(&target).expect("bind real");
        std::fs::remove_file(&socket).expect("remove file");
        std::os::unix::fs::symlink(&target, &socket).expect("symlink");
        assert!(verify_socket_file_owner(&socket).is_err());
        drop(listener);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn tighten_socket_permissions_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        let dir = temp_dir("perm-check");
        let socket = dir.join("hiren.socket");
        let listener = UnixListener::bind(&socket).expect("bind");
        tighten_socket_permissions(&socket).expect("chmod");
        let mode = std::fs::metadata(&socket).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn peer_uid_matches_own_uid() {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;

        let (a, b) = UnixStream::pair().expect("socketpair");
        let own = current_uid().expect("uid");
        verify_peer_uid(a.as_raw_fd()).expect("peer uid");
        verify_peer_uid(b.as_raw_fd()).expect("peer uid");
        assert_eq!(sys::peer_uid(a.as_raw_fd()).expect("peer uid"), own);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn peer_uid_rejects_invalid_descriptor() {
        assert!(sys::peer_uid(-1).is_err());
        assert!(verify_peer_uid(-1).is_err());
    }
}
