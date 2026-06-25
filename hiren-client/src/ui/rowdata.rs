//! AppRowData — glib::Object-подкласс для хранения данных приложения в gio::ListStore.
//!
//! Каждый экземпляр представляет одну строку в списке результатов. Используется
//! GtkListView через GtkSignalListItemFactory для реактивного отображения.

use glib::prelude::*;
use glib::subclass::prelude::*;
use hiren_shared::{AppEntry, AppMode};
use std::cell::RefCell;

mod imp {
    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::AppRowData)]
    pub struct AppRowData {
        #[property(get, set)]
        pub(super) id: RefCell<String>,
        #[property(get, set)]
        pub(super) name: RefCell<String>,
        #[property(get, set)]
        pub(super) exec: RefCell<String>,
        #[property(get, set)]
        pub(super) mode: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AppRowData {
        const NAME: &'static str = "AppRowData";
        type Type = super::AppRowData;
    }

    #[glib::derived_properties]
    impl ObjectImpl for AppRowData {}
}

glib::wrapper! {
    pub struct AppRowData(ObjectSubclass<imp::AppRowData>);
}

impl AppRowData {
    /// Создать экземпляр из записи о приложении.
    pub fn from_entry(entry: &AppEntry) -> Self {
        let mode_str = match entry.mode {
            AppMode::Drun => "app",
            AppMode::Run => "terminal",
            AppMode::Window => "window",
            AppMode::Calc => "calc",
        };
        glib::Object::builder()
            .property("id", entry.id.clone())
            .property("name", entry.name.clone())
            .property("exec", entry.exec.clone())
            .property("mode", mode_str)
            .build()
    }
}
