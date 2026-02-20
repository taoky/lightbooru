use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use adw::{Application, BottomSheet, NavigationSplitView, ToggleGroup};
use gtk::{
    self, Box as GtkBox, Button, GridView, Label, Picture, SearchEntry, SelectionMode,
    SignalListItemFactory, SingleSelection,
};
use tracing::debug;

use super::image_loader::{ImageLoader, ImageRequestKind};
use super::view::{
    append_pending_tags_input, ensure_selected_item_visible, grid_cell_widgets,
    infer_thumbnail_title, install_tag_editor_css, rebuild_tag_wrap, rebuild_view, refresh_detail,
    refresh_grid, rescan_library, save_selected_edits, set_status, show_banner, show_toast,
    sync_browser_selection,
};
use super::*;

#[derive(Clone)]
pub(super) struct GridItemData {
    pub(super) item_idx: usize,
    pub(super) texture: Rc<RefCell<Option<gtk::gdk::Texture>>>,
    pub(super) pending_request_id: Rc<Cell<Option<u64>>>,
}

impl Ui {
    fn from_builder(
        builder: &gtk::Builder,
        state: &Rc<RefCell<AppState>>,
        image_loader: Rc<ImageLoader>,
    ) -> (Self, UiControls) {
        let window: ApplicationWindow = builder_object(builder, "main_window");
        let toast_overlay: ToastOverlay = builder_object(builder, "toast_overlay");
        let compact_back_button: Button = builder_object(builder, "compact_back_button");
        let search: SearchEntry = builder_object(builder, "search");
        let search_bar: gtk::SearchBar = builder_object(builder, "search_bar");
        let search_button: gtk::ToggleButton = builder_object(builder, "search_button");
        let browse_mode_group: ToggleGroup = builder_object(builder, "browse_mode_group");
        let banner: Banner = builder_object(builder, "banner");
        let split: NavigationSplitView = builder_object(builder, "split");
        let list: ListBox = builder_object(builder, "list");
        let list_scroll: ScrolledWindow = builder_object(builder, "list_scroll");
        let grid: GridView = builder_object(builder, "grid");
        let browser_stack: ViewStack = builder_object(builder, "browser_stack");
        let picture: Picture = builder_object(builder, "picture");
        let title: Label = builder_object(builder, "title");
        let author: Label = builder_object(builder, "author");
        let date: Label = builder_object(builder, "date");
        let detail: Label = builder_object(builder, "detail");
        let tags_wrap: WrapBox = builder_object(builder, "tags_wrap");
        let tags_add_button: Button = builder_object(builder, "tags_add_button");
        let tags_input: Entry = builder_object(builder, "tags_input");
        let notes: TextView = builder_object(builder, "notes");
        let item_sensitive: gtk::Switch = builder_object(builder, "item_sensitive");
        let status: Label = builder_object(builder, "status");
        let detail_stack: ViewStack = builder_object(builder, "detail_stack");
        let edit_sheet: BottomSheet = builder_object(builder, "edit_sheet");
        let edit_bar: gtk::CenterBox = builder_object(builder, "edit_bar");
        let save_button: Button = builder_object(builder, "save_button");

        list.set_selection_mode(SelectionMode::Single);
        let (grid_store, grid_selection) = setup_grid_factory(state, &grid, image_loader.clone());

        let ui = Self {
            list,
            list_scroll,
            grid,
            grid_store,
            grid_selection,
            browser_stack,
            picture,
            title,
            author,
            date,
            detail,
            tags_wrap,
            tags_add_button,
            tags_input,
            tag_values: Rc::new(RefCell::new(Vec::new())),
            notes,
            item_sensitive,
            status,
            detail_stack,
            edit_sheet,
            toast_overlay,
            banner,
            detail_image_seq: Rc::new(Cell::new(0)),
            detail_pending_request_id: Rc::new(Cell::new(None)),
            grid_loaded_version: Rc::new(Cell::new(0)),
            image_loader,
        };

        let controls = UiControls {
            window,
            compact_back_button,
            search,
            search_bar,
            search_button,
            browse_mode_group,
            split,
            save_button,
            edit_bar,
        };

        (ui, controls)
    }
}

pub(crate) fn build_ui(app: &Application, state: Rc<RefCell<AppState>>) {
    install_tag_editor_css();

    let image_loader = Rc::new(ImageLoader::new());
    let builder = gtk::Builder::new();
    let scope = gtk::BuilderRustScope::new();
    install_builder_callbacks(&scope, &builder);
    builder.set_scope(Some(&scope));
    builder
        .add_from_string(APP_UI)
        .expect("failed to load UI from blueprint output");
    let (ui, controls) = Ui::from_builder(&builder, &state, image_loader);
    controls.window.set_application(Some(app));
    controls.search_bar.connect_entry(&controls.search);
    controls
        .search_bar
        .set_key_capture_widget(Some(&controls.window));
    let browser_mode = state.borrow().browser_mode;
    controls
        .browse_mode_group
        .set_active_name(Some(browser_mode.as_name()));
    ui.browser_stack
        .set_visible_child_name(browser_mode.as_name());
    sync_compact_controls(&controls);
    install_edit_sheet_open_gesture(&controls.edit_bar, &ui.edit_sheet);
    rebuild_tag_wrap(&ui);
    controls.window.present();
    rebuild_view(&state, &ui);
    connect_ui_signals(&state, &ui, &controls);
}

fn install_builder_callbacks(scope: &gtk::BuilderRustScope, builder: &gtk::Builder) {
    let builder_for_search = builder.clone();
    scope.add_callback("on_search_button_toggled", move |_| {
        let search_button: gtk::ToggleButton = builder_object(&builder_for_search, "search_button");
        let search_bar: gtk::SearchBar = builder_object(&builder_for_search, "search_bar");
        let search: SearchEntry = builder_object(&builder_for_search, "search");

        let enabled = search_button.is_active();
        search_bar.set_search_mode(enabled);
        if enabled {
            search.grab_focus();
        }
        None
    });

    let builder_for_search_mode = builder.clone();
    scope.add_callback("on_search_mode_enabled", move |_| {
        let search_button: gtk::ToggleButton =
            builder_object(&builder_for_search_mode, "search_button");
        let search_bar: gtk::SearchBar = builder_object(&builder_for_search_mode, "search_bar");
        let enabled = search_bar.is_search_mode();
        if search_button.is_active() != enabled {
            search_button.set_active(enabled);
        }
        None
    });

    let builder_for_split = builder.clone();
    scope.add_callback("on_split_layout_changed", move |_| {
        let compact_back_button: Button = builder_object(&builder_for_split, "compact_back_button");
        let split: NavigationSplitView = builder_object(&builder_for_split, "split");
        let search_button: gtk::ToggleButton = builder_object(&builder_for_split, "search_button");
        let browse_mode_group: ToggleGroup =
            builder_object(&builder_for_split, "browse_mode_group");
        let search_bar: gtk::SearchBar = builder_object(&builder_for_split, "search_bar");

        sync_compact_back_button(&compact_back_button, &split);
        sync_compact_header_controls(&search_button, &browse_mode_group, &search_bar, &split);
        None
    });
}

fn sync_compact_controls(controls: &UiControls) {
    sync_compact_back_button(&controls.compact_back_button, &controls.split);
    sync_compact_header_controls(
        &controls.search_button,
        &controls.browse_mode_group,
        &controls.search_bar,
        &controls.split,
    );
}

fn install_edit_sheet_open_gesture(edit_bar: &gtk::CenterBox, edit_sheet: &BottomSheet) {
    let edit_sheet = edit_sheet.clone();
    let bar_click = gtk::GestureClick::new();
    bar_click.connect_released(move |_, _, _, _| {
        if edit_sheet.can_open() {
            edit_sheet.set_open(true);
        }
    });
    edit_bar.add_controller(bar_click);
}

fn connect_ui_signals(state: &Rc<RefCell<AppState>>, ui: &Ui, controls: &UiControls) {
    {
        let ui = ui.clone();
        let tags_add_button = ui.tags_add_button.clone();
        tags_add_button.connect_clicked(move |_| {
            append_pending_tags_input(&ui);
            ui.tags_input.grab_focus();
        });
    }
    {
        let state_handle = state.clone();
        let ui = ui.clone();
        controls
            .browse_mode_group
            .connect_active_name_notify(move |group| {
                let mode = group
                    .active_name()
                    .as_deref()
                    .map(BrowserMode::from_name)
                    .unwrap_or(BrowserMode::List);
                {
                    let mut state = state_handle.borrow_mut();
                    state.browser_mode = mode;
                }
                ui.browser_stack.set_visible_child_name(mode.as_name());
                if matches!(mode, BrowserMode::Grid) {
                    refresh_grid(&state_handle, &ui);
                }
                let selected_pos = state_handle.borrow().selected_pos;
                sync_browser_selection(&ui, selected_pos);
                let ui_handle = ui.clone();
                gtk::glib::idle_add_local_once(move || {
                    ensure_selected_item_visible(&ui_handle, selected_pos);
                });
            });
    }
    {
        let state_handle = state.clone();
        let ui = ui.clone();
        controls.search.connect_search_changed(move |entry| {
            let mut state = state_handle.borrow_mut();
            state.query = entry.text().to_string();
            state.rebuild_filter();
            drop(state);
            rebuild_view(&state_handle, &ui);
        });
    }
    {
        let state_handle = state.clone();
        let ui = ui.clone();
        let show_sensitive_action = gtk::gio::SimpleAction::new_stateful(
            "show-sensitive",
            None,
            &gtk::glib::Variant::from(state.borrow().show_sensitive),
        );
        show_sensitive_action.connect_activate(move |action, _| {
            let mut state = state_handle.borrow_mut();
            state.show_sensitive = !state.show_sensitive;
            state.rebuild_filter();
            let show_sensitive = state.show_sensitive;
            drop(state);
            action.set_state(&gtk::glib::Variant::from(show_sensitive));
            rebuild_view(&state_handle, &ui);
            if show_sensitive {
                show_toast(&ui, "Showing sensitive items");
            } else {
                show_toast(&ui, "Hiding sensitive items");
            }
        });
        controls.window.add_action(&show_sensitive_action);
    }
    {
        let split = controls.split.clone();
        let state_handle = state.clone();
        let ui = ui.clone();
        let list_handle = ui.list.clone();
        list_handle.connect_row_selected(move |_list, row| {
            let mut state = state_handle.borrow_mut();
            let selected_pos = row
                .and_then(|row| usize::try_from(row.index()).ok())
                .filter(|pos| *pos < state.filtered_indices.len());
            state.selected_pos = selected_pos;
            drop(state);
            sync_browser_selection(&ui, selected_pos);
            refresh_detail(&state_handle, &ui);
            if split.is_collapsed() {
                split.set_show_content(selected_pos.is_some());
            }
        });
    }
    {
        let split = controls.split.clone();
        let list_handle = ui.list.clone();
        list_handle.connect_row_activated(move |_, _| {
            if split.is_collapsed() {
                split.set_show_content(true);
            }
        });
    }
    {
        let split = controls.split.clone();
        let state_handle = state.clone();
        let ui = ui.clone();
        let grid_selection_handle = ui.grid_selection.clone();
        grid_selection_handle.connect_selected_notify(move |selection| {
            let selected_pos = match selection.selected() {
                gtk::INVALID_LIST_POSITION => None,
                pos => usize::try_from(pos).ok(),
            };
            let selected_pos = {
                let mut state = state_handle.borrow_mut();
                let selected_pos = selected_pos.filter(|pos| *pos < state.filtered_indices.len());
                if state.selected_pos == selected_pos {
                    return;
                }
                state.selected_pos = selected_pos;
                selected_pos
            };

            sync_browser_selection(&ui, selected_pos);
            refresh_detail(&state_handle, &ui);
            if split.is_collapsed() {
                split.set_show_content(selected_pos.is_some());
            }
        });
    }
    {
        let split = controls.split.clone();
        let state_handle = state.clone();
        let ui = ui.clone();
        let grid_handle = ui.grid.clone();
        grid_handle.connect_activate(move |_, pos| {
            let activated_pos = usize::try_from(pos).ok();
            let (selected_pos, changed) = {
                let mut state = state_handle.borrow_mut();
                let selected_pos =
                    activated_pos.filter(|position| *position < state.filtered_indices.len());
                let changed = state.selected_pos != selected_pos;
                state.selected_pos = selected_pos;
                (selected_pos, changed)
            };

            if changed {
                sync_browser_selection(&ui, selected_pos);
                refresh_detail(&state_handle, &ui);
            }
            if split.is_collapsed() {
                split.set_show_content(selected_pos.is_some());
            }
        });
    }
    {
        let state_handle = state.clone();
        let ui = ui.clone();
        controls.save_button.connect_clicked(move |_| {
            if let Err(err) = save_selected_edits(&state_handle, &ui) {
                set_status(&ui, &format!("failed to save: {err}"));
                show_banner(&ui, &format!("Failed to save edits: {err}"));
            }
        });
    }
    {
        let state_handle = state.clone();
        let ui = ui.clone();
        let rescan_action = gtk::gio::SimpleAction::new("rescan", None);
        rescan_action.connect_activate(move |_, _| {
            if let Err(err) = rescan_library(&state_handle, &ui) {
                set_status(&ui, &format!("failed to rescan: {err}"));
                show_banner(&ui, &format!("Failed to rescan library: {err}"));
            }
        });
        controls.window.add_action(&rescan_action);
    }
    {
        let split = controls.split.clone();
        let state_handle = state.clone();
        let ui = ui.clone();
        let compact_back_button = controls.compact_back_button.clone();
        compact_back_button.connect_clicked(move |_| {
            if split.is_collapsed() && split.shows_content() {
                {
                    let mut state = state_handle.borrow_mut();
                    state.selected_pos = None;
                }
                sync_browser_selection(&ui, None);
                refresh_detail(&state_handle, &ui);
                split.set_show_content(false);
            }
        });
    }
    {
        let ui = ui.clone();
        let tags_input = ui.tags_input.clone();
        tags_input.connect_activate(move |_| {
            append_pending_tags_input(&ui);
        });
    }
}

fn setup_grid_factory(
    state: &Rc<RefCell<AppState>>,
    grid: &GridView,
    image_loader: Rc<ImageLoader>,
) -> (gtk::gio::ListStore, SingleSelection) {
    let grid_store = gtk::gio::ListStore::new::<gtk::glib::BoxedAnyObject>();
    let grid_selection = SingleSelection::new(Some(grid_store.clone()));
    grid_selection.set_autoselect(false);
    grid_selection.set_can_unselect(true);

    let grid_factory = SignalListItemFactory::new();
    grid_factory.connect_setup(|_, list_item_obj| {
        let Some(list_item) = list_item_obj.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let builder = gtk::Builder::from_string(GRID_CELL_UI);
        let card: GtkBox = builder_object(&builder, "card");
        list_item.set_child(Some(&card));
    });

    {
        let state_handle = state.clone();
        let image_loader_handle = image_loader.clone();
        grid_factory.connect_bind(move |_, list_item_obj| {
            let Some(list_item) = list_item_obj.downcast_ref::<gtk::ListItem>() else {
                return;
            };

            let Some(boxed_item) = list_item
                .item()
                .and_then(|obj| obj.downcast::<gtk::glib::BoxedAnyObject>().ok())
            else {
                return;
            };

            let data = boxed_item.borrow::<GridItemData>();
            let item_idx = data.item_idx;
            let texture_slot = data.texture.clone();
            let pending_request_id = data.pending_request_id.clone();
            drop(data);

            let Some((card, thumb, caption)) = grid_cell_widgets(list_item) else {
                return;
            };

            let (title, tooltip, image_path) = {
                let state = state_handle.borrow();
                let Some(item) = state.library.index.items.get(item_idx) else {
                    thumb.set_paintable(None::<&gtk::gdk::Texture>);
                    caption.set_text("(missing)");
                    card.set_tooltip_text(None::<&str>);
                    return;
                };

                let title = infer_thumbnail_title(item);
                let tooltip = if item.merged_sensitive() {
                    format!("[Sensitive] {}", item.image_path.display())
                } else {
                    item.image_path.display().to_string()
                };
                (title, tooltip, item.image_path.clone())
            };

            caption.set_text(&title);
            card.set_tooltip_text(Some(&tooltip));

            if let Some(texture) = texture_slot.borrow().as_ref() {
                thumb.set_paintable(Some(texture));
                return;
            }

            if let Some(previous_request_id) = pending_request_id.replace(None) {
                image_loader_handle.cancel_if_queued(previous_request_id);
            }

            thumb.set_paintable(None::<&gtk::gdk::Texture>);
            let tooltip_guard = tooltip.clone();
            let thumb_weak = thumb.downgrade();
            let card_weak = card.downgrade();
            let pending_request_slot = pending_request_id.clone();
            debug!("Load {}", image_path.display());
            let request_id = image_loader_handle.load(
                image_path,
                Some((156, 156)),
                ImageRequestKind::GridThumb,
                move |finished_id, result| {
                    if pending_request_slot.get() == Some(finished_id) {
                        pending_request_slot.set(None);
                    }

                    let Some(thumb) = thumb_weak.upgrade() else {
                        return;
                    };
                    let Some(card) = card_weak.upgrade() else {
                        return;
                    };

                    if card.tooltip_text().as_deref() != Some(tooltip_guard.as_str()) {
                        return;
                    }

                    match result {
                        Ok(texture) => {
                            texture_slot.borrow_mut().replace(texture.clone());
                            thumb.set_paintable(Some(&texture));
                        }
                        Err(_) => thumb.set_paintable(None::<&gtk::gdk::Texture>),
                    }
                },
            );
            pending_request_id.set(Some(request_id));
        });
    }

    {
        let image_loader_handle = image_loader.clone();
        grid_factory.connect_unbind(move |_, list_item_obj| {
            let Some(list_item) = list_item_obj.downcast_ref::<gtk::ListItem>() else {
                return;
            };

            let pending_request_id = list_item
                .item()
                .and_then(|obj| obj.downcast::<gtk::glib::BoxedAnyObject>().ok())
                .and_then(|boxed_item| {
                    let data = boxed_item.borrow::<GridItemData>();
                    data.pending_request_id.replace(None)
                });
            if let Some(request_id) = pending_request_id {
                debug!("Canceling task {}", request_id);
                image_loader_handle.cancel_if_queued(request_id);
            }

            let Some((card, thumb, caption)) = grid_cell_widgets(list_item) else {
                return;
            };

            thumb.set_paintable(Option::<&gtk::gdk::Texture>::None);
            caption.set_text("");
            card.set_tooltip_text(None::<&str>);
        });
    }

    grid.set_model(Some(&grid_selection));
    grid.set_factory(Some(&grid_factory));
    grid.set_single_click_activate(false);
    grid.set_max_columns(4);
    grid.set_min_columns(2);
    (grid_store, grid_selection)
}

fn sync_compact_back_button(button: &Button, split: &NavigationSplitView) {
    let show_button = split.is_collapsed() && split.shows_content();
    button.set_visible(show_button);
    button.set_sensitive(show_button);
}

fn sync_compact_header_controls(
    search_button: &gtk::ToggleButton,
    browse_mode_group: &ToggleGroup,
    search_bar: &gtk::SearchBar,
    split: &NavigationSplitView,
) {
    let show_controls = !(split.is_collapsed() && split.shows_content());
    search_button.set_visible(show_controls);
    browse_mode_group.set_visible(show_controls);

    if !show_controls {
        if search_button.is_active() {
            search_button.set_active(false);
        }
        if search_bar.is_search_mode() {
            search_bar.set_search_mode(false);
        }
    }
}
