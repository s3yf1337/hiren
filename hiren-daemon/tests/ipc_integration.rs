//! Интеграционные тесты IPC: запускают настоящий `hiren-daemon` с приватным
//! `HIREN_SOCKET_PATH` и изолированными XDG data dirs и общаются с ним через
//! UNIX-сокет.
//!
//! Покрывают: handshake/round-trip, версию протокола, DoS-лимиты (длина
//! фрейма, длина запроса, malformed JSON), закрытие соединения на мусоре,
//! права 0600 на сокете, single-instance, stale-сокет и отказ от чужого
//! файла на месте сокета.

use hiren_shared::{
    decode_frame, encode_frame, read_frame_length, IPCMessage, MAX_FRAME_SIZE, PROTOCOL_VERSION,
};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Изолированный демон: при drop убивается, временные каталоги удаляются.
struct TestDaemon {
    child: Child,
    socket: PathBuf,
    data_home: PathBuf,
    data_dirs: PathBuf,
    home: PathBuf,
    #[allow(dead_code)]
    root: tempfile::TempDir,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_foo_desktop(data_home: &Path) {
    let apps = data_home.join("applications");
    std::fs::create_dir_all(&apps).expect("create applications dir");
    std::fs::write(
        apps.join("foo.desktop"),
        "[Desktop Entry]\nType=Application\nName=Foo App\nExec=/bin/true %u\nKeywords=foo;demo;\n",
    )
    .expect("write foo.desktop");
}

/// Запустить демон с изолированным окружением. Сокет ещё может быть не готов.
fn spawn_daemon() -> TestDaemon {
    spawn_daemon_with_fixture(true)
}

/// Запустить демон с изолированным окружением; `write_fixture == false`
/// означает, что каталог `applications/` появится уже после старта.
fn spawn_daemon_with_fixture(write_fixture: bool) -> TestDaemon {
    let root = tempfile::tempdir().expect("tempdir");
    let data_home = root.path().join("data-home");
    let data_dirs = root.path().join("data-dirs");
    let home = root.path().join("home");
    let socket = root.path().join("hiren.socket");
    for dir in [&data_home, &data_dirs, &home] {
        std::fs::create_dir_all(dir).expect("create isolated dir");
    }
    if write_fixture {
        write_foo_desktop(&data_home);
    }

    let child = daemon_command(&socket, &data_home, &data_dirs, &home)
        .spawn()
        .expect("spawn hiren-daemon");

    TestDaemon {
        child,
        socket,
        data_home,
        data_dirs,
        home,
        root,
    }
}

fn daemon_command(
    socket: &Path,
    data_home: &Path,
    data_dirs: &Path,
    home: &Path,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hiren-daemon"));
    command
        .env("HIREN_SOCKET_PATH", socket)
        .env("XDG_DATA_HOME", data_home)
        .env("XDG_DATA_DIRS", data_dirs)
        .env("HOME", home)
        .env("RUST_LOG", "error")
        .env_remove("XDG_CURRENT_DESKTOP")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

impl TestDaemon {
    fn wait_until_ready(&self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if self.socket.exists() && UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "daemon did not start listening on {} within {STARTUP_TIMEOUT:?}",
            self.socket.display()
        );
    }

    fn connect(&self) -> UnixStream {
        let stream = UnixStream::connect(&self.socket).expect("connect to daemon");
        stream
            .set_read_timeout(Some(IO_TIMEOUT))
            .expect("read timeout");
        stream
            .set_write_timeout(Some(IO_TIMEOUT))
            .expect("write timeout");
        stream
    }
}

fn send_message(stream: &mut UnixStream, msg: &IPCMessage) {
    let frame = encode_frame(msg).expect("encode frame");
    stream.write_all(&frame).expect("write frame");
}

fn read_message(stream: &mut UnixStream) -> IPCMessage {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read frame length");
    let len = read_frame_length(&len_buf) as usize;
    assert!(len > 0 && len <= MAX_FRAME_SIZE, "bad frame length {len}");
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).expect("read frame body");
    decode_frame(&body).expect("decode frame")
}

/// Прочитать ошибку демона и убедиться, что соединение после неё закрыто.
fn expect_error_then_close(mut stream: UnixStream) -> String {
    match read_message(&mut stream) {
        IPCMessage::Error(message) => {
            // После невалидного фрейма демон обязан закрыть соединение.
            let mut buf = [0u8; 1];
            let closed = match stream.read(&mut buf) {
                Ok(0) => true,
                Ok(_) => false,
                Err(e) => {
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                    )
                }
            };
            assert!(closed, "daemon must close connection after an error");
            message
        }
        other => panic!("expected IPCMessage::Error, got {other:?}"),
    }
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[test]
fn daemon_serves_search_over_private_socket() {
    let daemon = spawn_daemon();
    daemon.wait_until_ready();

    // Сокет: UNIX-сокет, владелец — мы, права 0600.
    let metadata = std::fs::metadata(&daemon.socket).expect("socket metadata");
    assert!(
        metadata.file_type().is_socket(),
        "{} must be a UNIX socket",
        daemon.socket.display()
    );
    assert_eq!(metadata.uid(), hiren_shared::current_uid().expect("uid"));
    assert_eq!(
        metadata.permissions().mode() & 0o777,
        0o600,
        "socket must be 0600"
    );

    let mut stream = daemon.connect();
    send_message(&mut stream, &IPCMessage::request_search("foo"));
    match read_message(&mut stream) {
        IPCMessage::ResponseApps(apps) => {
            assert!(
                apps.iter().any(|app| app.id == "foo" && app.name == "Foo App"),
                "expected Foo App in results, got {apps:?}"
            );
            assert!(apps.iter().all(|app| app.mode == hiren_shared::AppMode::Drun));
        }
        other => panic!("expected ResponseApps, got {other:?}"),
    }
}

#[test]
fn daemon_handles_multiple_requests_on_one_connection() {
    let daemon = spawn_daemon();
    daemon.wait_until_ready();

    let mut stream = daemon.connect();
    for query in ["foo", "foo app", ""] {
        send_message(&mut stream, &IPCMessage::request_search(query));
        match read_message(&mut stream) {
            IPCMessage::ResponseApps(_) => {}
            other => panic!("query {query:?}: expected ResponseApps, got {other:?}"),
        }
    }
}

#[test]
fn protocol_version_mismatch_is_reported() {
    let daemon = spawn_daemon();
    daemon.wait_until_ready();

    let mut stream = daemon.connect();
    send_message(
        &mut stream,
        &IPCMessage::RequestSearch {
            version: PROTOCOL_VERSION + 1,
            query: "foo".into(),
        },
    );
    let error = expect_error_then_close(stream);
    assert!(
        error.contains("protocol version mismatch"),
        "unexpected error: {error}"
    );
}

#[test]
fn oversized_query_is_rejected_and_connection_closed() {
    let daemon = spawn_daemon();
    daemon.wait_until_ready();

    let mut stream = daemon.connect();
    let query = "x".repeat(hiren_shared::MAX_QUERY_LEN + 1);
    send_message(&mut stream, &IPCMessage::request_search(query));
    let error = expect_error_then_close(stream);
    assert!(error.contains("query too long"), "unexpected error: {error}");
}

#[test]
fn malformed_json_frame_closes_connection_but_daemon_survives() {
    let daemon = spawn_daemon();
    daemon.wait_until_ready();

    let mut stream = daemon.connect();
    let body = b"this is not json";
    stream
        .write_all(&(body.len() as u32).to_le_bytes())
        .expect("write length");
    stream.write_all(body).expect("write body");
    let error = expect_error_then_close(stream);
    assert!(error.contains("malformed"), "unexpected error: {error}");

    // Демон должен продолжать обслуживать новые подключения.
    let mut stream = daemon.connect();
    send_message(&mut stream, &IPCMessage::request_search("foo"));
    assert!(matches!(read_message(&mut stream), IPCMessage::ResponseApps(_)));
}

#[test]
fn oversized_frame_length_is_rejected_without_allocation() {
    let daemon = spawn_daemon();
    daemon.wait_until_ready();

    let mut stream = daemon.connect();
    // Только префикс: тело в 256 КиБ + 1 демон читать не должен.
    stream
        .write_all(&((MAX_FRAME_SIZE + 1) as u32).to_le_bytes())
        .expect("write length");
    let error = expect_error_then_close(stream);
    assert!(
        error.contains("invalid frame length"),
        "unexpected error: {error}"
    );

    let mut stream = daemon.connect();
    send_message(&mut stream, &IPCMessage::request_search("foo"));
    assert!(matches!(read_message(&mut stream), IPCMessage::ResponseApps(_)));
}

#[test]
fn zero_length_frame_is_rejected() {
    let daemon = spawn_daemon();
    daemon.wait_until_ready();

    let mut stream = daemon.connect();
    stream.write_all(&0u32.to_le_bytes()).expect("write length");
    let error = expect_error_then_close(stream);
    assert!(
        error.contains("invalid frame length"),
        "unexpected error: {error}"
    );
}

#[test]
fn second_daemon_exits_cleanly_when_instance_is_running() {
    let daemon = spawn_daemon();
    daemon.wait_until_ready();

    let mut second = daemon_command(
        &daemon.socket,
        &daemon.data_home,
        &daemon.data_dirs,
        &daemon.home,
    )
    .spawn()
    .expect("spawn second daemon");
    let status = wait_for_exit(&mut second, EXIT_TIMEOUT).expect("second daemon must exit");
    assert!(
        status.success(),
        "second instance must exit cleanly, got {status:?}"
    );
}

#[test]
fn stale_socket_is_replaced() {
    use std::os::unix::net::UnixListener;

    let root = tempfile::tempdir().expect("tempdir");
    let socket = root.path().join("hiren.socket");
    let data_home = root.path().join("data-home");
    let data_dirs = root.path().join("data-dirs");
    let home = root.path().join("home");
    for dir in [&data_home, &data_dirs, &home] {
        std::fs::create_dir_all(dir).expect("create dir");
    }
    write_foo_desktop(&data_home);

    // Stale-сокет: файл на месте, никто не слушает.
    {
        let listener = UnixListener::bind(&socket).expect("bind stale");
        drop(listener);
    }
    assert!(socket.exists());

    let mut child = daemon_command(&socket, &data_home, &data_dirs, &home)
        .spawn()
        .expect("spawn daemon");
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut ready = false;
    while Instant::now() < deadline {
        if UnixStream::connect(&socket).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(ready, "daemon must replace stale socket and start");
}

#[test]
fn daemon_refuses_to_replace_foreign_regular_file() {
    let root = tempfile::tempdir().expect("tempdir");
    let socket = root.path().join("hiren.socket");
    let data_home = root.path().join("data-home");
    let data_dirs = root.path().join("data-dirs");
    let home = root.path().join("home");
    for dir in [&data_home, &data_dirs, &home] {
        std::fs::create_dir_all(dir).expect("create dir");
    }
    std::fs::write(&socket, b"not ours").expect("write foreign file");

    let mut child = daemon_command(&socket, &data_home, &data_dirs, &home)
        .spawn()
        .expect("spawn daemon");
    let status = wait_for_exit(&mut child, EXIT_TIMEOUT).expect("daemon must exit on foreign file");
    assert!(!status.success(), "daemon must fail closed, got {status:?}");
    assert_eq!(
        std::fs::read(&socket).expect("foreign file untouched"),
        b"not ours"
    );
}

#[test]
fn applications_dir_created_after_startup_is_picked_up() {
    // Регрессия: раньше вотчер ставился только на уже существующие каталоги,
    // и созданный после старта `applications/` (в т.ч. целиком новый
    // XDG data dir) не попадал в кэш до перезапуска демона.
    let daemon = spawn_daemon_with_fixture(false);
    daemon.wait_until_ready();

    // На старте applications/ ещё нет.
    let apps_dir = daemon.data_home.join("applications");
    assert!(!apps_dir.exists());

    write_foo_desktop(&daemon.data_home);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut stream = daemon.connect();
        send_message(&mut stream, &IPCMessage::request_search("foo"));
        if let IPCMessage::ResponseApps(apps) = read_message(&mut stream) {
            if apps.iter().any(|app| app.id == "foo") {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not pick up applications/ created after startup"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
