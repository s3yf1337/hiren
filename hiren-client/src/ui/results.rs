//! ResultsList — список результатов: GtkListView внутри GtkScrolledWindow.
//!
//! ScrolledWindow задаёт max_content_height (из конфига окна), ListView
//! получает полную высоту внутри Viewport и не активирует свой скролл.
//! Скроллбар — от ScrolledWindow.

use crate::ui::rowdata::AppRowData;
use gtk4 as gtk;
use gtk::prelude::*;
use gtk::pango::EllipsizeMode;
use gtk::{
    Align, Image, Label, ListItem, ListView, Orientation, Overlay, PolicyType, ScrolledWindow,
    SignalListItemFactory, SingleSelection,
};
use hiren_shared::AppEntry;

const INVALID_LIST_POSITION: u32 = u32::MAX;

pub struct ResultsList {
    scrolled: ScrolledWindow,
    list_view: ListView,
    model: gtk::gio::ListStore,
    selection: SingleSelection,
}

impl ResultsList {
    /// `max_height` — максимальная высота видимой области (под ListView).
    /// `text_align` — выравнивание текста (0.0 = слева, 0.5 = по центру).
    pub fn new(max_height: i32, text_align: f32) -> Self {
        let model = gtk::gio::ListStore::new::<AppRowData>();
        let selection = SingleSelection::new(Some(model.clone()));
        let factory = Self::create_factory(text_align);
        let list_view = ListView::new(Some(selection.clone()), Some(factory.clone()));

        list_view.add_css_class("results-list");
        list_view.set_halign(Align::Fill);
        list_view.set_hexpand(true);

        let scrolled = ScrolledWindow::builder()
            .hscrollbar_policy(PolicyType::Never)
            .vscrollbar_policy(PolicyType::Automatic)
            .max_content_height(max_height)
            .propagate_natural_height(true)
            .hexpand(true)
            .build();
        scrolled.set_child(Some(&list_view));

        Self { scrolled, list_view, model, selection }
    }

    pub fn widget(&self) -> &ScrolledWindow {
        &self.scrolled
    }

    pub fn list_view(&self) -> &ListView {
        &self.list_view
    }

    pub fn update_results(&self, apps: &[AppEntry]) {
        self.model.remove_all();
        for app in apps {
            self.model.append(&AppRowData::from_entry(app));
        }
        if self.model.n_items() > 0 {
            self.selection.set_selected(0);
        }
    }

    pub fn navigate(&self, delta: i32) {
        let n_items = self.model.n_items();
        if n_items == 0 { return; }

        let current = self.selection.selected();
        let new_index: u32 = if current == INVALID_LIST_POSITION {
            if delta > 0 { 0 } else { n_items.saturating_sub(1) }
        } else {
            let pos = current as i32;
            (pos + delta).clamp(0, n_items as i32 - 1) as u32
        };

        self.selection.set_selected(new_index);

        let vadj = self.scrolled.vadjustment();
        glib::idle_add_local(move || {
            let upper = vadj.upper();
            let page_size = vadj.page_size();
            if upper <= page_size || upper <= 0.0 {
                return glib::ControlFlow::Break;
            }
            // Реальная высота строки = всё содержимое / кол-во элементов
            let row_h = upper / n_items as f64;
            let item_top = new_index as f64 * row_h;
            let item_bottom = item_top + row_h;
            let view_top = vadj.value();
            let view_bottom = view_top + page_size;

            let new_scroll = if item_top < view_top {
                item_top
            } else if item_bottom > view_bottom {
                (item_bottom - page_size).max(0.0)
            } else {
                return glib::ControlFlow::Break;
            };
            vadj.set_value(new_scroll.clamp(0.0, (upper - page_size).max(0.0)));
            glib::ControlFlow::Break
        });
    }

    pub fn selected_index(&self) -> u32 { self.selection.selected() }
    pub fn len(&self) -> u32 { self.model.n_items() }
    pub fn is_empty(&self) -> bool { self.len() == 0 }

    pub fn set_visible(&self, visible: bool) {
        self.scrolled.set_visible(visible);
    }

    // -----------------------------------------------------------------

    fn create_factory(text_align: f32) -> SignalListItemFactory {
        let factory = SignalListItemFactory::new();
        factory.connect_setup(move |_, list_item_obj| {
            let Some(item) = list_item_obj.downcast_ref::<ListItem>() else { return };
            item.set_child(Some(&Self::make_row(text_align)));
        });
        factory.connect_bind(|_, list_item_obj| {
            let Some(item) = list_item_obj.downcast_ref::<ListItem>() else { return };
            let Some(data_obj) = item.item() else { return };
            let Ok(data) = data_obj.downcast::<AppRowData>() else { return };
            let Some(child) = item.child() else { return };
            Self::fill_row(&child, &data);
        });
        factory
    }

    fn make_row(text_align: f32) -> Overlay {
        // Текстовая метка: выравнивание из конфига
        let label = Label::new(None);
        label.add_css_class("app-name");
        label.set_halign(Align::Fill);
        label.set_hexpand(true);
        label.set_xalign(text_align);
        label.set_ellipsize(EllipsizeMode::End);

        let label_box = gtk::Box::new(Orientation::Horizontal, 0);
        label_box.set_halign(Align::Fill);
        label_box.set_hexpand(true);
        label_box.append(&label);

        // Иконка: оверлей поверх текста, прижата влево
        let icon = Image::new();
        icon.add_css_class("app-icon");
        icon.set_pixel_size(24);
        icon.set_halign(Align::Start);
        icon.set_valign(Align::Center);

        let overlay = Overlay::new();
        overlay.set_child(Some(&label_box));
        overlay.add_overlay(&icon);

        overlay
    }

    fn fill_row(child: &gtk::Widget, data: &AppRowData) {
        // Обходим всех детей Overlay (базовый + оверлейные) без разбора
        if let Some(ov) = child.downcast_ref::<Overlay>() {
            // Базовый ребёнок: GtkBox с Label внутри
            if let Some(base) = ov.child() {
                let mut bnext = base.first_child();
                while let Some(bw) = bnext.take() {
                    if let Some(lbl) = bw.downcast_ref::<Label>() {
                        lbl.set_label(&data.name());
                        break;
                    }
                    bnext = bw.next_sibling();
                }
            }
            // Оверлейные дети: Image
            let mut next = ov.first_child();
            while let Some(w) = next.take() {
                if let Some(img) = w.downcast_ref::<Image>() {
                    if data.mode() == "app" {
                        img.set_visible(true);
                        img.set_icon_name(Some(&data.id()));
                    } else {
                        img.set_visible(false);
                    }
                    break;
                }
                next = w.next_sibling();
            }
        }
    }
}
