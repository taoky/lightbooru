use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use crate::alias::{
    expand_search_terms_with_aliases, load_alias_map_from_roots, normalize_search_terms,
    AliasWarning, ALIAS_FILE_NAME,
};
use crate::config::BooruConfig;
use crate::error::BooruError;
use crate::metadata::{
    extract_bool_field, extract_nested_scalar_field, extract_scalar_field, extract_string_field,
    extract_tags, BooruEdits,
};
use crate::path::{booru_path_for_image, metadata_path_for_image, resolve_image_path};

#[derive(Clone, Debug)]
pub struct ImageItem {
    pub image_path: PathBuf,
    pub meta_path: PathBuf,
    pub booru_path: PathBuf,
    pub original: Value,
    pub edits: BooruEdits,
}

impl ImageItem {
    pub fn merged_tags(&self) -> Vec<String> {
        let original_tags = extract_tags(&self.original);
        self.edits.merged_tags(&original_tags)
    }

    pub fn merged_detail(&self) -> Option<String> {
        let category = extract_string_field(&self.original, &["category"]);

        if category.as_deref() == Some("bilibili") {
            if let Some(detail) = bilibili_detail_text(&self.original) {
                return Some(detail);
            }
        }

        for key in [
            "detail",
            "text_raw",
            "text",
            "content",
            "body",
            "caption",
            "description",
            "summary",
            "title",
            "spoiler_text",
        ] {
            if let Some(detail) = extract_string_field(&self.original, &[key]) {
                if let Some(sanitized) = sanitize_detail_for_category(category.as_deref(), detail) {
                    return Some(sanitized);
                }
            }
        }

        for path in [
            &["status", "text_raw"][..],
            &["status", "text"][..],
            &["status", "longTextContent_raw"][..],
            &["status", "longTextContent"][..],
        ] {
            if let Some(detail) = extract_nested_scalar_field(&self.original, &[path]) {
                if let Some(sanitized) = sanitize_detail_for_category(category.as_deref(), detail) {
                    return Some(sanitized);
                }
            }
        }

        None
    }

    pub fn merged_author(&self) -> Option<String> {
        if let Some(author) = extract_string_field(
            &self.original,
            &["author", "username", "blog_name", "tag_string_artist"],
        ) {
            return Some(author);
        }

        extract_nested_scalar_field(
            &self.original,
            &[
                &["user", "name"],
                &["user", "username"],
                &["user", "screen_name"],
                &["user", "id"],
                &["status", "user", "name"],
                &["status", "user", "username"],
                &["status", "user", "screen_name"],
                &["status", "user", "idstr"],
                &["status", "user", "id"],
                &["account", "display_name"],
                &["account", "username"],
                &["account", "acct"],
                &["blog", "name"],
                &["blog", "title"],
            ],
        )
        .or_else(|| extract_first_array_string_field(&self.original, &["tags_artist"]))
    }

    pub fn merged_date(&self) -> Option<String> {
        extract_scalar_field(
            &self.original,
            &[
                "date",
                "created_at",
                "create_date",
                "published_at",
                "timestamp",
            ],
        )
        .or_else(|| {
            extract_nested_scalar_field(
                &self.original,
                &[&["detail", "modules", "module_author", "pub_ts"]],
            )
        })
        .or_else(|| {
            extract_nested_scalar_field(
                &self.original,
                &[&["status", "date"], &["status", "created_at"]],
            )
        })
    }

    pub fn merged_sensitive(&self) -> bool {
        if let Some(sensitive) = self.edits.sensitive {
            return sensitive;
        }

        if let Some(flag) = extract_bool_field(
            &self.original,
            &["sensitive", "nsfw", "is_sensitive", "is_nsfw"],
        ) {
            return flag;
        }

        if let Some(value) = extract_scalar_field(
            &self.original,
            &["sensitive", "nsfw", "is_sensitive", "is_nsfw"],
        ) {
            if let Some(flag) = sensitive_value_to_bool(&value) {
                return flag;
            }
        }
        false
    }

    pub fn platform_url(&self) -> Option<String> {
        let category = extract_string_field(&self.original, &["category"])?;
        match category.as_str() {
            "twitter" => twitter_status_url(&self.original),
            "weibo" => weibo_status_url(&self.original),
            "pixiv" => pixiv_artwork_url(&self.original),
            "danbooru" => danbooru_post_url(&self.original),
            "yandere" => yandere_post_url(&self.original),
            "tumblr" => extract_string_field(&self.original, &["post_url", "short_url"]),
            "mastodon" => extract_string_field(&self.original, &["uri", "url"]),
            "bilibili" => bilibili_space_url(&self.original),
            _ => extract_string_field(
                &self.original,
                &["post_url", "uri", "source_url", "url", "live_url"],
            ),
        }
    }
}

fn sensitive_value_to_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sensitive" | "nsfw" | "adult" | "explicit" | "questionable" | "r18" | "mature" => {
            Some(true)
        }
        "safe" | "sfw" | "general" => Some(false),
        _ => None,
    }
}

fn extract_first_array_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    for key in keys {
        let Some(arr) = obj.get(*key).and_then(|v| v.as_array()) else {
            continue;
        };
        for item in arr {
            match item {
                Value::String(s) => {
                    let s = s.trim();
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
                Value::Object(map) => {
                    for field in ["name", "tag", "text"] {
                        if let Some(Value::String(s)) = map.get(field) {
                            let s = s.trim();
                            if !s.is_empty() {
                                return Some(s.to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn twitter_status_url(value: &Value) -> Option<String> {
    let tweet_id = extract_scalar_field(value, &["tweet_id", "id"])?;
    if let Some(author) = extract_string_field(value, &["author"]).or_else(|| {
        extract_nested_scalar_field(
            value,
            &[
                &["author", "name"],
                &["author", "username"],
                &["author", "screen_name"],
                &["user", "name"],
                &["user", "username"],
                &["user", "screen_name"],
            ],
        )
    }) {
        let handle = author.trim_start_matches('@');
        if !handle.is_empty() {
            return Some(format!("https://x.com/{handle}/status/{tweet_id}"));
        }
    }
    Some(format!("https://x.com/i/status/{tweet_id}"))
}

fn weibo_status_url(value: &Value) -> Option<String> {
    if let Some(url) = extract_nested_scalar_field(value, &[&["status", "url"]]) {
        return Some(url);
    }

    let mblogid = extract_nested_scalar_field(value, &[&["status", "mblogid"]])?;
    if let Some(uid) = extract_nested_scalar_field(value, &[&["status", "user", "idstr"]]) {
        return Some(format!("https://weibo.com/{uid}/{mblogid}"));
    }
    Some(format!("https://weibo.com/n/{mblogid}"))
}

fn pixiv_artwork_url(value: &Value) -> Option<String> {
    let id = extract_scalar_field(value, &["id"])?;
    Some(format!("https://www.pixiv.net/artworks/{id}"))
}

fn danbooru_post_url(value: &Value) -> Option<String> {
    let id = extract_scalar_field(value, &["id"])?;
    Some(format!("https://danbooru.donmai.us/posts/{id}"))
}

fn yandere_post_url(value: &Value) -> Option<String> {
    let id = extract_scalar_field(value, &["id"])?;
    Some(format!("https://yande.re/post/show/{id}"))
}

fn bilibili_space_url(value: &Value) -> Option<String> {
    let opus_id = extract_nested_scalar_field(value, &[&["detail", "id_str"], &["id"]])?;
    Some(format!("https://www.bilibili.com/opus/{opus_id}"))
}

fn bilibili_detail_text(value: &Value) -> Option<String> {
    let paragraphs = value
        .pointer("/detail/modules/module_content/paragraphs")
        .and_then(Value::as_array)?;

    let mut out = String::new();
    for paragraph in paragraphs {
        let Some(nodes) = paragraph
            .get("text")
            .and_then(|v| v.get("nodes"))
            .and_then(Value::as_array)
        else {
            continue;
        };

        for node in nodes {
            if let Some(words) = node
                .get("word")
                .and_then(|v| v.get("words"))
                .and_then(Value::as_str)
            {
                out.push_str(words);
                continue;
            }

            if let Some(rich) = node.get("rich") {
                if let Some(text) = rich.get("orig_text").and_then(Value::as_str) {
                    out.push_str(text);
                    continue;
                }
                if let Some(text) = rich.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
        }
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn sanitize_detail_for_category(category: Option<&str>, detail: String) -> Option<String> {
    match category {
        Some(name) if name.eq_ignore_ascii_case("tumblr") => sanitize_tumblr_detail(&detail),
        _ => Some(detail),
    }
}

fn sanitize_tumblr_detail(detail: &str) -> Option<String> {
    if !detail.contains('<') {
        let trimmed = detail.trim();
        return if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }

    let mut out = String::new();
    let mut chars = detail.chars().peekable();
    let mut in_tag = false;
    let mut tag_buf = String::new();

    while let Some(ch) = chars.next() {
        if in_tag {
            if ch == '>' {
                if should_insert_line_break_for_tag(&tag_buf) {
                    push_line_break(&mut out);
                }
                tag_buf.clear();
                in_tag = false;
            } else {
                tag_buf.push(ch);
            }
            continue;
        }

        if ch == '<' {
            let is_tag_start = matches!(
                chars.peek(),
                Some(next) if next.is_ascii_alphabetic() || *next == '/' || *next == '!' || *next == '?'
            );
            if is_tag_start {
                in_tag = true;
                continue;
            }
        }

        out.push(ch);
    }

    if in_tag {
        out.push('<');
        out.push_str(&tag_buf);
    }

    let normalized = out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn should_insert_line_break_for_tag(raw_tag: &str) -> bool {
    let tag = raw_tag.trim_start();
    let tag = match tag.strip_prefix('/') {
        Some(stripped) => stripped.trim_start(),
        None => tag,
    };

    let name_end = tag
        .find(|ch: char| ch.is_ascii_whitespace() || ch == '/')
        .unwrap_or(tag.len());
    if name_end == 0 {
        return false;
    }

    matches!(
        &tag[..name_end].to_ascii_lowercase()[..],
        "br" | "p" | "div" | "figure" | "figcaption" | "li" | "tr"
    )
}

fn push_line_break(out: &mut String) {
    if out.is_empty() || out.ends_with('\n') {
        return;
    }
    out.push('\n');
}

#[derive(Debug)]
pub struct ScanWarning {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug)]
pub struct ScanReport {
    pub index: Index,
    pub warnings: Vec<ScanWarning>,
}

#[derive(Debug, Default)]
pub struct Index {
    pub items: Vec<ImageItem>,
    by_path: HashMap<PathBuf, usize>,
}

impl Index {
    pub fn get_by_path(&self, path: &Path) -> Option<&ImageItem> {
        self.by_path.get(path).and_then(|idx| self.items.get(*idx))
    }

    pub fn iter(&self) -> impl Iterator<Item = &ImageItem> {
        self.items.iter()
    }

    pub fn search_by_tags_all(&self, tags: &[String]) -> Vec<&ImageItem> {
        let mut results = Vec::new();
        for item in &self.items {
            let merged = item.merged_tags();
            let mut ok = true;
            for tag in tags {
                if !merged.contains(tag) {
                    ok = false;
                    break;
                }
            }
            if ok {
                results.push(item);
            }
        }
        results
    }
}

pub struct Library {
    pub config: BooruConfig,
    pub index: Index,
    pub warnings: Vec<ScanWarning>,
}

#[derive(Clone, Debug, Default)]
pub struct SearchQuery {
    pub terms: Vec<String>,
    pub use_aliases: bool,
}

impl SearchQuery {
    pub fn new(terms: Vec<String>) -> Self {
        Self {
            terms,
            use_aliases: false,
        }
    }

    pub fn with_aliases(mut self, use_aliases: bool) -> Self {
        self.use_aliases = use_aliases;
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct SearchResult {
    pub normalized_terms: Vec<String>,
    pub expanded_terms: Vec<String>,
    pub indices: Vec<usize>,
    pub alias_warnings: Vec<AliasWarning>,
}

impl Library {
    pub fn scan(config: BooruConfig) -> Result<Self, BooruError> {
        let report = scan_roots(&config.roots)?;
        Ok(Self {
            config,
            index: report.index,
            warnings: report.warnings,
        })
    }

    pub fn resolve_image_path(&self, input: &Path) -> PathBuf {
        resolve_image_path(input, &self.config.roots)
    }

    pub fn search(&self, query: SearchQuery) -> SearchResult {
        let normalized_terms = normalize_search_terms(query.terms);

        let (expanded_terms, alias_warnings) = if query.use_aliases {
            let (alias_map, warnings) = load_alias_map_from_roots(&self.config.roots);
            (
                expand_search_terms_with_aliases(normalized_terms.clone(), &alias_map),
                warnings,
            )
        } else {
            (normalized_terms.clone(), Vec::new())
        };

        let indices = self
            .index
            .items
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| {
                item_matches_search_terms(item, &expanded_terms).then_some(idx)
            })
            .collect();

        SearchResult {
            normalized_terms,
            expanded_terms,
            indices,
            alias_warnings,
        }
    }
}

pub fn item_matches_search_terms(item: &ImageItem, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }

    let tags = item
        .merged_tags()
        .into_iter()
        .map(|tag| tag.to_lowercase())
        .collect::<Vec<_>>();
    let author = item.merged_author().map(|author| author.to_lowercase());
    let detail = item.merged_detail().map(|detail| detail.to_lowercase());

    terms.iter().any(|term| {
        let needle = term.to_lowercase();
        tags.iter().any(|tag| tag.contains(&needle))
            || author
                .as_ref()
                .map(|author| author.contains(&needle))
                .unwrap_or(false)
            || detail
                .as_ref()
                .map(|detail| detail.contains(&needle))
                .unwrap_or(false)
    })
}

pub fn scan_roots(roots: &[PathBuf]) -> Result<ScanReport, BooruError> {
    let mut index = Index::default();
    let mut warnings = Vec::new();

    for root in roots {
        if !root.exists() {
            warnings.push(ScanWarning {
                path: root.clone(),
                message: "root does not exist".to_string(),
            });
            continue;
        }

        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if file_name == ALIAS_FILE_NAME {
                continue;
            }
            if !file_name.ends_with(".json") || file_name.ends_with(".booru.json") {
                continue;
            }

            let image_path = path.with_extension("");
            if !image_path.exists() {
                warnings.push(ScanWarning {
                    path: image_path.clone(),
                    message: "missing image for metadata".to_string(),
                });
                continue;
            }

            let original = match read_json(path) {
                Ok(value) => value,
                Err(err) => {
                    warnings.push(ScanWarning {
                        path: path.to_path_buf(),
                        message: format!("{err}"),
                    });
                    continue;
                }
            };

            let booru_path = booru_path_for_image(&image_path);
            let edits = match BooruEdits::load(&booru_path) {
                Ok(Some(edits)) => edits,
                Ok(None) => BooruEdits::default(),
                Err(err) => {
                    warnings.push(ScanWarning {
                        path: booru_path.clone(),
                        message: format!("failed to parse booru edits: {err}"),
                    });
                    BooruEdits::default()
                }
            };

            let image_path = fs::canonicalize(&image_path).unwrap_or(image_path);
            let meta_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let booru_path = fs::canonicalize(&booru_path).unwrap_or(booru_path);

            let item = ImageItem {
                image_path: image_path.clone(),
                meta_path,
                booru_path,
                original,
                edits,
            };

            let idx = index.items.len();
            index.by_path.insert(image_path, idx);
            index.items.push(item);
        }
    }

    Ok(ScanReport { index, warnings })
}

pub fn load_item_for_image(image_path: &Path) -> Result<ImageItem, BooruError> {
    let meta_path = metadata_path_for_image(image_path);
    let original = read_json(&meta_path)?;

    let booru_path = booru_path_for_image(image_path);
    let edits = match BooruEdits::load(&booru_path)? {
        Some(edits) => edits,
        None => BooruEdits::default(),
    };

    Ok(ImageItem {
        image_path: image_path.to_path_buf(),
        meta_path,
        booru_path,
        original,
        edits,
    })
}

fn read_json(path: &Path) -> Result<Value, BooruError> {
    let data = fs::read(path).map_err(|source| BooruError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&data).map_err(|source| BooruError::Json {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::{scan_roots, ImageItem, Index, Library, SearchQuery};
    use crate::config::BooruConfig;
    use crate::metadata::BooruEdits;

    fn make_item(original: serde_json::Value) -> ImageItem {
        ImageItem {
            image_path: PathBuf::new(),
            meta_path: PathBuf::new(),
            booru_path: PathBuf::new(),
            original,
            edits: BooruEdits::default(),
        }
    }

    #[test]
    fn merged_sensitive_parses_sensitive_keywords() {
        let nsfw_item = make_item(json!({ "sensitive": "nsfw" }));
        let sfw_item = make_item(json!({ "sensitive": "sfw" }));
        assert!(nsfw_item.merged_sensitive());
        assert!(!sfw_item.merged_sensitive());
    }

    #[test]
    fn merged_sensitive_ignores_unrelated_field() {
        let item = make_item(json!({ "score": "explicit" }));
        assert!(!item.merged_sensitive());
    }

    #[test]
    fn merged_detail_reads_weibo_status_text() {
        let item = make_item(json!({
            "status": {
                "text": "weibo text",
            }
        }));
        assert_eq!(item.merged_detail().as_deref(), Some("weibo text"));
    }

    #[test]
    fn merged_detail_strips_tumblr_html_and_images() {
        let item = make_item(json!({
            "category": "tumblr",
            "detail": "<div class=\"npf_row\"><figure class=\"tmblr-full\"><img src=\"https://example.com/image.png\"/></figure></div><p>2022.2.10</p><p>ゆるキャン△</p>"
        }));
        assert_eq!(
            item.merged_detail().as_deref(),
            Some("2022.2.10\nゆるキャン△")
        );
    }

    #[test]
    fn merged_detail_tumblr_falls_back_when_primary_is_only_images() {
        let item = make_item(json!({
            "category": "tumblr",
            "detail": "<div><img src=\"https://example.com/image.png\"/></div>",
            "summary": "summary text"
        }));
        assert_eq!(item.merged_detail().as_deref(), Some("summary text"));
    }

    #[test]
    fn platform_url_twitter_from_tweet_id_and_author() {
        let item = make_item(json!({
            "category": "twitter",
            "tweet_id": "12345",
            "author": "alice",
        }));
        assert_eq!(
            item.platform_url().as_deref(),
            Some("https://x.com/alice/status/12345")
        );
    }

    #[test]
    fn platform_url_twitter_from_nested_author_object() {
        let item = make_item(json!({
            "category": "twitter",
            "tweet_id": "12345",
            "author": { "name": "alice" },
        }));
        assert_eq!(
            item.platform_url().as_deref(),
            Some("https://x.com/alice/status/12345")
        );
    }

    #[test]
    fn platform_url_weibo_from_mblogid() {
        let item = make_item(json!({
            "category": "weibo",
            "status": {
                "mblogid": "PdVpABGap",
                "user": {
                    "idstr": "7521361627"
                }
            }
        }));
        assert_eq!(
            item.platform_url().as_deref(),
            Some("https://weibo.com/7521361627/PdVpABGap")
        );
    }

    #[test]
    fn platform_url_bilibili_uses_opus_id() {
        let item = make_item(json!({
            "category": "bilibili",
            "detail": {
                "id_str": "1156189210217021443"
            }
        }));
        assert_eq!(
            item.platform_url().as_deref(),
            Some("https://www.bilibili.com/opus/1156189210217021443")
        );
    }

    #[test]
    fn merged_detail_reads_bilibili_module_content() {
        let item = make_item(json!({
            "category": "bilibili",
            "detail": {
                "modules": {
                    "module_content": {
                        "paragraphs": [
                            {
                                "text": {
                                    "nodes": [
                                        { "word": { "words": "第一句" } },
                                        { "rich": { "orig_text": "[表情]" } },
                                        { "word": { "words": "第二句" } }
                                    ]
                                }
                            }
                        ]
                    }
                }
            }
        }));
        assert_eq!(item.merged_detail().as_deref(), Some("第一句[表情]第二句"));
    }

    #[test]
    fn merged_date_reads_bilibili_pub_ts() {
        let item = make_item(json!({
            "category": "bilibili",
            "detail": {
                "modules": {
                    "module_author": {
                        "pub_ts": 1768034678
                    }
                }
            }
        }));
        assert_eq!(item.merged_date().as_deref(), Some("1768034678"));
    }

    #[test]
    fn merged_author_reads_danbooru_tag_string_artist() {
        let item = make_item(json!({
            "category": "danbooru",
            "tag_string_artist": "myowa"
        }));
        assert_eq!(item.merged_author().as_deref(), Some("myowa"));
    }

    #[test]
    fn merged_author_reads_danbooru_tags_artist_array() {
        let item = make_item(json!({
            "category": "danbooru",
            "tags_artist": ["myowa"]
        }));
        assert_eq!(item.merged_author().as_deref(), Some("myowa"));
    }

    #[test]
    fn library_search_expands_aliases_when_enabled() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lightbooru-search-alias-on-{unique}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("alias.json"),
            "[[\"yurucamp\", \"ゆるキャン\", \"摇曳露营\"]]",
        )
        .unwrap();

        let mut index = Index::default();
        index.items.push(make_item(json!({
            "tags": ["ゆるキャン"],
        })));

        let library = Library {
            config: BooruConfig {
                roots: vec![root.clone()],
            },
            index,
            warnings: Vec::new(),
        };

        let result =
            library.search(SearchQuery::new(vec!["yurucamp".to_string()]).with_aliases(true));
        assert_eq!(result.indices, vec![0]);
        assert!(result.alias_warnings.is_empty());
        assert!(result.expanded_terms.contains(&"ゆるキャン".to_string()));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn library_search_does_not_expand_aliases_when_disabled() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lightbooru-search-alias-off-{unique}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("alias.json"),
            "[[\"yurucamp\", \"ゆるキャン\", \"摇曳露营\"]]",
        )
        .unwrap();

        let mut index = Index::default();
        index.items.push(make_item(json!({
            "tags": ["ゆるキャン"],
        })));

        let library = Library {
            config: BooruConfig {
                roots: vec![root.clone()],
            },
            index,
            warnings: Vec::new(),
        };

        let result =
            library.search(SearchQuery::new(vec!["yurucamp".to_string()]).with_aliases(false));
        assert!(result.indices.is_empty());
        assert!(result.alias_warnings.is_empty());
        assert_eq!(result.expanded_terms, vec!["yurucamp".to_string()]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_roots_ignores_alias_json() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lightbooru-scan-alias-{unique}"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("alias.json"), "[[\"a\", \"b\"]]").unwrap();

        let report = scan_roots(std::slice::from_ref(&root)).expect("scan should succeed");
        assert!(report.index.items.is_empty());
        assert!(report.warnings.is_empty());

        std::fs::remove_dir_all(root).unwrap();
    }
}
