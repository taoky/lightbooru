use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::BooruError;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TagEdits {
    pub set: Option<Vec<String>>,
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BooruEdits {
    pub tags: TagEdits,
    pub notes: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Clone, Debug, Default)]
pub struct EditUpdate {
    pub set_tags: Option<Vec<String>>,
    pub add_tags: Vec<String>,
    pub remove_tags: Vec<String>,
    pub clear_tags: bool,
    pub notes: Option<String>,
}

impl BooruEdits {
    pub fn load(path: &Path) -> Result<Option<Self>, BooruError> {
        match fs::read(path) {
            Ok(data) => {
                let edits = serde_json::from_slice(&data).map_err(|source| BooruError::Json {
                    path: path.to_path_buf(),
                    source,
                })?;
                Ok(Some(edits))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(BooruError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), BooruError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| BooruError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let data = serde_json::to_vec_pretty(self).map_err(|source| BooruError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        fs::write(path, data).map_err(|source| BooruError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn apply_update(&mut self, update: EditUpdate) {
        if update.clear_tags {
            self.tags.set = Some(Vec::new());
            self.tags.add.clear();
            self.tags.remove.clear();
        }

        if let Some(set_tags) = update.set_tags {
            self.tags.set = Some(normalize_tags(set_tags));
            self.tags.add.clear();
            self.tags.remove.clear();
        }

        if !update.add_tags.is_empty() || !update.remove_tags.is_empty() {
            let add_tags = normalize_tags(update.add_tags);
            let remove_tags = normalize_tags(update.remove_tags);

            match &mut self.tags.set {
                Some(current) => {
                    let mut set = to_ordered_set(current.clone());
                    for tag in add_tags {
                        if set.insert(tag.clone()) {
                            current.push(tag);
                        }
                    }
                    if !remove_tags.is_empty() {
                        let remove_set: HashSet<String> = remove_tags.iter().cloned().collect();
                        current.retain(|tag| !remove_set.contains(tag));
                    }
                }
                None => {
                    self.tags.add = merge_tag_list(self.tags.add.clone(), add_tags);
                    self.tags.remove = merge_tag_list(self.tags.remove.clone(), remove_tags);
                    let remove_set: HashSet<String> = self.tags.remove.iter().cloned().collect();
                    self.tags.add.retain(|tag| !remove_set.contains(tag));
                }
            }
        }

        if let Some(notes) = update.notes {
            self.notes = Some(notes);
        }
    }

    pub fn merged_tags(&self, original_tags: &[String]) -> Vec<String> {
        if let Some(set) = &self.tags.set {
            return normalize_tags(set.clone());
        }

        let mut tags = normalize_tags(original_tags.to_vec());
        let remove_set: HashSet<String> = self.tags.remove.iter().cloned().collect();
        tags.retain(|tag| !remove_set.contains(tag));
        for tag in &self.tags.add {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
        tags
    }
}

pub fn extract_tags(value: &Value) -> Vec<String> {
    let mut tags = Vec::new();
    let mut seen = HashSet::new();

    let Some(obj) = value.as_object() else {
        return tags;
    };

    let keys = [
        "tags",
        "hashtags",
        "tag_string",
        "tag_string_general",
        "tag_string_character",
        "tag_string_artist",
        "tag_string_copyright",
        "tag_string_meta",
        "tag_string_other",
        "keywords",
    ];

    for key in keys {
        if let Some(v) = obj.get(key) {
            collect_tags(v, &mut tags, &mut seen);
        }
    }

    tags
}

pub fn extract_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    for key in keys {
        if let Some(Value::String(s)) = obj.get(*key) {
            if !s.trim().is_empty() {
                return Some(s.clone());
            }
        }
    }
    None
}

pub fn extract_scalar_field(value: &Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    for key in keys {
        if let Some(v) = obj.get(*key) {
            if let Some(s) = nonempty_scalar_string(v) {
                return Some(s);
            }
        }
    }
    None
}

pub fn extract_nested_scalar_field(value: &Value, paths: &[&[&str]]) -> Option<String> {
    for path in paths {
        if let Some(v) = get_nested_value(value, path) {
            if let Some(s) = nonempty_scalar_string(v) {
                return Some(s);
            }
        }
    }
    None
}

pub fn extract_bool_field(value: &Value, keys: &[&str]) -> Option<bool> {
    let obj = value.as_object()?;
    for key in keys {
        if let Some(v) = obj.get(*key) {
            if let Some(flag) = nonempty_bool(v) {
                return Some(flag);
            }
        }
    }
    None
}

fn collect_tags(value: &Value, tags: &mut Vec<String>, seen: &mut HashSet<String>) {
    match value {
        Value::String(s) => {
            for tag in split_tag_string(s) {
                push_tag(&tag, tags, seen);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                match item {
                    Value::String(s) => push_tag(s, tags, seen),
                    Value::Object(obj) => {
                        if let Some(Value::String(name)) = obj.get("name") {
                            push_tag(name, tags, seen);
                        }
                        if let Some(Value::String(tag)) = obj.get("tag") {
                            push_tag(tag, tags, seen);
                        }
                        if let Some(Value::String(text)) = obj.get("text") {
                            push_tag(text, tags, seen);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn get_nested_value<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(value);
    }

    let mut current = value;
    for key in path {
        let obj = current.as_object()?;
        current = obj.get(*key)?;
    }
    Some(current)
}

fn nonempty_scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn nonempty_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|x| x != 0),
        Value::String(s) => parse_bool_string(s),
        _ => None,
    }
}

fn parse_bool_string(s: &str) -> Option<bool> {
    let normalized = s.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

fn split_tag_string(input: &str) -> Vec<String> {
    if input.contains(',') {
        input
            .split(',')
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect()
    } else {
        input
            .split_whitespace()
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty())
            .collect()
    }
}

fn push_tag(tag: &str, tags: &mut Vec<String>, seen: &mut HashSet<String>) {
    let tag = tag.trim();
    if tag.is_empty() {
        return;
    }
    if seen.insert(tag.to_string()) {
        tags.push(tag.to_string());
    }
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() {
            continue;
        }
        if seen.insert(tag.to_string()) {
            normalized.push(tag.to_string());
        }
    }
    normalized
}

fn merge_tag_list(mut current: Vec<String>, incoming: Vec<String>) -> Vec<String> {
    let mut seen = to_ordered_set(current.clone());
    for tag in incoming {
        if seen.insert(tag.clone()) {
            current.push(tag);
        }
    }
    current
}

fn to_ordered_set(tags: Vec<String>) -> HashSet<String> {
    let mut seen = HashSet::new();
    for tag in tags {
        seen.insert(tag);
    }
    seen
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        extract_bool_field, extract_nested_scalar_field, extract_scalar_field,
        extract_string_field, extract_tags,
    };

    #[test]
    fn extract_string_field_ignores_empty() {
        let value = json!({
            "a": "",
            "b": "  hello  ",
        });
        assert_eq!(
            extract_string_field(&value, &["a", "b"]).as_deref(),
            Some("  hello  ")
        );
    }

    #[test]
    fn extract_scalar_field_supports_number() {
        let value = json!({
            "timestamp": 1700000000,
        });
        assert_eq!(
            extract_scalar_field(&value, &["date", "timestamp"]).as_deref(),
            Some("1700000000")
        );
    }

    #[test]
    fn extract_nested_scalar_field_reads_nested_author() {
        let value = json!({
            "user": {
                "name": "alice",
            }
        });
        assert_eq!(
            extract_nested_scalar_field(&value, &[&["user", "name"]]).as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn extract_bool_field_supports_bool_and_string() {
        let value = json!({
            "sensitive": true,
            "nsfw": "false",
        });
        assert_eq!(extract_bool_field(&value, &["sensitive"]), Some(true));
        assert_eq!(extract_bool_field(&value, &["nsfw"]), Some(false));
    }

    #[test]
    fn extract_tags_reads_twitter_hashtags() {
        let value = json!({
            "hashtags": [
                "理由もなく再掲していいタグ",
                {"text": "シェリハン"}
            ]
        });
        assert_eq!(
            extract_tags(&value),
            vec!["理由もなく再掲していいタグ", "シェリハン"]
        );
    }
}
