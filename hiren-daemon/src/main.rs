//! hiren-daemon — фоновый демон лаунчера.
//!
//! - Сканирует `/usr/share/applications` и `~/.local/share/applications`
//! - Кэширует распарсенные `.desktop`-файлы в `Arc<RwLock<Vec<AppEntry>>>`
//! - Отслеживает изменения в этих директориях через `notify`
//! - Слушает UNIX-сокет `/tmp/hiren.socket`
//! - Обрабатывает `RequestSearch` с помощью `fuzzy-matcher` (skim)

mod desktop;

use anyhow::{Context, Result};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use hiren_shared::{decode_frame, encode_frame, read_frame_length, AppEntry, IPCMessage};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::RwLock;



/// Директории для сканирования `.desktop`-файлов.
fn desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/usr/share/applications")];
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    dirs
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

    /// Пересобрать кэш из файловой системы.
    async fn rebuild_cache(&self) -> Result<()> {
        let mut entries: Vec<AppEntry> = Vec::new();

        for dir in desktop_dirs() {
            if !dir.is_dir() {
                log::debug!("Directory not found: {}", dir.display());
                continue;
            }
            let read_dir = std::fs::read_dir(&dir)
                .with_context(|| format!("Failed to read {}", dir.display()))?;

            for entry in read_dir {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("Skipping unreadable entry: {e}");
                        continue;
                    }
                };
                let path = entry.path();
                if path.extension().map_or(false, |ext| ext == "desktop") {
                    if let Some(app) = desktop::parse_desktop_file(&path) {
                        log::debug!("Parsed: {} -> {}", app.id, app.name);
                        entries.push(app);
                    }
                }
            }
        }

        // Дедупликация по id: первый (из системной директории) — канонический,
        // локальный (~/.local/...) — перезаписывает.
        let mut seen = std::collections::HashMap::new();
        for entry in entries {
            seen.insert(entry.id.clone(), entry);
        }

        let unique: Vec<AppEntry> = seen.into_values().collect();
        log::info!("Cache rebuilt: {} entries", unique.len());

        let mut cache = self.cache.write().await;
        *cache = unique;
        Ok(())
    }

    /// Выполнить размытый поиск по кэшу. Результат отсортирован по релевантности.
    async fn search(&self, query: &str) -> Vec<AppEntry> {
        let query = query.trim();
        let cache = self.cache.read().await;
        if query.is_empty() {
            return cache.clone();
        }

        let mut scored: Vec<(i64, AppEntry)> = cache
            .iter()
            .filter_map(|entry| {
                // search_text предвычислен при построении кэша (name + keywords)
                self.matcher
                    .fuzzy_match(&entry.search_text, query)
                    .map(|score| (score, entry.clone()))
            })
            .collect();

        // Сортировка по убыванию score
        scored.sort_by(|a, b| b.0.cmp(&a.0));

        scored.into_iter().map(|(_, entry)| entry).collect()
    }
}

// ---------------------------------------------------------------------------
// Файловый вотчер
// ---------------------------------------------------------------------------

fn spawn_watcher(state: Arc<AppState>) -> Result<()> {
    let (tx, rx) = std::sync::mpsc::channel::<Event>();

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        match res {
            Ok(event) => {
                let _ = tx.send(event);
            }
            Err(e) => log::error!("Watcher error: {e}"),
        }
    })
    .context("Failed to create file watcher")?;

    for dir in desktop_dirs() {
        if dir.is_dir() {
            watcher
                .watch(&dir, RecursiveMode::NonRecursive)
                .with_context(|| format!("Failed to watch {}", dir.display()))?;
            log::info!("Watching: {}", dir.display());
        }
    }

    // Фоновый поток: слушает события, дебаунсит и пересобирает кэш.
    std::thread::spawn(move || {
        // Важно: watcher должен жить в этом потоке, иначе он дропнется.
        let _watcher = watcher;
        let mut needs_rebuild = false;
        let mut last_event = std::time::Instant::now();
        let debounce = Duration::from_millis(500);

        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(event) => {
                    if is_relevant_event(&event) {
                        log::debug!("Watcher event: {:?}", event.kind);
                        needs_rebuild = true;
                        last_event = std::time::Instant::now();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if needs_rebuild && last_event.elapsed() >= debounce {
                        log::info!("Debounce expired, rebuilding cache…");
                        if let Err(e) = state.rt_handle.block_on(state.rebuild_cache()) {
                            log::error!("Cache rebuild failed: {e}");
                        }
                        needs_rebuild = false;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    log::error!("Watcher channel disconnected");
                    break;
                }
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
    ) && event
        .paths
        .iter()
        .any(|p| p.extension().map_or(false, |ext| ext == "desktop"))
}

// ---------------------------------------------------------------------------
// Обработка подключений
// ---------------------------------------------------------------------------

async fn handle_connection(
    mut stream: tokio::net::UnixStream,
    state: Arc<AppState>,
) -> Result<()> {
    loop {
        // Читаем 4-байтовый префикс длины
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) => {
                log::debug!("Client disconnected (read len): {e}");
                return Ok(());
            }
        }

        let body_len = read_frame_length(&len_buf) as usize;
        if body_len > hiren_shared::MAX_FRAME_SIZE {
            log::warn!("Frame too large: {body_len} bytes, dropping connection");
            return Ok(());
        }

        let mut body = vec![0u8; body_len];
        if let Err(e) = stream.read_exact(&mut body).await {
            log::debug!("Client disconnected (read body): {e}");
            return Ok(());
        }

        let msg = match decode_frame(&body) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Failed to decode message: {e}");
                continue;
            }
        };

        match msg {
            IPCMessage::RequestSearch(query) => {
                let results = state.search(&query).await;
                let response = IPCMessage::ResponseApps(results);
                let frame = encode_frame(&response)
                    .map_err(|e| anyhow::anyhow!("Encode error: {e}"))?;
                stream.write_all(&frame).await?;
            }
            _ => {
                log::warn!("Unexpected message from client: {:?}", msg);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Точка входа
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    // Удаляем stale socket
    if let Err(e) = std::fs::remove_file(hiren_shared::SOCKET_PATH) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("Failed to remove old socket: {e}");
        }
    }

    let rt = tokio::runtime::Runtime::new().context("Failed to create tokio runtime")?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let rt_handle = tokio::runtime::Handle::current();
    let state = Arc::new(AppState::new(rt_handle));
    state
        .rebuild_cache()
        .await
        .context("Initial cache rebuild failed")?;

    spawn_watcher(state.clone())?;

    let socket_path = hiren_shared::SOCKET_PATH;
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("Failed to bind to {socket_path}"))?;

    log::info!("hiren-daemon listening on {socket_path}");

    loop {
        let (stream, addr) = listener
            .accept()
            .await
            .context("Failed to accept connection")?;
        log::debug!("New connection from {:?}", addr);

        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state).await {
                log::error!("Connection error: {e:#}");
            }
        });
    }
}
