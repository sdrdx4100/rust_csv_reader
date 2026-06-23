//! A deliberately minimal desktop viewer built on egui.
//!
//! It reuses the same Arrow-backed [`Table`] as the terminal UI and renders a
//! *virtualised* table — only the rows currently on screen are built each
//! frame — so opening a file with a million rows stays smooth. A single search
//! box filters rows across every column.

use std::path::{Path, PathBuf};

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::data::{is_numeric_type, LoadOptions, Table};

/// Launch the desktop viewer, optionally opening `path` on start-up.
pub fn run(path: Option<PathBuf>) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Tessera",
        options,
        Box::new(move |_cc| Ok(Box::new(TesseraGui::new(path)))),
    )
}

struct TesseraGui {
    table: Option<Table>,
    error: Option<String>,
    path_input: String,

    /// Live search query and the row indices that currently match it.
    query: String,
    last_query: String,
    filtered: Vec<usize>,
    /// Lazily-built, lowercased per-row text used for fast substring filtering.
    haystack: Option<Vec<String>>,
}

impl TesseraGui {
    fn new(path: Option<PathBuf>) -> Self {
        let mut app = TesseraGui {
            table: None,
            error: None,
            path_input: String::new(),
            query: String::new(),
            last_query: String::new(),
            filtered: Vec::new(),
            haystack: None,
        };
        if let Some(p) = path {
            app.open(&p);
        }
        app
    }

    fn open(&mut self, path: &Path) {
        match Table::load(path, &LoadOptions::default()) {
            Ok(table) => {
                self.filtered = (0..table.num_rows()).collect();
                self.table = Some(table);
                self.error = None;
                self.haystack = None;
                self.query.clear();
                self.last_query.clear();
                self.path_input = path.display().to_string();
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
            }
        }
    }

    /// Build (once) the lowercased haystack used for substring search.
    fn ensure_haystack(&mut self) {
        if self.haystack.is_some() {
            return;
        }
        let Some(table) = &self.table else { return };
        let rows = table.num_rows();
        let mut out = Vec::with_capacity(rows);
        match table.formatters() {
            Ok(fmts) => {
                for r in 0..rows {
                    let mut line = String::new();
                    for (c, f) in fmts.iter().enumerate() {
                        if c > 0 {
                            line.push('\u{1f}');
                        }
                        line.push_str(&f.value(r).to_string());
                    }
                    out.push(line.to_lowercase());
                }
            }
            Err(_) => out.resize(rows, String::new()),
        }
        self.haystack = Some(out);
    }

    /// Recompute `filtered` when the query text changes.
    fn refresh_filter(&mut self) {
        if self.query == self.last_query {
            return;
        }
        self.last_query = self.query.clone();
        let Some(rows) = self.table.as_ref().map(Table::num_rows) else {
            return;
        };

        if self.query.is_empty() {
            self.filtered = (0..rows).collect();
            return;
        }
        self.ensure_haystack();
        let needle = self.query.to_lowercase();
        let hay = self.haystack.as_ref().expect("haystack built");
        self.filtered = (0..rows).filter(|&r| hay[r].contains(&needle)).collect();
    }
}

/// Render one data cell: numeric values are right-aligned, the full value
/// shows on hover (so clipped cells stay readable), and a click copies it.
fn cell_ui(ui: &mut egui::Ui, text: &str, numeric: bool) {
    let add = |ui: &mut egui::Ui| {
        let resp = ui.add(
            egui::Label::new(text)
                .truncate()
                .sense(egui::Sense::click()),
        );
        if !text.is_empty() && resp.on_hover_text(text).clicked() {
            ui.ctx().copy_text(text.to_owned());
        }
    };
    if numeric {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add);
    } else {
        add(ui);
    }
}

impl eframe::App for TesseraGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Accept a file dropped anywhere on the window.
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .find_map(|f| f.path.clone())
        });
        if let Some(path) = dropped {
            self.open(&path);
        }

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("File:");
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.path_input)
                        .desired_width(360.0)
                        .hint_text("path to a .csv / .parquet file"),
                );
                let open_clicked = ui.button("Open").clicked();
                if open_clicked || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                {
                    let p = PathBuf::from(self.path_input.trim());
                    if !p.as_os_str().is_empty() {
                        self.open(&p);
                    }
                }

                ui.separator();
                ui.label("Search:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .desired_width(220.0)
                        .hint_text("filter all columns"),
                );
                if ui.button("✖").on_hover_text("clear search").clicked() {
                    self.query.clear();
                }
            });

            // Status line: row counts / errors / hints.
            ui.horizontal(|ui| {
                if let Some(table) = &self.table {
                    let total = table.num_rows();
                    let shown = self.filtered.len();
                    let name = table
                        .path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("?");
                    let suffix = if shown == total {
                        format!("{total} rows")
                    } else {
                        format!("{shown} / {total} rows")
                    };
                    ui.weak(format!(
                        "{name}  ·  [{}]  ·  {} cols  ·  {suffix}",
                        table.kind.label(),
                        table.num_cols(),
                    ));
                } else {
                    ui.weak("Drag a CSV or Parquet file onto the window, or type a path above.");
                }
                if let Some(err) = &self.error {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                }
            });
            ui.add_space(2.0);
        });

        self.refresh_filter();

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(table) = &self.table else {
                return;
            };
            let ncols = table.num_cols();
            if ncols == 0 {
                ui.label("(no columns)");
                return;
            }

            let names = table.column_names();
            let types = table.column_types();
            let filtered = &self.filtered;
            // Build the column formatters once per frame; cell access is then cheap.
            let fmts = table.formatters().ok();

            let row_height = 18.0;
            let mut builder = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::auto().at_least(40.0)); // row-number gutter
            for _ in 0..ncols {
                builder = builder.column(Column::initial(140.0).at_least(40.0).clip(true).resizable(true));
            }

            builder
                .header(22.0, |mut header| {
                    header.col(|ui| {
                        ui.strong("#");
                    });
                    for name in names {
                        header.col(|ui| {
                            ui.strong(name);
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_height, filtered.len(), |mut row| {
                        let table_row = filtered[row.index()];
                        row.col(|ui| {
                            ui.weak((table_row + 1).to_string());
                        });
                        for c in 0..ncols {
                            row.col(|ui| {
                                let text = match &fmts {
                                    Some(f) => f[c].value(table_row).to_string(),
                                    None => table.cell(table_row, c),
                                };
                                cell_ui(ui, &text, is_numeric_type(&types[c]));
                            });
                        }
                    });
                });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sample_csv() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tessera_gui_{}_{}.csv",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "id,name,score").unwrap();
        for i in 0..50 {
            writeln!(f, "{i},name{i},{}.5", i * 2).unwrap();
        }
        f.flush().unwrap();
        p
    }

    #[test]
    fn opens_and_filters_rows() {
        let path = sample_csv();
        let mut app = TesseraGui::new(Some(path.clone()));
        assert!(app.table.is_some());
        assert_eq!(app.filtered.len(), 50);

        // "name1" matches name1 and name10..name19 → 11 rows.
        app.query = "name1".to_string();
        app.refresh_filter();
        assert_eq!(app.filtered.len(), 11);

        // Clearing the query restores every row.
        app.query.clear();
        app.refresh_filter();
        assert_eq!(app.filtered.len(), 50);

        std::fs::remove_file(&path).ok();
    }
}
