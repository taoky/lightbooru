use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use booru_core::{
    alias_path_for_root, apply_update_to_image, compute_hashes_with_cache, group_duplicates,
    load_alias_groups_from_root, merge_alias_terms, metadata_path_for_image,
    normalize_search_terms, remove_alias_terms, resolve_image_path, save_alias_groups_to_root,
    BooruConfig, EditUpdate, FuzzyHashAlgorithm, HashCache, Library, ProgressObserver, SearchQuery,
};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::engine::{
    ArgValueCompleter, CompletionCandidate, PathCompleter, ValueCompleter,
};
use clap_complete::{generate, CompleteEnv, Shell};
use indicatif::{ProgressBar, ProgressStyle};

const COMPLETE_ENV_VAR: &str = "BOORUCTL_COMPLETE";

#[derive(Parser)]
#[command(name = "booructl", version, about = "CLI tools for LightBooru")]
struct Cli {
    /// Base directory for gallery-dl downloads (can be repeated)
    #[arg(long, short)]
    base: Vec<PathBuf>,

    /// Suppress scan warnings
    #[arg(long)]
    quiet: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show merged metadata for an image
    Info {
        #[arg(
            value_hint = clap::ValueHint::AnyPath,
            add = ArgValueCompleter::new(complete_image_path_with_base)
        )]
        path: PathBuf,
        /// Print original metadata JSON
        #[arg(long)]
        original: bool,
        /// Print booru edit JSON
        #[arg(long)]
        booru: bool,
    },
    /// Edit booru metadata for an image
    Edit {
        #[arg(
            value_hint = clap::ValueHint::AnyPath,
            add = ArgValueCompleter::new(complete_image_path_with_base)
        )]
        path: PathBuf,
        #[arg(long = "set-tag")]
        set_tags: Vec<String>,
        #[arg(long = "add-tag")]
        add_tags: Vec<String>,
        #[arg(long = "remove-tag")]
        remove_tags: Vec<String>,
        #[arg(long)]
        clear_tags: bool,
        #[arg(long)]
        notes: Option<String>,
    },
    /// Search images by substring in tags/author/detail
    Search {
        terms: Vec<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Show or manage alias groups in alias.json
    Alias {
        #[command(subcommand)]
        command: AliasCommands,
    },
    /// Find perceptual-hash duplicates
    Dupes {
        #[arg(long, value_enum, default_value = "dhash")]
        algo: HashAlgo,
        #[arg(long, default_value_t = 8)]
        threshold: u32,
        /// Disable sqlite hash cache
        #[arg(long)]
        no_cache: bool,
        /// Override cache path
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Generate shell completion script
    Completion {
        #[arg(value_enum)]
        shell: Shell,
        /// Generate static (AOT) completion script instead of dynamic registration
        #[arg(long)]
        aot: bool,
    },
}

#[derive(Subcommand)]
enum AliasCommands {
    /// Show alias groups
    List,
    /// Add terms into one alias group (and merge overlapping groups)
    Add { terms: Vec<String> },
    /// Remove terms from all alias groups
    Remove { terms: Vec<String> },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HashAlgo {
    Ahash,
    Dhash,
    Phash,
}

fn main() -> Result<()> {
    CompleteEnv::with_factory(|| Cli::command())
        .var(COMPLETE_ENV_VAR)
        .complete();

    let cli = Cli::parse();
    let config = if cli.base.is_empty() {
        BooruConfig::default()
    } else {
        BooruConfig::with_roots(cli.base.clone())
    };

    match cli.command {
        Commands::Info {
            path,
            original,
            booru,
        } => info_command(&config, &path, original, booru, cli.quiet),
        Commands::Edit {
            path,
            set_tags,
            add_tags,
            remove_tags,
            clear_tags,
            notes,
        } => edit_command(
            &config,
            &path,
            set_tags,
            add_tags,
            remove_tags,
            clear_tags,
            notes,
        ),
        Commands::Search { terms, limit } => search_command(&config, terms, limit, cli.quiet),
        Commands::Alias { command } => alias_command(&config, command, cli.quiet),
        Commands::Dupes {
            algo,
            threshold,
            no_cache,
            cache,
        } => dupes_command(&config, algo, threshold, no_cache, cache, cli.quiet),
        Commands::Completion { shell, aot } => completion_command(shell, aot),
    }
}

fn completion_command(shell: Shell, aot: bool) -> Result<()> {
    if aot {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        generate(shell, &mut cmd, name, &mut std::io::stdout());
        return Ok(());
    }

    let current_dir = std::env::current_dir().ok();
    let argv0 = std::env::args_os()
        .next()
        .unwrap_or_else(|| OsString::from("booructl"));
    let args = vec![argv0, OsString::from("--")];
    let shell_name = shell.to_string().to_ascii_lowercase();

    std::env::set_var(COMPLETE_ENV_VAR, shell_name);
    let completed = CompleteEnv::with_factory(|| Cli::command())
        .var(COMPLETE_ENV_VAR)
        .try_complete(args, current_dir.as_deref())?;
    std::env::remove_var(COMPLETE_ENV_VAR);

    if !completed {
        return Err(anyhow!("failed to generate dynamic completion script"));
    }
    Ok(())
}

fn complete_image_path_with_base(current: &OsStr) -> Vec<CompletionCandidate> {
    let fallback = |value: &OsStr| PathCompleter::any().complete(value);
    let Some(current) = current.to_str() else {
        return fallback(current);
    };
    let current_unescaped = unescape_shell_backslashes(current);

    // If the user explicitly types an absolute/relative prefix, fall back to shell-like completion.
    if has_explicit_path_prefix(current) {
        let out = fallback(OsStr::new(current));
        if !out.is_empty() {
            return out;
        }
        if let Some(unescaped) = current_unescaped.as_deref() {
            return fallback(OsStr::new(unescaped));
        }
        return out;
    }

    let roots = completion_roots_from_env();
    let mut out = collect_relative_candidates_for_roots(&roots, current);
    if out.is_empty() {
        if let Some(unescaped) = current_unescaped.as_deref() {
            out = collect_relative_candidates_for_roots(&roots, unescaped);
        }
    }
    if out.is_empty() {
        let fallback_out = fallback(OsStr::new(current));
        if !fallback_out.is_empty() {
            return fallback_out;
        }
        if let Some(unescaped) = current_unescaped.as_deref() {
            return fallback(OsStr::new(unescaped));
        }
        fallback_out
    } else {
        out.sort_by(|a, b| a.get_value().cmp(b.get_value()));
        out
    }
}

fn has_explicit_path_prefix(current: &str) -> bool {
    current.starts_with('/')
        || current.starts_with("./")
        || current.starts_with("../")
        || current.starts_with('~')
}

fn collect_relative_candidates_for_roots(
    roots: &[PathBuf],
    current: &str,
) -> Vec<CompletionCandidate> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        collect_relative_candidates(root, current, &mut seen, &mut out);
    }
    out
}

fn unescape_shell_backslashes(input: &str) -> Option<String> {
    if !input.contains('\\') {
        return None;
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    if out == input {
        None
    } else {
        Some(out)
    }
}

fn completion_roots_from_env() -> Vec<PathBuf> {
    let words = completion_words_from_env();
    let mut bases = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let token = words[i].to_string_lossy();
        if token == "--base" || token == "-b" {
            if let Some(value) = words.get(i + 1) {
                bases.push(PathBuf::from(value));
                i += 2;
                continue;
            }
            break;
        }
        if let Some(rest) = token.strip_prefix("--base=") {
            if !rest.is_empty() {
                bases.push(PathBuf::from(rest));
            }
            i += 1;
            continue;
        }
        if token.len() > 2 && token.starts_with("-b") {
            bases.push(PathBuf::from(&token[2..]));
            i += 1;
            continue;
        }
        i += 1;
    }

    if bases.is_empty() {
        BooruConfig::default().roots
    } else {
        BooruConfig::with_roots(bases).roots
    }
}

fn completion_words_from_env() -> Vec<OsString> {
    let mut out = Vec::new();
    let mut after_sep = false;
    for arg in std::env::args_os().skip(1) {
        if after_sep {
            out.push(arg);
            continue;
        }
        if arg.as_os_str() == OsStr::new("--") {
            after_sep = true;
        }
    }
    out
}

fn collect_relative_candidates(
    root: &Path,
    current: &str,
    seen: &mut HashSet<OsString>,
    out: &mut Vec<CompletionCandidate>,
) {
    let (parent, partial) = split_parent_and_partial(current);
    let search_dir = if parent.is_empty() {
        root.to_path_buf()
    } else {
        root.join(parent)
    };
    let Ok(entries) = fs::read_dir(search_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with(partial) {
            continue;
        }

        let mut rel = if parent.is_empty() {
            name_str.into_owned()
        } else {
            format!("{parent}/{name_str}")
        };
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            rel.push('/');
        }

        let rel_os = OsString::from(&rel);
        if seen.insert(rel_os.clone()) {
            out.push(CompletionCandidate::new(rel_os));
        }
    }
}

fn split_parent_and_partial(current: &str) -> (&str, &str) {
    if let Some(stripped) = current.strip_suffix('/') {
        return (stripped, "");
    }
    if let Some((parent, partial)) = current.rsplit_once('/') {
        return (parent, partial);
    }
    ("", current)
}

fn info_command(
    config: &BooruConfig,
    path: &Path,
    original: bool,
    booru: bool,
    quiet: bool,
) -> Result<()> {
    let library = scan_library(config, quiet)?;
    let image_path = resolve_image_path(path, &library.config.roots);
    let item = library
        .index
        .get_by_path(&image_path)
        .ok_or_else(|| anyhow!("image not found in scan: {}", image_path.display()))?;

    println!("Image: {}", item.image_path.display());
    println!("Metadata: {}", item.meta_path.display());
    println!("Booru edits: {}", item.booru_path.display());
    let tags = item.merged_tags();
    if tags.is_empty() {
        println!("Tags: (none)");
    } else {
        println!("Tags: {}", tags.join(" "));
    }
    println!(
        "Author: {}",
        item.merged_author().unwrap_or_else(|| "(none)".to_string())
    );
    println!("Date: {}", format_date_for_display(item.merged_date()));
    println!(
        "Platform URL: {}",
        item.platform_url().unwrap_or_else(|| "(none)".to_string())
    );
    match item.merged_detail() {
        Some(detail) if detail.contains('\n') => println!("Detail:\n{detail}"),
        Some(detail) => println!("Detail: {detail}"),
        None => println!("Detail: (none)"),
    }
    println!(
        "Sensitive (NSFW): {}",
        if item.merged_sensitive() { "yes" } else { "no" }
    );
    println!(
        "Notes (user): {}",
        item.edits.notes.as_deref().unwrap_or("(none)")
    );

    if original {
        let pretty = serde_json::to_string_pretty(&item.original)?;
        println!("\nOriginal metadata:\n{pretty}");
    }
    if booru {
        let pretty = serde_json::to_string_pretty(&item.edits)?;
        println!("\nBooru edits:\n{pretty}");
    }

    Ok(())
}

fn edit_command(
    config: &BooruConfig,
    path: &Path,
    set_tags: Vec<String>,
    add_tags: Vec<String>,
    remove_tags: Vec<String>,
    clear_tags: bool,
    notes: Option<String>,
) -> Result<()> {
    let image_path = resolve_image_path(path, &config.roots);
    if !image_path.exists() {
        return Err(anyhow!("image not found: {}", image_path.display()));
    }

    let meta_path = metadata_path_for_image(&image_path);
    if !meta_path.exists() {
        return Err(anyhow!("metadata not found: {}", meta_path.display()));
    }

    let update = EditUpdate {
        set_tags: normalize_tag_args(set_tags),
        add_tags: flatten_tag_args(add_tags),
        remove_tags: flatten_tag_args(remove_tags),
        clear_tags,
        notes,
        sensitive: None,
    };

    let edits =
        apply_update_to_image(&image_path, update).context("failed to write booru edits")?;
    println!("Updated: {}", image_path.display());
    println!("Booru edits: {}", serde_json::to_string_pretty(&edits)?);
    Ok(())
}

fn search_command(
    config: &BooruConfig,
    terms: Vec<String>,
    limit: usize,
    quiet: bool,
) -> Result<()> {
    let library = scan_library(config, quiet)?;
    let search = library.search(SearchQuery::new(terms).with_aliases(true));

    if search.normalized_terms.is_empty() {
        return Err(anyhow!("no search terms provided"));
    }
    if !quiet {
        for warning in search.alias_warnings {
            eprintln!("warning: {}: {}", warning.path.display(), warning.message);
        }
    }

    let mut results = search
        .indices
        .iter()
        .filter_map(|idx| library.index.items.get(*idx))
        .collect::<Vec<_>>();
    results.sort_by_key(|item| item.image_path.clone());
    for item in results.into_iter().take(limit) {
        println!("{}", item.image_path.display());
    }
    Ok(())
}

fn alias_command(config: &BooruConfig, command: AliasCommands, quiet: bool) -> Result<()> {
    match command {
        AliasCommands::List => alias_list_command(config, quiet),
        AliasCommands::Add { terms } => alias_add_command(config, terms),
        AliasCommands::Remove { terms } => alias_remove_command(config, terms),
    }
}

fn alias_list_command(config: &BooruConfig, quiet: bool) -> Result<()> {
    let show_root = config.roots.len() > 1;
    for (idx, root) in config.roots.iter().enumerate() {
        if show_root {
            if idx > 0 {
                println!();
            }
            println!("Root: {}", root.display());
        }

        match load_alias_groups_from_root(root) {
            Ok(groups) => {
                if groups.is_empty() {
                    println!("(none)");
                } else {
                    for group in groups {
                        println!("{}", group.join(" | "));
                    }
                }
            }
            Err(err) => {
                let path = alias_path_for_root(root);
                if !quiet {
                    eprintln!("warning: {}: {}", path.display(), err);
                }
                println!("(invalid alias file)");
            }
        }
    }
    Ok(())
}

fn alias_add_command(config: &BooruConfig, terms: Vec<String>) -> Result<()> {
    let root = alias_edit_root(config)?;
    let terms = normalize_search_terms(terms);
    if terms.len() < 2 {
        return Err(anyhow!("alias add requires at least 2 non-empty terms"));
    }

    let path = alias_path_for_root(root);
    let mut groups =
        load_alias_groups_from_root(root).map_err(|err| anyhow!("{}: {}", path.display(), err))?;
    let changed = merge_alias_terms(&mut groups, terms);
    if changed {
        save_alias_groups_to_root(root, &groups)
            .map_err(|err| anyhow!("{}: {}", path.display(), err))?;
        println!("Updated {}", path.display());
    } else {
        println!("No changes.");
    }
    Ok(())
}

fn alias_remove_command(config: &BooruConfig, terms: Vec<String>) -> Result<()> {
    let root = alias_edit_root(config)?;
    let terms = normalize_search_terms(terms);
    if terms.is_empty() {
        return Err(anyhow!("alias remove requires at least 1 non-empty term"));
    }

    let path = alias_path_for_root(root);
    let mut groups =
        load_alias_groups_from_root(root).map_err(|err| anyhow!("{}: {}", path.display(), err))?;
    let changed = remove_alias_terms(&mut groups, terms);
    if changed {
        save_alias_groups_to_root(root, &groups)
            .map_err(|err| anyhow!("{}: {}", path.display(), err))?;
        println!("Updated {}", path.display());
    } else {
        println!("No changes.");
    }
    Ok(())
}

fn alias_edit_root(config: &BooruConfig) -> Result<&PathBuf> {
    if config.roots.len() != 1 {
        return Err(anyhow!(
            "alias add/remove requires exactly one base root; pass a single --base"
        ));
    }
    Ok(&config.roots[0])
}

fn dupes_command(
    config: &BooruConfig,
    algo: HashAlgo,
    threshold: u32,
    no_cache: bool,
    cache_path: Option<PathBuf>,
    quiet: bool,
) -> Result<()> {
    let library = scan_library(config, quiet)?;
    let algo = match algo {
        HashAlgo::Ahash => FuzzyHashAlgorithm::AHash,
        HashAlgo::Dhash => FuzzyHashAlgorithm::DHash,
        HashAlgo::Phash => FuzzyHashAlgorithm::PHash,
    };

    let mut cache = if no_cache {
        None
    } else if let Some(path) = cache_path {
        Some(HashCache::open(&path).context("failed to open cache")?)
    } else {
        match HashCache::open_default() {
            Ok(cache) => Some(cache),
            Err(err) => {
                if !quiet {
                    eprintln!("warning: cache disabled: {err}");
                }
                None
            }
        }
    };

    let show_progress = !quiet && std::io::stderr().is_terminal();
    let progress = if show_progress {
        let pb = ProgressBar::new(library.index.items.len() as u64);
        pb.set_style(
            ProgressStyle::with_template("{spinner:.green} {msg} [{bar:40.cyan/blue}] {pos}/{len}")
                .unwrap()
                .progress_chars("=>-"),
        );
        pb.set_message("hashing");
        Some(pb)
    } else {
        None
    };

    let observer = progress.as_ref().map(|pb| HashProgress { pb: pb.clone() });
    let computation = compute_hashes_with_cache(
        &library.index.items,
        algo,
        cache.as_mut(),
        observer.as_ref().map(|o| o as &dyn ProgressObserver),
    );
    if let Some(pb) = &progress {
        pb.finish_and_clear();
    }

    let spinner = if show_progress {
        let sp = ProgressBar::new_spinner();
        sp.set_message("comparing");
        sp.enable_steady_tick(std::time::Duration::from_millis(120));
        Some(sp)
    } else {
        None
    };
    let groups = group_duplicates(&library.index.items, &computation.hashes, threshold, true);
    if let Some(sp) = spinner {
        sp.finish_and_clear();
    }

    for warning in &computation.warnings {
        eprintln!("warning: {}: {}", warning.path.display(), warning.message);
    }

    if groups.is_empty() {
        println!("No duplicates found.");
        return Ok(());
    }

    for (idx, group) in groups.iter().enumerate() {
        println!("Group {}:", idx + 1);
        for item_idx in &group.items {
            if let Some(item) = library.index.items.get(*item_idx) {
                println!("  {}", item.image_path.display());
            }
        }
    }
    Ok(())
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

fn normalize_tag_args(tags: Vec<String>) -> Option<Vec<String>> {
    let tags = flatten_tag_args(tags);
    if tags.is_empty() {
        None
    } else {
        Some(tags)
    }
}

fn flatten_tag_args(tags: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for tag in tags {
        if tag.contains(',') {
            for part in tag.split(',') {
                let part = part.trim();
                if !part.is_empty() {
                    out.push(part.to_string());
                }
            }
        } else {
            let tag = tag.trim();
            if !tag.is_empty() {
                out.push(tag.to_string());
            }
        }
    }
    out
}

struct HashProgress {
    pb: ProgressBar,
}

impl ProgressObserver for HashProgress {
    fn inc(&self, delta: u64) {
        self.pb.inc(delta);
    }
}

fn format_date_for_display(raw: Option<String>) -> String {
    let Some(raw) = raw else {
        return "(none)".to_string();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "(none)".to_string();
    }
    format_date_string(trimmed).unwrap_or_else(|| trimmed.to_string())
}

fn format_date_string(raw: &str) -> Option<String> {
    if let Ok(ts) = raw.parse::<i64>() {
        return format_unix_timestamp(ts);
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(format_local_datetime(dt.with_timezone(&Local)));
    }

    if let Ok(dt) = DateTime::parse_from_str(raw, "%a %b %d %H:%M:%S %z %Y") {
        return Some(format_local_datetime(dt.with_timezone(&Local)));
    }

    for fmt in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y/%m/%d %H:%M:%S",
        "%Y/%m/%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw, fmt) {
            if let Some(local_dt) = localize_naive_datetime(naive) {
                return Some(format_local_datetime(local_dt));
            }
        }
    }

    None
}

fn format_unix_timestamp(ts: i64) -> Option<String> {
    let (seconds, nanos) = if ts.abs() >= 1_000_000_000_000 {
        let seconds = ts.div_euclid(1000);
        let millis = ts.rem_euclid(1000) as u32;
        (seconds, millis * 1_000_000)
    } else {
        (ts, 0)
    };

    let utc = Utc.timestamp_opt(seconds, nanos).single()?;
    Some(format_local_datetime(utc.with_timezone(&Local)))
}

fn localize_naive_datetime(naive: NaiveDateTime) -> Option<DateTime<Local>> {
    let local = Local.from_local_datetime(&naive);
    local
        .single()
        .or_else(|| local.earliest())
        .or_else(|| local.latest())
}

fn format_local_datetime(dt: DateTime<Local>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S %:z").to_string()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use booru_core::BooruEdits;
    use chrono::{Local, TimeZone, Utc};
    use serde_json::json;

    use booru_core::item_matches_search_terms;

    use super::format_date_string;

    fn make_item(original: serde_json::Value) -> booru_core::ImageItem {
        booru_core::ImageItem {
            image_path: PathBuf::new(),
            meta_path: PathBuf::new(),
            booru_path: PathBuf::new(),
            original,
            edits: BooruEdits::default(),
        }
    }

    #[test]
    fn format_unix_seconds_for_display() {
        let expected = Utc
            .timestamp_opt(1768034678, 0)
            .single()
            .unwrap()
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string();
        assert_eq!(
            format_date_string("1768034678").as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn format_naive_datetime_for_display() {
        let formatted = format_date_string("2025-02-12 03:33:51").expect("should parse");
        assert!(formatted.starts_with("2025-02-12 03:33:51 "));
    }

    #[test]
    fn search_matches_tag_author_and_detail_by_substring() {
        let item = make_item(json!({
            "tags": ["flower_garden"],
            "author": "AlicePainter",
            "detail": "Sunlight over the hills",
        }));

        assert!(item_matches_search_terms(&item, &[String::from("garden")]));
        assert!(item_matches_search_terms(&item, &[String::from("painter")]));
        assert!(item_matches_search_terms(
            &item,
            &[String::from("sunlight")]
        ));
    }

    #[test]
    fn search_is_case_insensitive_and_uses_any_term() {
        let item = make_item(json!({
            "tags": ["blue_sky"],
            "author": "Bob",
            "detail": "Evening clouds",
        }));

        assert!(item_matches_search_terms(
            &item,
            &[String::from("nomatch"), String::from("CLOUD")]
        ));
    }
}
