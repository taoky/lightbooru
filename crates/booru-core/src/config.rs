use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct BooruConfig {
    pub roots: Vec<PathBuf>,
}

impl BooruConfig {
    pub fn default() -> Self {
        let root = default_root();
        Self { roots: vec![root] }
    }

    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        let expanded = roots
            .into_iter()
            .map(|p| expand_tilde(&p))
            .collect();
        Self { roots: expanded }
    }
}

pub fn default_root() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        return home.join("Pictures").join("gallery-dl");
    }
    PathBuf::from("./gallery-dl")
}

pub fn expand_tilde(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();
    if path_str == "~" || path_str.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            let suffix = path_str.trim_start_matches('~');
            return home.join(suffix.trim_start_matches('/'));
        }
    }
    path.to_path_buf()
}
