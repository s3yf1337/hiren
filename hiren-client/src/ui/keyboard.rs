//! KeyboardHandler — изолированная логика обработки клавиатурных событий.
//!
//! Обрабатывает (на Capture-фазе, до того как Entry потребит события):
//! - Escape — закрыть лаунчер
//! - Пользовательские бинды из Config (Enter, Ctrl+Enter и т.д.) — запуск с префиксом
//! - Стрелки вверх/вниз — навигация по списку результатов
//! - Все остальные клавиши пропускаются к Entry

use crate::config::Config;
use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use gtk::glib::translate::IntoGlib;
use gtk::prelude::*;
use gtk::{ApplicationWindow, EventControllerKey, PropagationPhase};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

pub struct KeyboardHandler {
    _controller: EventControllerKey,
}

impl KeyboardHandler {
    pub fn new(
        window: &ApplicationWindow,
        entry: gtk::Entry,
        config: Rc<Config>,
        last_activity: Rc<Cell<Instant>>,
        on_navigate: impl Fn(i32) + 'static,
        on_launch: impl Fn(Option<String>) + 'static,
        on_close: impl Fn() + 'static,
    ) -> Self {
        let controller = EventControllerKey::new();
        controller.set_propagation_phase(PropagationPhase::Capture);

        let mod_mask = gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::SHIFT_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SUPER_MASK
            | gdk::ModifierType::META_MASK;

        controller.connect_key_pressed(
            move |_controller: &EventControllerKey,
                  keyval: gdk::Key,
                  _keycode: u32,
                  state: gdk::ModifierType| {
                let raw: u32 = keyval.into_glib();
                let mods = state & mod_mask;

                last_activity.set(Instant::now());

                // Escape — закрыть лаунчер
                if raw == 0xFF1B {
                    on_close();
                    return glib::Propagation::Stop;
                }

                // Поиск совпадения в биндах (первый совпавший используется)
                for binding in &config.bindings {
                    if raw == binding.keyval && mods == binding.modifiers {
                        on_launch(binding.prefix.clone());
                        return glib::Propagation::Stop;
                    }
                }

                // Стрелка вниз — навигация, только если entry не в фокусе
                // (иначе пропускаем к entry для перемещения курсора)
                if (raw == 0xFF54 || raw == 0xFF9A)
                    || (raw == 0xFF52 || raw == 0xFF98)
                {
                    if !entry.has_focus() {
                        let delta = if raw == 0xFF54 || raw == 0xFF9A { 1 } else { -1 };
                        on_navigate(delta);
                        return glib::Propagation::Stop;
                    }
                    // entry в фокусе — пропускаем событие дальше для редактирования
                }

                glib::Propagation::Proceed
            },
        );

        window.add_controller(controller.clone());

        Self {
            _controller: controller,
        }
    }
}
