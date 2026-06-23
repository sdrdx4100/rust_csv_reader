//! Tessera — an ultimate terminal viewer for CSV and Parquet files.
//!
//! ```text
//! tessera                 # opens the built-in file browser
//! tessera data.csv
//! tessera data.parquet
//! tessera --delimiter '\t' --no-header data.tsv
//! ```

mod app;
mod data;
mod ui;

use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;

use crate::app::App;
use crate::data::{FileKind, LoadOptions, Table};

/// Command-line interface.
#[derive(Parser, Debug)]
#[command(
    name = "tessera",
    version,
    about = "An ultimate terminal viewer for CSV and Parquet files",
    long_about = None,
)]
struct Cli {
    /// Path to a .csv, .tsv or .parquet file. Omit it to open the file browser.
    file: Option<PathBuf>,

    /// Force the file type instead of auto-detecting (csv or parquet).
    #[arg(short = 't', long = "type", value_parser = parse_kind)]
    kind: Option<FileKind>,

    /// Field delimiter for CSV input (e.g. ',' ';' or '\t').
    #[arg(short, long, default_value = ",")]
    delimiter: String,

    /// Treat the first CSV row as data rather than a header.
    #[arg(long)]
    no_header: bool,
}

fn parse_kind(s: &str) -> Result<FileKind, String> {
    match s.to_ascii_lowercase().as_str() {
        "csv" | "tsv" => Ok(FileKind::Csv),
        "parquet" | "pq" => Ok(FileKind::Parquet),
        other => Err(format!("unknown type '{other}', expected csv or parquet")),
    }
}

/// Interpret a one-character (or escaped) delimiter string.
fn parse_delimiter(s: &str) -> Result<u8> {
    let resolved = match s {
        "\\t" | "tab" => "\t",
        "\\n" => "\n",
        other => other,
    };
    let bytes = resolved.as_bytes();
    anyhow::ensure!(
        bytes.len() == 1,
        "delimiter must be a single byte, got {:?}",
        s
    );
    Ok(bytes[0])
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let opts = LoadOptions {
        kind: cli.kind,
        delimiter: parse_delimiter(&cli.delimiter)?,
        has_header: !cli.no_header,
        ..Default::default()
    };

    // With a path, load before touching the terminal so errors print cleanly.
    // Without one, start in the file browser rooted at the working directory.
    let app = match &cli.file {
        Some(path) => {
            let table = Table::load(path, &opts)
                .with_context(|| format!("failed to load {}", path.display()))?;
            App::new(table, opts)
        }
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            App::browser_only(opts, cwd)
        }
    };

    let mut terminal = setup_terminal().context("failed to initialise terminal")?;
    let result = run(&mut terminal, app);
    restore_terminal(&mut terminal).ok();
    result
}

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    // Make sure the terminal is restored even on panic.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        prev(info);
    }));

    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn run(terminal: &mut Tui, mut app: App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|f| ui::render(f, &mut app))?;

        // Poll so resize and redraw stay responsive without busy-looping.
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                Event::Mouse(m) => app.on_mouse(m),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        // Honour any copy request via an OSC 52 escape, which lets the host
        // terminal place text on the system clipboard — even over SSH.
        if let Some(text) = app.take_clipboard() {
            copy_to_clipboard(&text);
        }
    }
    Ok(())
}

/// Emit an OSC 52 clipboard-write sequence. Best-effort: terminals that don't
/// support it simply ignore the escape.
fn copy_to_clipboard(text: &str) {
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let _ = out.flush();
}

/// Standard base64 encoding (no external dependency needed for a few bytes).
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
