//! hiren-client — графический клиент лаунчера на GTK4 + layer-shell.
//!
//! Точка входа: создаёт GtkApplication, загружает CSS, строит UI,
//! запускает главный цикл.

mod config;
mod freq;
mod ipc;
mod modes;
mod ui;

use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use std::path::PathBuf;
use std::rc::Rc;

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id("com.hiren.client")
        .build();

    app.connect_activate(move |app| {
        if let Err(e) = activate(app) {
            eprintln!("[hiren-client] Fatal: {e:#}");
            app.quit();
        }
    });

    app.run()
}

fn activate(app: &gtk::Application) -> anyhow::Result<()> {
    // --- Загрузка конфига ---
    let config = config::Config::load();

    // --- Загрузка CSS ---
    load_css();

    // --- Построение UI ---
    let ctx = ui::UiContext::build(app, config);

    // Достаём окно (дешёвый clone — инкремент GObject refcount)
    let window = ctx.window_launcher.window().clone();

    // Сохраняем ctx в window как user-data, чтобы Rc жил пока живо окно.
    unsafe {
        window.set_data("hiren-ctx", Rc::clone(&ctx));
    }

    // При разрушении окна явно завершаем приложение
    let app_ref = app.clone();
    window.connect_destroy(move |_| {
        eprintln!("[hiren-client] Window destroyed, quitting");
        app_ref.quit();
    });

    // --- Layer-shell (Wayland-оверлей) ---
    setup_layer_shell(&window);

    // --- Показываем окно ---
    ctx.window_launcher.present();

    // Автоматический фокус на поле ввода
    ctx.window_launcher.search_input.widget().grab_focus();

    Ok(())
}

/// Настроить окно как Wayland-оверлей через gtk4-layer-shell.
fn setup_layer_shell(window: &gtk::ApplicationWindow) {
    window.init_layer_shell();

    // Поверх всего
    window.set_layer(Layer::Overlay);

    // OnDemand — стандартный режим для лаунчеров, не блокирует маршрутизацию клавиш в GTK
    window.set_keyboard_mode(KeyboardMode::OnDemand);
}

// ---------------------------------------------------------------------------
// CSS
// ---------------------------------------------------------------------------

/// Загрузить CSS: всегда fallback как база, пользовательский — поверх.
///
/// Fallback гарантирует что все базовые классы имеют стили
/// даже при наличии пользовательского CSS.
fn load_css() {
    let Some(display) = gdk::Display::default() else {
        return;
    };

    // 1. База: встроенный fallback (ПРИОРИТЕТ_FALLBACK)
    let fallback_provider = gtk::CssProvider::new();
    fallback_provider.load_from_data(&default_css());
    gtk::style_context_add_provider_for_display(
        &display,
        &fallback_provider,
        gtk::STYLE_PROVIDER_PRIORITY_FALLBACK,
    );

    // 2. Оверлей: пользовательский CSS (ПРИОРИТЕТ_USER) — переопределяет fallback
    if let Some(user_css) = load_user_css() {
        eprintln!("[hiren-client] Loaded user CSS (overriding fallback)");
        let user_provider = gtk::CssProvider::new();
        user_provider.load_from_data(&user_css);
        gtk::style_context_add_provider_for_display(
            &display,
            &user_provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER,
        );
    } else {
        eprintln!("[hiren-client] Using built-in fallback CSS only");
    }
}

/// Попытаться прочитать `~/.config/hiren/style.css`.
fn load_user_css() -> Option<String> {
    let path = css_path()?;
    std::fs::read_to_string(&path).ok()
}

fn css_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("hiren").join("style.css"))
}

/// Минимальный fallback-стиль, гарантирующий корректную геометрию
/// и прозрачность даже без пользовательского CSS.
///
/// CSS-классы (v2 — модульная архитектура):
/// - `.launcher-window` — окно лаунчера
/// - `.outer-box`     — видимая «карточка»
/// - `.search-entry`  — поле ввода
/// - `.results-list`  — список результатов (GtkListView)
/// - `.app-icon`      — иконка приложения в строке
/// - `.app-name`      — название приложения в строке
fn default_css() -> String {
    r#"
/* === hiren fallback styles v2 === */

/* Окно: прозрачный фон */
.launcher-window {
    background: transparent;
}

/* Внешний контейнер */
.outer-box {
    background: rgba(30, 30, 46, 0.92);
    border-radius: 16px;
    padding: 16px;
}

/* Поле ввода */
.search-entry {
    font-size: 16px;
    padding: 10px 16px;
    border-radius: 12px;
    background: rgba(255, 255, 255, 0.08);
    color: #cdd6f4;
    border: 1px solid rgba(255, 255, 255, 0.12);
    margin-bottom: 8px;
    caret-color: #89b4fa;
    text-align: center;
}

.search-entry:focus {
    border-color: #89b4fa;
    background: rgba(255, 255, 255, 0.10);
    box-shadow: 0 0 0 3px rgba(137, 180, 250, 0.15);
}

/* Индикатор режима */
.mode-indicator {
    font-size: 11px;
    color: rgba(205, 214, 244, 0.5);
    text-transform: uppercase;
    letter-spacing: 1px;
}

/* Список результатов */
.results-list {
    background: transparent;
}

/* Строки результатов (CSS-ноды row внутри GtkListView) */
.results-list row {
    padding: 8px 12px;
    border-radius: 8px;
    color: #cdd6f4;
    font-size: 15px;
    background: transparent;
}

.results-list row:selected {
    background: rgba(137, 180, 250, 0.25);
    color: #ffffff;
}

/* Иконки приложений */
.app-icon {
    margin-right: 8px;
    min-width: 24px;
    min-height: 24px;
}

.app-name {
    font-weight: 500;
    text-align: center;
}
"#
    .to_string()
}
