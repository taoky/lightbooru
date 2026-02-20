mod build;
mod image_loader;
mod view;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Once;

use adw::{
    ApplicationWindow, Banner, BottomSheet, NavigationSplitView, ToastOverlay, ToggleGroup,
    ViewStack, WrapBox,
};
use booru_core::{Library, SearchQuery, SearchSort};
use gtk::{
    self, Button, Entry, GridView, Label, ListBox, Picture, ScrolledWindow, SearchEntry,
    SingleSelection, TextView,
};

use self::image_loader::ImageLoader;

pub(crate) use build::build_ui;
pub(crate) use view::scan_library;

const APP_CSS: &str = include_str!("style.css");
const APP_UI: &str = include_str!(concat!(env!("OUT_DIR"), "/main.ui"));
const GRID_CELL_UI: &str = include_str!(concat!(env!("OUT_DIR"), "/grid_cell.ui"));
const TAG_CHIP_UI: &str = include_str!(concat!(env!("OUT_DIR"), "/tag_chip.ui"));

static APP_CSS_ONCE: Once = Once::new();

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

pub(crate) struct AppState {
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
    pub(crate) fn new(library: Library, show_sensitive: bool, quiet: bool) -> Self {
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

fn builder_object<T: gtk::prelude::IsA<gtk::glib::Object>>(builder: &gtk::Builder, id: &str) -> T {
    builder
        .object(id)
        .unwrap_or_else(|| panic!("missing `{id}` in UI definition"))
}

fn split_search_terms(input: &str) -> Vec<String> {
    input
        .split_whitespace()
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(ToString::to_string)
        .collect()
}
