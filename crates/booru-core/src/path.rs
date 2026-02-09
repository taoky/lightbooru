use std::fs;
use std::path::{Path, PathBuf};

use crate::config::expand_tilde;

pub fn normalize_image_path(path: &Path) -> PathBuf {
    let path = expand_tilde(path);
    if let Some(file_name) = path.file_name().and_then(|s| s.to_str()) {
        if let Some(stripped) = file_name.strip_suffix(".booru.json") {
            return path.with_file_name(stripped);
        }
        if let Some(stripped) = file_name.strip_suffix(".json") {
            return path.with_file_name(stripped);
        }
    }
    path
}

pub fn resolve_image_path(input: &Path, roots: &[PathBuf]) -> PathBuf {
    let normalized = normalize_image_path(input);
    if normalized.is_absolute() {
        return canonicalize_or_self(&normalized);
    }
    for root in roots {
        let candidate = root.join(&normalized);
        if candidate.exists() {
            return canonicalize_or_self(&candidate);
        }
    }
    canonicalize_or_self(&normalized)
}

pub fn metadata_path_for_image(image_path: &Path) -> PathBuf {
    match image_path.extension().and_then(|s| s.to_str()) {
        Some(ext) if !ext.is_empty() => image_path.with_extension(format!("{ext}.json")),
        _ => image_path.with_extension("json"),
    }
}

pub fn booru_path_for_image(image_path: &Path) -> PathBuf {
    let file_name = image_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let new_name = format!("{}.booru.json", file_name);
    image_path.with_file_name(new_name)
}

fn canonicalize_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
