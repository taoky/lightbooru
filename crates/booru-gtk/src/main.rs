use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
use tracing::{debug, warn};
use tracing_subscriber::EnvFilter;

use adw::prelude::*;
use adw::{
    ActionRow, Application, ApplicationWindow, Banner, Clamp, EntryRow, HeaderBar, NavigationPage,
    NavigationSplitView, StatusPage, SwitchRow, Toast, ToastOverlay, Toggle, ToggleGroup,
    ToolbarView, ViewStack, WindowTitle,
};
use anyhow::Result;
use booru_core::{
    apply_update_to_image, BooruConfig, EditUpdate, Library, SearchQuery, SearchSort,
};
use clap::Parser;
use gtk::{
    self, Align, Box as GtkBox, Button, GridView, Label, ListBox, Orientation, Picture,
    ScrolledWindow, SearchEntry, SelectionMode, SignalListItemFactory, SingleSelection,
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

#[derive(Clone, Copy)]
enum BrowserMode {
    List,
    Grid,
}

impl BrowserMode {
    fn as_name(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Grid => "grid",
        }
    }

    fn from_name(name: &str) -> Self {
        match name {
            "grid" => Self::Grid,
            _ => Self::List,
        }
    }
}

#[derive(Clone)]
struct GridItemData {
    item_idx: usize,
    texture: Rc<RefCell<Option<gtk::gdk::Texture>>>,
}

struct AppState {
    library: Library,
    filtered_indices: Vec<usize>,
    selected_pos: Option<usize>,
    filter_version: u64,
    browser_mode: BrowserMode,
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
            filter_version: 0,
            browser_mode: BrowserMode::List,
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
        self.filter_version = self.filter_version.wrapping_add(1);
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
    grid_store: gtk::gio::ListStore,
    grid_selection: SingleSelection,
    browser_stack: ViewStack,
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
    detail_image_seq: Rc<Cell<u64>>,
    grid_loaded_version: Rc<Cell<u64>>,
    image_loader: Rc<ImageLoader>,
}

type ImageLoadCallback = Box<dyn FnOnce(Result<gtk::gdk::Texture, String>)>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ImageRequestKind {
    Detail,
    GridThumb,
}

#[derive(Debug)]
struct ImageDecodeTask {
    id: u64,
    path: PathBuf,
    scale: Option<(i32, i32)>,
    kind: ImageRequestKind,
}

#[derive(Clone)]
struct DecodedImage {
    width: i32,
    height: i32,
    rowstride: usize,
    format: gtk::gdk::MemoryFormat,
    pixels: gtk::glib::Bytes,
}

enum ImageDecodeResult {
    Ok { id: u64, image: DecodedImage },
    Err { id: u64, message: String },
}

#[derive(Default)]
struct ImageTaskQueues {
    detail: VecDeque<ImageDecodeTask>,
    grid: VecDeque<ImageDecodeTask>,
}

#[derive(Clone)]
struct ImageLoader {
    next_id: Rc<Cell<u64>>,
    callbacks: Rc<RefCell<HashMap<u64, ImageLoadCallback>>>,
    queue_state: Arc<(Mutex<ImageTaskQueues>, Condvar)>,
}

impl ImageLoader {
    fn new() -> Self {
        let (result_tx, result_rx) = mpsc::channel::<ImageDecodeResult>();
        let queue_state = Arc::new((Mutex::new(ImageTaskQueues::default()), Condvar::new()));

        let callbacks = Rc::new(RefCell::new(HashMap::<u64, ImageLoadCallback>::new()));
        {
            let callbacks_handle = callbacks.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(8), move || {
                while let Ok(message) = result_rx.try_recv() {
                    let id = match &message {
                        ImageDecodeResult::Ok { id, .. } => *id,
                        ImageDecodeResult::Err { id, .. } => *id,
                    };

                    let Some(callback) = callbacks_handle.borrow_mut().remove(&id) else {
                        continue;
                    };

                    match message {
                        ImageDecodeResult::Ok { image, .. } => {
                            let texture = gtk::gdk::MemoryTexture::new(
                                image.width,
                                image.height,
                                image.format,
                                &image.pixels,
                                image.rowstride,
                            );
                            callback(Ok(texture.upcast::<gtk::gdk::Texture>()));
                        }
                        ImageDecodeResult::Err { message, .. } => callback(Err(message)),
                    }
                }

                gtk::glib::ControlFlow::Continue
            });
        }

        thread::Builder::new()
            .name("booru-image-worker".to_string())
            .spawn({
                let queue_state = queue_state.clone();
                move || image_decode_worker(queue_state, result_tx)
            })
            .expect("failed to start booru image worker thread");

        Self {
            next_id: Rc::new(Cell::new(1)),
            callbacks,
            queue_state,
        }
    }

    fn load<F>(&self, path: PathBuf, scale: Option<(i32, i32)>, kind: ImageRequestKind, callback: F)
    where
        F: FnOnce(Result<gtk::gdk::Texture, String>) + 'static,
    {
        const MAX_PENDING_GRID_TASKS: usize = 96;
        const MAX_PENDING_DETAIL_TASKS: usize = 4;

        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        self.callbacks.borrow_mut().insert(id, Box::new(callback));

        let mut dropped_ids = Vec::new();
        let task = ImageDecodeTask {
            id,
            path,
            scale,
            kind,
        };
        {
            let (lock, condvar) = &*self.queue_state;
            let mut queues = lock.lock().expect("image queue mutex poisoned");

            match kind {
                ImageRequestKind::Detail => {
                    if queues.detail.len() >= MAX_PENDING_DETAIL_TASKS {
                        if let Some(dropped) = queues.detail.pop_front() {
                            dropped_ids.push(dropped.id);
                        }
                    }
                    queues.detail.push_back(task);
                }
                ImageRequestKind::GridThumb => {
                    if queues.grid.len() >= MAX_PENDING_GRID_TASKS {
                        if let Some(dropped) = queues.grid.pop_front() {
                            dropped_ids.push(dropped.id);
                        }
                    }
                    queues.grid.push_back(task);
                }
            }

            condvar.notify_one();
        }

        if !dropped_ids.is_empty() {
            let mut callbacks = self.callbacks.borrow_mut();
            for dropped_id in dropped_ids {
                callbacks.remove(&dropped_id);
            }
        }
    }
}

fn main() -> Result<()> {
    init_tracing();

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

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("booru_gtk=debug"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
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
    let image_loader = Rc::new(ImageLoader::new());

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

    let browse_mode_group = ToggleGroup::new();
    browse_mode_group.set_can_shrink(true);
    browse_mode_group.set_homogeneous(true);
    browse_mode_group.add(
        Toggle::builder()
            .name("list")
            .icon_name("view-list-symbolic")
            .tooltip("List mode")
            .build(),
    );
    browse_mode_group.add(
        Toggle::builder()
            .name("grid")
            .icon_name("view-grid-symbolic")
            .tooltip("Thumbnail mode")
            .build(),
    );
    browse_mode_group.set_active_name(Some(state.borrow().browser_mode.as_name()));
    header.pack_start(&browse_mode_group);

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

    let grid_store = gtk::gio::ListStore::new::<gtk::glib::BoxedAnyObject>();
    let grid_selection = SingleSelection::new(Some(grid_store.clone()));
    grid_selection.set_autoselect(false);
    grid_selection.set_can_unselect(true);

    let grid_factory = SignalListItemFactory::new();
    grid_factory.connect_setup(|_, list_item_obj| {
        let Some(list_item) = list_item_obj.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let card = GtkBox::new(Orientation::Vertical, 6);
        card.set_margin_start(6);
        card.set_margin_end(6);
        card.set_margin_top(6);
        card.set_margin_bottom(6);

        let thumb = Picture::new();
        thumb.set_content_fit(gtk::ContentFit::Cover);
        thumb.set_size_request(156, 156);
        thumb.set_can_shrink(true);
        thumb.set_halign(Align::Fill);

        let caption = Label::new(None);
        caption.set_wrap(false);
        caption.set_ellipsize(gtk::pango::EllipsizeMode::End);
        caption.set_max_width_chars(20);
        caption.set_xalign(0.0);
        caption.add_css_class("caption");

        card.append(&thumb);
        card.append(&caption);
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

            thumb.set_paintable(None::<&gtk::gdk::Texture>);
            let tooltip_guard = tooltip.clone();
            let thumb_weak = thumb.downgrade();
            let card_weak = card.downgrade();
            image_loader_handle.load(
                image_path,
                Some((156, 156)),
                ImageRequestKind::GridThumb,
                move |result| {
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
        });
    }

    grid_factory.connect_unbind(|_, list_item_obj| {
        let Some(list_item) = list_item_obj.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let Some((card, thumb, caption)) = grid_cell_widgets(list_item) else {
            return;
        };

        thumb.set_paintable(Option::<&gtk::gdk::Texture>::None);
        caption.set_text("");
        card.set_tooltip_text(None::<&str>);
    });

    let grid = GridView::new(Some(grid_selection.clone()), Some(grid_factory));
    grid.set_single_click_activate(true);
    grid.set_max_columns(4);
    grid.set_min_columns(2);
    let grid_scroll = ScrolledWindow::new();
    grid_scroll.set_child(Some(&grid));
    grid_scroll.set_hexpand(true);
    grid_scroll.set_vexpand(true);
    grid_scroll.add_css_class("navigation-sidebar");

    let browser_stack = ViewStack::new();
    browser_stack.set_hhomogeneous(false);
    browser_stack.set_vhomogeneous(false);
    browser_stack.add_titled(&list_scroll, Some("list"), "List");
    browser_stack.add_titled(&grid_scroll, Some("grid"), "Grid");
    browser_stack.set_visible_child_name(state.borrow().browser_mode.as_name());

    let sidebar_page = NavigationPage::new(&browser_stack, "Library");
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
        grid_store,
        grid_selection,
        browser_stack,
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
        detail_image_seq: Rc::new(Cell::new(0)),
        grid_loaded_version: Rc::new(Cell::new(0)),
        image_loader,
    };

    window.present();
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
        browse_mode_group.connect_active_name_notify(move |group| {
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
                let selected_pos = state_handle.borrow().selected_pos;
                sync_browser_selection(&ui, selected_pos);
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
        let state_handle = state.clone();
        let ui = ui.clone();
        let grid_handle = grid.clone();
        grid_handle.connect_activate(move |_grid, position| {
            let selected_pos = usize::try_from(position).ok();
            {
                let mut state = state_handle.borrow_mut();
                state.selected_pos = selected_pos.filter(|pos| *pos < state.filtered_indices.len());
            }

            if let Some(pos) = selected_pos {
                ui.grid_selection.set_selected(pos as u32);
            } else {
                ui.grid_selection.set_selected(gtk::INVALID_LIST_POSITION);
            }
            sync_browser_selection(&ui, selected_pos);
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
}

fn rebuild_view(state: &Rc<RefCell<AppState>>, ui: &Ui) {
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

    sync_browser_selection(ui, selected_pos);
}

fn refresh_grid(state: &Rc<RefCell<AppState>>, ui: &Ui) {
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
        return;
    }

    ui.grid_store.remove_all();
    for item_idx in filtered_indices {
        let boxed = gtk::glib::BoxedAnyObject::new(GridItemData {
            item_idx,
            texture: Rc::new(RefCell::new(None)),
        });
        ui.grid_store.append(&boxed);
    }

    ui.grid_loaded_version.set(filter_version);
    sync_browser_selection(ui, selected_pos);
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
            state.library.index.items.len(),
        )
    };

    ui.detail_stack.set_visible_child_name("detail");
    ui.title.set_text(&snapshot.2);
    ui.author.set_text(&format!("Author: {}", snapshot.3));
    ui.date.set_text(&format!("Date: {}", snapshot.4));
    ui.detail.set_text(&snapshot.5);
    ui.tags.set_text(&snapshot.6);
    ui.item_sensitive.set_active(snapshot.7);
    ui.picture.set_paintable(None::<&gtk::gdk::Texture>);
    hide_banner(ui);
    set_status(ui, &format!("Loading image: {}", snapshot.1.display()));

    let load_seq = ui.detail_image_seq.get().wrapping_add(1);
    ui.detail_image_seq.set(load_seq);

    let ui_handle = ui.clone();
    let image_path = snapshot.1.clone();
    let total_items = snapshot.8;
    ui.image_loader.load(
        image_path.clone(),
        None,
        ImageRequestKind::Detail,
        move |result| {
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
                            snapshot.0 + 1,
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
}

fn clear_detail(ui: &Ui) {
    ui.detail_image_seq
        .set(ui.detail_image_seq.get().wrapping_add(1));
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

fn infer_thumbnail_title(item: &booru_core::ImageItem) -> String {
    let base = infer_title(item);
    if item.merged_sensitive() {
        format!("[S] {base}")
    } else {
        base
    }
}

fn grid_cell_widgets(list_item: &gtk::ListItem) -> Option<(GtkBox, Picture, Label)> {
    let card = list_item.child()?.downcast::<GtkBox>().ok()?;
    let thumb = card.first_child()?.downcast::<Picture>().ok()?;
    let caption = thumb.next_sibling()?.downcast::<Label>().ok()?;
    Some((card, thumb, caption))
}

fn image_decode_worker(
    queue_state: Arc<(Mutex<ImageTaskQueues>, Condvar)>,
    result_tx: mpsc::Sender<ImageDecodeResult>,
) {
    loop {
        let task = {
            let (lock, condvar) = &*queue_state;
            let mut queues = lock.lock().expect("image queue mutex poisoned");

            while queues.detail.is_empty() && queues.grid.is_empty() {
                queues = condvar
                    .wait(queues)
                    .expect("image queue mutex poisoned while waiting");
            }

            if let Some(task) = queues.detail.pop_back() {
                task
            } else {
                queues
                    .grid
                    .pop_back()
                    .expect("grid queue unexpectedly empty")
            }
        };

        debug!(kind = ?task.kind, path = %task.path.display(), "render");
        let outcome = decode_image_for_texture(&task.path, task.scale)
            .map(|image| ImageDecodeResult::Ok { id: task.id, image })
            .unwrap_or_else(|message| {
                warn!(
                    kind = ?task.kind,
                    path = %task.path.display(),
                    error = %message,
                    "render failed"
                );
                ImageDecodeResult::Err {
                    id: task.id,
                    message,
                }
            });

        if result_tx.send(outcome).is_err() {
            break;
        }
    }
}

fn decode_image_for_texture(
    path: &PathBuf,
    scale: Option<(i32, i32)>,
) -> Result<DecodedImage, String> {
    let pixbuf = match scale {
        Some((width, height)) => {
            gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, width, height, true)
        }
        None => gtk::gdk_pixbuf::Pixbuf::from_file(path),
    }
    .map_err(|err| err.to_string())?;

    if pixbuf.colorspace() != gtk::gdk_pixbuf::Colorspace::Rgb {
        return Err("unsupported pixbuf colorspace".to_string());
    }
    if pixbuf.bits_per_sample() != 8 {
        return Err("unsupported bits-per-sample (expected 8)".to_string());
    }

    let format = match (pixbuf.has_alpha(), pixbuf.n_channels()) {
        (true, 4) => gtk::gdk::MemoryFormat::R8g8b8a8,
        (false, 3) => gtk::gdk::MemoryFormat::R8g8b8,
        (has_alpha, channels) => {
            return Err(format!(
                "unsupported channel layout (has_alpha={has_alpha}, channels={channels})"
            ));
        }
    };

    let rowstride =
        usize::try_from(pixbuf.rowstride()).map_err(|_| "invalid rowstride".to_string())?;
    Ok(DecodedImage {
        width: pixbuf.width(),
        height: pixbuf.height(),
        rowstride,
        format,
        pixels: pixbuf.read_pixel_bytes(),
    })
}

fn sync_browser_selection(ui: &Ui, selected_pos: Option<usize>) {
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
