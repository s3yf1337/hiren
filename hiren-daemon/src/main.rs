//! hiren-daemon — фоновый демон лаунчера.
//!
//! - Сканирует каталоги `applications/` из XDG data dirs (рекурсивно),
//!   кэширует распарсенные `.desktop`-файлы в `Arc<RwLock<Vec<AppEntry>>>`;
//! - пересобирает кэш при изменениях через `notify` (с дебаунсом и
//!   ограничением максимальной задержки);
//! - слушает UNIX-сокет по [`hiren_shared::socket_path`] с правами 0600,
//!   проверяет `SO_PEERCRED` и ограничивает число соединений;
//! - обрабатывает `RequestSearch` с помощью `fuzzy-matcher` (skim).
//!
//! Безопасность сокета (P0):
//! - сокет по умолчанию лежит в приватном `$XDG_RUNTIME_DIR` (или в
//!   `$TMPDIR`/`/tmp` с uid в имени — fallback);
//! - файл сокета получает права 0600;
//! - перед удалением stale-сокета проверяется `lstat`: это должен быть
//!   сокет текущего uid (чужой файл/симлинк не трогаем, а отказываемся
//!   стартовать);
//! - если сокет уже обслуживается — демон выходит штатно (single instance);
//! - каждый клиент проверяется через `SO_PEERCRED` (uid == наш euid).

mod desktop;

use anyhow::{Context, Result};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use hiren_shared::{
    decode_frame, encode_frame, read_frame_length, verify_peer_uid, AppEntry, IPCMessage,
    MAX_FRAME_SIZE, MAX_QUERY_LEN, PROTOCOL_VERSION,
};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{RwLock, Semaphore};
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Ограничения DoS
// ---------------------------------------------------------------------------

/// Максимум одновременных обслуживаемых соединений. Новые подключения
/// сверх лимита сразу закрываются (accept-loop не блокируется).
const MAX_CONNECTIONS: usize = 32;

/// Таймаут на чтение одного фрейма и на запись ответа.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Пауза после ошибки accept(), чтобы не крутить цикл на EMFILE и т.п.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Дебаунс событий файлового вотчера.
const REBUILD_DEBOUNCE: Duration = Duration::from_millis(500);

/// Максимальная задержка пересборки под непрерывным потоком событий:
/// даже если изменения идут без пауз, кэш пересоберётся не позже этого срока.
const REBUILD_MAX_LATENCY: Duration = Duration::from_secs(2);

/// Период пробуждения ватчера (пересинхронизация списка watched-каталогов).
const WATCH_TICK: Duration = Duration::from_millis(250);

/// Максимальная глубина рекурсивного обхода `applications/` (защита от циклов
/// через симлинки; каталоги-симлинки не обходятся вообще).
const MAX_SCAN_DEPTH: usize = 8;

// ---------------------------------------------------------------------------
// XDG data dirs
// ---------------------------------------------------------------------------

fn non_empty_env(key: &str) -> Option<OsString> {
    std::env::var_os(key).filter(|value| !value.is_empty())
}

fn home_dir() -> Option<PathBuf> {
    non_empty_env("HOME").map(PathBuf::from)
}

/// Каталоги XDG data в порядке приоритета (первый — самый приоритетный):
/// `$XDG_DATA_HOME` (или `~/.local/share`), `$XDG_DATA_DIRS`
/// (по умолчанию `/usr/local/share:/usr/share`), затем `~/.local/share`.
///
/// `~/.local/share` добавляется даже при заданном `XDG_DATA_HOME`:
/// так не теряются записи, установленные в стандартный каталог.
fn data_dirs() -> Vec<PathBuf> {
    data_dirs_from(
        non_empty_env("XDG_DATA_HOME").map(PathBuf::from),
        non_empty_env("XDG_DATA_DIRS"),
        home_dir(),
    )
}

/// Чистое ядро [`data_dirs`] (тестируемо без переменных окружения).
fn data_dirs_from(
    xdg_data_home: Option<PathBuf>,
    xdg_data_dirs: Option<OsString>,
    home: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let home_local = home.map(|home| home.join(".local/share"));

    match xdg_data_home {
        Some(dir) => dirs.push(dir),
        None => {
            if let Some(local) = &home_local {
                dirs.push(local.clone());
            }
        }
    }

    let system = xdg_data_dirs.unwrap_or_else(|| OsString::from("/usr/local/share:/usr/share"));
    for dir in system.to_string_lossy().split(':') {
        if !dir.trim().is_empty() {
            dirs.push(PathBuf::from(dir.trim()));
        }
    }

    if let Some(local) = home_local {
        dirs.push(local);
    }

    dedupe_paths(dirs)
}

/// Каталоги `applications/` в порядке приоритета.
fn applications_dirs() -> Vec<PathBuf> {
    dedupe_paths(
        data_dirs()
            .into_iter()
            .map(|dir| dir.join("applications"))
            .collect(),
    )
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Сканирование .desktop-файлов
// ---------------------------------------------------------------------------

/// Просканировать `applications/` во всех приоритетных каталогах.
///
/// Дедупликация — по desktop-id с учётом приоритета XDG:
/// - первый найденный файл с данным id «выигрывает» — файлы с тем же id в
///   каталогах с более низким приоритетом игнорируются целиком;
/// - `Hidden=true` в файле высокого приоритета — tombstone: запись с тем же
///   id из более низкого приоритета удаляется (не показывается);
/// - одинаковый `Exec` у разных id — это разные приложения, оба остаются.
fn scan_dirs(apps_dirs: &[PathBuf], ctx: &desktop::ParseContext) -> Vec<AppEntry> {
    let mut by_id: HashMap<String, Option<AppEntry>> = HashMap::new();

    for apps_dir in apps_dirs {
        if !apps_dir.is_dir() {
            log::debug!("Directory not found: {}", apps_dir.display());
            continue;
        }
        let mut files = Vec::new();
        walk_desktop_files(apps_dir, apps_dir, 0, &mut files);
        // Детерминированный порядок внутри каталога.
        files.sort_by(|a, b| a.0.cmp(&b.0));

        for (id, path) in files {
            // Приоритет: первый каталог выигрывает, включая tombstone/no-display.
            if by_id.contains_key(&id) {
                continue;
            }
            match desktop::parse_desktop_file_with(&path, ctx) {
                desktop::DesktopFile::Entry(mut app) => {
                    log::debug!("Parsed: {} -> {}", id, app.name);
                    app.id = id.clone();
                    by_id.insert(id, Some(app));
                }
                desktop::DesktopFile::Hidden => {
                    log::debug!("Hidden tombstone: {id}");
                    by_id.insert(id, None);
                }
                desktop::DesktopFile::Skip => {
                    by_id.insert(id, None);
                }
            }
        }
    }

    let mut entries: Vec<AppEntry> = by_id.into_values().flatten().collect();
    // Стабильный порядок кэша: по id, затем по имени (для одинаковых id быть
    // не может, но это делает порядок детерминированным в любом случае).
    entries.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.name.cmp(&b.name)));
    entries
}

/// Просканировать все XDG `applications/` с текущим контекстом окружения.
fn scan_desktop_entries() -> Vec<AppEntry> {
    scan_dirs(&applications_dirs(), &desktop::ParseContext::from_env())
}

fn walk_desktop_files(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    if depth > MAX_SCAN_DEPTH {
        log::warn!("Max scan depth reached at {}; skipping", dir.display());
        return;
    }
    let read_dir = match std::fs::read_dir(dir) {
        Ok(read_dir) => read_dir,
        Err(e) => {
            log::warn!("Cannot read {}: {e}", dir.display());
            return;
        }
    };
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                log::warn!("Skipping unreadable entry in {}: {e}", dir.display());
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(e) => {
                log::debug!("Cannot stat {}: {e}", path.display());
                continue;
            }
        };
        if file_type.is_dir() {
            walk_desktop_files(root, &path, depth + 1, out);
        } else if file_type.is_symlink() {
            // Симлинк на файл — принимаем (реальные .desktop часто симлинки);
            // симлинк на каталог не обходим (защита от циклов).
            let is_file = std::fs::metadata(&path).map(|meta| meta.is_file()).unwrap_or(false);
            if is_file && has_desktop_extension(&path) {
                if let Some(id) = desktop_id(root, &path) {
                    out.push((id, path));
                }
            }
        } else if file_type.is_file() && has_desktop_extension(&path) {
            if let Some(id) = desktop_id(root, &path) {
                out.push((id, path));
            }
        }
    }
}

fn has_desktop_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("desktop"))
}

/// desktop-id: путь относительно `applications/` без `.desktop`, где `/`
/// заменён на `-` (напр. `kde/foo.desktop` → `kde-foo`).
fn desktop_id(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let relative = relative.with_extension("");
    let mut id = String::new();
    for component in relative.components() {
        let part = component.as_os_str().to_str()?;
        if part.is_empty() {
            continue;
        }
        if !id.is_empty() {
            id.push('-');
        }
        id.push_str(part);
    }
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

// ---------------------------------------------------------------------------
// Состояние демона
// ---------------------------------------------------------------------------

struct AppState {
    cache: RwLock<Vec<AppEntry>>,
    matcher: SkimMatcherV2,
    rt_handle: tokio::runtime::Handle,
}

impl AppState {
    fn new(rt_handle: tokio::runtime::Handle) -> Self {
        Self {
            cache: RwLock::new(Vec::new()),
            matcher: SkimMatcherV2::default(),
            rt_handle,
        }
    }

    /// Пересобрать кэш из файловой системы (тяжёлый IO — в blocking-пуле).
    async fn rebuild_cache(&self) -> Result<()> {
        let entries = tokio::task::spawn_blocking(scan_desktop_entries)
            .await
            .context("desktop scan task failed")?;
        log::info!("Cache rebuilt: {} entries", entries.len());
        *self.cache.write().await = entries;
        Ok(())
    }

    /// Выполнить размытый поиск по кэшу. Результат отсортирован по
    /// релевантности, затем по имени/id — одинаковые запросы дают одинаковый
    /// порядок между перезапусками.
    async fn search(&self, query: &str) -> Vec<AppEntry> {
        let cache = self.cache.read().await;
        rank_entries(&self.matcher, &cache, query)
    }
}

/// Ранжирование записей (чистая функция — тестируется без рантайма).
fn rank_entries(matcher: &SkimMatcherV2, entries: &[AppEntry], query: &str) -> Vec<AppEntry> {
    let query = query.trim();
    if query.is_empty() {
        // Кэш уже отсортирован при пересборке — стабильный порядок.
        return entries.to_vec();
    }

    let mut scored: Vec<(i64, AppEntry)> = entries
        .iter()
        .filter_map(|entry| {
            // Матчим имя и keywords отдельно. Совпадение по имени весит
            // полностью, по keywords — со штрафом (0.4), чтобы точное
            // попадание в название всегда было выше случайного совпадения
            // в ключевых словах (иначе "steam" лезет вверх из-за
            // telegram/sms в keywords у AyuGram).
            let name_score = matcher.fuzzy_match(&entry.name, query);
            let kw_score = matcher
                .fuzzy_match(&entry.keywords, query)
                .map(|score| (score as f64 * 0.4) as i64);
            let score = name_score.max(kw_score).unwrap_or(0);
            if score > 0 {
                Some((score, entry.clone()))
            } else {
                None
            }
        })
        .collect();

    // score desc, затем name asc и id asc — детерминированный tie-break.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.name.to_lowercase().cmp(&b.1.name.to_lowercase()))
            .then_with(|| a.1.id.cmp(&b.1.id))
    });

    scored
        .into_iter()
        .map(|(score, mut entry)| {
            entry.score = score;
            entry
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Файловый вотчер
// ---------------------------------------------------------------------------

/// Желаемые наблюдения: существующие `applications/` — рекурсивно; если
/// каталог ещё не создан — его ближайший существующий родитель
/// (NonRecursive), чтобы поймать создание каталога после старта.
fn desired_watches() -> HashMap<PathBuf, RecursiveMode> {
    let mut desired: HashMap<PathBuf, RecursiveMode> = HashMap::new();
    for apps_dir in applications_dirs() {
        if apps_dir.is_dir() {
            desired.insert(apps_dir, RecursiveMode::Recursive);
            continue;
        }
        for ancestor in apps_dir.ancestors().skip(1) {
            if ancestor.is_dir() {
                desired
                    .entry(ancestor.to_path_buf())
                    .or_insert(RecursiveMode::NonRecursive);
                break;
            }
        }
    }
    desired
}

/// Привести набор наблюдаемых путей в соответствие с текущим состоянием
/// (каталоги могли появиться/исчезнуть после старта).
fn sync_watches(
    watcher: &mut notify::RecommendedWatcher,
    watched: &mut HashMap<PathBuf, RecursiveMode>,
) {
    let desired = desired_watches();

    let stale: Vec<PathBuf> = watched
        .keys()
        .filter(|path| !desired.contains_key(*path))
        .cloned()
        .collect();
    for path in stale {
        if let Err(e) = watcher.unwatch(&path) {
            log::debug!("Cannot unwatch {}: {e}", path.display());
        }
        watched.remove(&path);
    }

    for (path, mode) in desired {
        if watched.get(&path) == Some(&mode) {
            continue;
        }
        if watched.contains_key(&path) {
            let _ = watcher.unwatch(&path);
        }
        match watcher.watch(&path, mode) {
            Ok(()) => {
                log::info!("Watching {} ({mode:?})", path.display());
                watched.insert(path, mode);
            }
            Err(e) => log::warn!("Cannot watch {}: {e}", path.display()),
        }
    }
}

fn spawn_watcher(state: Arc<AppState>) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if tx.send(res).is_err() {
            log::error!("Watcher channel closed");
        }
    })
    .context("Failed to create file watcher")?;

    let mut watched: HashMap<PathBuf, RecursiveMode> = HashMap::new();
    sync_watches(&mut watcher, &mut watched);

    // Фоновый поток: слушает события, дебаунсит и пересобирает кэш.
    std::thread::spawn(move || {
        let mut watcher = watcher;
        let mut needs_rebuild = false;
        let mut first_event: Option<Instant> = None;
        let mut last_event = Instant::now();

        loop {
            match rx.recv_timeout(WATCH_TICK) {
                Ok(Ok(event)) => {
                    if is_relevant_event(&event) {
                        log::debug!("Watcher event: {:?}", event.kind);
                        needs_rebuild = true;
                        let now = Instant::now();
                        last_event = now;
                        first_event.get_or_insert(now);
                    }
                }
                Ok(Err(e)) => log::error!("Watcher error: {e}"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    log::error!("Watcher channel disconnected");
                    break;
                }
            }

            // Пересинхронизация: каталоги могли появиться или исчезнуть.
            sync_watches(&mut watcher, &mut watched);

            if should_rebuild(
                needs_rebuild,
                last_event.elapsed(),
                first_event.map(|first| first.elapsed()),
                REBUILD_DEBOUNCE,
                REBUILD_MAX_LATENCY,
            ) {
                log::info!("Rebuilding cache after file changes…");
                if let Err(e) = state.rt_handle.block_on(state.rebuild_cache()) {
                    log::error!("Cache rebuild failed: {e:#}");
                }
                needs_rebuild = false;
                first_event = None;
            }
        }
    });

    Ok(())
}

fn is_relevant_event(event: &Event) -> bool {
    use notify::event::ModifyKind;
    matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_))
            | EventKind::Remove(_)
    )
}

/// Решение о пересборке: после затишья (дебаунс) или принудительно по
/// максимальной задержке, чтобы непрерывный поток событий не откладывал
/// пересборку бесконечно.
fn should_rebuild(
    needs_rebuild: bool,
    since_last_event: Duration,
    since_first_event: Option<Duration>,
    debounce: Duration,
    max_latency: Duration,
) -> bool {
    if !needs_rebuild {
        return false;
    }
    if since_last_event >= debounce {
        return true;
    }
    since_first_event.is_some_and(|elapsed| elapsed >= max_latency)
}

// ---------------------------------------------------------------------------
// Обработка подключений
// ---------------------------------------------------------------------------

/// Отправить сообщение клиенту с таймаутом.
async fn send_message(stream: &mut UnixStream, msg: &IPCMessage) -> Result<()> {
    let frame = encode_frame(msg).map_err(|e| anyhow::anyhow!("encode error: {e}"))?;
    match timeout(IO_TIMEOUT, stream.write_all(&frame)).await {
        Ok(result) => result.context("write to client failed"),
        Err(_) => Err(anyhow::anyhow!("write timeout after {}s", IO_TIMEOUT.as_secs())),
    }
}

/// Обслужить одно соединение. Любой невалидный фрейм/сообщение закрывает
/// соединение (после попытки отправить `IPCMessage::Error`).
async fn handle_connection(mut stream: UnixStream, state: Arc<AppState>) -> Result<()> {
    loop {
        // Читаем 4-байтовый префикс длины.
        let mut len_buf = [0u8; 4];
        match timeout(IO_TIMEOUT, stream.read_exact(&mut len_buf)).await {
            Err(_) => {
                log::debug!("Read timeout (frame length); closing connection");
                return Ok(());
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                log::debug!("Client disconnected");
                return Ok(());
            }
            Ok(Err(e)) => return Err(e).context("failed to read frame length"),
            Ok(Ok(_)) => {}
        }

        let body_len = read_frame_length(&len_buf) as usize;
        // Проверка до аллокации: никаких Vec по длине из сети.
        if body_len == 0 || body_len > MAX_FRAME_SIZE {
            log::warn!("Invalid frame length {body_len} (max {MAX_FRAME_SIZE}); closing");
            let _ = send_message(
                &mut stream,
                &IPCMessage::Error(format!("invalid frame length {body_len}")),
            )
            .await;
            return Ok(());
        }

        let mut body = vec![0u8; body_len];
        match timeout(IO_TIMEOUT, stream.read_exact(&mut body)).await {
            Err(_) => {
                log::debug!("Read timeout (frame body); closing connection");
                return Ok(());
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                log::debug!("Client disconnected mid-frame");
                return Ok(());
            }
            Ok(Err(e)) => return Err(e).context("failed to read frame body"),
            Ok(Ok(_)) => {}
        }

        let msg = match decode_frame(&body) {
            Ok(msg) => msg,
            Err(e) => {
                log::warn!("Malformed frame: {e}; closing connection");
                let _ = send_message(&mut stream, &IPCMessage::Error(format!("malformed request: {e}")))
                    .await;
                return Ok(());
            }
        };

        match msg {
            IPCMessage::RequestSearch { version, query } => {
                if version != PROTOCOL_VERSION {
                    log::warn!(
                        "Protocol version mismatch: client {version}, daemon {PROTOCOL_VERSION}"
                    );
                    let _ = send_message(
                        &mut stream,
                        &IPCMessage::Error(format!(
                            "protocol version mismatch: client {version}, daemon {PROTOCOL_VERSION}"
                        )),
                    )
                    .await;
                    return Ok(());
                }
                if query.chars().count() > MAX_QUERY_LEN {
                    log::warn!("Query too long: {} chars; closing", query.chars().count());
                    let _ = send_message(
                        &mut stream,
                        &IPCMessage::Error(format!(
                            "query too long (max {MAX_QUERY_LEN} characters)"
                        )),
                    )
                    .await;
                    return Ok(());
                }
                let results = state.search(&query).await;
                send_message(&mut stream, &IPCMessage::ResponseApps(results)).await?;
            }
            other => {
                log::warn!("Unexpected message from client: {other:?}; closing");
                let _ = send_message(&mut stream, &IPCMessage::Error("unexpected message".into()))
                    .await;
                return Ok(());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Подготовка сокета
// ---------------------------------------------------------------------------

/// Результат [`prepare_socket_location`].
#[derive(Debug, PartialEq, Eq)]
enum SocketPreparation {
    /// Путь свободен (stale-сокет удалён) — можно bind().
    Ready,
    /// Сокет уже обслуживается другим демоном — нужно штатно выйти.
    AlreadyRunning,
}

/// Подготовить путь сокета до старта рантайма:
/// 1. создать родительский каталог (0700), если нужно;
/// 2. если файл существует — только `lstat` + проверка владельца
///    (чужой файл/симлинк → отказ, а не удаление);
/// 3. если сокет отвечает на connect — другой демон уже работает;
/// 4. иначе удалить stale-сокет и вернуть [`SocketPreparation::Ready`].
fn prepare_socket_location(path: &Path) -> Result<SocketPreparation> {
    hiren_shared::ensure_socket_dir(path)
        .with_context(|| format!("cannot create socket directory for {}", path.display()))?;

    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SocketPreparation::Ready),
        Err(e) => {
            Err(e).with_context(|| format!("cannot inspect socket path {}", path.display()))
        }
        Ok(_) => {
            // Чужой файл, обычный файл или симлинк — отказ (fail closed).
            hiren_shared::verify_socket_file_owner(path)
                .with_context(|| format!("refusing to use socket path {}", path.display()))?;

            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => {
                    log::info!(
                        "Another hiren-daemon is already running on {}; exiting",
                        path.display()
                    );
                    Ok(SocketPreparation::AlreadyRunning)
                }
                Err(e) => {
                    log::info!("Removing stale socket {} ({e})", path.display());
                    std::fs::remove_file(path).with_context(|| {
                        format!("cannot remove stale socket {}", path.display())
                    })?;
                    Ok(SocketPreparation::Ready)
                }
            }
        }
    }
}

/// Bind + chmod 0600 + проверка прав. `Ok(None)` — гонку выиграл другой
/// демон (сокет уже обслуживается), нужно выйти штатно.
async fn bind_and_harden(path: &Path) -> Result<Option<UnixListener>> {
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(e) => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                log::info!(
                    "Another hiren-daemon won the bind race on {}; exiting",
                    path.display()
                );
                return Ok(None);
            }
            return Err(e).with_context(|| format!("failed to bind socket {}", path.display()));
        }
    };

    hiren_shared::tighten_socket_permissions(path)
        .with_context(|| format!("failed to set 0600 permissions on {}", path.display()))?;

    // Fail closed: если права не 0600, IPC-сокет не должен работать.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("failed to stat socket {}", path.display()))?
            .permissions()
            .mode()
            & 0o777;
        if mode != 0o600 {
            anyhow::bail!(
                "socket {} has mode {mode:o}, expected 600",
                path.display()
            );
        }
    }

    Ok(Some(listener))
}

// ---------------------------------------------------------------------------
// Точка входа
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let socket_path = hiren_shared::socket_path();
    match prepare_socket_location(&socket_path)? {
        SocketPreparation::AlreadyRunning => return Ok(()),
        SocketPreparation::Ready => {}
    }
    log::info!("hiren-daemon socket: {}", socket_path.display());

    let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;
    rt.block_on(async_main(socket_path))
}

async fn async_main(socket_path: PathBuf) -> Result<()> {
    let state = Arc::new(AppState::new(tokio::runtime::Handle::current()));
    state
        .rebuild_cache()
        .await
        .context("Initial cache rebuild failed")?;

    spawn_watcher(state.clone())?;

    let Some(listener) = bind_and_harden(&socket_path).await? else {
        return Ok(());
    };
    log::info!("hiren-daemon listening on {}", socket_path.display());

    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(connection) => connection,
            Err(e) => {
                // accept() не должен ронять демон: логируем, ждём и продолжаем.
                log::warn!("accept() failed: {e}");
                tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                continue;
            }
        };

        // SO_PEERCRED: обслуживаем только процессы своего uid.
        if let Err(e) = verify_peer_uid(stream.as_raw_fd()) {
            log::warn!("Rejecting connection from {addr:?}: {e}");
            continue;
        }

        // Ограничение одновременных соединений: превышение — сразу закрываем.
        let permit = match connections.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                log::warn!(
                    "Connection limit ({MAX_CONNECTIONS}) reached; dropping {addr:?}"
                );
                continue;
            }
        };

        let state = state.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle_connection(stream, state).await {
                log::warn!("Connection error: {e:#}");
            }
        });
    }
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
            "hiren-daemon-test-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_desktop(root: &Path, relative: &str, content: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
        std::fs::write(&path, content).expect("write desktop");
        path
    }

    fn desktop(name: &str, exec: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n")
    }

    // -- data dirs -----------------------------------------------------

    #[test]
    fn data_dirs_defaults_and_precedence() {
        let dirs = data_dirs_from(None, None, Some(PathBuf::from("/home/u")));
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/home/u/.local/share"),
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ]
        );
    }

    #[test]
    fn data_dirs_xdg_home_first_and_home_local_kept_last() {
        let dirs = data_dirs_from(
            Some(PathBuf::from("/xdg/data")),
            Some(OsString::from("/opt/share:/usr/share")),
            Some(PathBuf::from("/home/u")),
        );
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/xdg/data"),
                PathBuf::from("/opt/share"),
                PathBuf::from("/usr/share"),
                PathBuf::from("/home/u/.local/share"),
            ]
        );
    }

    #[test]
    fn data_dirs_dedupes_identical_paths() {
        let dirs = data_dirs_from(
            Some(PathBuf::from("/usr/share")),
            Some(OsString::from("/usr/share:/usr/share")),
            None,
        );
        assert_eq!(dirs, vec![PathBuf::from("/usr/share")]);
    }

    #[test]
    fn applications_dirs_are_under_applications() {
        let dirs = data_dirs_from(
            Some(PathBuf::from("/xdg/data")),
            Some(OsString::from("/usr/share")),
            None,
        );
        let apps: Vec<PathBuf> = dirs
            .into_iter()
            .map(|dir| dir.join("applications"))
            .collect();
        assert_eq!(
            apps,
            vec![
                PathBuf::from("/xdg/data/applications"),
                PathBuf::from("/usr/share/applications")
            ]
        );
    }

    // -- desktop-id / scanning ----------------------------------------

    #[test]
    fn desktop_id_uses_relative_path_with_dashes() {
        let root = Path::new("/usr/share/applications");
        assert_eq!(
            desktop_id(root, &root.join("firefox.desktop")),
            Some("firefox".to_string())
        );
        assert_eq!(
            desktop_id(root, &root.join("kde/kfoo.desktop")),
            Some("kde-kfoo".to_string())
        );
        assert_eq!(
            desktop_id(root, &root.join("a/b/c.desktop")),
            Some("a-b-c".to_string())
        );
        assert_eq!(desktop_id(root, Path::new("/other/x.desktop")), None);
    }

    #[test]
    fn scan_reads_system_and_user_dirs_recursively() {
        let sys = temp_dir("scan-sys");
        let local = temp_dir("scan-local");
        write_desktop(&sys, "foo.desktop", &desktop("Foo", "/usr/bin/foo"));
        write_desktop(
            &sys,
            "nested/bar.desktop",
            &desktop("Bar", "/usr/bin/bar"),
        );
        std::fs::create_dir_all(local.join("applications")).expect("local apps");

        let ctx = desktop::ParseContext::default();
        let entries = scan_dirs(
            &[sys.clone(), local.join("applications")],
            &ctx,
        );
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["foo", "nested-bar"]);
        assert_eq!(entries[0].name, "Foo");
        assert_eq!(entries[1].name, "Bar");

        let _ = std::fs::remove_dir_all(&sys);
        let _ = std::fs::remove_dir_all(&local);
    }

    #[test]
    fn higher_precedence_file_overrides_whole_entry() {
        let high = temp_dir("override-high");
        let low = temp_dir("override-low");
        write_desktop(&low, "app.desktop", &desktop("Low Name", "/usr/bin/low"));
        write_desktop(&high, "app.desktop", &desktop("High Name", "/usr/bin/high"));

        let ctx = desktop::ParseContext::default();
        let entries = scan_dirs(&[high.clone(), low.clone()], &ctx);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "High Name");
        assert_eq!(entries[0].exec, "/usr/bin/high");

        let _ = std::fs::remove_dir_all(&high);
        let _ = std::fs::remove_dir_all(&low);
    }

    #[test]
    fn hidden_tombstone_removes_lower_precedence_entry() {
        let high = temp_dir("hidden-high");
        let low = temp_dir("hidden-low");
        write_desktop(&low, "gone.desktop", &desktop("Gone", "/usr/bin/gone"));
        write_desktop(
            &high,
            "gone.desktop",
            "[Desktop Entry]\nType=Application\nHidden=true\n",
        );
        write_desktop(&low, "kept.desktop", &desktop("Kept", "/usr/bin/kept"));

        let ctx = desktop::ParseContext::default();
        let entries = scan_dirs(&[high.clone(), low.clone()], &ctx);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["kept"]);

        let _ = std::fs::remove_dir_all(&high);
        let _ = std::fs::remove_dir_all(&low);
    }

    #[test]
    fn hidden_tombstone_in_lower_precedence_does_not_remove_real_entry() {
        let high = temp_dir("tomb-high");
        let low = temp_dir("tomb-low");
        write_desktop(&high, "app.desktop", &desktop("Real", "/usr/bin/real"));
        write_desktop(
            &low,
            "app.desktop",
            "[Desktop Entry]\nType=Application\nHidden=true\n",
        );

        let ctx = desktop::ParseContext::default();
        let entries = scan_dirs(&[high.clone(), low.clone()], &ctx);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Real");

        let _ = std::fs::remove_dir_all(&high);
        let _ = std::fs::remove_dir_all(&low);
    }

    #[test]
    fn distinct_apps_with_same_exec_are_both_kept() {
        let dir = temp_dir("same-exec");
        write_desktop(&dir, "a.desktop", &desktop("A", "/usr/bin/same"));
        write_desktop(&dir, "b.desktop", &desktop("B", "/usr/bin/same"));

        let ctx = desktop::ParseContext::default();
        let entries = scan_dirs(std::slice::from_ref(&dir), &ctx);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_order_is_deterministic_by_id() {
        let dir = temp_dir("order");
        write_desktop(&dir, "z.desktop", &desktop("Z", "/usr/bin/z"));
        write_desktop(&dir, "a.desktop", &desktop("A", "/usr/bin/a"));
        write_desktop(&dir, "m.desktop", &desktop("M", "/usr/bin/m"));

        let ctx = desktop::ParseContext::default();
        let entries = scan_dirs(std::slice::from_ref(&dir), &ctx);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "m", "z"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- ranking -------------------------------------------------------

    fn entry(id: &str, name: &str) -> AppEntry {
        AppEntry::run(id.to_string(), name.to_string(), format!("/usr/bin/{id}"))
    }

    #[test]
    fn empty_query_returns_cache_order() {
        let matcher = SkimMatcherV2::default();
        let cache = vec![entry("a", "Alpha"), entry("b", "Beta")];
        let ranked = rank_entries(&matcher, &cache, "  ");
        let ids: Vec<&str> = ranked.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn ranking_tiebreak_is_deterministic() {
        let matcher = SkimMatcherV2::default();
        // Одинаковые имена/скоры — порядок должен решаться id.
        let cache = vec![
            entry("zz", "Same"),
            entry("aa", "Same"),
            entry("mm", "Same"),
        ];
        let ranked = rank_entries(&matcher, &cache, "same");
        let ids: Vec<&str> = ranked.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["aa", "mm", "zz"]);
        assert!(ranked.iter().all(|e| e.score > 0));
    }

    #[test]
    fn ranking_prefers_name_over_keywords() {
        let matcher = SkimMatcherV2::default();
        let mut kw = entry("kw", "Telegram");
        kw.keywords = "steam".into();
        let named = entry("name", "Steam");
        let ranked = rank_entries(&matcher, &[kw, named], "steam");
        assert_eq!(ranked[0].id, "name");
    }

    // -- debounce policy ----------------------------------------------

    #[test]
    fn rebuild_needs_events() {
        assert!(!should_rebuild(
            false,
            Duration::from_secs(10),
            None,
            REBUILD_DEBOUNCE,
            REBUILD_MAX_LATENCY
        ));
    }

    #[test]
    fn rebuild_after_quiet_period() {
        assert!(should_rebuild(
            true,
            Duration::from_millis(600),
            Some(Duration::from_millis(600)),
            REBUILD_DEBOUNCE,
            REBUILD_MAX_LATENCY
        ));
    }

    #[test]
    fn rebuild_is_forced_by_max_latency_under_event_stream() {
        // События идут непрерывно: с последнего < debounce, но с первого
        // прошло больше max_latency — пересобираем принудительно.
        assert!(should_rebuild(
            true,
            Duration::from_millis(10),
            Some(REBUILD_MAX_LATENCY),
            REBUILD_DEBOUNCE,
            REBUILD_MAX_LATENCY
        ));
        assert!(!should_rebuild(
            true,
            Duration::from_millis(10),
            Some(Duration::from_millis(100)),
            REBUILD_DEBOUNCE,
            REBUILD_MAX_LATENCY
        ));
    }

    // -- socket preparation -------------------------------------------

    #[test]
    fn prepare_socket_ready_for_fresh_path() {
        let dir = temp_dir("prep-fresh");
        let path = dir.join("hiren.socket");
        assert_eq!(
            prepare_socket_location(&path).expect("prepare"),
            SocketPreparation::Ready
        );
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_socket_detects_running_instance() {
        use std::os::unix::net::UnixListener;
        let dir = temp_dir("prep-live");
        let path = dir.join("hiren.socket");
        let _listener = UnixListener::bind(&path).expect("bind");
        assert_eq!(
            prepare_socket_location(&path).expect("prepare"),
            SocketPreparation::AlreadyRunning
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_socket_removes_stale_socket() {
        use std::os::unix::net::UnixListener;
        let dir = temp_dir("prep-stale");
        let path = dir.join("hiren.socket");
        {
            let listener = UnixListener::bind(&path).expect("bind");
            drop(listener); // путь остаётся, но никто не слушает
        }
        assert!(path.exists());
        assert_eq!(
            prepare_socket_location(&path).expect("prepare"),
            SocketPreparation::Ready
        );
        assert!(!path.exists(), "stale socket must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_socket_refuses_foreign_regular_file() {
        let dir = temp_dir("prep-file");
        let path = dir.join("hiren.socket");
        std::fs::write(&path, b"do not delete me").expect("write");
        assert!(prepare_socket_location(&path).is_err());
        // Чужой/не-сокет файл не удалён.
        assert_eq!(
            std::fs::read(&path).expect("still there"),
            b"do not delete me"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn prepare_socket_refuses_symlink() {
        use std::os::unix::net::UnixListener;
        let dir = temp_dir("prep-symlink");
        let target = dir.join("target.sock");
        let path = dir.join("hiren.socket");
        let _listener = UnixListener::bind(&target).expect("bind");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");
        assert!(prepare_socket_location(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bind_and_harden_creates_0600_socket() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("bind-harden");
        let path = dir.join("hiren.socket");
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let listener = rt
            .block_on(bind_and_harden(&path))
            .expect("bind")
            .expect("listener");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        // Сокет живой: клиент может подключиться.
        let _client = std::os::unix::net::UnixStream::connect(&path).expect("connect");
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
