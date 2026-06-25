//! hiren-shared — общие типы данных и протокол IPC для hiren-daemon и hiren-client.
//!
//! Формат сообщений в сокете: [4 байта LE-длина][JSON-тело]

use serde::{Deserialize, Serialize};

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
    /// Идентификатор (stem имени файла, например `"firefox"`).
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
        }
    }
}

/// Путь к UNIX-сокету IPC.
pub const SOCKET_PATH: &str = "/tmp/hiren.socket";

/// Сообщения протокола IPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IPCMessage {
    /// Запрос от клиента: строка поиска.
    RequestSearch(String),
    /// Ответ от демона: отфильтрованный и отсортированный список приложений.
    ResponseApps(Vec<AppEntry>),
}

// ---------------------------------------------------------------------------
// Сериализация / десериализация фреймов
// ---------------------------------------------------------------------------

/// Максимальный допустимый размер фрейма (10 МиБ).
pub const MAX_FRAME_SIZE: usize = 10 * 1024 * 1024;

/// Упаковать сообщение в готовый к отправке фрейм:
/// `[u32 LE длина JSON] [JSON байты]`.
pub fn encode_frame(msg: &IPCMessage) -> Result<Vec<u8>, serde_json::Error> {
    let json = serde_json::to_vec(msg)?;
    let len = json.len() as u32;
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
