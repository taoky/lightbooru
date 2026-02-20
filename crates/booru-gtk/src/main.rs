mod ui;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use adw::Application;
use anyhow::Result;
use booru_core::BooruConfig;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "booru-gtk", version, about = "GTK GUI for LightBooru")]
struct Cli {
    /// Base directory for gallery-dl downloads (can be repeated)
    #[arg(long, short)]
    base: Vec<PathBuf>,

    /// Suppress scan warnings
    #[arg(long)]
    quiet: bool,

    /// Show sensitive images (default: hidden)
    #[arg(long)]
    sensitive: bool,
}

fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let config = if cli.base.is_empty() {
        BooruConfig::default()
    } else {
        BooruConfig::with_roots(cli.base)
    };

    let library = ui::scan_library(&config, cli.quiet)?;
    let state = Rc::new(RefCell::new(ui::AppState::new(
        library,
        cli.sensitive,
        cli.quiet,
    )));

    let app = Application::builder()
        .application_id("moe.taoky.lightbooru.gtk")
        .build();
    let state_for_activate = state.clone();
    app.connect_activate(move |app| ui::build_ui(app, state_for_activate.clone()));
    app.run();

    Ok(())
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("booru_gtk=debug"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
