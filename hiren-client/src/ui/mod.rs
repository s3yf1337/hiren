//! UI-модуль hiren-client: оркестрация виджетов, обработка ввода, запуск приложений.
//!
//! Модульная архитектура:
//! - `window` — главное окно (LauncherWindow)
//! - `search` — поле ввода (SearchInput)
//! - `results` — список результатов (ResultsList на gio::ListStore + GtkListView)
//! - `rowdata` — модель данных для ListStore (AppRowData)
//! - `keyboard` — изолированная обработка клавиатуры (KeyboardHandler)
//!
//! Все активные источники (drun, run, calc, window) работают одновременно —
//! без переключения режимов. Результаты объединяются в один список.

pub mod keyboard;
pub mod results;
pub mod rowdata;
pub mod search;
pub mod window;

use crate::config::Config;
use crate::freq::FreqHistory;
use crate::modes::{self, SearchMode, SearchResult};
use gtk4 as gtk;
use gtk::glib;
use gtk::prelude::*;
use hiren_shared::{AppEntry, AppMode};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Debounce-хелпер для поиска
// ---------------------------------------------------------------------------

struct Debounce {
    source_id: Rc<RefCell<Option<glib::SourceId>>>,
}

impl Debounce {
    fn new() -> Self {
        Self {
            source_id: Rc::new(RefCell::new(None)),
        }
    }

    fn schedule<F: FnOnce(String) + 'static>(&self, query: String, delay: Duration, f: F) {
        if let Some(id) = self.source_id.borrow_mut().take() {
            id.remove();
        }
        let mut f = Some(f);
        let mut query = Some(query);
        let source_ref = Rc::clone(&self.source_id);
        let id = glib::timeout_add_local(delay, move || {
            source_ref.borrow_mut().take();
            if let (Some(q), Some(cb)) = (query.take(), f.take()) {
                cb(q);
            }
            glib::ControlFlow::Break
        });
        self.source_id.borrow_mut().replace(id);
    }
}

// ---------------------------------------------------------------------------
// Контекст UI — оркестратор
// ---------------------------------------------------------------------------

pub struct UiContext {
    pub window_launcher: window::LauncherWindow,
    apps: Rc<RefCell<Vec<AppEntry>>>,
    config: Rc<Config>,
    _debounce: Rc<Debounce>,
    last_activity: Rc<Cell<Instant>>,
    _keyboard: RefCell<Option<keyboard::KeyboardHandler>>,
    /// Источники данных (по одному на каждый активный режим).
    sources: Rc<RefCell<HashMap<AppMode, Box<dyn SearchMode>>>>,
    /// Частотная история запусков.
    freq: Rc<RefCell<FreqHistory>>,
}

impl UiContext {
    pub fn build(app: &gtk::Application, config: Config) -> Rc<Self> {
        let auto_close_timeout = config.auto_close_timeout_secs;
        let config = Rc::new(config);
        let debounce = Rc::new(Debounce::new());
        let apps: Rc<RefCell<Vec<AppEntry>>> = Rc::new(RefCell::new(Vec::new()));
        let last_activity = Rc::new(Cell::new(Instant::now()));
        let freq = Rc::new(RefCell::new(FreqHistory::load()));

        // Создаём источники для всех активных режимов
        let sources: Rc<RefCell<HashMap<AppMode, Box<dyn SearchMode>>>> =
            Rc::new(RefCell::new(HashMap::new()));
        {
            let mut src = sources.borrow_mut();
            for mode in config.active_modes() {
                let mut instance = modes::create_mode(mode);
                instance.init(&config);
                src.insert(mode, instance);
            }
        }

        // --- Построение окна со всеми виджетами ---
        let window_launcher = window::LauncherWindow::build(app, &config);

        // Убираем индикатор режима — теперь без переключений
        window_launcher.hide_mode_indicator();

        let ctx = Rc::new(Self {
            window_launcher,
            apps: apps.clone(),
            config: config.clone(),
            _debounce: debounce.clone(),
            last_activity: last_activity.clone(),
            _keyboard: RefCell::new(None),
            sources: sources.clone(),
            freq: freq.clone(),
        });

        // --- Сигнал: изменение текста → debounced поиск ---
        ctx.connect_search(debounce);

        // --- Сигнал: активация строки в списке ---
        ctx.connect_list_activate();

        // --- Клавиатурный обработчик ---
        ctx.setup_keyboard();

        // --- Загружаем все приложения при старте ---
        ctx.trigger_search(String::new());

        // --- Safety timeout: авто-закрытие через N секунд бездействия ---
        if auto_close_timeout > 0 {
            let ctx_weak = Rc::downgrade(&ctx);
            glib::timeout_add_local(Duration::from_secs(1), move || {
                if let Some(ctx) = ctx_weak.upgrade() {
                    if ctx.last_activity.get().elapsed()
                        > Duration::from_secs(auto_close_timeout)
                    {
                        eprintln!("[hiren-client] Auto-close: inactivity timeout");
                        ctx.destroy_window();
                        return glib::ControlFlow::Break;
                    }
                    glib::ControlFlow::Continue
                } else {
                    glib::ControlFlow::Break
                }
            });
        }

        ctx
    }

    // -----------------------------------------------------------------------
    // Клавиатура
    // -----------------------------------------------------------------------

    fn setup_keyboard(self: &Rc<Self>) {
        let ctx_weak = Rc::downgrade(self);
        let window = self.window_launcher.window();
        let entry = self.window_launcher.search_input.widget().clone();

        let kb = keyboard::KeyboardHandler::new(
            window,
            entry,
            self.config.clone(),
            self.last_activity.clone(),
            // navigate
            {
                let ctx_weak = ctx_weak.clone();
                move |delta| {
                    if let Some(ctx) = ctx_weak.upgrade() {
                        ctx.last_activity.set(Instant::now());
                        ctx.navigate_list(delta);
                    }
                }
            },
            // launch
            {
                let ctx_weak = ctx_weak.clone();
                move |prefix| {
                    if let Some(ctx) = ctx_weak.upgrade() {
                        ctx.launch_selected(prefix);
                    }
                }
            },
            // close
            {
                let ctx_weak = ctx_weak.clone();
                move || {
                    if let Some(ctx) = ctx_weak.upgrade() {
                        ctx.destroy_window();
                    }
                }
            },
        );

        *self._keyboard.borrow_mut() = Some(kb);
    }

    // -----------------------------------------------------------------------
    // Сигнал: изменение текста → debounced поиск
    // -----------------------------------------------------------------------

    fn connect_search(self: &Rc<Self>, debounce: Rc<Debounce>) {
        let ctx = Rc::downgrade(self);
        self.window_launcher
            .search_input
            .widget()
            .connect_changed(move |e: &gtk::Entry| {
                let query = e.text().to_string();
                if let Some(ctx) = ctx.upgrade() {
                    let ctx2 = Rc::clone(&ctx);
                    debounce.schedule(query, Duration::from_millis(150), move |q| {
                        ctx2.trigger_search(q);
                    });
                }
            });
    }

    // -----------------------------------------------------------------------
    // Сигнал: активация строки в списке
    // -----------------------------------------------------------------------

    fn connect_list_activate(self: &Rc<Self>) {
        let ctx = Rc::downgrade(self);
        self.window_launcher
            .results_list
            .list_view()
            .connect_activate(move |_list: &gtk::ListView, _position: u32| {
                if let Some(ctx) = ctx.upgrade() {
                    ctx.launch_selected(None);
                }
            });
    }

    // -----------------------------------------------------------------------
    // Комбинированный поиск по всем активным источникам
    // -----------------------------------------------------------------------

    fn trigger_search(self: &Rc<Self>, query: String) {
        let query = query.trim().to_string();
        let mut calc_results: Vec<AppEntry> = Vec::new();
        let mut window_results: Vec<AppEntry> = Vec::new();
        let mut other_results: Vec<AppEntry> = Vec::new();

        let sources = self.sources.borrow();

        // 1. calc — всегда первый если есть валидное выражение
        //    CalcMode::search() возвращает не более 1 результата, скоринг не нужен
        if sources.contains_key(&AppMode::Calc) {
            if let Some(calc) = sources.get(&AppMode::Calc) {
                match calc.search(&query) {
                    SearchResult::Entries(entries) => {
                        for e in entries {
                            if !e.name.starts_with("Error") && !e.name.contains("Enter a math") {
                                calc_results.push(e);
                            }
                        }
                    }
                    SearchResult::Error(_) => {}
                }
            }
        }

        // 2. drun — результаты уже отсортированы демоном (SkimMatcherV2, score desc)
        if sources.contains_key(&AppMode::Drun) {
            if let Some(drun) = sources.get(&AppMode::Drun) {
                match drun.search(&query) {
                    SearchResult::Entries(entries) => other_results.extend(entries),
                    SearchResult::Error(e) => eprintln!("[hiren-client] drun error: {e}"),
                }
            }
        }

        // 3. run — результаты уже отсортированы RunMode (SkimMatcherV2, score desc)
        if sources.contains_key(&AppMode::Run) {
            if let Some(run) = sources.get(&AppMode::Run) {
                match run.search(&query) {
                    SearchResult::Entries(entries) => other_results.extend(entries),
                    SearchResult::Error(e) => eprintln!("[hiren-client] run error: {e}"),
                }
            }
        }

        // 4. window — результаты уже отсортированы WindowMode
        if sources.contains_key(&AppMode::Window) {
            if let Some(win) = sources.get(&AppMode::Window) {
                match win.search(&query) {
                    SearchResult::Entries(entries) => window_results.extend(entries),
                    SearchResult::Error(e) => eprintln!("[hiren-client] window error: {e}"),
                }
            }
        }

        // Каждый источник уже вернул результаты в порядке убывания релевантности.
        // Повторный скоринг не требуется — все режимы используют SkimMatcherV2::default().
        if query.is_empty() {
            // Пустой запрос: частые приложения — наверх (после window)
            let freq = self.freq.borrow();
            let (frequent, mut rest) = freq.partition_by_freq(other_results);
            rest.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            other_results = frequent;
            other_results.extend(rest);
        } else {
            // Непустой запрос: гибридная сортировка «релевантность × частота».
            // Часто используемые приложения поднимаются выше, но точный
            // релевантный матч всё ещё побеждает.
            let freq = self.freq.borrow();
            let weight = self.config.freq_weight;
            other_results = freq.sort_by_frecency(other_results, weight);
        }

        // Финальный порядок: calc → window → частые → остальные drun/run
        let mut all_entries = calc_results;
        all_entries.extend(window_results);
        all_entries.extend(other_results);

        let is_empty = all_entries.is_empty();
        *self.apps.borrow_mut() = all_entries.clone();
        self.window_launcher
            .results_list
            .update_results(&all_entries);
        self.window_launcher
            .results_list
            .set_visible(!is_empty);
    }

    // -----------------------------------------------------------------------
    // Навигация по списку
    // -----------------------------------------------------------------------

    fn navigate_list(&self, delta: i32) {
        let results = &self.window_launcher.results_list;
        if results.is_empty() {
            return;
        }
        results.navigate(delta);
    }

    // -----------------------------------------------------------------------
    // Запуск / выполнение — диспатч по типу записи
    // -----------------------------------------------------------------------

    fn launch_selected(&self, prefix: Option<String>) {
        let apps = self.apps.borrow();

        let results = &self.window_launcher.results_list;
        let index = if !results.is_empty() {
            let sel = results.selected_index();
            if sel == u32::MAX {
                None
            } else {
                Some(sel as usize)
            }
        } else {
            None
        };

        let app_entry = index
            .and_then(|i| apps.get(i))
            .or_else(|| apps.first());

        let Some(app_entry) = app_entry else {
            eprintln!("[hiren-client] No entry to launch");
            return;
        };

        let mode = app_entry.mode;

        // Диспатч по режиму записи
        if mode == AppMode::Calc {
            let sources = self.sources.borrow();
            if let Some(calc) = sources.get(&AppMode::Calc) {
                calc.execute(app_entry, &self.config);
            }
            self.destroy_window();
            return;
        }

        if mode == AppMode::Window {
            let sources = self.sources.borrow();
            if let Some(win) = sources.get(&AppMode::Window) {
                win.execute(app_entry, &self.config);
            }
            self.destroy_window();
            return;
        }

        // Drun / Run — стандартный запуск
        let exec = match &prefix {
            Some(p) => format!("{p} {}", app_entry.exec),
            None => app_entry.exec.clone(),
        };

        // Записываем в историю
        self.freq.borrow_mut().record_launch(&app_entry.exec);

        eprintln!("[hiren-client] Launching: {exec}");
        crate::modes::exec_detached(&exec);

        self.destroy_window();
    }

    /// Уничтожить окно и выйти из приложения.
    fn destroy_window(&self) {
        self.window_launcher.destroy();
    }
}
