//! `tessera-gui` — a minimal desktop table viewer for CSV and Parquet.
//!
//! Build it with the optional GUI feature:
//!
//! ```text
//! cargo run --features gui --bin tessera-gui -- data.parquet
//! ```

// Don't pop up a console window alongside the GUI on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "tessera-gui",
    version,
    about = "A minimal desktop table viewer for CSV and Parquet files",
    long_about = None,
)]
struct Cli {
    /// Path to a .csv, .tsv or .parquet file. Omit it and drag a file in.
    file: Option<PathBuf>,
}

fn main() -> eframe::Result<()> {
    let cli = Cli::parse();
    tessera::gui::run(cli.file)
}
