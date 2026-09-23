//! Tessera — a fast terminal (and optional desktop) viewer for CSV and Parquet.
//!
//! The crate is split so both front-ends share one data layer:
//!
//! - [`data`] loads CSV/Parquet into a single Arrow batch with O(1) cell access.
//! - [`app`] and [`ui`] implement the terminal (TUI) viewer.
//! - [`sql`] runs DataFusion SQL over a file without loading it into memory.
//! - [`gui`] (behind the `gui` feature) is a minimal egui desktop table.

pub mod app;
pub mod data;
pub mod ui;

#[cfg(feature = "gui")]
pub mod gui;

pub mod sql;

/// A readable one-line description of an error and its causes. Errors from the
/// SQL engine often repeat the same text at each level of their cause chain;
/// repeated parts are dropped so the message says each thing once.
pub fn error_text(e: &anyhow::Error) -> String {
    let mut out = String::new();
    for cause in e.chain() {
        let msg = cause.to_string();
        let msg = msg.trim();
        if msg.is_empty() || out.contains(msg) {
            continue;
        }
        if !out.is_empty() {
            out.push_str(": ");
        }
        out.push_str(msg);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn error_text_drops_repeated_causes() {
        let e = anyhow::anyhow!("No field named nope")
            .context("Schema error: No field named nope")
            .context("failed to run query");
        assert_eq!(
            super::error_text(&e),
            "failed to run query: Schema error: No field named nope"
        );
    }
}
