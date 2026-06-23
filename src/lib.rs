//! Tessera — a fast terminal (and optional desktop) viewer for CSV and Parquet.
//!
//! The crate is split so both front-ends share one data layer:
//!
//! - [`data`] loads CSV/Parquet into a single Arrow batch with O(1) cell access.
//! - [`app`] and [`ui`] implement the terminal (TUI) viewer.
//! - [`gui`] (behind the `gui` feature) is a minimal egui desktop table.

pub mod app;
pub mod data;
pub mod ui;

#[cfg(feature = "gui")]
pub mod gui;
