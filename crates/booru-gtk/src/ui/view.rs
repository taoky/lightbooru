use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::{ActionRow, Toast};
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
    let browser_mode = state.borrow().browser_mode;
    ui.browser_stack
        .set_visible_child_name(browser_mode.as_name());
    refresh_list(state, ui);
    if matches!(browser_mode, BrowserMode::Grid) {
        refresh_grid(state, ui);
    }
    refresh_detail(state, ui);
}

fn refresh_list(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    while let Some(row) = ui.list.row_at_index(0) {
        ui.list.remove(&row);
    }

    let (rows, selected_pos) = {
        let state = state.borrow();
        let rows = state
            .filtered_indices
            .iter()
            .map(|item_idx| {
                let item = &state.library.index.items[*item_idx];
                let title = infer_title(item);
                let author = item.merged_author().unwrap_or_else(|| "-".to_string());
                let date = item.merged_date().unwrap_or_else(|| "-".to_string());
                let prefix = if item.merged_sensitive() { "[S] " } else { "" };
                (format!("{prefix}{title}"), format!("{author} | {date}"))
            })
            .collect::<Vec<(String, String)>>();
        (rows, state.selected_pos)
    };

    for (title, subtitle) in rows {
        let row = ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .activatable(true)
            .build();
        ui.list.append(&row);
    }

    sync_browser_selection(ui, selected_pos);
    ensure_selected_item_visible(ui, selected_pos);
}

pub(super) fn refresh_grid(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    let (filtered_indices, selected_pos, filter_version) = {
        let state = state.borrow();
        (
            state.filtered_indices.clone(),
            state.selected_pos,
            state.filter_version,
        )
    };

    if ui.grid_loaded_version.get() == filter_version {
        sync_browser_selection(ui, selected_pos);
        ensure_selected_item_visible(ui, selected_pos);
        return;
    }

    ui.grid_store.remove_all();
    for item_idx in filtered_indices {
        let boxed = gtk::glib::BoxedAnyObject::new(super::build::GridItemData {
            item_idx,
            texture: Rc::new(RefCell::new(None)),
            pending_request_id: Rc::new(Cell::new(None)),
        });
        ui.grid_store.append(&boxed);
    }

    ui.grid_loaded_version.set(filter_version);
    sync_browser_selection(ui, selected_pos);
    ensure_selected_item_visible(ui, selected_pos);
}

struct DetailSnapshot {
    position: usize,
    image_path: PathBuf,
    title: String,
    author: String,
    date: String,
    source_url: Option<String>,
    detail: String,
    tags: Vec<String>,
    notes: String,
    sensitive: bool,
    total_items: usize,
}

pub(super) fn refresh_detail(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    let snapshot = {
        let state = state.borrow();
        let Some(idx) = state.selected_item_index() else {
            return clear_detail(ui);
        };
        let item = &state.library.index.items[idx];
        DetailSnapshot {
            position: idx,
            image_path: item.image_path.clone(),
            title: infer_title(item),
            author: item.merged_author().unwrap_or_else(|| "-".to_string()),
            date: item.merged_date().unwrap_or_else(|| "-".to_string()),
            source_url: item.platform_url(),
            detail: item.merged_detail().unwrap_or_default(),
            tags: item.merged_tags(),
            notes: item.edits.notes.clone().unwrap_or_default(),
            sensitive: item.merged_sensitive(),
            total_items: state.library.index.items.len(),
        }
    };

    ui.detail_stack.set_visible_child_name("detail");
    ui.edit_sheet.set_can_open(true);
    ui.title.set_text(&snapshot.title);
    ui.author.set_text(&format!("Author: {}", snapshot.author));
    ui.date.set_text(&format!("Date: {}", snapshot.date));
    match snapshot.source_url.as_deref() {
        Some(url) => {
            ui.source_url.set_uri(url);
            ui.source_url.set_label(url);
            ui.source_url.set_sensitive(true);
        }
        None => {
            ui.source_url.set_uri("about:blank");
            ui.source_url.set_label("(none)");
            ui.source_url.set_sensitive(false);
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
    set_status(ui, &format!("Loading image: {}", snapshot.image_path.display()));

    if let Some(previous_request_id) = ui.detail_pending_request_id.replace(None) {
        ui.image_loader.cancel_if_queued(previous_request_id);
    }

    let load_seq = ui.detail_image_seq.get().wrapping_add(1);
    ui.detail_image_seq.set(load_seq);

    let ui_handle = ui.clone();
    let image_path = snapshot.image_path.clone();
    let current_pos = snapshot.position + 1;
    let total_items = snapshot.total_items;
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
                    set_status(
                        &ui_handle,
                        &format!(
                            "Showing {} ({}/{})",
                            image_path.display(),
                            current_pos,
                            total_items
                        ),
                    );
                }
                Err(err) => {
                    ui_handle.picture.set_paintable(None::<&gtk::gdk::Texture>);
                    set_status(
                        &ui_handle,
                        &format!(
                            "image preview unavailable: {} ({err})",
                            image_path.display()
                        ),
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
    ui.author.set_text("");
    ui.date.set_text("");
    ui.source_url.set_uri("about:blank");
    ui.source_url.set_label("(none)");
    ui.source_url.set_sensitive(false);
    ui.open_file_button.set_sensitive(false);
    ui.detail.set_text("");
    ui.tag_values.borrow_mut().clear();
    ui.tags_input.set_text("");
    rebuild_tag_wrap(ui);
    set_notes_text(&ui.notes, "");
    ui.item_sensitive.set_active(false);
    ui.picture.set_paintable(None::<&gtk::gdk::Texture>);
    set_status(ui, "No item selected.");
}

pub(super) fn open_selected_file(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    let Some(image_path) = ({
        let state = state.borrow();
        state
            .selected_item_index()
            .and_then(|idx| state.library.index.items.get(idx))
            .map(|item| item.image_path.clone())
    }) else {
        set_status(ui, "No selected item.");
        return;
    };

    let uri = gtk::gio::File::for_path(&image_path).uri();
    match launch_uri(uri.as_str()) {
        Ok(()) => {
            set_status(ui, &format!("Opened file: {}", image_path.display()));
            hide_banner(ui);
        }
        Err(err) => {
            set_status(ui, &format!("failed to open file: {err}"));
            show_banner(ui, &format!("Failed to open file: {err}"));
        }
    }
}

pub(super) fn open_selected_source_url(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    let Some(source_url) = ({
        let state = state.borrow();
        state
            .selected_item_index()
            .and_then(|idx| state.library.index.items.get(idx))
            .and_then(|item| item.platform_url())
    }) else {
        set_status(ui, "No source URL for selected item.");
        return;
    };

    match launch_uri(&source_url) {
        Ok(()) => {
            set_status(ui, &format!("Opened source URL: {source_url}"));
            hide_banner(ui);
        }
        Err(err) => {
            set_status(ui, &format!("failed to open source URL: {err}"));
            show_banner(ui, &format!("Failed to open source URL: {err}"));
        }
    }
}

fn launch_uri(uri: &str) -> Result<()> {
    gtk::gio::AppInfo::launch_default_for_uri(uri, None::<&gtk::gio::AppLaunchContext>)
        .map_err(|err| anyhow!("cannot open `{uri}`: {err}"))
}

pub(super) fn save_selected_edits(state: &Rc<RefCell<AppState>>, ui: &Ui) -> Result<()> {
    let (item_idx, image_path) = {
        let state = state.borrow();
        let Some(item_idx) = state.selected_item_index() else {
            set_status(ui, "No selected item.");
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
    set_status(ui, &format!("Saved edits: {}", image_path.display()));
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
    set_status(ui, "Rescan complete.");
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
    let current_list_pos = ui
        .list
        .selected_row()
        .and_then(|row| usize::try_from(row.index()).ok());
    let current_grid_pos = match ui.grid_selection.selected() {
        gtk::INVALID_LIST_POSITION => None,
        pos => usize::try_from(pos).ok(),
    };

    match selected_pos {
        Some(pos) => {
            if current_list_pos != Some(pos) {
                if let Some(row) = ui.list.row_at_index(pos as i32) {
                    ui.list.select_row(Some(&row));
                } else {
                    ui.list.unselect_all();
                }
            }

            if current_grid_pos != Some(pos) {
                ui.grid_selection.set_selected(pos as u32);
            }
        }
        None => {
            if current_list_pos.is_some() {
                ui.list.unselect_all();
            }
            if current_grid_pos.is_some() {
                ui.grid_selection.set_selected(gtk::INVALID_LIST_POSITION);
            }
        }
    }
}

pub(super) fn ensure_selected_item_visible(ui: &Ui, selected_pos: Option<usize>) {
    let Some(pos) = selected_pos else {
        return;
    };

    match ui.browser_stack.visible_child_name().as_deref() {
        Some("grid") => {
            if pos < ui.grid_store.n_items() as usize {
                ui.grid
                    .scroll_to(pos as u32, gtk::ListScrollFlags::NONE, None);
            }
        }
        _ => {
            let Some(row) = ui.list.row_at_index(pos as i32) else {
                return;
            };
            let Some(bounds) = row.compute_bounds(&ui.list) else {
                return;
            };

            let row_top = f64::from(bounds.y());
            let row_bottom = row_top + f64::from(bounds.height());
            let adjustment = ui.list_scroll.vadjustment();
            let view_top = adjustment.value();
            let view_bottom = view_top + adjustment.page_size();
            let min = adjustment.lower();
            let max = (adjustment.upper() - adjustment.page_size()).max(min);

            if row_top < view_top {
                adjustment.set_value(row_top.clamp(min, max));
            } else if row_bottom > view_bottom {
                adjustment.set_value((row_bottom - adjustment.page_size()).clamp(min, max));
            }
        }
    }
}

pub(super) fn set_status(ui: &Ui, message: &str) {
    ui.status.set_text(message);
}

pub(super) fn show_toast(ui: &Ui, message: &str) {
    let toast = Toast::new(message);
    toast.set_timeout(2);
    ui.toast_overlay.add_toast(toast);
}

pub(super) fn show_banner(ui: &Ui, message: &str) {
    ui.banner.set_title(message);
    ui.banner.set_revealed(true);
}

pub(super) fn hide_banner(ui: &Ui) {
    ui.banner.set_revealed(false);
}
