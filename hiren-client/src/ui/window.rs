//! LauncherWindow — главное окно лаунчера.
//!
//! Без декораций, размеры из TOML-конфига.
//!
//! Компоновка: window → root_box(Fill,Start) → outer_box(видимая карточка)
//!   → SearchInput + ResultsList(ScrolledWindow.max_content_height)

use crate::config::Config;
use crate::ui::results::ResultsList;
use crate::ui::search::SearchInput;
use gtk4 as gtk;
use gtk::prelude::*;
use gtk::{Align, Application, ApplicationWindow, Box as GtkBox, Label, Orientation};

pub struct LauncherWindow {
    window: ApplicationWindow,
    pub search_input: SearchInput,
    pub results_list: ResultsList,
    mode_label: Label,
}

impl LauncherWindow {
    pub fn build(app: &Application, config: &Config) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("hiren")
            .default_width(config.window_width)
            .default_height(config.window_height)
            .decorated(false)
            .resizable(false)
            .build();
        window.add_css_class("launcher-window");

        let root_box = GtkBox::new(Orientation::Vertical, 0);
        root_box.set_halign(Align::Fill);
        root_box.set_valign(Align::Start);
        root_box.set_hexpand(true);
        root_box.set_vexpand(true);
        window.set_child(Some(&root_box));

        let outer_box = GtkBox::new(Orientation::Vertical, 0);
        outer_box.add_css_class("outer-box");
        outer_box.set_halign(Align::Fill);
        outer_box.set_hexpand(true);

        // Индикатор режима — скрыт по умолчанию (без переключений)
        let mode_label = Label::new(None);
        mode_label.add_css_class("mode-indicator");
        mode_label.set_halign(Align::Center);
        mode_label.set_margin_bottom(4);
        mode_label.set_visible(false);
        outer_box.append(&mode_label);

        let search_input = SearchInput::new(config.text_align);

        let max_list = (config.window_height - 100).max(100);
        let results_list = ResultsList::new(max_list, config.text_align);
        results_list.set_visible(false);

        outer_box.append(search_input.widget());
        outer_box.append(results_list.widget());
        root_box.append(&outer_box);

        Self { window, search_input, results_list, mode_label }
    }

    /// Скрыть индикатор режима.
    pub fn hide_mode_indicator(&self) {
        self.mode_label.set_visible(false);
    }

    pub fn window(&self) -> &ApplicationWindow { &self.window }
    pub fn present(&self) { self.window.present(); }
    pub fn destroy(&self) { self.window.destroy(); }
}
