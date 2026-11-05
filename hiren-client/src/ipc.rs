//! IPC client for hiren-daemon.
//!
//! Синхронный запрос поиска к демону через UNIX-сокет. Безопасность:
//! - путь сокета берётся из [`hiren_shared::socket_path`] (приватный
//!   `$XDG_RUNTIME_DIR`, fallback — `/tmp` с uid в имени);
//! - до подключения проверяется владелец файла сокета (`lstat`): чужой
//!   сокет в предсказуемом каталоге отвергается;
//! - после подключения проверяется `SO_PEERCRED`: peer-процесс должен
//!   работать под нашим uid;
//! - ответ ограничен [`MAX_FRAME_SIZE`], ответ-ошибка демона
//!   ([`IPCMessage::Error`]) возвращается как `anyhow::Error` и попадает
//!   в `LauncherState.error` через `state_bridge`.

use anyhow::{Context, Result};
use hiren_shared::{
    decode_frame, encode_frame, read_frame_length, verify_peer_uid, verify_socket_file_owner,
    AppEntry, IPCMessage, MAX_FRAME_SIZE,
};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Таймаут чтения/записи ответа демона.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Запросить результаты поиска у демона. Ошибка означает, что демон
/// недоступен или ответил [`IPCMessage::Error`] — вызывающая сторона
/// показывает её пользователю.
pub fn search_sync(query: &str) -> Result<Vec<AppEntry>> {
    let path = hiren_shared::socket_path();
    search_at(&path, query)
}

/// Реализация запроса по явному пути (тестируется с фейковым демоном).
fn search_at(path: &Path, query: &str) -> Result<Vec<AppEntry>> {
    // Владелец файла сокета: не подключаемся к чужому сокету.
    verify_socket_file_owner(path)
        .with_context(|| format!("refusing socket {}", path.display()))?;

    let mut stream = UnixStream::connect(path)
        .with_context(|| format!("Failed to connect to daemon at {}", path.display()))?;

    // SO_PEERCRED: не разговариваем с процессом другого uid.
    verify_peer_uid(stream.as_raw_fd())
        .with_context(|| format!("peer credentials check failed for {}", path.display()))?;

    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .context("Failed to set read timeout")?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .context("Failed to set write timeout")?;

    let frame = encode_frame(&IPCMessage::request_search(query))
        .map_err(|e| anyhow::anyhow!("Encode: {e}"))?;
    stream.write_all(&frame).context("Failed to send request")?;

    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .context("Failed to read response length")?;
    let body_len = read_frame_length(&len_buf) as usize;
    if body_len == 0 || body_len > MAX_FRAME_SIZE {
        anyhow::bail!("Invalid response frame size: {body_len} bytes");
    }
    let mut body = vec![0u8; body_len];
    stream
        .read_exact(&mut body)
        .context("Failed to read response body")?;

    match decode_frame(&body).map_err(|e| anyhow::anyhow!("Decode: {e}"))? {
        IPCMessage::ResponseApps(apps) => Ok(apps),
        IPCMessage::Error(msg) => anyhow::bail!("daemon error: {msg}"),
        other => anyhow::bail!("Unexpected response: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hiren_shared::PROTOCOL_VERSION;
    use std::os::unix::net::UnixListener;

    /// Поднять фейковый демон на `socket_path`; `handler` получает принятое
    /// соединение. Возвращает join-handle, чтобы тест дождался обработки.
    fn fake_daemon(
        socket_path: &Path,
        handler: impl FnOnce(UnixStream) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket_path).expect("bind fake daemon");
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            handler(stream);
        })
    }

    fn read_request(stream: &mut UnixStream) -> IPCMessage {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = read_frame_length(&len_buf) as usize;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).expect("read body");
        decode_frame(&body).expect("decode request")
    }

    fn send_message(stream: &mut UnixStream, msg: &IPCMessage) {
        let frame = encode_frame(msg).expect("encode");
        stream.write_all(&frame).expect("write");
    }

    #[test]
    fn search_roundtrip_with_fake_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("hiren.socket");
        let expected = AppEntry::drun(
            "firefox".into(),
            "Firefox".into(),
            "/usr/bin/firefox".into(),
            None,
            String::new(),
        );
        let expected_clone = expected.clone();
        let server = fake_daemon(&socket_path, move |mut stream| {
            match read_request(&mut stream) {
                IPCMessage::RequestSearch { version, query } => {
                    assert_eq!(version, PROTOCOL_VERSION);
                    assert_eq!(query, "fire");
                }
                other => panic!("unexpected request: {other:?}"),
            }
            send_message(&mut stream, &IPCMessage::ResponseApps(vec![expected_clone.clone()]));
        });

        let apps = search_at(&socket_path, "fire").expect("search");
        assert_eq!(apps, vec![expected]);
        server.join().expect("server thread");
    }

    #[test]
    fn daemon_error_is_returned_as_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("hiren.socket");
        let server = fake_daemon(&socket_path, |mut stream| {
            let _ = read_request(&mut stream);
            send_message(
                &mut stream,
                &IPCMessage::Error("application index is empty".into()),
            );
        });

        let err = search_at(&socket_path, "fire").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("application index is empty"),
            "unexpected error: {err:#}"
        );
        server.join().expect("server thread");
    }

    #[test]
    fn malformed_response_body_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("hiren.socket");
        let server = fake_daemon(&socket_path, |mut stream| {
            let _ = read_request(&mut stream);
            let body = b"not json";
            stream
                .write_all(&(body.len() as u32).to_le_bytes())
                .expect("write len");
            stream.write_all(body).expect("write body");
        });

        let err = search_at(&socket_path, "fire").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("Decode"),
            "unexpected error: {err:#}"
        );
        server.join().expect("server thread");
    }

    #[test]
    fn oversized_response_frame_is_rejected_without_allocating_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("hiren.socket");
        let server = fake_daemon(&socket_path, |mut stream| {
            let _ = read_request(&mut stream);
            // Только префикс длины — тело не отправляем: клиент должен
            // отказаться до аллокации.
            stream
                .write_all(&((MAX_FRAME_SIZE + 1) as u32).to_le_bytes())
                .expect("write len");
        });

        let err = search_at(&socket_path, "fire").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("Invalid response frame size"),
            "unexpected error: {err:#}"
        );
        server.join().expect("server thread");
    }

    #[test]
    fn unexpected_response_variant_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("hiren.socket");
        let server = fake_daemon(&socket_path, |mut stream| {
            let _ = read_request(&mut stream);
            send_message(&mut stream, &IPCMessage::request_search("echo"));
        });

        let err = search_at(&socket_path, "fire").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("Unexpected response"),
            "unexpected error: {err:#}"
        );
        server.join().expect("server thread");
    }

    #[test]
    fn missing_daemon_socket_is_a_clear_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = search_at(&dir.path().join("missing.socket"), "fire").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("Failed to connect"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn socket_path_pointing_to_regular_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hiren.socket");
        std::fs::write(&path, b"not a socket").expect("write");
        let err = search_at(&path, "fire").expect_err("must fail");
        assert!(
            format!("{err:#}").contains("refusing socket"),
            "unexpected error: {err:#}"
        );
    }
}
