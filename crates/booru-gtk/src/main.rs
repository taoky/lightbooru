use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::{
    ActionRow, Application, ApplicationWindow, Banner, Clamp, EntryRow, HeaderBar, NavigationPage,
    NavigationSplitView, StatusPage, SwitchRow, Toast, ToastOverlay, ToolbarView, ViewStack,
    WindowTitle,
};
use anyhow::Result;
use booru_core::{
    apply_update_to_image, BooruConfig, EditUpdate, Library, SearchQuery, SearchSort,
};
use clap::Parser;
use gtk::{
    self, Align, Box as GtkBox, Button, Label, ListBox, Orientation, Picture, ScrolledWindow,
    SearchEntry, SelectionMode,
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
    tags: EntryRow,
    item_sensitive: SwitchRow,
    status: Label,
    detail_stack: ViewStack,
    toast_overlay: ToastOverlay,
    banner: Banner,
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

    let toast_overlay = ToastOverlay::new();
    let toolbar = ToolbarView::new();
    toast_overlay.set_child(Some(&toolbar));

    let header = HeaderBar::new();
    let window_title = WindowTitle::new("lightbooru", "gallery-dl local library");
    header.set_title_widget(Some(&window_title));

    let search = SearchEntry::new();
    search.set_placeholder_text(Some("Search tags/author/detail"));
    let search_bar = gtk::SearchBar::new();
    search_bar.set_show_close_button(true);
    search_bar.set_search_mode(false);
    search_bar.connect_entry(&search);
    search_bar.set_key_capture_widget(Some(&window));
    search_bar.set_child(Some(&search));

    let search_button = gtk::ToggleButton::builder()
        .icon_name("system-search-symbolic")
        .build();
    search_button.add_css_class("flat");
    search_button.set_tooltip_text(Some("Search"));
    header.pack_start(&search_button);

    let main_menu = gtk::gio::Menu::new();
    main_menu.append(Some("Show sensitive"), Some("win.show-sensitive"));
    main_menu.append(Some("Rescan library"), Some("win.rescan"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&main_menu)
        .build();
    menu_button.set_tooltip_text(Some("Main menu"));
    header.pack_end(&menu_button);
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&search_bar);

    let content = GtkBox::new(Orientation::Vertical, 0);
    content.set_vexpand(true);
    let banner = Banner::new("");
    banner.set_revealed(false);
    content.append(&banner);

    let split = NavigationSplitView::new();
    split.set_hexpand(true);
    split.set_vexpand(true);
    split.set_min_sidebar_width(280.0);
    split.set_max_sidebar_width(440.0);
    split.set_sidebar_width_fraction(0.32);
    split.set_show_content(true);

    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::Single);
    list.add_css_class("boxed-list");
    let list_scroll = ScrolledWindow::new();
    list_scroll.set_child(Some(&list));
    list_scroll.set_hexpand(true);
    list_scroll.set_vexpand(true);
    list_scroll.add_css_class("navigation-sidebar");
    let sidebar_page = NavigationPage::new(&list_scroll, "Library");
    split.set_sidebar(Some(&sidebar_page));

    let detail_wrap = GtkBox::new(Orientation::Vertical, 12);
    detail_wrap.set_margin_start(12);
    detail_wrap.set_margin_end(12);
    detail_wrap.set_margin_top(12);
    detail_wrap.set_margin_bottom(12);

    let title = Label::new(None);
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.add_css_class("title-2");

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
    picture.set_content_fit(gtk::ContentFit::Contain);

    let detail = Label::new(None);
    detail.set_xalign(0.0);
    detail.set_wrap(true);
    detail.set_selectable(true);

    let tags = EntryRow::builder().title("Tags").build();

    let item_sensitive = SwitchRow::builder().title("Sensitive").build();
    let edit_group = adw::PreferencesGroup::builder()
        .title("Edits")
        .description("Saved to *.booru.json")
        .build();
    edit_group.add(&tags);
    edit_group.add(&item_sensitive);

    let save_button = Button::with_label("Save");
    save_button.set_halign(Align::End);
    save_button.add_css_class("suggested-action");

    detail_wrap.append(&title);
    detail_wrap.append(&author);
    detail_wrap.append(&date);
    detail_wrap.append(&picture);
    detail_wrap.append(&detail);
    detail_wrap.append(&edit_group);
    detail_wrap.append(&save_button);

    let detail_scroll = ScrolledWindow::new();
    let detail_clamp = Clamp::new();
    detail_clamp.set_maximum_size(960);
    detail_clamp.set_tightening_threshold(560);
    detail_clamp.set_child(Some(&detail_wrap));
    detail_scroll.set_child(Some(&detail_clamp));
    detail_scroll.set_hexpand(true);
    detail_scroll.set_vexpand(true);

    let empty_page = StatusPage::new();
    empty_page.set_icon_name(Some("image-x-generic-symbolic"));
    empty_page.set_title("No item selected");
    empty_page.set_description(Some(
        "Select an item from the left panel to preview and edit metadata.",
    ));

    let detail_stack = ViewStack::new();
    detail_stack.set_hhomogeneous(false);
    detail_stack.set_vhomogeneous(false);
    detail_stack.add_titled(&detail_scroll, Some("detail"), "Detail");
    detail_stack.add_titled(&empty_page, Some("empty"), "Empty");
    detail_stack.set_visible_child_name("empty");

    let detail_page = NavigationPage::new(&detail_stack, "Details");
    split.set_content(Some(&detail_page));

    let status = Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.add_css_class("dim-label");
    detail_wrap.append(&status);

    content.append(&split);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toast_overlay));

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
        detail_stack,
        toast_overlay,
        banner,
    };

    rebuild_view(&state, &ui);

    {
        let search_bar = search_bar.clone();
        let search = search.clone();
        search_button.connect_toggled(move |button| {
            let enabled = button.is_active();
            search_bar.set_search_mode(enabled);
            if enabled {
                search.grab_focus();
            }
        });
    }

    {
        let search_button = search_button.clone();
        search_bar.connect_search_mode_enabled_notify(move |bar| {
            let enabled = bar.is_search_mode();
            if search_button.is_active() != enabled {
                search_button.set_active(enabled);
            }
        });
    }

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
        window.add_action(&show_sensitive_action);
    }

    {
        let state_handle = state.clone();
        let ui = ui.clone();
        let list_handle = ui.list.clone();
        list_handle.connect_row_selected(move |_list, row| {
            let mut state = state_handle.borrow_mut();
            state.selected_pos = row
                .and_then(|row| usize::try_from(row.index()).ok())
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
        window.add_action(&rescan_action);
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

    ui.detail_stack.set_visible_child_name("detail");
    ui.title.set_text(&snapshot.2);
    ui.author.set_text(&format!("Author: {}", snapshot.3));
    ui.date.set_text(&format!("Date: {}", snapshot.4));
    ui.detail.set_text(&snapshot.5);
    ui.tags.set_text(&snapshot.6);
    ui.item_sensitive.set_active(snapshot.7);
    hide_banner(ui);

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
    ui.detail_stack.set_visible_child_name("empty");
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
    show_toast(ui, "Edits saved");
    hide_banner(ui);
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
    show_toast(ui, "Rescan complete");
    hide_banner(ui);
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

fn show_toast(ui: &Ui, message: &str) {
    let toast = Toast::new(message);
    toast.set_timeout(2);
    ui.toast_overlay.add_toast(toast);
}

fn show_banner(ui: &Ui, message: &str) {
    ui.banner.set_title(message);
    ui.banner.set_revealed(true);
}

fn hide_banner(ui: &Ui) {
    ui.banner.set_revealed(false);
}
