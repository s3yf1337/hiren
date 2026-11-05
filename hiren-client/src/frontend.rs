//! Frontend core — launcher behavior shared by all window backends.
//!
//! `AppCore` owns the bridge between the UI runtime and launcher logic. Window
//! backends (winit toplevel, wlr layer-shell) translate their events into
//! `UiKey`/`UiMods`/click coordinates and call into this; nothing backend-
//! specific leaks into launcher behavior, and nothing here knows Wayland.

use crate::config::{KeyBinding, LauncherConfig, Modifiers};
use crate::launcher::ObservableState;
use crate::modes::{self, LaunchErrors};
use crate::ui_runtime::node::{NodeAction, ResolvedNode};
use crate::ui_runtime::state_bridge::SearchBridge;
use crate::ui_runtime::{theme::Theme, UiRuntime};
use hiren_shared::{AppEntry, AppMode};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Backend-agnostic key event.
#[derive(Debug, Clone, PartialEq)]
pub enum UiKey {
    Char(String),
    Backspace,
    Escape,
    Enter,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UiMods {
    pub ctrl: bool,
    pub alt: bool,
    pub super_key: bool,
    pub shift: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    None,
    /// Launcher finished (close requested or a launch is in progress).
    Exit,
}

/// Как запускать активированную запись.
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchMode {
    /// Обычный запуск: `Exec=` напрямую (или в терминале, если запись
    /// пришла с `Terminal=true` от daemon).
    Plain,
    /// Запуск в терминале из `[terminal]` конфига (Ctrl+Enter по умолчанию).
    Terminal,
    /// Пользовательский префикс из `[[bindings]] prefix` (shell-обёртка).
    Custom(String),
}

/// Выбрать режим запуска по биндингу для Return.
///
/// `Ctrl+Return` без явного `prefix` запускает в терминале, используя
/// `[terminal] command/exec_flag` из конфига — раньше этот конфиг
/// парсился, но нигде не применялся.
pub fn resolve_launch_mode(bindings: &[KeyBinding], ctrl: bool) -> LaunchMode {
    let flag = if ctrl { Modifiers::CTRL } else { 0 };
    let binding = bindings.iter().find(|b| {
        b.key_name == "return"
            && (b.modifiers.0 & flag) == flag
            && (b.modifiers.0 & !flag) == 0
    });
    match binding {
        Some(b) => match &b.prefix {
            Some(prefix) if !prefix.trim().is_empty() => LaunchMode::Custom(prefix.clone()),
            _ if b.modifiers.contains(Modifiers::CTRL) => LaunchMode::Terminal,
            _ => LaunchMode::Plain,
        },
        None => LaunchMode::Plain,
    }
}

pub struct AppCore {
    pub runtime: UiRuntime,
    pub state: Rc<ObservableState>,
    pub bridge: SearchBridge,
    pub config: LauncherConfig,
    /// Whether theme hot-reload polling is enabled (dev convenience).
    pub reload: bool,
    input_buffer: String,
    auto_close: Option<Instant>,
    last_nodes: Vec<ResolvedNode>,
    last_uses_time: bool,
    last_uses_impulse: bool,
    size: (u32, u32),
    /// When the theme binds `launcher.launching`, close is deferred so exit
    /// motion can play. Instant close if the theme does not use it.
    delay_exit: Option<Instant>,
    /// Ошибки запуска из reaper-потоков (см. `modes::spawn_monitored`).
    launch_errors: LaunchErrors,
    /// Бэкенд должен немедленно отпустить эксклюзивный клавиатурный захват,
    /// чтобы запущенное приложение получило клавиатуру.
    release_grab: bool,
}

impl AppCore {
    pub fn new(
        theme: Theme,
        state: Rc<ObservableState>,
        bridge: SearchBridge,
        config: LauncherConfig,
        size_override: Option<(u32, u32)>,
        reload: bool,
    ) -> Self {
        let size = size_override.unwrap_or_else(|| {
            theme
                .window
                .effective_size((config.window_width.max(1) as u32, config.window_height.max(1) as u32))
        });
        let runtime = UiRuntime::new(theme, state.clone());
        // Initial population (empty query → recent apps / calc / etc.)
        bridge.search("", &state);
        let auto_close = if config.auto_close_timeout_secs > 0 {
            Some(Instant::now() + Duration::from_secs(config.auto_close_timeout_secs))
        } else {
            None
        };
        Self {
            runtime,
            state,
            bridge,
            config,
            reload,
            input_buffer: String::new(),
            auto_close,
            last_nodes: Vec::new(),
            last_uses_time: false,
            last_uses_impulse: false,
            size,
            delay_exit: None,
            launch_errors: modes::new_launch_errors(),
            release_grab: false,
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Adopt a new logical window size (layer-shell configure, reload).
    pub fn set_size(&mut self, w: u32, h: u32) {
        if w > 0 && h > 0 && (w, h) != self.size {
            self.size = (w, h);
        }
    }

    /// True while an auto-close deadline is pending (idle wake-ups needed).
    pub fn auto_close_pending(&self) -> bool {
        self.auto_close.is_some() || self.delay_exit.is_some()
    }

    /// Debug access for the loop instrumentation (HIREN_LOOP_DEBUG=1).
    pub fn last_uses_time_debug(&self) -> bool {
        self.last_uses_time
    }

    /// Window transparency preference from the theme.
    pub fn config_theme_transparent(&self) -> bool {
        self.runtime.theme().window.transparent
    }

    /// Frame interval for `time`-only themes (no spring in motion). Default
    /// ~45 ms (~22 fps); `[window] time_hz = 60` drops this to vsync-ish 16 ms.
    pub fn time_frame_ms(&self) -> i32 {
        match self.runtime.theme().window.time_hz.unwrap_or(0) {
            0 => 45,
            hz => (1000 / hz.max(1)).clamp(8, 100) as i32,
        }
    }

    pub fn auto_close_expired(&self) -> bool {
        self.auto_close.map(|d| Instant::now() >= d).unwrap_or(false)
            || self.delay_exit.map(|d| Instant::now() >= d).unwrap_or(false)
    }

    /// Сколько осталось до конца exit-анимации (для pacing'а poll-цикла без
    /// busy-spin). `None`, когда задержки нет.
    pub fn exit_delay_remaining_ms(&self) -> Option<i32> {
        self.delay_exit.map(|deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32
        })
    }

    /// Keep rendering while animations are running, the theme uses `time`,
    /// an impact envelope is decaying, or a launch exit is in flight.
    pub fn needs_frame(&self) -> bool {
        self.runtime.animating() || self.last_uses_time || self.impulse_ticking() || self.delay_exit.is_some()
    }

    /// Theme wants impulse frames and the envelope is still live.
    pub fn impulse_ticking(&self) -> bool {
        self.last_uses_impulse && self.runtime.impulse_active()
    }

    /// Идёт запуск (или exit-анимация после него): новый ввод/активации
    /// игнорируются — иначе Enter во время 420 мс анимации запускает второй раз.
    pub fn launching(&self) -> bool {
        self.state.with(|s| s.launching) || self.delay_exit.is_some()
    }

    /// Забрать запрос «отпустить клавиатурный захват» (вызывается бэкендом
    /// сразу после обработки ввода, который запустил приложение).
    pub fn take_grab_release(&mut self) -> bool {
        std::mem::take(&mut self.release_grab)
    }

    /// Забрать ошибки запуска, накопленные reaper-потоками, и показать их в
    /// `LauncherState.error`. Вызывается бэкендами каждый цикл кадра.
    /// Возвращает `true`, если пора перерисоваться.
    pub fn poll_launch_errors(&mut self) -> bool {
        let messages = match self.launch_errors.lock() {
            Ok(mut queue) => std::mem::take(&mut *queue),
            Err(_) => return false,
        };
        if messages.is_empty() {
            return false;
        }
        let message = messages.join("; ");
        log::warn!("hiren: {message}");
        self.state.update(|s| {
            s.error = Some(message);
            // Запуск не удался — лаунчер остаётся открытым и usable.
            s.launching = false;
        });
        // Отменяем exit-анимацию: показываем ошибку в открытом окне.
        self.delay_exit = None;
        true
    }

    /// Resolve + render the current frame; returns resolved nodes.
    pub fn render_frame(&mut self, scale: f32) -> &[ResolvedNode] {
        let out = self.runtime.render_frame(self.size, scale);
        if self.reload {
            if self.runtime.try_reload(true) {
                // Re-resolve with the fresh theme on the next frame.
                self.size = self.runtime.theme().window.effective_size((self.config.window_width.max(1) as u32, self.config.window_height.max(1) as u32));
            }
        }
        for w in self.runtime.take_warnings() {
            log::warn!("theme binding: {w}");
        }
        self.last_nodes = out.nodes;
        self.last_uses_time = out.uses_time;
        self.last_uses_impulse = out.uses_impulse;
        &self.last_nodes
    }

    // -----------------------------------------------------------------
    // Query / selection
    // -----------------------------------------------------------------

    fn reset_idle_timer(&mut self) {
        if self.config.auto_close_timeout_secs > 0 {
            self.auto_close = Some(Instant::now() + Duration::from_secs(self.config.auto_close_timeout_secs));
        }
    }

    fn set_query(&mut self, q: String) {
        self.reset_idle_timer();
        self.input_buffer = q.clone();
        self.bridge.search(&q, &self.state);
    }

    fn move_selection(&mut self, delta: i32) {
        self.reset_idle_timer();
        self.state.update(|s| s.select_next(delta));
    }

    fn select(&mut self, idx: usize) {
        self.reset_idle_timer();
        self.state.update(|s| s.select(idx));
    }

    // -----------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------

    pub fn handle_key(&mut self, key: UiKey, mods: UiMods) -> Outcome {
        if self.launching() {
            // Во время exit-анимации окно ещё видно; текст/Enter игнорируются,
            // Escape позволяет закрыть сразу.
            return if key == UiKey::Escape { Outcome::Exit } else { Outcome::None };
        }
        self.reset_idle_timer();
        match key {
            UiKey::Escape => return Outcome::Exit,
            UiKey::Enter => {
                let mode = resolve_launch_mode(&self.config.bindings, mods.ctrl);
                return self.activate(None, mode);
            }
            UiKey::ArrowDown => self.move_selection(1),
            UiKey::ArrowUp => self.move_selection(-1),
            UiKey::Tab => self.move_selection(1),
            UiKey::BackTab => self.move_selection(-1),
            UiKey::Home => {
                let s = self.state.get();
                if !s.results.is_empty() {
                    self.select(0);
                }
            }
            UiKey::End => {
                let s = self.state.get();
                if !s.results.is_empty() {
                    self.select(s.results.len() - 1);
                }
            }
            UiKey::PageDown => self.move_selection(6),
            UiKey::PageUp => self.move_selection(-6),
            UiKey::Backspace => {
                if mods.ctrl {
                    self.set_query(String::new());
                } else {
                    let mut q = self.input_buffer.clone();
                    q.pop();
                    self.set_query(q);
                }
            }
            UiKey::Char(s) => {
                if mods.ctrl || mods.alt || mods.super_key {
                    return Outcome::None;
                }
                let mut q = self.input_buffer.clone();
                q.push_str(&s);
                self.set_query(q);
            }
        }
        Outcome::None
    }

    /// Click at logical (x, y): run the action of the topmost hit node.
    pub fn handle_click(&mut self, x: f64, y: f64, mods: UiMods) -> Outcome {
        if self.launching() {
            // Разрешаем только явное «закрыть» — клики по результатам
            // во время exit-анимации не должны запускать второе приложение.
            let close = self
                .topmost_actionable(x, y)
                .and_then(|node| node.action.clone())
                .map(|a| a == NodeAction::Close)
                .unwrap_or(false);
            return if close { Outcome::Exit } else { Outcome::None };
        }
        let Some(node) = self.topmost_actionable(x, y) else {
            return Outcome::None;
        };
        let action = node.action.clone().expect("checked");
        self.reset_idle_timer();
        match action {
            NodeAction::Activate(idx) => {
                let mode = resolve_launch_mode(&self.config.bindings, mods.ctrl);
                self.activate(idx, mode)
            }
            NodeAction::Select(idx) => match idx {
                Some(i) => {
                    self.select(i);
                    Outcome::None
                }
                None => Outcome::None,
            },
            NodeAction::Move(d) => {
                self.move_selection(d);
                Outcome::None
            }
            NodeAction::SetQuery(q) => {
                self.set_query(q);
                Outcome::None
            }
            NodeAction::Close => Outcome::Exit,
        }
    }

    /// Hover over a result node changes selection to it. Returns `true` when the
    /// selection actually changed (backends repaint only then).
    pub fn handle_hover(&mut self, x: f64, y: f64) -> bool {
        if self.launching() {
            return false;
        }
        if let Some(node) = self.topmost_actionable(x, y) {
            if let Some(i) = node.action.as_ref().and_then(|a| a.targeted_index()) {
                let current = self.state.with(|s| s.selected_index);
                if i != current {
                    self.select(i);
                    return true;
                }
            }
        }
        false
    }

    fn topmost_actionable(&self, x: f64, y: f64) -> Option<&ResolvedNode> {
        self.last_nodes
            .iter()
            .rev()
            .find(|n| n.visible && n.opacity > 0.1 && n.action.is_some() && n.hit(x, y))
    }

    // -----------------------------------------------------------------
    // Activation / launch (launcher-owned functionality)
    // -----------------------------------------------------------------

    fn activate(&mut self, idx: Option<usize>, mode: LaunchMode) -> Outcome {
        let entry = {
            let s = self.state.get();
            let i = idx.unwrap_or(s.selected_index);
            s.results.get(i).cloned()
        };
        let Some(entry) = entry else { return Outcome::None }; // nothing to launch — stay open
        if let Some(i) = idx {
            self.state.update(|s| s.select(i));
        }

        let errors = self.launch_errors.clone();
        if let Err(e) = self.execute_entry(&entry, &mode, &errors) {
            self.report_launch_error(format!("cannot launch {}: {e}", entry.name));
            return Outcome::None;
        }
        self.bridge.record_launch(&entry);
        self.state.update(|s| s.launching = true);
        // Запущенное приложение должно получить клавиатуру сразу, не дожидаясь
        // конца exit-анимации (иначе первые нажатия теряются в нашем захвате).
        self.release_grab = true;
        // Themes that bind `launching` get a short window to play an exit
        // transition. Themes that do not are unchanged (instant close).
        if crate::ui_runtime::layout::theme_uses_token(self.runtime.theme(), &["launching"]) {
            self.delay_exit = Some(Instant::now() + Duration::from_millis(420));
            return Outcome::None;
        }
        Outcome::Exit
    }

    /// Выполнить активацию записи согласно её режиму и выбранному способу.
    fn execute_entry(
        &self,
        entry: &AppEntry,
        mode: &LaunchMode,
        errors: &LaunchErrors,
    ) -> Result<(), String> {
        match entry.mode {
            AppMode::Window | AppMode::Calc => {
                let source = self
                    .bridge
                    .sources
                    .get(&entry.mode)
                    .ok_or_else(|| format!("{:?} mode is not enabled in config", entry.mode))?;
                source.execute_with(entry, &self.config, errors)
            }
            // Drun/Run: план запуска строится здесь, чтобы применить биндинг
            // Ctrl+Enter / AppEntry.terminal / пользовательский префикс.
            _ => {
                let terminal = match mode {
                    LaunchMode::Terminal => Some(&self.config.terminal),
                    LaunchMode::Custom(_) => None,
                    LaunchMode::Plain => modes::entry_requests_terminal(entry)
                        .then_some(&self.config.terminal),
                };
                let user_prefix = match mode {
                    LaunchMode::Custom(prefix) => Some(prefix.as_str()),
                    _ => None,
                };
                let plan = modes::plan_launch(&entry.exec, terminal, user_prefix)?;
                eprintln!("[hiren-client] Launching: {} {mode:?}", entry.name);
                modes::spawn_monitored(&plan, &entry.name, errors)
            }
        }
    }

    fn report_launch_error(&mut self, message: String) {
        eprintln!("[hiren-client] {message}");
        self.state.update(|s| {
            s.error = Some(message);
            s.launching = false;
        });
        self.delay_exit = None;
    }

    #[cfg(test)]
    pub(crate) fn force_launching_for_test(&mut self) {
        self.state.update(|s| s.launching = true);
        self.delay_exit = Some(Instant::now() + Duration::from_millis(420));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Modifiers;

    fn binding(key: &str, key_name: &str, mods: u8, prefix: Option<&str>) -> KeyBinding {
        KeyBinding {
            key: key.into(),
            prefix: prefix.map(str::to_string),
            modifiers: Modifiers(mods),
            key_name: key_name.into(),
        }
    }

    fn default_like_bindings() -> Vec<KeyBinding> {
        vec![
            binding("Return", "return", Modifiers::NONE.0, None),
            binding("Ctrl+Return", "return", Modifiers::CTRL, None),
        ]
    }

    #[test]
    fn return_without_ctrl_is_plain_launch() {
        assert_eq!(resolve_launch_mode(&default_like_bindings(), false), LaunchMode::Plain);
    }

    #[test]
    fn ctrl_return_defaults_to_terminal_from_config() {
        // Ранее [terminal] парсился, но Ctrl+Enter запускал без терминала.
        assert_eq!(resolve_launch_mode(&default_like_bindings(), true), LaunchMode::Terminal);
    }

    #[test]
    fn explicit_prefix_wins() {
        let bindings = vec![
            binding("Return", "return", Modifiers::NONE.0, None),
            binding("Ctrl+Return", "return", Modifiers::CTRL, Some("env FOO=1")),
        ];
        assert_eq!(
            resolve_launch_mode(&bindings, true),
            LaunchMode::Custom("env FOO=1".into())
        );
    }

    #[test]
    fn blank_explicit_prefix_falls_back_to_terminal() {
        let bindings = vec![binding("Ctrl+Return", "return", Modifiers::CTRL, Some("  "))];
        assert_eq!(resolve_launch_mode(&bindings, true), LaunchMode::Terminal);
    }

    #[test]
    fn no_matching_binding_is_plain() {
        assert_eq!(resolve_launch_mode(&[], true), LaunchMode::Plain);
        let only_return = vec![binding("Return", "return", Modifiers::NONE.0, None)];
        assert_eq!(resolve_launch_mode(&only_return, true), LaunchMode::Plain);
    }

    #[test]
    fn ctrl_binding_with_extra_modifiers_is_not_matched() {
        // Биндинг Ctrl+Shift+Return не должен срабатывать как Ctrl+Return.
        let bindings = vec![binding(
            "Ctrl+Shift+Return",
            "return",
            Modifiers::CTRL | Modifiers::SHIFT,
            None,
        )];
        assert_eq!(resolve_launch_mode(&bindings, true), LaunchMode::Plain);
    }

    // -----------------------------------------------------------------
    // Integration-ish: launch failure surfacing and double-activation guard
    // -----------------------------------------------------------------

    fn test_core() -> AppCore {
        let config = LauncherConfig {
            // Никаких активных режимов: тесты не трогают IPC/WM и не
            // перезаписывают пользовательские файлы.
            auto_close_timeout_secs: 0,
            modes: crate::config::ModeConfig { drun: false, run: false, window: false, calc: false },
            ..LauncherConfig::default()
        };
        let state = ObservableState::new(crate::launcher::LauncherState::new());
        let bridge = SearchBridge::new(&config);
        AppCore::new(
            Theme::fallback(),
            state,
            bridge,
            config,
            Some((640, 420)),
            false,
        )
    }

    #[test]
    fn spawn_failure_is_reported_and_launcher_stays_open() {
        let mut core = test_core();
        core.state.update(|s| {
            s.set_results(vec![AppEntry::run(
                "missing".into(),
                "Missing".into(),
                "/nonexistent/hiren-test-binary".into(),
            )]);
        });
        let outcome = core.handle_key(UiKey::Enter, UiMods::default());
        // Бинарник не найден: ошибка видна в UI, лаунчер остаётся открытым,
        // а не закрывается с проглоченной ошибкой (раньше — sh -c + /dev/null).
        assert_eq!(outcome, Outcome::None);
        let error = core.state.with(|s| s.error.clone());
        assert!(
            error.as_deref().unwrap_or("").contains("cannot launch"),
            "unexpected error: {error:?}"
        );
        assert!(!core.state.with(|s| s.launching));
    }

    #[test]
    fn double_activation_is_ignored_during_exit_animation() {
        let mut core = test_core();
        core.state.update(|s| {
            s.set_results(vec![AppEntry::run(
                "missing".into(),
                "Missing".into(),
                "/nonexistent/hiren-test-binary".into(),
            )]);
        });
        core.force_launching_for_test();

        // Пока идёт exit-анимация, Enter/текст игнорируются: ошибки запуска
        // не появляется (иначе второй Enter попытался бы запустить запись).
        assert_eq!(core.handle_key(UiKey::Enter, UiMods::default()), Outcome::None);
        assert_eq!(
            core.handle_key(UiKey::Char("x".into()), UiMods::default()),
            Outcome::None
        );
        assert!(core.state.with(|s| s.error.is_none()));
        assert!(core.state.with(|s| s.query.is_empty()));
        // Escape по-прежнему закрывает.
        assert_eq!(core.handle_key(UiKey::Escape, UiMods::default()), Outcome::Exit);
    }

    #[test]
    fn hover_returns_whether_selection_changed() {
        let mut core = test_core();
        // Без разрешённых нод ховер ничего не меняет.
        assert!(!core.handle_hover(10.0, 10.0));
    }
}
