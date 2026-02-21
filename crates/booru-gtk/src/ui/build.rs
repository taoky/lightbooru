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
    append_pending_tags_input, apply_search, ensure_selected_item_visible, grid_cell_widgets,
    infer_thumbnail_title, install_tag_editor_css, open_selected_file, open_selected_source_url,
    rebuild_tag_wrap, rebuild_view, refresh_detail, refresh_grid, rescan_library,
    save_selected_edits, selected_author, selected_source_url, show_error_dialog, show_toast,
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
        let search: SearchEntry = builder_object(builder, "search");
        let search_bar: gtk::SearchBar = builder_object(builder, "search_bar");
        let browse_mode_group: ToggleGroup = builder_object(builder, "browse_mode_group");
        let banner: Banner = builder_object(builder, "banner");
        let split: NavigationSplitView = builder_object(builder, "split");
        let list: ListBox = builder_object(builder, "list");
        let list_scroll: ScrolledWindow = builder_object(builder, "list_scroll");
        let grid: GridView = builder_object(builder, "grid");
        let browser_stack: ViewStack = builder_object(builder, "browser_stack");
        let picture: Picture = builder_object(builder, "picture");
        let title: Label = builder_object(builder, "title");
        let author: Button = builder_object(builder, "author");
        let date: Label = builder_object(builder, "date");
        let source_url: gtk::LinkButton = builder_object(builder, "source_url");
        let search_same_source_button: Button =
            builder_object(builder, "search_same_source_button");
        let open_file_button: Button = builder_object(builder, "open_file_button");
        let detail: Label = builder_object(builder, "detail");
        let tags_wrap: WrapBox = builder_object(builder, "tags_wrap");
        let tags_add_button: Button = builder_object(builder, "tags_add_button");
        let tags_input: Entry = builder_object(builder, "tags_input");
        let notes: TextView = builder_object(builder, "notes");
        let item_sensitive: gtk::Switch = builder_object(builder, "item_sensitive");
        let detail_stack: ViewStack = builder_object(builder, "detail_stack");
        let edit_sheet: BottomSheet = builder_object(builder, "edit_sheet");
        let edit_bar: gtk::CenterBox = builder_object(builder, "edit_bar");
        let save_button: Button = builder_object(builder, "save_button");

        list.set_selection_mode(SelectionMode::Single);
        let (grid_store, grid_selection) = setup_grid_factory(state, &grid, image_loader.clone());

        let ui = Self {
            window: window.clone(),
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
            source_url,
            search_same_source_button,
            open_file_button,
            detail,
            tags_wrap,
            tags_add_button,
            tags_input,
            tag_values: Rc::new(RefCell::new(Vec::new())),
            notes,
            item_sensitive,
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
            search,
            search_bar,
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
    ui.detail_stack.set_visible_child_name("empty");
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

fn build_item_context_popover(parent: &impl gtk::prelude::IsA<gtk::Widget>) -> gtk::PopoverMenu {
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Open file"), Some("win.open-file"));
    menu.append(Some("Open source URL"), Some("win.open-source-url"));
    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(parent);
    popover
}

fn popup_context_menu(popover: &gtk::PopoverMenu, x: f64, y: f64) {
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.popup();
}

fn connect_ui_signals(state: &Rc<RefCell<AppState>>, ui: &Ui, controls: &UiControls) {
    let suppress_search_changed = Rc::new(Cell::new(false));
    {
        let list = ui.list.clone();
        let popover = build_item_context_popover(&list);

        let list_handle = list.clone();
        let popover_handle = popover.clone();
        let right_click = gtk::GestureClick::builder().button(3).build();
        right_click.connect_pressed(move |gesture, _, x, y| {
            let Some(row) = list_handle.row_at_y(y as i32) else {
                return;
            };
            list_handle.select_row(Some(&row));
            popup_context_menu(&popover_handle, x, y);
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        list.add_controller(right_click);
    }
    {
        let search_bar = controls.search_bar.clone();
        let search = controls.search.clone();
        let key_controller = gtk::EventControllerKey::new();
        key_controller.connect_key_pressed(move |_, key, _, modifiers| {
            let is_ctrl_f = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && matches!(key, gtk::gdk::Key::f | gtk::gdk::Key::F);
            if is_ctrl_f {
                search_bar.set_search_mode(true);
                search.grab_focus();
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
        controls.window.add_controller(key_controller);
    }
    {
        let state_handle = state.clone();
        let ui = ui.clone();
        let search = controls.search.clone();
        let search_bar = controls.search_bar.clone();
        let suppress = suppress_search_changed.clone();
        let author = ui.author.clone();
        author.connect_clicked(move |_| {
            let Some(author_name) = selected_author(&state_handle) else {
                show_toast(&ui, "No author available for selected item");
                return;
            };

            suppress.set(true);
            search.set_text(&author_name);
            suppress.set(false);
            search_bar.set_search_mode(true);
            apply_search(&state_handle, &ui, author_name);
        });
    }
    {
        let state_handle = state.clone();
        let ui = ui.clone();
        let open_file_button = ui.open_file_button.clone();
        open_file_button.connect_clicked(move |_| {
            open_selected_file(&state_handle, &ui);
        });
    }
    {
        let state_handle = state.clone();
        let ui = ui.clone();
        let open_file_action = gtk::gio::SimpleAction::new("open-file", None);
        open_file_action.connect_activate(move |_, _| {
            open_selected_file(&state_handle, &ui);
        });
        controls.window.add_action(&open_file_action);
    }
    {
        let state_handle = state.clone();
        let ui = ui.clone();
        let open_source_action = gtk::gio::SimpleAction::new("open-source-url", None);
        open_source_action.connect_activate(move |_, _| {
            open_selected_source_url(&state_handle, &ui);
        });
        controls.window.add_action(&open_source_action);
    }
    {
        let state_handle = state.clone();
        let ui = ui.clone();
        let search = controls.search.clone();
        let search_bar = controls.search_bar.clone();
        let suppress = suppress_search_changed.clone();
        let search_same_source_button = ui.search_same_source_button.clone();
        search_same_source_button.connect_clicked(move |_| {
            let Some(source_url) = selected_source_url(&state_handle) else {
                show_toast(&ui, "No source URL available for selected item");
                return;
            };

            suppress.set(true);
            search.set_text(&source_url);
            suppress.set(false);
            search_bar.set_search_mode(true);
            apply_search(&state_handle, &ui, source_url);
            show_toast(&ui, "Filtered by source URL; clear search to reset");
        });
    }
    {
        let split = controls.split.clone();
        let grid = ui.grid.clone();
        grid.set_single_click_activate(split.is_collapsed());
        split.connect_collapsed_notify(move |split| {
            grid.set_single_click_activate(split.is_collapsed());
        });
    }
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
        let suppress = suppress_search_changed.clone();
        controls.search.connect_search_changed(move |entry| {
            if suppress.get() {
                return;
            }
            apply_search(&state_handle, &ui, entry.text().to_string());
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
                show_error_dialog(&ui, "Failed to save edits", &format!("{err}"));
            }
        });
    }
    {
        let state_handle = state.clone();
        let ui = ui.clone();
        let rescan_action = gtk::gio::SimpleAction::new("rescan", None);
        rescan_action.connect_activate(move |_, _| {
            if let Err(err) = rescan_library(&state_handle, &ui) {
                show_error_dialog(&ui, "Failed to rescan library", &format!("{err}"));
            }
        });
        controls.window.add_action(&rescan_action);
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
    {
        let grid_selection_handle = grid_selection.clone();
        grid_factory.connect_setup(move |_, list_item_obj| {
            let Some(list_item) = list_item_obj.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let list_item_handle = list_item.clone();

            let builder = gtk::Builder::from_string(GRID_CELL_UI);
            let card: GtkBox = builder_object(&builder, "card");

            let popover = build_item_context_popover(&card);

            let selection_handle = grid_selection_handle.clone();
            let popover_handle = popover.clone();
            let right_click = gtk::GestureClick::builder().button(3).build();
            right_click.connect_pressed(move |gesture, _, x, y| {
                let pos = list_item_handle.position();
                if pos == gtk::INVALID_LIST_POSITION {
                    return;
                }
                selection_handle.set_selected(pos);
                popup_context_menu(&popover_handle, x, y);
                gesture.set_state(gtk::EventSequenceState::Claimed);
            });
            card.add_controller(right_click);

            list_item.set_child(Some(&card));
        });
    }

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
                Some((256, 256)),
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
