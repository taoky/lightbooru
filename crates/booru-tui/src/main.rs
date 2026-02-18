use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use booru_core::{apply_update_to_image, BooruConfig, EditUpdate, Library, SearchQuery};
use clap::Parser;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use image::DynamicImage;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};

const TICK_RATE: Duration = Duration::from_millis(150);

#[derive(Parser)]
#[command(name = "booru-tui", version, about = "TUI browser for LightBooru")]
struct Cli {
    /// Base directory for gallery-dl downloads (can be repeated)
    #[arg(long, short)]
    base: Vec<PathBuf>,

    /// Suppress scan warnings
    #[arg(long)]
    quiet: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputMode {
    Normal,
    Search,
    Tag,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusPane {
    Images,
    Detail,
}

#[derive(Clone, Copy, Debug, Default)]
struct LayoutInfo {
    list_area: Rect,
    right_area: Rect,
    detail_area: Rect,
    preview_area: Rect,
    detail_preview_divider_y: Option<u16>,
}

struct Preview {
    picker: Picker,
    current_path: Option<PathBuf>,
    protocol: Option<StatefulProtocol>,
    last_error: Option<String>,
}

impl Preview {
    fn new(picker: Picker) -> Self {
        Self {
            picker,
            current_path: None,
            protocol: None,
            last_error: None,
        }
    }

    fn load_for_path(&mut self, path: &Path) {
        if self.current_path.as_deref() == Some(path) {
            return;
        }
        self.current_path = Some(path.to_path_buf());

        match load_image(path) {
            Ok(image) => {
                self.protocol = Some(self.picker.new_resize_protocol(image));
                self.last_error = None;
            }
            Err(err) => {
                self.protocol = None;
                self.last_error = Some(format!("failed to load image: {err}"));
            }
        }
    }
}

struct App {
    library: Library,
    filtered_indices: Vec<usize>,
    selected: usize,
    mode: InputMode,
    focus: FocusPane,
    search_input: String,
    input_buffer: String,
    detail_scroll: u16,
    detail_split_percent: u16,
    dragging_split: bool,
    layout: LayoutInfo,
    status: String,
    preview: Option<Preview>,
}

impl App {
    fn new(library: Library) -> Self {
        let mut app = Self {
            library,
            filtered_indices: Vec::new(),
            selected: 0,
            mode: InputMode::Normal,
            focus: FocusPane::Images,
            search_input: String::new(),
            input_buffer: String::new(),
            detail_scroll: 0,
            detail_split_percent: 50,
            dragging_split: false,
            layout: LayoutInfo::default(),
            status: String::from(
                "Tab switch focus, / search, j/k move or scroll, t add tags, s toggle sensitive, q quit",
            ),
            preview: None,
        };
        app.rebuild_filter();
        app
    }

    fn set_preview_picker(&mut self, picker: Picker) {
        let mut preview = Preview::new(picker);
        if let Some(idx) = self.selected_item_index() {
            let path = self.library.index.items[idx].image_path.clone();
            preview.load_for_path(&path);
        }
        self.preview = Some(preview);
    }

    fn rebuild_filter(&mut self) {
        let search = self
            .library
            .search(SearchQuery::new(split_search_terms(&self.search_input)).with_aliases(true));
        self.filtered_indices = search.indices;

        if self.filtered_indices.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered_indices.len() {
            self.selected = self.filtered_indices.len() - 1;
        }
    }

    fn selected_item_index(&self) -> Option<usize> {
        self.filtered_indices.get(self.selected).copied()
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered_indices.is_empty() {
            self.selected = 0;
            return;
        }

        let len = self.filtered_indices.len() as isize;
        let old = self.selected;
        let mut next = self.selected as isize + delta;
        if next < 0 {
            next = 0;
        }
        if next >= len {
            next = len - 1;
        }
        self.selected = next as usize;
        if self.selected != old {
            self.detail_scroll = 0;
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            FocusPane::Images => FocusPane::Detail,
            FocusPane::Detail => FocusPane::Images,
        };
    }

    fn set_focus(&mut self, focus: FocusPane) {
        self.focus = focus;
    }

    fn scroll_detail(&mut self, delta: isize) {
        if delta >= 0 {
            self.detail_scroll = self.detail_scroll.saturating_add(delta as u16);
        } else {
            self.detail_scroll = self.detail_scroll.saturating_sub((-delta) as u16);
        }
    }

    fn update_detail_split_from_mouse(&mut self, y: u16) {
        if self.layout.right_area.height < 4 {
            return;
        }
        let rel = y.saturating_sub(self.layout.right_area.y);
        let mut pct = (u32::from(rel) * 100 / u32::from(self.layout.right_area.height)) as i32;
        pct = pct.clamp(25, 75);
        self.detail_split_percent = pct as u16;
    }

    fn toggle_sensitive(&mut self) -> Result<()> {
        let Some(idx) = self.selected_item_index() else {
            self.status = "No selected item.".to_string();
            return Ok(());
        };
        let item = &self.library.index.items[idx];
        let new_value = !item.merged_sensitive();
        let image_path = item.image_path.clone();

        let edits = apply_update_to_image(
            &image_path,
            EditUpdate {
                set_tags: None,
                add_tags: Vec::new(),
                remove_tags: Vec::new(),
                clear_tags: false,
                notes: None,
                sensitive: Some(new_value),
            },
        )
        .with_context(|| format!("failed to update {}", image_path.display()))?;

        self.library.index.items[idx].edits = edits;
        self.status = format!(
            "Sensitive set to {} for {}",
            if new_value { "ON" } else { "OFF" },
            image_path.display()
        );
        Ok(())
    }

    fn add_tags_from_input(&mut self) -> Result<()> {
        let tags = parse_tags(&self.input_buffer);
        if tags.is_empty() {
            self.status = "No tags entered.".to_string();
            return Ok(());
        }

        let Some(idx) = self.selected_item_index() else {
            self.status = "No selected item.".to_string();
            return Ok(());
        };
        let image_path = self.library.index.items[idx].image_path.clone();

        let edits = apply_update_to_image(
            &image_path,
            EditUpdate {
                set_tags: None,
                add_tags: tags.clone(),
                remove_tags: Vec::new(),
                clear_tags: false,
                notes: None,
                sensitive: None,
            },
        )
        .with_context(|| format!("failed to update {}", image_path.display()))?;

        self.library.index.items[idx].edits = edits;
        self.rebuild_filter();
        self.status = format!("Added tags [{}]", tags.join(", "));
        Ok(())
    }
}

fn parse_tags(input: &str) -> Vec<String> {
    let normalized = input.replace(',', " ");
    normalized
        .split_whitespace()
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn split_search_terms(input: &str) -> Vec<String> {
    input
        .split_whitespace()
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn load_image(path: &Path) -> Result<DynamicImage> {
    image::open(path).with_context(|| format!("unable to decode {}", path.display()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = if cli.base.is_empty() {
        BooruConfig::default()
    } else {
        BooruConfig::with_roots(cli.base)
    };

    let library = Library::scan(config)?;
    if !cli.quiet {
        for warning in &library.warnings {
            eprintln!("warning: {}: {}", warning.path.display(), warning.message);
        }
    }

    run_tui(App::new(library))
}

fn run_tui(mut app: App) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alt screen")?;
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    app.set_preview_picker(picker);

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend).context("failed to init terminal")?;

    let result = run_event_loop(&mut terminal, &mut app);

    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();

    result
}

fn run_event_loop(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        terminal.draw(|frame| render_ui(frame, app))?;

        if !event::poll(TICK_RATE)? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if handle_key_event(app, key)? {
                    break;
                }
            }
            Event::Mouse(mouse) => handle_mouse_event(app, mouse),
            Event::Resize(_, _) => {}
            _ => {}
        }
    }

    Ok(())
}

fn handle_key_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    match app.mode {
        InputMode::Normal => handle_normal_mode(app, key),
        InputMode::Search => Ok(handle_text_mode(app, key, InputMode::Search)?),
        InputMode::Tag => Ok(handle_text_mode(app, key, InputMode::Tag)?),
    }
}

fn handle_normal_mode(app: &mut App, key: KeyEvent) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Tab => app.toggle_focus(),
        KeyCode::Char('h') | KeyCode::Left => app.set_focus(FocusPane::Images),
        KeyCode::Char('l') | KeyCode::Right => app.set_focus(FocusPane::Detail),
        KeyCode::Char('j') | KeyCode::Down => match app.focus {
            FocusPane::Images => app.move_selection(1),
            FocusPane::Detail => app.scroll_detail(1),
        },
        KeyCode::Char('k') | KeyCode::Up => match app.focus {
            FocusPane::Images => app.move_selection(-1),
            FocusPane::Detail => app.scroll_detail(-1),
        },
        KeyCode::PageDown => match app.focus {
            FocusPane::Images => app.move_selection(10),
            FocusPane::Detail => app.scroll_detail(10),
        },
        KeyCode::PageUp => match app.focus {
            FocusPane::Images => app.move_selection(-10),
            FocusPane::Detail => app.scroll_detail(-10),
        },
        KeyCode::Char('/') => {
            app.mode = InputMode::Search;
            app.input_buffer = app.search_input.clone();
            app.status = "Search mode: type query and press Enter".to_string();
        }
        KeyCode::Char('t') => {
            app.mode = InputMode::Tag;
            app.input_buffer.clear();
            app.status = "Tag mode: input tags (space/comma separated) and press Enter".to_string();
        }
        KeyCode::Char('s') => {
            if let Err(err) = app.toggle_sensitive() {
                app.status = err.to_string();
            }
        }
        _ => {}
    }

    Ok(false)
}

fn handle_mouse_event(app: &mut App, mouse: MouseEvent) {
    let (x, y) = (mouse.column, mouse.row);
    let on_divider = app
        .layout
        .detail_preview_divider_y
        .map(|divider_y| {
            point_in_rect(x, y, app.layout.right_area)
                && (y == divider_y || y == divider_y.saturating_sub(1))
        })
        .unwrap_or(false);

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if on_divider {
                app.dragging_split = true;
                app.status = "Resizing detail/preview split".to_string();
                return;
            }

            if point_in_rect(x, y, app.layout.list_area) {
                app.set_focus(FocusPane::Images);
                select_list_row_from_mouse(app, y);
                return;
            }

            if point_in_rect(x, y, app.layout.detail_area) {
                app.set_focus(FocusPane::Detail);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.dragging_split {
                app.update_detail_split_from_mouse(y);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.dragging_split = false;
        }
        MouseEventKind::ScrollUp => {
            if point_in_rect(x, y, app.layout.detail_area) {
                app.set_focus(FocusPane::Detail);
                app.scroll_detail(-3);
            } else if point_in_rect(x, y, app.layout.list_area) {
                app.set_focus(FocusPane::Images);
                app.move_selection(-3);
            }
        }
        MouseEventKind::ScrollDown => {
            if point_in_rect(x, y, app.layout.detail_area) {
                app.set_focus(FocusPane::Detail);
                app.scroll_detail(3);
            } else if point_in_rect(x, y, app.layout.list_area) {
                app.set_focus(FocusPane::Images);
                app.move_selection(3);
            }
        }
        _ => {}
    }
}

fn select_list_row_from_mouse(app: &mut App, row: u16) {
    if app.filtered_indices.is_empty() {
        return;
    }
    let inner = inner_rect(app.layout.list_area);
    if inner.height == 0 || row < inner.y {
        return;
    }

    let offset = row - inner.y;
    if usize::from(offset) >= app.filtered_indices.len() {
        return;
    }
    app.selected = usize::from(offset);
    app.detail_scroll = 0;
}

fn handle_text_mode(app: &mut App, key: KeyEvent, mode: InputMode) -> Result<bool> {
    match key.code {
        KeyCode::Esc => {
            app.mode = InputMode::Normal;
            app.input_buffer.clear();
            app.status = "Canceled.".to_string();
        }
        KeyCode::Enter => {
            if mode == InputMode::Search {
                app.search_input = app.input_buffer.trim().to_string();
                app.rebuild_filter();
                app.status = format!("Filter updated: {} result(s)", app.filtered_indices.len());
            } else {
                if let Err(err) = app.add_tags_from_input() {
                    app.status = err.to_string();
                }
            }
            app.mode = InputMode::Normal;
            app.input_buffer.clear();
        }
        KeyCode::Backspace => {
            app.input_buffer.pop();
        }
        KeyCode::Char(ch) => {
            if !key.modifiers.contains(KeyModifiers::CONTROL) {
                app.input_buffer.push(ch);
            }
        }
        _ => {}
    }

    Ok(false)
}

fn render_ui(frame: &mut Frame, app: &mut App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(frame.area());

    render_search_panel(frame, areas[0], app);
    render_main_panel(frame, areas[1], app);
    render_status(frame, areas[2], app);
}

fn render_search_panel(frame: &mut Frame, area: Rect, app: &App) {
    let label = match app.mode {
        InputMode::Search => format!("Search: {}_", app.input_buffer),
        _ => format!("Search: {}", app.search_input),
    };
    let paragraph =
        Paragraph::new(label).block(Block::default().borders(Borders::ALL).title("Filter"));
    frame.render_widget(paragraph, area);
}

fn render_main_panel(frame: &mut Frame, area: Rect, app: &mut App) {
    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    app.layout.list_area = main[0];
    app.layout.right_area = main[1];

    render_list_panel(frame, main[0], app);
    render_detail_and_preview(frame, main[1], app);
}

fn render_list_panel(frame: &mut Frame, area: Rect, app: &App) {
    let total = app.filtered_indices.len();
    let current = if total == 0 { 0 } else { app.selected + 1 };

    let items = app
        .filtered_indices
        .iter()
        .map(|idx| {
            let item = &app.library.index.items[*idx];
            let file_name = item
                .image_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("(unknown)");
            let author = item
                .merged_author()
                .unwrap_or_else(|| "(unknown)".to_string());
            ListItem::new(format!("{file_name} | {author}"))
        })
        .collect::<Vec<_>>();

    let list =
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(
                if app.focus == FocusPane::Images {
                    format!("Images ({current}/{total}) [Focus]")
                } else {
                    format!("Images ({current}/{total})")
                },
            ))
            .highlight_symbol("> ")
            .highlight_style(Style::default().add_modifier(Modifier::BOLD));

    let mut state = ListState::default();
    if !app.filtered_indices.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_detail_and_preview(frame: &mut Frame, area: Rect, app: &mut App) {
    let detail_pct = app.detail_split_percent.clamp(25, 75);
    let columns = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(detail_pct),
            Constraint::Percentage(100 - detail_pct),
        ])
        .split(area);
    app.layout.detail_area = columns[0];
    app.layout.preview_area = columns[1];
    app.layout.detail_preview_divider_y = Some(columns[1].y);

    let Some(item_idx) = app.selected_item_index() else {
        let empty =
            Paragraph::new("No items.").block(Block::default().borders(Borders::ALL).title(
                if app.focus == FocusPane::Detail {
                    "Detail [Focus]"
                } else {
                    "Detail"
                },
            ));
        frame.render_widget(empty, columns[0]);
        render_preview_panel(
            frame,
            columns[1],
            app,
            None,
            "Preview not available.".to_string(),
        );
        return;
    };

    let (detail_text, image_path, preview_fallback) = {
        let item = &app.library.index.items[item_idx];
        let detail_text = format!(
            "Path: {}\nAuthor: {}\nDate: {}\nSensitive: {}\nTags: {}\nNotes: {}\nURL: {}\n\nDetail:\n{}",
            item.image_path.display(),
            item.merged_author().unwrap_or_else(|| "(none)".to_string()),
            item.merged_date().unwrap_or_else(|| "(none)".to_string()),
            if item.merged_sensitive() { "yes" } else { "no" },
            {
                let tags = item.merged_tags();
                if tags.is_empty() {
                    "(none)".to_string()
                } else {
                    tags.join(" ")
                }
            },
            item.edits.notes.as_deref().unwrap_or("(none)"),
            item.platform_url().unwrap_or_else(|| "(none)".to_string()),
            item.merged_detail().unwrap_or_else(|| "(none)".to_string())
        );
        let preview_fallback = format!(
            "Image: {}\nMetadata: {}\nBooru edits: {}",
            item.image_path.display(),
            item.meta_path.display(),
            item.booru_path.display()
        );
        (detail_text, item.image_path.clone(), preview_fallback)
    };

    let detail_block =
        Block::default()
            .borders(Borders::ALL)
            .title(if app.focus == FocusPane::Detail {
                "Detail [Focus]"
            } else {
                "Detail"
            });
    let detail_inner = detail_block.inner(columns[0]);
    let detail_visible_lines = detail_inner.height;
    let detail_total_lines = estimate_wrapped_lines(&detail_text, detail_inner.width);
    let max_scroll = detail_total_lines.saturating_sub(detail_visible_lines);
    app.detail_scroll = app.detail_scroll.min(max_scroll);

    let detail = Paragraph::new(detail_text)
        .block(detail_block)
        .scroll((app.detail_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(detail, columns[0]);

    render_preview_panel(frame, columns[1], app, Some(image_path), preview_fallback);
}

fn render_preview_panel(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    image_path: Option<PathBuf>,
    fallback: String,
) {
    let title = "Preview";
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(path) = image_path else {
        let text = Paragraph::new(fallback).wrap(Wrap { trim: false });
        frame.render_widget(text, inner);
        return;
    };

    let Some(preview) = app.preview.as_mut() else {
        let text = Paragraph::new(format!("{fallback}\n\nPreview backend is not initialized."))
            .wrap(Wrap { trim: false });
        frame.render_widget(text, inner);
        return;
    };

    preview.load_for_path(&path);
    if let Some(protocol) = preview.protocol.as_mut() {
        frame.render_stateful_widget(
            StatefulImage::default().resize(Resize::Fit(None)),
            inner,
            protocol,
        );
        return;
    }

    let error = preview
        .last_error
        .as_deref()
        .unwrap_or("unknown image decode error");
    let text = Paragraph::new(format!("{fallback}\n\nPreview unavailable: {error}"))
        .wrap(Wrap { trim: false });
    frame.render_widget(text, inner);
}

fn estimate_wrapped_lines(text: &str, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }

    let mut total: u32 = 0;
    let width_u32 = u32::from(width);
    for line in text.split('\n') {
        let line_width = line.chars().count() as u32;
        let wrapped = if line_width == 0 {
            1
        } else {
            line_width.div_ceil(width_u32)
        };
        total = total.saturating_add(wrapped);
    }
    total.min(u32::from(u16::MAX)) as u16
}

fn point_in_rect(x: u16, y: u16, rect: Rect) -> bool {
    if rect.width == 0 || rect.height == 0 {
        return false;
    }
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

fn inner_rect(rect: Rect) -> Rect {
    if rect.width <= 2 || rect.height <= 2 {
        return Rect::new(rect.x, rect.y, 0, 0);
    }
    Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2)
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    let prefix = match app.mode {
        InputMode::Normal => "NORMAL",
        InputMode::Search => "SEARCH",
        InputMode::Tag => "TAG",
    };
    let focus = match app.focus {
        FocusPane::Images => "Images",
        FocusPane::Detail => "Detail",
    };
    let status = Paragraph::new(format!("[{prefix} | Focus:{focus}] {}", app.status))
        .block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(status, area);
}
