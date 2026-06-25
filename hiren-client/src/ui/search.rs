//! SearchInput — кастомный GtkEntry для ввода поискового запроса.
//!
//! Никакой бизнес-логики внутри — только создание виджета и CSS-класс `.search-entry`.

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::{Align, Entry};

/// Обёртка над GtkEntry с преднастроенными свойствами и CSS-классом.
pub struct SearchInput {
    entry: Entry,
}

impl SearchInput {
    /// Создать новое поле ввода.
    pub fn new(xalign: f32) -> Self {
        let entry = Entry::builder()
            .placeholder_text("Type to search…")
            .halign(Align::Fill)
            .hexpand(true)
            .xalign(xalign)
            .build();
        entry.add_css_class("search-entry");

        Self { entry }
    }

    /// Получить ссылку на внутренний GtkEntry.
    pub fn widget(&self) -> &Entry {
        &self.entry
    }
}

impl Default for SearchInput {
    fn default() -> Self {
        Self::new(0.0)
    }
}
