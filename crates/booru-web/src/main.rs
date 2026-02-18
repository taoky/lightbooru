use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use askama::Template;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use booru_core::{BooruConfig, Library, SearchQuery, SearchSort};
use clap::Parser;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::Deserialize;
use tokio::signal;

#[derive(Parser, Debug)]
#[command(
    name = "booru-web",
    version,
    about = "Read-only web browser for LightBooru"
)]
struct Cli {
    /// Base directory for gallery-dl downloads (can be repeated)
    #[arg(long, short)]
    base: Vec<PathBuf>,

    /// Suppress scan warnings
    #[arg(long)]
    quiet: bool,

    /// Bind host (default localhost only)
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Bind port
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Show sensitive images by default
    #[arg(long)]
    sensitive: bool,

    /// Maximum items shown in one page
    #[arg(long, default_value_t = 120)]
    limit: usize,
}

#[derive(Clone)]
struct AppState {
    library: Arc<Library>,
    default_show_sensitive: bool,
    default_limit: usize,
}

#[derive(Debug, Default, Deserialize)]
struct IndexParams {
    q: Option<String>,
    source: Option<String>,
    show_sensitive: Option<String>,
    limit: Option<usize>,
    page: Option<usize>,
    from: Option<usize>,
    sy: Option<u32>,
    randomize: Option<String>,
    seed: Option<u64>,
}

#[derive(Clone, Debug)]
struct GridItem {
    id: usize,
    detail_href: String,
    title: String,
    author: String,
    author_href: Option<String>,
    date: String,
    detail: String,
    tags: Vec<TagLink>,
    sensitive: bool,
}

#[derive(Clone, Debug)]
struct TagLink {
    label: String,
    href: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    query: String,
    source_filter: Option<String>,
    show_sensitive: bool,
    randomize: bool,
    seed: Option<u64>,
    reshuffle_href: Option<String>,
    total_matches: usize,
    shown_count: usize,
    limit: usize,
    page: usize,
    total_pages: usize,
    start_item: usize,
    end_item: usize,
    prev_page: Option<usize>,
    next_page: Option<usize>,
    items: Vec<GridItem>,
}

#[derive(Template)]
#[template(path = "item.html")]
struct ItemTemplate {
    id: usize,
    back_href: String,
    title: String,
    author: String,
    author_href: Option<String>,
    date: String,
    detail: String,
    sensitive: bool,
    platform_url: Option<String>,
    source_search_href: Option<String>,
    tags: Vec<TagLink>,
    original_json: String,
    edits_json: String,
}

struct HtmlTemplate<T>(T);

impl<T> IntoResponse for HtmlTemplate<T>
where
    T: Template,
{
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(content) => Html(content).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to render template: {err}"),
            )
                .into_response(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = if cli.base.is_empty() {
        BooruConfig::default()
    } else {
        BooruConfig::with_roots(cli.base)
    };
    let library = scan_library(&config, cli.quiet)?;

    let state = AppState {
        library: Arc::new(library),
        default_show_sensitive: cli.sensitive,
        default_limit: cli.limit.clamp(1, 1000),
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/items/:id", get(item_handler))
        .route("/media/:id", get(media_handler))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port)
        .parse()
        .context("invalid bind host/port")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("failed to bind TCP listener")?;
    let local_addr = listener
        .local_addr()
        .context("failed to read bound address")?;
    println!("booru-web listening on http://{local_addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("web server exited with error")?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = signal::ctrl_c().await;
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

async fn index_handler(
    State(state): State<AppState>,
    Query(params): Query<IndexParams>,
) -> impl IntoResponse {
    let query = params.q.unwrap_or_default();
    let query_trimmed = query.trim().to_string();
    let source_filter = params
        .source
        .map(|source| source.trim().to_string())
        .filter(|source| !source.is_empty());
    let show_sensitive = params
        .show_sensitive
        .as_deref()
        .map(parse_truthy)
        .unwrap_or(state.default_show_sensitive);
    let randomize = params
        .randomize
        .as_deref()
        .map(parse_truthy)
        .unwrap_or(true);
    let limit = params.limit.unwrap_or(state.default_limit).clamp(1, 1000);
    let requested_page = params.page.unwrap_or(1).max(1);
    let seed = if randomize {
        Some(params.seed.unwrap_or_else(generate_seed))
    } else {
        None
    };

    let use_aliases = !query_trimmed.is_empty();
    let mut indices = state
        .library
        .search(
            SearchQuery::new(split_search_terms(&query_trimmed))
                .with_aliases(use_aliases)
                .with_source_url(source_filter.clone())
                .with_sort(SearchSort::FileNameAsc),
        )
        .indices;

    if !show_sensitive {
        indices.retain(|idx| !state.library.index.items[*idx].merged_sensitive());
    }
    if let Some(seed) = seed {
        let mut rng = StdRng::seed_from_u64(seed);
        indices.shuffle(&mut rng);
    }

    let total_matches = indices.len();
    let total_pages = if total_matches == 0 {
        1
    } else {
        (total_matches + limit - 1) / limit
    };
    let page = requested_page.min(total_pages);
    let start = (page - 1) * limit;
    let end = usize::min(start + limit, total_matches);
    let (start_item, end_item) = if total_matches == 0 {
        (0, 0)
    } else {
        (start + 1, end)
    };
    let nav = IndexNav {
        query: query_trimmed.clone(),
        source_url: source_filter.clone(),
        show_sensitive,
        randomize,
        seed,
        limit,
        page,
    };

    let items = indices[start..end]
        .iter()
        .copied()
        .filter_map(|idx| {
            state
                .library
                .index
                .items
                .get(idx)
                .map(|item| to_grid_item(idx, item, &nav))
        })
        .collect::<Vec<_>>();

    let reshuffle_href = seed.map(|current_seed| {
        build_index_href(&IndexNav {
            query: query_trimmed.clone(),
            source_url: source_filter.clone(),
            show_sensitive,
            randomize: true,
            seed: Some(next_seed(current_seed)),
            limit,
            page: 1,
        })
    });

    HtmlTemplate(IndexTemplate {
        query: query_trimmed,
        source_filter,
        show_sensitive,
        randomize,
        seed,
        reshuffle_href,
        total_matches,
        shown_count: items.len(),
        limit,
        page,
        total_pages,
        start_item,
        end_item,
        prev_page: page.checked_sub(1).filter(|p| *p >= 1),
        next_page: if page < total_pages {
            Some(page + 1)
        } else {
            None
        },
        items,
    })
}

async fn item_handler(
    State(state): State<AppState>,
    Path(id): Path<usize>,
    Query(params): Query<IndexParams>,
) -> impl IntoResponse {
    let Some(item) = state.library.index.items.get(id) else {
        return (StatusCode::NOT_FOUND, "item not found").into_response();
    };
    let query_trimmed = params.q.unwrap_or_default().trim().to_string();
    let source_filter = params
        .source
        .map(|source| source.trim().to_string())
        .filter(|source| !source.is_empty());
    let show_sensitive = params
        .show_sensitive
        .as_deref()
        .map(parse_truthy)
        .unwrap_or(state.default_show_sensitive);
    let randomize = params
        .randomize
        .as_deref()
        .map(parse_truthy)
        .unwrap_or(true);
    let seed = if randomize { params.seed } else { None };
    let limit = params.limit.unwrap_or(state.default_limit).clamp(1, 1000);
    let page = params.page.unwrap_or(1).max(1);
    let mut back_href = build_index_href(&IndexNav {
        query: query_trimmed,
        source_url: source_filter,
        show_sensitive,
        randomize,
        seed,
        limit,
        page,
    });
    if let Some(scroll_y) = params.sy {
        if back_href.contains('?') {
            back_href.push('&');
        } else {
            back_href.push('?');
        }
        back_href.push_str(&format!("sy={scroll_y}"));
    }
    if let Some(from_id) = params.from {
        back_href.push_str(&format!("#item-{from_id}"));
    }
    let tag_nav = IndexNav {
        query: String::new(),
        source_url: None,
        show_sensitive,
        randomize,
        seed,
        limit,
        page: 1,
    };
    let author = item
        .merged_author()
        .unwrap_or_else(|| "(unknown)".to_string());

    let original_json =
        serde_json::to_string_pretty(&item.original).unwrap_or_else(|_| "{}".to_string());
    let edits_json = serde_json::to_string_pretty(&item.edits).unwrap_or_else(|_| "{}".to_string());
    let platform_url = item.platform_url();
    let source_search_href = platform_url
        .as_deref()
        .and_then(|source| build_source_search_href(source, &tag_nav));

    HtmlTemplate(ItemTemplate {
        id,
        back_href,
        title: infer_title(item),
        author: author.clone(),
        author_href: build_author_search_href(&author, &tag_nav),
        date: item
            .merged_date()
            .unwrap_or_else(|| "(unknown)".to_string()),
        detail: item
            .merged_detail()
            .unwrap_or_else(|| "(no description)".to_string()),
        sensitive: item.merged_sensitive(),
        platform_url,
        source_search_href,
        tags: item
            .merged_tags()
            .into_iter()
            .map(|tag| TagLink {
                href: build_tag_search_href(&tag, &tag_nav),
                label: tag,
            })
            .collect(),
        original_json,
        edits_json,
    })
    .into_response()
}

async fn media_handler(State(state): State<AppState>, Path(id): Path<usize>) -> impl IntoResponse {
    let Some(item) = state.library.index.items.get(id) else {
        return (StatusCode::NOT_FOUND, "item not found").into_response();
    };

    match tokio::fs::read(&item.image_path).await {
        Ok(bytes) => {
            let mime = mime_guess::from_path(&item.image_path).first_or_octet_stream();
            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(mime.as_ref())
                    .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
            );
            response
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read image: {err}"),
        )
            .into_response(),
    }
}

fn to_grid_item(id: usize, item: &booru_core::ImageItem, nav: &IndexNav) -> GridItem {
    let author = item
        .merged_author()
        .unwrap_or_else(|| "(unknown)".to_string());
    GridItem {
        id,
        detail_href: build_item_href(id, nav),
        title: infer_title(item),
        author: author.clone(),
        author_href: build_author_search_href(&author, nav),
        date: item
            .merged_date()
            .unwrap_or_else(|| "(unknown)".to_string()),
        detail: truncate_for_preview(
            &item
                .merged_detail()
                .unwrap_or_else(|| "(no description)".to_string()),
            140,
        ),
        tags: item
            .merged_tags()
            .into_iter()
            .take(8)
            .map(|tag| TagLink {
                href: build_tag_search_href(tag.as_str(), nav),
                label: tag,
            })
            .collect(),
        sensitive: item.merged_sensitive(),
    }
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

fn truncate_for_preview(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn split_search_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter(|part| !part.trim().is_empty())
        .map(|part| part.to_string())
        .collect()
}

fn parse_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[derive(Clone, Debug)]
struct IndexNav {
    query: String,
    source_url: Option<String>,
    show_sensitive: bool,
    randomize: bool,
    seed: Option<u64>,
    limit: usize,
    page: usize,
}

fn build_item_href(id: usize, nav: &IndexNav) -> String {
    let query = build_index_query_string(nav);
    if query.is_empty() {
        format!("/items/{id}?from={id}")
    } else {
        format!("/items/{id}?{query}&from={id}")
    }
}

fn build_index_href(nav: &IndexNav) -> String {
    let query = build_index_query_string(nav);
    if query.is_empty() {
        "/".to_string()
    } else {
        format!("/?{query}")
    }
}

fn build_index_query_string(nav: &IndexNav) -> String {
    let mut pairs = Vec::new();
    if !nav.query.is_empty() {
        pairs.push(format!("q={}", urlencoding::encode(&nav.query)));
    }
    if let Some(source) = nav.source_url.as_deref() {
        if !source.is_empty() {
            pairs.push(format!("source={}", urlencoding::encode(source)));
        }
    }
    if nav.show_sensitive {
        pairs.push("show_sensitive=1".to_string());
    }
    if nav.randomize {
        pairs.push("randomize=1".to_string());
    } else {
        pairs.push("randomize=0".to_string());
    }
    if nav.randomize {
        if let Some(seed) = nav.seed {
            pairs.push(format!("seed={seed}"));
        }
    }
    pairs.push(format!("limit={}", nav.limit));
    pairs.push(format!("page={}", nav.page));
    pairs.join("&")
}

fn build_tag_search_href(tag: &str, nav: &IndexNav) -> String {
    build_term_search_href(tag, nav)
}

fn build_author_search_href(author: &str, nav: &IndexNav) -> Option<String> {
    let trimmed = author.trim();
    if trimmed.is_empty() || trimmed == "(unknown)" {
        return None;
    }
    Some(build_term_search_href(trimmed, nav))
}

fn build_term_search_href(term: &str, nav: &IndexNav) -> String {
    let tag_nav = IndexNav {
        query: term.to_string(),
        source_url: None,
        show_sensitive: nav.show_sensitive,
        randomize: nav.randomize,
        seed: nav.seed,
        limit: nav.limit,
        page: 1,
    };
    build_index_href(&tag_nav)
}

fn build_source_search_href(source: &str, nav: &IndexNav) -> Option<String> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return None;
    }
    let source_nav = IndexNav {
        query: String::new(),
        source_url: Some(trimmed.to_string()),
        show_sensitive: nav.show_sensitive,
        randomize: false,
        seed: None,
        limit: nav.limit,
        page: 1,
    };
    Some(build_index_href(&source_nav))
}

fn generate_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ 0x9e37_79b9_7f4a_7c15
}

fn next_seed(seed: u64) -> u64 {
    seed.wrapping_mul(6364136223846793005).wrapping_add(1)
}
