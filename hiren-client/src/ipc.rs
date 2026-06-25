//! Модуль IPC-клиента для общения с hiren-daemon через UNIX-сокет.
//!
//! Предоставляет только синхронный вызов (блокирует поток на время I/O;
//! для локального UNIX-сокета задержка < 1 мс).

use anyhow::{Context, Result};
use hiren_shared::{decode_frame, encode_frame, read_frame_length, AppEntry, IPCMessage, SOCKET_PATH};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Выполнить синхронный поиск через UNIX-сокет.
///
/// Блокирует поток на время I/O. Вызывать из главного потока GTK допустимо
/// благодаря низкой латентности локального сокета.
pub fn search_sync(query: &str) -> Result<Vec<AppEntry>> {
    let mut stream = UnixStream::connect(SOCKET_PATH)
        .with_context(|| format!("Failed to connect to daemon at {SOCKET_PATH}"))?;

    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .context("Failed to set read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .context("Failed to set write timeout")?;

    let msg = IPCMessage::RequestSearch(query.to_string());
    let frame = encode_frame(&msg).map_err(|e| anyhow::anyhow!("Encode: {e}"))?;
    stream
        .write_all(&frame)
        .context("Failed to send request")?;

    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .context("Failed to read response length")?;
    let body_len = read_frame_length(&len_buf) as usize;

    if body_len > hiren_shared::MAX_FRAME_SIZE {
        anyhow::bail!("Response too large: {body_len} bytes");
    }

    let mut body = vec![0u8; body_len];
    stream
        .read_exact(&mut body)
        .context("Failed to read response body")?;

    match decode_frame(&body).map_err(|e| anyhow::anyhow!("Decode: {e}"))? {
        IPCMessage::ResponseApps(apps) => Ok(apps),
        other => anyhow::bail!("Unexpected response: {other:?}"),
    }
}
