use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use booru_core::{
    apply_update_to_image, compute_hashes_with_cache, group_duplicates, metadata_path_for_image,
    resolve_image_path, BooruConfig, EditUpdate, FuzzyHashAlgorithm, HashCache, Library,
    ProgressObserver,
};
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
        rating: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        source: Option<String>,
    },
    /// Search images by tags (AND match)
    Search {
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
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
            rating,
            notes,
            source,
        } => edit_command(
            &config,
            &path,
            set_tags,
            add_tags,
            remove_tags,
            clear_tags,
            rating,
            notes,
            source,
        ),
        Commands::Search { tags, limit } => search_command(&config, tags, limit, cli.quiet),
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
    let fallback = || PathCompleter::any().complete(current);
    let Some(current) = current.to_str() else {
        return fallback();
    };

    // If the user explicitly types an absolute/relative prefix, fall back to shell-like completion.
    if current.starts_with('/')
        || current.starts_with("./")
        || current.starts_with("../")
        || current.starts_with('~')
    {
        return fallback();
    }

    let roots = completion_roots_from_env();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        collect_relative_candidates(&root, current, &mut seen, &mut out);
    }
    if out.is_empty() {
        fallback()
    } else {
        out.sort_by(|a, b| a.get_value().cmp(b.get_value()));
        out
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
        "Rating: {}",
        item.merged_rating().unwrap_or_else(|| "(none)".to_string())
    );
    println!(
        "Source: {}",
        item.merged_source().unwrap_or_else(|| "(none)".to_string())
    );
    if let Some(notes) = &item.edits.notes {
        println!("Notes: {notes}");
    }

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
    rating: Option<String>,
    notes: Option<String>,
    source: Option<String>,
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
        rating,
        notes,
        source,
    };

    let edits =
        apply_update_to_image(&image_path, update).context("failed to write booru edits")?;
    println!("Updated: {}", image_path.display());
    println!("Booru edits: {}", serde_json::to_string_pretty(&edits)?);
    Ok(())
}

fn search_command(
    config: &BooruConfig,
    tags: Vec<String>,
    limit: usize,
    quiet: bool,
) -> Result<()> {
    let library = scan_library(config, quiet)?;
    let query_tags = flatten_tag_args(tags);
    if query_tags.is_empty() {
        return Err(anyhow!("no tags provided"));
    }

    let mut results = library.index.search_by_tags_all(&query_tags);
    results.sort_by_key(|item| item.image_path.clone());
    for item in results.into_iter().take(limit) {
        println!("{}", item.image_path.display());
    }
    Ok(())
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
