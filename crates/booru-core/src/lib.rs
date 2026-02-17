pub mod alias;
pub mod config;
pub mod edit;
pub mod error;
pub mod hash;
pub mod metadata;
pub mod path;
pub mod scan;

pub use alias::{
    alias_map_from_groups, alias_path_for_root, expand_search_terms_with_aliases,
    load_alias_groups_from_path, load_alias_groups_from_root, load_alias_map_from_roots,
    merge_alias_terms, normalize_alias_groups, normalize_search_term, normalize_search_terms,
    remove_alias_terms, save_alias_groups_to_path, save_alias_groups_to_root, AliasGroups,
    AliasMap, AliasWarning, ALIAS_FILE_NAME,
};
pub use config::BooruConfig;
pub use edit::apply_update_to_image;
pub use error::BooruError;
pub use hash::{
    compute_hashes_with_cache, find_duplicates, find_duplicates_with_cache, group_duplicates,
    DuplicateGroup, DuplicateReport, FileFingerprint, FuzzyHashAlgorithm, HashCache,
    HashComputation, ProgressObserver,
};
pub use metadata::{extract_string_field, extract_tags, BooruEdits, EditUpdate, TagEdits};
pub use path::{
    booru_path_for_image, metadata_path_for_image, normalize_image_path, resolve_image_path,
};
pub use scan::{scan_roots, ImageItem, Index, Library, ScanReport, ScanWarning};
