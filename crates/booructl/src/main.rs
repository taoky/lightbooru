use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use booru_core::{
    apply_update_to_image, find_duplicates, metadata_path_for_image, resolve_image_path,
    BooruConfig, EditUpdate, FuzzyHashAlgorithm, Library,
};
use clap::{Parser, Subcommand, ValueEnum};

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
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HashAlgo {
    Ahash,
    Dhash,
    Phash,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = if cli.base.is_empty() {
        BooruConfig::default()
    } else {
        BooruConfig::with_roots(cli.base.clone())
    };

    match cli.command {
        Commands::Info { path, original, booru } => info_command(&config, &path, original, booru, cli.quiet),
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
        Commands::Dupes { algo, threshold } => dupes_command(&config, algo, threshold, cli.quiet),
    }
}

fn info_command(config: &BooruConfig, path: &Path, original: bool, booru: bool, quiet: bool) -> Result<()> {
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

    let edits = apply_update_to_image(&image_path, update).context("failed to write booru edits")?;
    println!("Updated: {}", image_path.display());
    println!("Booru edits: {}", serde_json::to_string_pretty(&edits)?);
    Ok(())
}

fn search_command(config: &BooruConfig, tags: Vec<String>, limit: usize, quiet: bool) -> Result<()> {
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

fn dupes_command(config: &BooruConfig, algo: HashAlgo, threshold: u32, quiet: bool) -> Result<()> {
    let library = scan_library(config, quiet)?;
    let algo = match algo {
        HashAlgo::Ahash => FuzzyHashAlgorithm::AHash,
        HashAlgo::Dhash => FuzzyHashAlgorithm::DHash,
        HashAlgo::Phash => FuzzyHashAlgorithm::PHash,
    };

    let report = find_duplicates(&library.index.items, algo, threshold);
    for warning in &report.warnings {
        eprintln!("warning: {}: {}", warning.path.display(), warning.message);
    }

    if report.groups.is_empty() {
        println!("No duplicates found.");
        return Ok(());
    }

    for (idx, group) in report.groups.iter().enumerate() {
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
