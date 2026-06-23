//! `tessera-gui` — a minimal desktop table viewer for CSV and Parquet.
//!
//! Build it with the optional GUI feature:
//!
//! ```text
//! cargo run --features gui --bin tessera-gui -- data.parquet
//! ```

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
