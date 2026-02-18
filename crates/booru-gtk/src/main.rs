use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::{Application, ApplicationWindow};
use anyhow::Result;
use booru_core::{
    apply_update_to_image, BooruConfig, EditUpdate, Library, SearchQuery, SearchSort,
};
use clap::Parser;
use gtk::{
    self, Align, Box as GtkBox, Button, CheckButton, Entry, Label, ListBox, ListBoxRow,
    Orientation, Paned, Picture, ScrolledWindow, SearchEntry, SelectionMode,
};

#[derive(Parser, Debug)]
#[command(name = "booru-gtk", version, about = "GTK GUI for LightBooru")]
struct Cli {
    /// Base directory for gallery-dl downloads (can be repeated)
    #[arg(long, short)]
    base: Vec<PathBuf>,

    /// Suppress scan warnings
    #[arg(long)]
    quiet: bool,

    /// Show sensitive images (default: hidden)
    #[arg(long)]
    sensitive: bool,
}

struct AppState {
    library: Library,
    filtered_indices: Vec<usize>,
    selected_pos: Option<usize>,
    show_sensitive: bool,
    query: String,
    quiet: bool,
}

impl AppState {
    fn new(library: Library, show_sensitive: bool, quiet: bool) -> Self {
        let mut state = Self {
            library,
            filtered_indices: Vec::new(),
            selected_pos: None,
            show_sensitive,
            query: String::new(),
            quiet,
        };
        state.rebuild_filter();
        state
    }

    fn rebuild_filter(&mut self) {
        let use_aliases = !self.query.trim().is_empty();
        let result = self.library.search(
            SearchQuery::new(split_search_terms(&self.query))
                .with_aliases(use_aliases)
                .with_sort(SearchSort::FileNameAsc),
        );

        self.filtered_indices = result
            .indices
            .into_iter()
            .filter(|idx| self.show_sensitive || !self.library.index.items[*idx].merged_sensitive())
            .collect();

        self.selected_pos = match (self.selected_pos, self.filtered_indices.is_empty()) {
            (_, true) => None,
            (Some(pos), false) => Some(pos.min(self.filtered_indices.len() - 1)),
            (None, false) => Some(0),
        };
    }

    fn selected_item_index(&self) -> Option<usize> {
        self.selected_pos
            .and_then(|pos| self.filtered_indices.get(pos))
            .copied()
    }
}

#[derive(Clone)]
struct Ui {
    list: ListBox,
    picture: Picture,
    title: Label,
    author: Label,
    date: Label,
    detail: Label,
    tags: Entry,
    item_sensitive: CheckButton,
    status: Label,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = if cli.base.is_empty() {
        BooruConfig::default()
    } else {
        BooruConfig::with_roots(cli.base)
    };

    let library = scan_library(&config, cli.quiet)?;
    let state = Rc::new(RefCell::new(AppState::new(
        library,
        cli.sensitive,
        cli.quiet,
    )));

    let app = Application::builder()
        .application_id("dev.lightbooru.gtk")
        .build();
    let state_for_activate = state.clone();
    app.connect_activate(move |app| build_ui(app, state_for_activate.clone()));
    app.run();

    Ok(())
}

fn scan_library(config: &BooruConfig, quiet: bool) -> Result<Library> {
    let library = Library::scan(config.clone())?;
    if !quiet {
        for warning in &library.warnings {
            eprintln!("warning: {}: {}", warning.path.display(), warning.message);
        }
    }
    Ok(library)
}

fn build_ui(app: &Application, state: Rc<RefCell<AppState>>) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("lightbooru")
        .default_width(1280)
        .default_height(800)
        .build();

    let root = GtkBox::new(Orientation::Vertical, 8);
    root.set_margin_start(8);
    root.set_margin_end(8);
    root.set_margin_top(8);
    root.set_margin_bottom(8);

    let controls = GtkBox::new(Orientation::Horizontal, 8);
    let search = SearchEntry::new();
    search.set_hexpand(true);
    search.set_placeholder_text(Some("Search tags/author/detail"));

    let show_sensitive = CheckButton::with_label("Show sensitive");
    show_sensitive.set_active(state.borrow().show_sensitive);

    let rescan_button = Button::with_label("Rescan");

    controls.append(&search);
    controls.append(&show_sensitive);
    controls.append(&rescan_button);

    let paned = Paned::new(Orientation::Horizontal);

    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::Single);
    let list_scroll = ScrolledWindow::new();
    list_scroll.set_child(Some(&list));
    list_scroll.set_hexpand(true);
    list_scroll.set_vexpand(true);
    paned.set_start_child(Some(&list_scroll));

    let detail_wrap = GtkBox::new(Orientation::Vertical, 8);
    detail_wrap.set_margin_start(8);
    detail_wrap.set_margin_end(8);
    detail_wrap.set_margin_top(8);
    detail_wrap.set_margin_bottom(8);

    let title = Label::new(None);
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.add_css_class("title-3");

    let author = Label::new(None);
    author.set_xalign(0.0);

    let date = Label::new(None);
    date.set_xalign(0.0);

    let picture = Picture::new();
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    picture.set_can_shrink(true);
    picture.set_halign(Align::Fill);
    picture.set_valign(Align::Fill);

    let detail = Label::new(None);
    detail.set_xalign(0.0);
    detail.set_wrap(true);
    detail.set_selectable(true);

    let tags = Entry::new();
    tags.set_placeholder_text(Some("Tags (space/comma separated)"));

    let item_sensitive = CheckButton::with_label("Sensitive");
    let save_button = Button::with_label("Save");

    detail_wrap.append(&title);
    detail_wrap.append(&author);
    detail_wrap.append(&date);
    detail_wrap.append(&picture);
    detail_wrap.append(&detail);
    detail_wrap.append(&tags);
    detail_wrap.append(&item_sensitive);
    detail_wrap.append(&save_button);

    let detail_scroll = ScrolledWindow::new();
    detail_scroll.set_child(Some(&detail_wrap));
    detail_scroll.set_hexpand(true);
    detail_scroll.set_vexpand(true);
    paned.set_end_child(Some(&detail_scroll));

    let status = Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);

    root.append(&controls);
    root.append(&paned);
    root.append(&status);

    window.set_content(Some(&root));

    let ui = Ui {
        list,
        picture,
        title,
        author,
        date,
        detail,
        tags,
        item_sensitive,
        status,
    };

    rebuild_view(&state, &ui);

    {
        let state_handle = state.clone();
        let ui = ui.clone();
        search.connect_search_changed(move |entry| {
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
        show_sensitive.connect_toggled(move |toggle| {
            let mut state = state_handle.borrow_mut();
            state.show_sensitive = toggle.is_active();
            state.rebuild_filter();
            drop(state);
            rebuild_view(&state_handle, &ui);
        });
    }

    {
        let state_handle = state.clone();
        let ui = ui.clone();
        let list_handle = ui.list.clone();
        list_handle.connect_row_selected(move |_list, row| {
            let mut state = state_handle.borrow_mut();
            state.selected_pos = row
                .and_then(|r| usize::try_from(r.index()).ok())
                .filter(|pos| *pos < state.filtered_indices.len());
            drop(state);
            refresh_detail(&state_handle, &ui);
        });
    }

    {
        let state_handle = state.clone();
        let ui = ui.clone();
        save_button.connect_clicked(move |_| {
            if let Err(err) = save_selected_edits(&state_handle, &ui) {
                set_status(&ui, &format!("failed to save: {err}"));
            }
        });
    }

    {
        let state_handle = state.clone();
        let ui = ui.clone();
        rescan_button.connect_clicked(move |_| {
            if let Err(err) = rescan_library(&state_handle, &ui) {
                set_status(&ui, &format!("failed to rescan: {err}"));
            }
        });
    }

    window.present();
}

fn rebuild_view(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    refresh_list(state, ui);
    refresh_detail(state, ui);
}

fn refresh_list(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    while let Some(child) = ui.list.first_child() {
        ui.list.remove(&child);
    }

    let (rows, selected_pos) = {
        let state = state.borrow();
        let rows = state
            .filtered_indices
            .iter()
            .map(|idx| {
                let item = &state.library.index.items[*idx];
                let title = infer_title(item);
                let author = item.merged_author().unwrap_or_else(|| "-".to_string());
                let date = item.merged_date().unwrap_or_else(|| "-".to_string());
                let prefix = if item.merged_sensitive() { "[S] " } else { "" };
                format!("{prefix}{title} | {author} | {date}")
            })
            .collect::<Vec<_>>();
        (rows, state.selected_pos)
    };

    for row_text in rows {
        let row = ListBoxRow::new();
        let label = Label::new(Some(&row_text));
        label.set_xalign(0.0);
        label.set_wrap(true);
        row.set_child(Some(&label));
        ui.list.append(&row);
    }

    if let Some(pos) = selected_pos {
        if let Some(row) = ui.list.row_at_index(pos as i32) {
            ui.list.select_row(Some(&row));
        }
    }
}

fn refresh_detail(state: &Rc<RefCell<AppState>>, ui: &Ui) {
    let snapshot = {
        let state = state.borrow();
        let Some(idx) = state.selected_item_index() else {
            return clear_detail(ui);
        };
        let item = &state.library.index.items[idx];
        (
            idx,
            item.image_path.clone(),
            infer_title(item),
            item.merged_author().unwrap_or_else(|| "-".to_string()),
            item.merged_date().unwrap_or_else(|| "-".to_string()),
            item.merged_detail().unwrap_or_default(),
            item.merged_tags().join(" "),
            item.merged_sensitive(),
        )
    };

    ui.title.set_text(&snapshot.2);
    ui.author.set_text(&format!("Author: {}", snapshot.3));
    ui.date.set_text(&format!("Date: {}", snapshot.4));
    ui.detail.set_text(&snapshot.5);
    ui.tags.set_text(&snapshot.6);
    ui.item_sensitive.set_active(snapshot.7);

    match gtk::gdk::Texture::from_filename(&snapshot.1) {
        Ok(texture) => {
            ui.picture.set_paintable(Some(&texture));
            set_status(
                ui,
                &format!(
                    "Showing {} ({}/{})",
                    snapshot.1.display(),
                    snapshot.0 + 1,
                    state.borrow().library.index.items.len()
                ),
            );
        }
        Err(err) => {
            ui.picture.set_paintable(None::<&gtk::gdk::Texture>);
            set_status(
                ui,
                &format!(
                    "image preview unavailable: {} ({err})",
                    snapshot.1.display()
                ),
            );
        }
    }
}

fn clear_detail(ui: &Ui) {
    ui.title.set_text("(no match)");
    ui.author.set_text("");
    ui.date.set_text("");
    ui.detail.set_text("");
    ui.tags.set_text("");
    ui.item_sensitive.set_active(false);
    ui.picture.set_paintable(None::<&gtk::gdk::Texture>);
    set_status(ui, "No item selected.");
}

fn save_selected_edits(state: &Rc<RefCell<AppState>>, ui: &Ui) -> Result<()> {
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

    let tags = parse_tags_input(&ui.tags.text());
    let sensitive = ui.item_sensitive.is_active();
    let edits = apply_update_to_image(
        &image_path,
        EditUpdate {
            set_tags: Some(tags),
            add_tags: Vec::new(),
            remove_tags: Vec::new(),
            clear_tags: false,
            notes: None,
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
    Ok(())
}

fn rescan_library(state: &Rc<RefCell<AppState>>, ui: &Ui) -> Result<()> {
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
    Ok(())
}

fn split_search_terms(input: &str) -> Vec<String> {
    input
        .split_whitespace()
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_tags_input(input: &str) -> Vec<String> {
    input
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(ToString::to_string)
        .collect()
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

fn set_status(ui: &Ui, message: &str) {
    ui.status.set_text(message);
}
