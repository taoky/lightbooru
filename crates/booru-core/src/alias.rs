use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

pub const ALIAS_FILE_NAME: &str = "alias.json";

pub type AliasMap = HashMap<String, Vec<String>>;
pub type AliasGroups = Vec<Vec<String>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasWarning {
    pub path: PathBuf,
    pub message: String,
}

pub fn alias_path_for_root(root: &Path) -> PathBuf {
    root.join(ALIAS_FILE_NAME)
}

pub fn normalize_search_terms(terms: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for term in terms {
        let Some(normalized) = normalize_search_term(&term) else {
            continue;
        };
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

pub fn normalize_search_term(term: &str) -> Option<String> {
    let trimmed = term.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_lowercase())
    }
}

pub fn normalize_alias_groups(groups: AliasGroups) -> AliasGroups {
    let mut graph = HashMap::<String, HashSet<String>>::new();

    for group in groups {
        let terms = normalize_search_terms(group);
        if terms.len() < 2 {
            continue;
        }
        for term in &terms {
            graph.entry(term.clone()).or_default();
        }
        let anchor = terms[0].clone();
        for term in terms.iter().skip(1) {
            graph
                .entry(anchor.clone())
                .or_default()
                .insert(term.clone());
            graph
                .entry(term.clone())
                .or_default()
                .insert(anchor.clone());
        }
    }

    let mut groups_out = Vec::new();
    let mut seen = HashSet::new();
    for term in graph.keys() {
        if seen.contains(term) {
            continue;
        }
        let mut queue = VecDeque::new();
        let mut component = Vec::new();
        seen.insert(term.clone());
        queue.push_back(term.clone());

        while let Some(cur) = queue.pop_front() {
            component.push(cur.clone());
            if let Some(neighbors) = graph.get(&cur) {
                for next in neighbors {
                    if seen.insert(next.clone()) {
                        queue.push_back(next.clone());
                    }
                }
            }
        }

        component.sort();
        component.dedup();
        if component.len() >= 2 {
            groups_out.push(component);
        }
    }

    groups_out.sort_by(|a, b| {
        let ka = a.first().map(|s| s.as_str()).unwrap_or("");
        let kb = b.first().map(|s| s.as_str()).unwrap_or("");
        ka.cmp(kb).then_with(|| a.len().cmp(&b.len()))
    });
    groups_out
}

pub fn alias_map_from_groups(groups: &AliasGroups) -> AliasMap {
    let mut out = AliasMap::new();
    for group in normalize_alias_groups(groups.clone()) {
        for term in &group {
            let entry = out.entry(term.clone()).or_default();
            for alias in &group {
                if alias != term && !entry.contains(alias) {
                    entry.push(alias.clone());
                }
            }
        }
    }
    out
}

pub fn load_alias_groups_from_path(path: &Path) -> Result<AliasGroups, String> {
    let bytes = fs::read(path).map_err(|err| format!("failed to read alias file: {err}"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|err| format!("failed to parse alias json: {err}"))?;
    parse_alias_groups(&value).map(normalize_alias_groups)
}

pub fn load_alias_groups_from_root(root: &Path) -> Result<AliasGroups, String> {
    let path = alias_path_for_root(root);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    load_alias_groups_from_path(&path)
}

pub fn save_alias_groups_to_path(path: &Path, groups: &AliasGroups) -> Result<(), String> {
    let normalized = normalize_alias_groups(groups.clone());
    let bytes = serde_json::to_vec_pretty(&normalized)
        .map_err(|err| format!("failed to serialize alias json: {err}"))?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create alias directory: {err}"))?;
    }
    fs::write(path, bytes).map_err(|err| format!("failed to write alias file: {err}"))?;
    Ok(())
}

pub fn save_alias_groups_to_root(root: &Path, groups: &AliasGroups) -> Result<(), String> {
    let path = alias_path_for_root(root);
    save_alias_groups_to_path(&path, groups)
}

pub fn merge_alias_terms(groups: &mut AliasGroups, terms: Vec<String>) -> bool {
    let mut current = normalize_alias_groups(std::mem::take(groups));
    let before = current.clone();

    let incoming = normalize_search_terms(terms);
    if incoming.len() < 2 {
        *groups = current;
        return false;
    }

    let incoming_set: HashSet<String> = incoming.iter().cloned().collect();
    let mut merged_terms = incoming;
    let mut kept = Vec::new();
    for group in current.drain(..) {
        if group.iter().any(|term| incoming_set.contains(term)) {
            merged_terms.extend(group);
        } else {
            kept.push(group);
        }
    }
    kept.push(merged_terms);

    *groups = normalize_alias_groups(kept);
    *groups != before
}

pub fn remove_alias_terms(groups: &mut AliasGroups, terms: Vec<String>) -> bool {
    let mut current = normalize_alias_groups(std::mem::take(groups));
    let before = current.clone();

    let remove_set: HashSet<String> = normalize_search_terms(terms).into_iter().collect();
    if remove_set.is_empty() {
        *groups = current;
        return false;
    }

    for group in &mut current {
        group.retain(|term| !remove_set.contains(term));
    }
    current.retain(|group| group.len() >= 2);

    *groups = normalize_alias_groups(current);
    *groups != before
}

pub fn load_alias_map_from_roots(roots: &[PathBuf]) -> (AliasMap, Vec<AliasWarning>) {
    let mut all_aliases = AliasMap::new();
    let mut warnings = Vec::new();

    for root in roots {
        let path = alias_path_for_root(root);
        if !path.is_file() {
            continue;
        }

        match load_alias_groups_from_path(&path) {
            Ok(groups) => {
                let parsed = alias_map_from_groups(&groups);
                merge_alias_map(&mut all_aliases, parsed);
            }
            Err(err) => warnings.push(AliasWarning {
                path: path.clone(),
                message: err,
            }),
        }
    }

    (all_aliases, warnings)
}

pub fn expand_search_terms_with_aliases(terms: Vec<String>, alias_map: &AliasMap) -> Vec<String> {
    if terms.is_empty() {
        return Vec::new();
    }

    let mut graph = HashMap::<String, HashSet<String>>::new();
    for (term, aliases) in alias_map {
        for alias in aliases {
            graph.entry(term.clone()).or_default().insert(alias.clone());
            graph.entry(alias.clone()).or_default().insert(term.clone());
        }
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    for term in terms {
        if seen.insert(term.clone()) {
            out.push(term.clone());
            queue.push_back(term);
        }
    }

    while let Some(term) = queue.pop_front() {
        let Some(aliases) = graph.get(&term) else {
            continue;
        };
        for alias in aliases {
            if seen.insert(alias.clone()) {
                out.push(alias.clone());
                queue.push_back(alias.clone());
            }
        }
    }

    out
}

fn parse_alias_groups(value: &Value) -> Result<AliasGroups, String> {
    let groups = value
        .as_array()
        .ok_or_else(|| "root value must be an array".to_string())?;

    let mut out = Vec::new();
    for (idx, group) in groups.iter().enumerate() {
        let terms =
            parse_alias_group(group).map_err(|err| format!("group at index {idx}: {err}"))?;
        out.push(terms);
    }
    Ok(out)
}

fn parse_alias_group(value: &Value) -> Result<Vec<String>, String> {
    let arr = value
        .as_array()
        .ok_or_else(|| "group must be an array".to_string())?;
    let mut out = Vec::new();
    for item in arr {
        let raw = item
            .as_str()
            .ok_or_else(|| "group contains non-string value".to_string())?;
        if let Some(term) = normalize_search_term(raw) {
            if !out.contains(&term) {
                out.push(term);
            }
        }
    }
    Ok(out)
}

fn merge_alias_map(dst: &mut AliasMap, src: AliasMap) {
    for (term, aliases) in src {
        let entry = dst.entry(term).or_default();
        for alias in aliases {
            if !entry.contains(&alias) {
                entry.push(alias);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        alias_map_from_groups, expand_search_terms_with_aliases, merge_alias_terms,
        normalize_alias_groups, normalize_search_terms, parse_alias_groups, remove_alias_terms,
        AliasMap,
    };

    #[test]
    fn alias_groups_parse_array_of_alias_groups() {
        let groups = parse_alias_groups(&json!([["摇曳露营", "ゆるキャン", "yurucamp"]]))
            .expect("should parse");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 3);
    }

    #[test]
    fn alias_groups_reject_object_format() {
        let err = parse_alias_groups(&json!({
            "摇曳露营": ["ゆるキャン", "yurucamp"]
        }))
        .expect_err("object format should be rejected");
        assert!(err.contains("root value must be an array"));
    }

    #[test]
    fn normalize_alias_groups_merges_connected_groups() {
        let groups = normalize_alias_groups(vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["b".to_string(), "c".to_string()],
        ]);
        assert_eq!(
            groups,
            vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]]
        );
    }

    #[test]
    fn alias_map_from_groups_is_bidirectional() {
        let map = alias_map_from_groups(&vec![vec![
            "摇曳露营".to_string(),
            "ゆるキャン".to_string(),
            "yurucamp".to_string(),
        ]]);
        assert_eq!(
            map.get("摇曳露营").cloned(),
            Some(vec!["yurucamp".to_string(), "ゆるキャン".to_string()])
        );
    }

    #[test]
    fn merge_alias_terms_merges_overlapping_groups() {
        let mut groups = vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["x".to_string(), "y".to_string()],
        ];
        assert!(merge_alias_terms(
            &mut groups,
            vec!["b".to_string(), "x".to_string(), "z".to_string()]
        ));
        assert_eq!(
            groups,
            vec![vec![
                "a".to_string(),
                "b".to_string(),
                "x".to_string(),
                "y".to_string(),
                "z".to_string()
            ]]
        );
    }

    #[test]
    fn remove_alias_terms_drops_small_groups() {
        let mut groups = vec![vec!["a".to_string(), "b".to_string(), "c".to_string()]];
        assert!(remove_alias_terms(
            &mut groups,
            vec!["a".to_string(), "b".to_string()]
        ));
        assert!(groups.is_empty());
    }

    #[test]
    fn search_terms_expand_with_aliases_bidirectionally() {
        let mut alias = AliasMap::new();
        alias.insert(
            "摇曳露营".to_string(),
            vec!["ゆるキャン".to_string(), "yurucamp".to_string()],
        );

        let expanded = expand_search_terms_with_aliases(vec!["yurucamp".to_string()], &alias);
        assert!(expanded.contains(&"摇曳露营".to_string()));
        assert!(expanded.contains(&"ゆるキャン".to_string()));
        assert!(expanded.contains(&"yurucamp".to_string()));
    }

    #[test]
    fn search_terms_normalize_to_lowercase_and_dedup() {
        let terms = normalize_search_terms(vec![
            "  YuruCamp ".to_string(),
            "yurucamp".to_string(),
            "".to_string(),
        ]);
        assert_eq!(terms, vec!["yurucamp".to_string()]);
    }
}
