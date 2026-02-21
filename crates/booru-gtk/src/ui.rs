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
    self, Button, Entry, GridView, Label, LinkButton, ListBox, Picture, ScrolledWindow,
    SearchEntry, SingleSelection, TextView,
};
use rand::seq::SliceRandom;

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
    random_sort: bool,
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
            browser_mode: BrowserMode::Grid,
            show_sensitive,
            random_sort: true,
            query: String::new(),
            quiet,
        };
        state.rebuild_filter();
        state
    }

    fn rebuild_filter(&mut self) {
        let (terms, source_url) = split_search_terms_and_source_url(&self.query);
        let has_source_url_filter = source_url.is_some();
        let use_aliases = !terms.is_empty();
        let result = self.library.search(
            SearchQuery::new(terms)
                .with_aliases(use_aliases)
                .with_source_url(source_url)
                .with_sort(SearchSort::FileNameAsc),
        );

        self.filtered_indices = result
            .indices
            .into_iter()
            .filter(|idx| self.show_sensitive || !self.library.index.items[*idx].merged_sensitive())
            .collect();
        if self.random_sort && !has_source_url_filter {
            let mut rng = rand::thread_rng();
            self.filtered_indices.shuffle(&mut rng);
        }

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
    window: ApplicationWindow,
    list: ListBox,
    list_scroll: ScrolledWindow,
    grid: GridView,
    grid_store: gtk::gio::ListStore,
    grid_selection: SingleSelection,
    browser_stack: ViewStack,
    picture: Picture,
    title: Label,
    author: Button,
    date: Label,
    source_url: LinkButton,
    search_same_source_button: Button,
    open_file_button: Button,
    detail: Label,
    tags_wrap: WrapBox,
    tags_add_button: Button,
    tags_input: Entry,
    tag_values: Rc<RefCell<Vec<String>>>,
    notes: TextView,
    item_sensitive: gtk::Switch,
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
    search: SearchEntry,
    search_bar: gtk::SearchBar,
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

fn split_search_terms_and_source_url(input: &str) -> (Vec<String>, Option<String>) {
    let mut terms = Vec::new();
    let mut source_url = None;

    for term in split_search_terms(input) {
        let lower = term.to_ascii_lowercase();
        if source_url.is_none() && (lower.starts_with("http://") || lower.starts_with("https://")) {
            source_url = Some(term);
        } else {
            terms.push(term);
        }
    }

    (terms, source_url)
}
