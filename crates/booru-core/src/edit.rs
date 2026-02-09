use std::path::Path;

use crate::error::BooruError;
use crate::metadata::{BooruEdits, EditUpdate};
use crate::path::booru_path_for_image;

pub fn apply_update_to_image(image_path: &Path, update: EditUpdate) -> Result<BooruEdits, BooruError> {
    let booru_path = booru_path_for_image(image_path);
    let mut edits = match BooruEdits::load(&booru_path)? {
        Some(existing) => existing,
        None => BooruEdits::default(),
    };
    edits.apply_update(update);
    edits.save(&booru_path)?;
    Ok(edits)
}
