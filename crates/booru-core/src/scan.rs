use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use walkdir::WalkDir;

use crate::config::BooruConfig;
use crate::error::BooruError;
use crate::metadata::{extract_string_field, extract_tags, BooruEdits};
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

    pub fn merged_rating(&self) -> Option<String> {
        if let Some(rating) = &self.edits.rating {
            return Some(rating.clone());
        }
        extract_string_field(&self.original, &["rating", "rating_string"])
    }

    pub fn merged_source(&self) -> Option<String> {
        if let Some(source) = &self.edits.source {
            return Some(source.clone());
        }
        extract_string_field(&self.original, &["source", "source_url", "file_url"])
    }
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
