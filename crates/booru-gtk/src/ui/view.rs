use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::{AlertDialog, Toast};
use anyhow::{anyhow, Result};
use booru_core::{apply_update_to_image, BooruConfig, EditUpdate, Library};
use gtk::{self, Box as GtkBox, Button, Label, Picture, TextView};

use super::image_loader::ImageRequestKind;
use super::*;

pub(crate) fn scan_library(config: &BooruConfig, quiet: bool) -> Result<Library> {
    let library = Library::scan(config.clone())?;
    if !quiet {
        for warning in &library.warnings {
            eprintln!("warning: {}: {}", warning.path.display(), warning.message);
        }
    }
    Ok(library)
}

pub(super) fn install_tag_editor_css() {
    APP_CSS_ONCE.call_once(|| {
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };

        let provider = gtk::CssProvider::new();
        provider.load_from_string(APP_CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

pub(super) fn rebuild_view(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    if let Some(action) = ui.window.lookup_action("reshuffle") {
        if let Ok(action) = action.downcast::<gtk::gio::SimpleAction>() {
            action.set_enabled(state.borrow().random_sort_active());
        }
    }
    refresh_browser(state, ui);
    refresh_detail(state, ui);
}

pub(super) fn refresh_browser(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    let (mode, selected_pos, version) = {
        let state = state.borrow();
        (state.browser_mode, state.selected_pos, state.filter_version)
    };

    // Keep the shared selection authoritative while replacing data or attaching
    // a view: intermediate selection notifications must not reload details.
    ui.updating_browser.set(true);
    match mode {
        BrowserMode::List => ui.grid.set_model(None::<&SingleSelection>),
        BrowserMode::Grid => ui.list.set_model(None::<&SingleSelection>),
    }

    if ui.browser_loaded_version.get() != version {
        let items = {
            let state = state.borrow();
            state
                .filtered_indices
                .iter()
                .map(|&item_idx| {
                    gtk::glib::BoxedAnyObject::new(super::build::BrowserItemData {
                        item_idx,
                        texture: Rc::new(RefCell::new(None)),
                        pending_request_id: Rc::new(Cell::new(None)),
                    })
                })
                .collect::<Vec<_>>()
        };
        ui.browser_store
            .splice(0, ui.browser_store.n_items(), &items);
        ui.browser_loaded_version.set(version);
    }

    match mode {
        BrowserMode::List if ui.list.model().is_none() => {
            ui.list.set_model(Some(&ui.browser_selection))
        }
        BrowserMode::Grid if ui.grid.model().is_none() => {
            ui.grid.set_model(Some(&ui.browser_selection))
        }
        _ => {}
    }
    sync_browser_selection(ui, selected_pos);
    ui.updating_browser.set(false);
    ui.browser_stack.set_visible_child_name(mode.as_name());
    ensure_selected_item_visible(ui, selected_pos);
}

struct DetailSnapshot {
    image_path: PathBuf,
    title: String,
    author: Option<String>,
    date: String,
    source_url: Option<String>,
    detail: String,
    tags: Vec<String>,
    notes: String,
    sensitive: bool,
}

pub(super) fn refresh_detail(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    let snapshot = {
        let state = state.borrow();
        let Some(idx) = state.selected_item_index() else {
            return clear_detail(ui);
        };
        let item = &state.library.index.items[idx];
        DetailSnapshot {
            image_path: item.image_path.clone(),
            title: infer_title(item),
            author: item
                .merged_author()
                .map(|author| author.trim().to_string())
                .filter(|author| !author.is_empty()),
            date: item.merged_date().unwrap_or_else(|| "-".to_string()),
            source_url: item.platform_url(),
            detail: item.merged_detail().unwrap_or_default(),
            tags: item.merged_tags(),
            notes: item.edits.notes.clone().unwrap_or_default(),
            sensitive: item.merged_sensitive(),
        }
    };

    ui.detail_stack.set_visible_child_name("detail");
    ui.edit_sheet.set_can_open(true);
    ui.title.set_text(&snapshot.title);
    ui.author
        .set_label(snapshot.author.as_deref().unwrap_or("-"));
    ui.author.set_sensitive(snapshot.author.is_some());
    ui.author.set_tooltip_text(
        snapshot
            .author
            .as_ref()
            .map(|_| "Click to search by author")
            .or(Some("No author available")),
    );
    ui.date.set_text(&format!("Date: {}", snapshot.date));
    match snapshot.source_url.as_deref() {
        Some(url) => {
            ui.source_url.set_uri(url);
            ui.source_url.set_label(url);
            ui.source_url.set_sensitive(true);
            ui.search_same_source_button.set_sensitive(true);
        }
        None => {
            ui.source_url.set_uri("about:blank");
            ui.source_url.set_label("(none)");
            ui.source_url.set_sensitive(false);
            ui.search_same_source_button.set_sensitive(false);
        }
    }
    ui.open_file_button.set_sensitive(true);
    ui.detail.set_text(&snapshot.detail);
    {
        let mut tag_values = ui.tag_values.borrow_mut();
        *tag_values = snapshot.tags.clone();
    }
    ui.tags_input.set_text("");
    rebuild_tag_wrap(ui);
    set_notes_text(&ui.notes, &snapshot.notes);
    ui.item_sensitive.set_active(snapshot.sensitive);
    ui.picture.set_paintable(None::<&gtk::gdk::Texture>);
    hide_banner(ui);

    if let Some(previous_request_id) = ui.detail_pending_request_id.replace(None) {
        ui.image_loader.cancel_if_queued(previous_request_id);
    }

    let load_seq = ui.detail_image_seq.get().wrapping_add(1);
    ui.detail_image_seq.set(load_seq);

    let ui_handle = ui.clone();
    let image_path = snapshot.image_path.clone();
    let pending_request_slot = ui.detail_pending_request_id.clone();
    let request_id = ui.image_loader.load(
        image_path.clone(),
        None,
        ImageRequestKind::Detail,
        move |finished_id, result| {
            if pending_request_slot.get() == Some(finished_id) {
                pending_request_slot.set(None);
            }

            if ui_handle.detail_image_seq.get() != load_seq {
                return;
            }

            match result {
                Ok(texture) => {
                    ui_handle.picture.set_paintable(Some(&texture));
                }
                Err(err) => {
                    ui_handle.picture.set_paintable(None::<&gtk::gdk::Texture>);
                    show_error_dialog(
                        &ui_handle,
                        "Image preview unavailable",
                        &format!("{} ({err})", image_path.display()),
                    );
                }
            }
        },
    );
    ui.detail_pending_request_id.set(Some(request_id));
}

fn clear_detail(ui: &Ui) {
    if let Some(request_id) = ui.detail_pending_request_id.replace(None) {
        ui.image_loader.cancel_if_queued(request_id);
    }
    ui.detail_image_seq
        .set(ui.detail_image_seq.get().wrapping_add(1));
    ui.edit_sheet.set_open(false);
    ui.edit_sheet.set_can_open(false);
    ui.detail_stack.set_visible_child_name("empty");
    ui.title.set_text("(no match)");
    ui.author.set_label("-");
    ui.author.set_sensitive(false);
    ui.author.set_tooltip_text(None::<&str>);
    ui.date.set_text("");
    ui.source_url.set_uri("about:blank");
    ui.source_url.set_label("(none)");
    ui.source_url.set_sensitive(false);
    ui.search_same_source_button.set_sensitive(false);
    ui.open_file_button.set_sensitive(false);
    ui.detail.set_text("");
    ui.tag_values.borrow_mut().clear();
    ui.tags_input.set_text("");
    rebuild_tag_wrap(ui);
    set_notes_text(&ui.notes, "");
    ui.item_sensitive.set_active(false);
    ui.picture.set_paintable(None::<&gtk::gdk::Texture>);
}

pub(super) fn open_selected_file(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    let Some(image_path) = ({
        let state = state.borrow();
        state
            .selected_item_index()
            .and_then(|idx| state.library.index.items.get(idx))
            .map(|item| item.image_path.clone())
    }) else {
        show_error_dialog(ui, "Open file failed", "No selected item.");
        return;
    };

    let uri = gtk::gio::File::for_path(&image_path).uri();
    match launch_uri(uri.as_str()) {
        Ok(()) => {
            hide_banner(ui);
        }
        Err(err) => {
            show_error_dialog(ui, "Failed to open file", &format!("{err}"));
        }
    }
}

pub(super) fn open_selected_source_url(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    let Some(source_url) = selected_source_url(state) else {
        show_error_dialog(
            ui,
            "Open source URL failed",
            "No source URL for selected item.",
        );
        return;
    };

    match launch_uri(&source_url) {
        Ok(()) => {
            hide_banner(ui);
        }
        Err(err) => {
            show_error_dialog(ui, "Failed to open source URL", &format!("{err}"));
        }
    }
}

pub(super) fn apply_search(state: &Rc<RefCell<AppState>>, ui: &Ui, query: String) {
    {
        let mut state = state.borrow_mut();
        state.query = query;
        state.rebuild_filter();
        // Keep search passive: changing the filter should not implicitly open a detail item.
        state.selected_pos = None;
    }
    rebuild_view(state, ui);
}

pub(super) fn selected_author(state: &Rc<RefCell<AppState>>) -> Option<String> {
    let state = state.borrow();
    state
        .selected_item_index()
        .and_then(|idx| state.library.index.items.get(idx))
        .and_then(|item| item.merged_author())
        .map(|author| author.trim().to_string())
        .filter(|author| !author.is_empty())
}

pub(super) fn selected_source_url(state: &Rc<RefCell<AppState>>) -> Option<String> {
    let state = state.borrow();
    state
        .selected_item_index()
        .and_then(|idx| state.library.index.items.get(idx))
        .and_then(|item| item.platform_url())
}

fn launch_uri(uri: &str) -> Result<()> {
    gtk::gio::AppInfo::launch_default_for_uri(uri, None::<&gtk::gio::AppLaunchContext>)
        .map_err(|err| anyhow!("cannot open `{uri}`: {err}"))
}

pub(super) fn save_selected_edits(state: &Rc<RefCell<AppState>>, ui: &Ui) -> Result<()> {
    let (item_idx, image_path) = {
        let state = state.borrow();
        let Some(item_idx) = state.selected_item_index() else {
            show_error_dialog(ui, "Save failed", "No selected item.");
            return Ok(());
        };
        (
            item_idx,
            state.library.index.items[item_idx].image_path.clone(),
        )
    };

    append_pending_tags_input(ui);
    let tags = ui.tag_values.borrow().clone();
    let notes = get_notes_text(&ui.notes);
    let sensitive = ui.item_sensitive.is_active();
    let edits = apply_update_to_image(
        &image_path,
        EditUpdate {
            set_tags: Some(tags),
            add_tags: Vec::new(),
            remove_tags: Vec::new(),
            clear_tags: false,
            notes: Some(notes),
            sensitive: Some(sensitive),
        },
    )?;

    {
        let mut state = state.borrow_mut();
        if let Some(item) = state.library.index.items.get_mut(item_idx) {
            item.edits = edits;
        }
        state.rebuild_filter();
    }

    rebuild_view(state, ui);
    show_toast(ui, "Edits saved");
    hide_banner(ui);
    Ok(())
}

pub(super) fn rescan_library(state: &Rc<RefCell<AppState>>, ui: &Ui) -> Result<()> {
    let (config, quiet) = {
        let state = state.borrow();
        (state.library.config.clone(), state.quiet)
    };
    let library = scan_library(&config, quiet)?;
    {
        let mut state = state.borrow_mut();
        state.library = library;
        state.rebuild_filter();
    }
    rebuild_view(state, ui);
    show_toast(ui, "Rescan complete");
    hide_banner(ui);
    Ok(())
}

fn parse_tags_input(input: &str) -> Vec<String> {
    input
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn set_notes_text(notes: &TextView, text: &str) {
    notes.buffer().set_text(text);
}

fn get_notes_text(notes: &TextView) -> String {
    let buffer = notes.buffer();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

pub(super) fn append_pending_tags_input(ui: &Ui) -> bool {
    let pending_tags = parse_tags_input(&ui.tags_input.text());
    if pending_tags.is_empty() {
        return false;
    }

    {
        let mut current_tags = ui.tag_values.borrow_mut();
        current_tags.extend(pending_tags);
    }
    ui.tags_input.set_text("");
    rebuild_tag_wrap(ui);
    true
}

pub(super) fn rebuild_tag_wrap(ui: &Ui) {
    ui.tags_wrap.remove_all();

    let tags = ui.tag_values.borrow().clone();
    for (index, tag) in tags.iter().enumerate() {
        let chip = build_tag_chip(ui, index, tag);
        ui.tags_wrap.append(&chip);
    }

    ui.tags_wrap.append(&ui.tags_add_button);
}

fn build_tag_chip(ui: &Ui, index: usize, tag: &str) -> GtkBox {
    let builder = gtk::Builder::from_string(TAG_CHIP_UI);
    let chip: GtkBox = builder_object(&builder, "chip");
    let label: Label = builder_object(&builder, "chip_label");
    let remove_button: Button = builder_object(&builder, "chip_remove_button");
    label.set_label(tag);

    let ui_handle = ui.clone();
    remove_button.connect_clicked(move |_| {
        {
            let mut tags = ui_handle.tag_values.borrow_mut();
            if index < tags.len() {
                tags.remove(index);
            }
        }
        rebuild_tag_wrap(&ui_handle);
    });

    chip
}

fn infer_title(item: &booru_core::ImageItem) -> String {
    booru_core::extract_string_field(&item.original, &["title", "filename"])
        .or_else(|| {
            item.image_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "(untitled)".to_string())
}

pub(super) fn infer_thumbnail_title(item: &booru_core::ImageItem) -> String {
    let base = infer_title(item);
    if item.merged_sensitive() {
        format!("[S] {base}")
    } else {
        base
    }
}

pub(super) fn grid_cell_widgets(list_item: &gtk::ListItem) -> Option<(GtkBox, Picture, Label)> {
    let card = list_item.child()?.downcast::<GtkBox>().ok()?;
    let thumb = card.first_child()?.downcast::<Picture>().ok()?;
    let caption = thumb.next_sibling()?.downcast::<Label>().ok()?;
    Some((card, thumb, caption))
}

pub(super) fn sync_browser_selection(ui: &Ui, selected_pos: Option<usize>) {
    let selected = selected_pos
        .map(|pos| pos as u32)
        .unwrap_or(gtk::INVALID_LIST_POSITION);
    if ui.browser_selection.selected() != selected {
        ui.browser_selection.set_selected(selected);
    }
}

pub(super) fn ensure_selected_item_visible(ui: &Ui, selected_pos: Option<usize>) {
    let Some(pos) = selected_pos else {
        return;
    };
    if pos >= ui.browser_store.n_items() as usize {
        return;
    }
    match ui.browser_stack.visible_child_name().as_deref() {
        Some("grid") => ui
            .grid
            .scroll_to(pos as u32, gtk::ListScrollFlags::NONE, None),
        _ => ui
            .list
            .scroll_to(pos as u32, gtk::ListScrollFlags::NONE, None),
    }
}

pub(super) fn show_toast(ui: &Ui, message: &str) {
    let toast = Toast::builder().use_markup(false).timeout(2).build();
    toast.set_title(message);
    ui.toast_overlay.add_toast(toast);
}

pub(super) fn show_error_dialog(ui: &Ui, heading: &str, message: &str) {
    let dialog = AlertDialog::new(Some(heading), Some(message));
    dialog.add_response("ok", "OK");
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("ok");
    dialog.present(Some(&ui.window));
}

pub(super) fn hide_banner(ui: &Ui) {
    ui.banner.set_revealed(false);
}
