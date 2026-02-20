use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{mpsc, Arc, Condvar, Mutex, Once};
use std::thread;
use std::time::Duration;
use tracing::{debug, warn};
use tracing_subscriber::EnvFilter;

use adw::prelude::*;
use adw::{
    ActionRow, Application, ApplicationWindow, Banner, BottomSheet, NavigationSplitView, Toast,
    ToastOverlay, ToggleGroup, ViewStack, WrapBox,
};
use anyhow::Result;
use booru_core::{
    apply_update_to_image, BooruConfig, EditUpdate, Library, SearchQuery, SearchSort,
};
use clap::Parser;
use gtk::{
    self, Box as GtkBox, Button, Entry, GridView, Label, ListBox, Picture, ScrolledWindow,
    SearchEntry, SelectionMode, SignalListItemFactory, SingleSelection, TextView,
};

const APP_CSS: &str = include_str!("style.css");
const APP_UI: &str = include_str!(concat!(env!("OUT_DIR"), "/main.ui"));
const GRID_CELL_UI: &str = include_str!(concat!(env!("OUT_DIR"), "/grid_cell.ui"));
const TAG_CHIP_UI: &str = include_str!(concat!(env!("OUT_DIR"), "/tag_chip.ui"));

static APP_CSS_ONCE: Once = Once::new();

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
    pending_request_id: Rc<Cell<Option<u64>>>,
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
    list_scroll: ScrolledWindow,
    grid: GridView,
    grid_store: gtk::gio::ListStore,
    grid_selection: SingleSelection,
    browser_stack: ViewStack,
    picture: Picture,
    title: Label,
    author: Label,
    date: Label,
    detail: Label,
    tags_wrap: WrapBox,
    tags_add_button: Button,
    tags_input: Entry,
    tag_values: Rc<RefCell<Vec<String>>>,
    notes: TextView,
    item_sensitive: gtk::Switch,
    status: Label,
    detail_stack: ViewStack,
    edit_sheet: BottomSheet,
    toast_overlay: ToastOverlay,
    banner: Banner,
    detail_image_seq: Rc<Cell<u64>>,
    detail_pending_request_id: Rc<Cell<Option<u64>>>,
    grid_loaded_version: Rc<Cell<u64>>,
    image_loader: Rc<ImageLoader>,
}

#[derive(Clone)]
struct UiControls {
    window: ApplicationWindow,
    compact_back_button: Button,
    search: SearchEntry,
    search_bar: gtk::SearchBar,
    search_button: gtk::ToggleButton,
    browse_mode_group: ToggleGroup,
    split: NavigationSplitView,
    save_button: Button,
    edit_bar: gtk::CenterBox,
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

type ImageLoadCallback = Box<dyn FnOnce(u64, Result<gtk::gdk::Texture, String>)>;

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

#[derive(Clone, Copy, Debug)]
enum ImageWorkerLane {
    Detail,
    Grid,
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
                            callback(id, Ok(texture.upcast::<gtk::gdk::Texture>()));
                        }
                        ImageDecodeResult::Err { message, .. } => callback(id, Err(message)),
                    }
                }

                gtk::glib::ControlFlow::Continue
            });
        }

        spawn_image_worker(
            "booru-image-worker-detail",
            ImageWorkerLane::Detail,
            queue_state.clone(),
            result_tx.clone(),
        );
        spawn_image_worker(
            "booru-image-worker-grid-0",
            ImageWorkerLane::Grid,
            queue_state.clone(),
            result_tx.clone(),
        );
        spawn_image_worker(
            "booru-image-worker-grid-1",
            ImageWorkerLane::Grid,
            queue_state.clone(),
            result_tx,
        );

        Self {
            next_id: Rc::new(Cell::new(1)),
            callbacks,
            queue_state,
        }
    }

    fn load<F>(
        &self,
        path: PathBuf,
        scale: Option<(i32, i32)>,
        kind: ImageRequestKind,
        callback: F,
    ) -> u64
    where
        F: FnOnce(u64, Result<gtk::gdk::Texture, String>) + 'static,
    {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        self.callbacks.borrow_mut().insert(id, Box::new(callback));

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
                ImageRequestKind::Detail => queues.detail.push_back(task),
                ImageRequestKind::GridThumb => queues.grid.push_back(task),
            }

            condvar.notify_all();
        }

        id
    }

    fn cancel_if_queued(&self, id: u64) -> bool {
        let removed = {
            let (lock, _) = &*self.queue_state;
            let mut queues = lock.lock().expect("image queue mutex poisoned");
            remove_queued_task(&mut queues.detail, id) || remove_queued_task(&mut queues.grid, id)
        };

        if removed {
            self.callbacks.borrow_mut().remove(&id);
        }

        removed
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

fn builder_object<T: gtk::prelude::IsA<gtk::glib::Object>>(builder: &gtk::Builder, id: &str) -> T {
    builder
        .object(id)
        .unwrap_or_else(|| panic!("missing `{id}` in UI definition"))
}

fn build_ui(app: &Application, state: Rc<RefCell<AppState>>) {
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

fn install_tag_editor_css() {
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
    ensure_selected_item_visible(ui, selected_pos);
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
        ensure_selected_item_visible(ui, selected_pos);
        return;
    }

    ui.grid_store.remove_all();
    for item_idx in filtered_indices {
        let boxed = gtk::glib::BoxedAnyObject::new(GridItemData {
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
            item.merged_tags(),
            item.edits.notes.clone().unwrap_or_default(),
            item.merged_sensitive(),
            state.library.index.items.len(),
        )
    };

    ui.detail_stack.set_visible_child_name("detail");
    ui.edit_sheet.set_can_open(true);
    ui.title.set_text(&snapshot.2);
    ui.author.set_text(&format!("Author: {}", snapshot.3));
    ui.date.set_text(&format!("Date: {}", snapshot.4));
    ui.detail.set_text(&snapshot.5);
    {
        let mut tag_values = ui.tag_values.borrow_mut();
        *tag_values = snapshot.6.clone();
    }
    ui.tags_input.set_text("");
    rebuild_tag_wrap(ui);
    set_notes_text(&ui.notes, &snapshot.7);
    ui.item_sensitive.set_active(snapshot.8);
    ui.picture.set_paintable(None::<&gtk::gdk::Texture>);
    hide_banner(ui);
    set_status(ui, &format!("Loading image: {}", snapshot.1.display()));

    if let Some(previous_request_id) = ui.detail_pending_request_id.replace(None) {
        ui.image_loader.cancel_if_queued(previous_request_id);
    }

    let load_seq = ui.detail_image_seq.get().wrapping_add(1);
    ui.detail_image_seq.set(load_seq);

    let ui_handle = ui.clone();
    let image_path = snapshot.1.clone();
    let total_items = snapshot.9;
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
    ui.detail.set_text("");
    ui.tag_values.borrow_mut().clear();
    ui.tags_input.set_text("");
    rebuild_tag_wrap(ui);
    set_notes_text(&ui.notes, "");
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

fn set_notes_text(notes: &TextView, text: &str) {
    notes.buffer().set_text(text);
}

fn get_notes_text(notes: &TextView) -> String {
    let buffer = notes.buffer();
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

fn append_pending_tags_input(ui: &Ui) -> bool {
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

fn rebuild_tag_wrap(ui: &Ui) {
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
    lane: ImageWorkerLane,
    queue_state: Arc<(Mutex<ImageTaskQueues>, Condvar)>,
    result_tx: mpsc::Sender<ImageDecodeResult>,
) {
    loop {
        let task = {
            let (lock, condvar) = &*queue_state;
            let mut queues = lock.lock().expect("image queue mutex poisoned");

            while queue_is_empty_for_lane(&queues, lane) {
                queues = condvar
                    .wait(queues)
                    .expect("image queue mutex poisoned while waiting");
            }

            pop_task_for_lane(&mut queues, lane).expect("worker lane queue unexpectedly empty")
        };

        debug!(lane = ?lane, kind = ?task.kind, path = %task.path.display(), "render");
        let outcome = decode_image_for_texture(&task.path, task.scale)
            .map(|image| ImageDecodeResult::Ok { id: task.id, image })
            .unwrap_or_else(|message| {
                warn!(
                    lane = ?lane,
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

fn spawn_image_worker(
    name: &str,
    lane: ImageWorkerLane,
    queue_state: Arc<(Mutex<ImageTaskQueues>, Condvar)>,
    result_tx: mpsc::Sender<ImageDecodeResult>,
) {
    thread::Builder::new()
        .name(name.to_string())
        .spawn(move || image_decode_worker(lane, queue_state, result_tx))
        .expect("failed to start booru image worker thread");
}

fn queue_is_empty_for_lane(queues: &ImageTaskQueues, lane: ImageWorkerLane) -> bool {
    match lane {
        ImageWorkerLane::Detail => queues.detail.is_empty(),
        ImageWorkerLane::Grid => queues.grid.is_empty(),
    }
}

fn pop_task_for_lane(
    queues: &mut ImageTaskQueues,
    lane: ImageWorkerLane,
) -> Option<ImageDecodeTask> {
    match lane {
        ImageWorkerLane::Detail => queues.detail.pop_back(),
        ImageWorkerLane::Grid => queues.grid.pop_front(),
    }
}

fn remove_queued_task(queue: &mut VecDeque<ImageDecodeTask>, id: u64) -> bool {
    let Some(position) = queue.iter().position(|task| task.id == id) else {
        return false;
    };
    queue.remove(position);
    true
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

fn ensure_selected_item_visible(ui: &Ui, selected_pos: Option<usize>) {
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
