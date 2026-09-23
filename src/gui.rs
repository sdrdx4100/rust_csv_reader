//! A deliberately minimal desktop viewer built on egui.
//!
//! It reuses the same Arrow-backed [`Table`] as the terminal UI and renders a
//! *virtualised* table — only the rows currently on screen are built each
//! frame — so opening a file with a million rows stays smooth. A single search
//! box filters rows across every column. Very large Parquet files are not
//! loaded at all: they open in SQL-only mode backed by DataFusion.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use crate::app::{group_digits, PREVIEW_ROWS};
use crate::data::{inspect, is_numeric_type, read_head, FileKind, LoadOptions, StreamLimits, Table};
use crate::sql::{SqlEngine, SqlResult};

/// Most rows a SQL result will display (keeps memory and build time bounded).
const SQL_ROW_CAP: usize = 100_000;

/// A file too large to load (see [`StreamLimits`]), opened in SQL-only mode:
/// nothing is read up front beyond metadata, and DataFusion streams through
/// the file for each query.
struct LazyFile {
    path: PathBuf,
    kind: FileKind,
    /// Known for Parquet (from the footer); `None` for a big CSV.
    rows: Option<usize>,
    cols: Option<usize>,
}

/// CSV field delimiter choices offered in the toolbar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Delimiter {
    Comma,
    Tab,
    Semicolon,
    Pipe,
}

impl Delimiter {
    const ALL: [Delimiter; 4] = [
        Delimiter::Comma,
        Delimiter::Tab,
        Delimiter::Semicolon,
        Delimiter::Pipe,
    ];

    fn byte(self) -> u8 {
        match self {
            Delimiter::Comma => b',',
            Delimiter::Tab => b'\t',
            Delimiter::Semicolon => b';',
            Delimiter::Pipe => b'|',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Delimiter::Comma => "Comma",
            Delimiter::Tab => "Tab",
            Delimiter::Semicolon => "Semicolon",
            Delimiter::Pipe => "Pipe",
        }
    }
}

/// Launch the desktop viewer, optionally opening `path` on start-up.
pub fn run(path: Option<PathBuf>) -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]);
    // Taskbar / Alt-Tab icon (best-effort; ignored if decoding fails).
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
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
    /// Set instead of `table` when a large Parquet file is open in SQL-only mode.
    lazy: Option<LazyFile>,
    limits: StreamLimits,
    error: Option<String>,
    path_input: String,

    /// CSV parse settings (ignored for Parquet), adjustable from the toolbar.
    delimiter: Delimiter,
    has_header: bool,

    /// Live search query and the row indices that currently match it.
    query: String,
    last_query: String,
    filtered: Vec<usize>,
    /// Lazily-built, lowercased per-row text used for fast substring filtering.
    haystack: Option<Vec<String>>,

    /// When true the toolbar shows a SQL box instead of the substring search.
    sql_mode: bool,
    sql_input: String,
    /// DataFusion session (built lazily on first query), result, and any error.
    sql_engine: Option<Arc<SqlEngine>>,
    sql_result: Option<SqlResult>,
    sql_error: Option<String>,
    /// A query running on a background thread, and when it started.
    sql_pending: Option<Receiver<Result<SqlResult, String>>>,
    sql_started: Option<Instant>,
    /// How long the last finished query took.
    sql_elapsed: Option<Duration>,
}

impl TesseraGui {
    fn new(path: Option<PathBuf>) -> Self {
        let mut app = TesseraGui {
            table: None,
            lazy: None,
            limits: StreamLimits::default(),
            error: None,
            path_input: String::new(),
            delimiter: Delimiter::Comma,
            has_header: true,
            query: String::new(),
            last_query: String::new(),
            filtered: Vec::new(),
            haystack: None,
            sql_mode: false,
            sql_input: String::new(),
            sql_engine: None,
            sql_result: None,
            sql_error: None,
            sql_pending: None,
            sql_started: None,
            sql_elapsed: None,
        };
        if let Some(p) = path {
            app.open(&p);
        }
        app
    }

    fn open(&mut self, path: &Path) {
        // Files too big to load are not read at all: only their metadata is
        // looked at, and DataFusion streams through them per query.
        match inspect(path, None, &self.limits) {
            Ok(info) if info.stream => {
                self.open_lazy(path, info.kind, info.rows, info.cols);
                return;
            }
            Ok(_) => {}
            Err(e) => {
                self.error = Some(crate::error_text(&e));
                return;
            }
        }

        let opts = LoadOptions {
            delimiter: self.delimiter.byte(),
            has_header: self.has_header,
            ..Default::default()
        };
        match Table::load(path, &opts) {
            Ok(table) => {
                self.filtered = (0..table.num_rows()).collect();
                self.table = Some(table);
                self.lazy = None;
                self.error = None;
                self.haystack = None;
                self.query.clear();
                self.last_query.clear();
                self.path_input = path.display().to_string();
                // A new file means a fresh SQL session and cleared results.
                self.reset_sql();
            }
            Err(e) => {
                self.error = Some(crate::error_text(&e));
            }
        }
    }

    /// Open a file too big to load in SQL-only mode, showing its first rows.
    fn open_lazy(&mut self, path: &Path, kind: FileKind, rows: Option<usize>, cols: Option<usize>) {
        self.table = None;
        self.filtered.clear();
        self.haystack = None;
        self.query.clear();
        self.last_query.clear();
        self.error = None;
        self.path_input = path.display().to_string();
        self.lazy = Some(LazyFile {
            path: path.to_path_buf(),
            kind,
            rows,
            cols,
        });
        self.reset_sql();
        self.sql_mode = true;
        self.sql_input = format!("SELECT * FROM {} WHERE ", SqlEngine::TABLE);
        // Preview: the file's first rows, read directly (in file order).
        let opts = LoadOptions {
            delimiter: self.delimiter.byte(),
            has_header: self.has_header,
            ..Default::default()
        };
        match read_head(path, &opts, PREVIEW_ROWS) {
            Ok(head) => {
                let fmts = head.formatters().ok();
                let rows = (0..head.num_rows())
                    .map(|r| {
                        (0..head.num_cols())
                            .map(|c| match &fmts {
                                Some(f) => f[c].value(r).to_string(),
                                None => head.cell(r, c),
                            })
                            .collect()
                    })
                    .collect();
                if let Some(l) = &mut self.lazy {
                    l.cols = l.cols.or(Some(head.num_cols()));
                }
                self.sql_result = Some(SqlResult {
                    columns: head.column_names().to_vec(),
                    rows,
                    truncated: false,
                });
            }
            Err(e) => self.sql_error = Some(crate::error_text(&e)),
        }
    }

    fn reset_sql(&mut self) {
        self.sql_engine = None;
        self.sql_result = None;
        self.sql_error = None;
        self.sql_pending = None;
        self.sql_started = None;
        self.sql_elapsed = None;
    }

    /// Path and kind of whatever is open, loaded or lazy.
    fn current_source(&self) -> Option<(PathBuf, FileKind)> {
        if let Some(l) = &self.lazy {
            return Some((l.path.clone(), l.kind));
        }
        self.table.as_ref().map(|t| (t.path.clone(), t.kind))
    }

    /// Re-open the current file — used when the CSV parse settings change.
    fn reopen(&mut self) {
        if let Some((path, _)) = self.current_source() {
            self.open(&path);
        }
    }

    /// Execute the SQL box against the open file, building the engine on demand.
    fn run_sql(&mut self) {
        let Some((path, kind)) = self.current_source() else {
            self.sql_error = Some("open a file first".into());
            self.sql_result = None;
            return;
        };
        if self.sql_input.trim().is_empty() {
            self.sql_result = None;
            self.sql_error = None;
            return;
        }
        if self.sql_engine.is_none() {
            match SqlEngine::new(&path, kind, self.delimiter.byte(), self.has_header) {
                Ok(engine) => self.sql_engine = Some(Arc::new(engine)),
                Err(e) => {
                    self.sql_error = Some(crate::error_text(&e));
                    self.sql_result = None;
                    return;
                }
            }
        }
        // Run the query off the UI thread: a full scan of a big file can take a
        // while, and the window should stay responsive meanwhile. Starting a new
        // query simply drops the old receiver, so a stale result is discarded.
        let engine = Arc::clone(self.sql_engine.as_ref().expect("engine built"));
        let sql = self.sql_input.trim().to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let res = engine
                .query(&sql, SQL_ROW_CAP)
                .map_err(|e| crate::error_text(&e));
            let _ = tx.send(res);
        });
        self.sql_pending = Some(rx);
        self.sql_started = Some(Instant::now());
        self.sql_error = None;
    }

    /// Pick up a finished background query. Returns true while one is running.
    fn poll_sql(&mut self) -> bool {
        let Some(rx) = &self.sql_pending else {
            return false;
        };
        let outcome = match rx.try_recv() {
            Ok(res) => res,
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => Err("query stopped unexpectedly".to_string()),
        };
        self.sql_pending = None;
        self.sql_elapsed = self.sql_started.take().map(|t| t.elapsed());
        match outcome {
            Ok(res) => {
                self.sql_result = Some(res);
                self.sql_error = None;
            }
            Err(e) => {
                self.sql_result = None;
                self.sql_error = Some(e);
            }
        }
        false
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
        if self.poll_sql() {
            // Keep repainting so the result shows up (and the timer ticks).
            ctx.request_repaint_after(Duration::from_millis(100));
        }
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

                // CSV parse settings. Changing either re-opens the current file
                // (and resets the SQL session) so both views agree.
                let lazy = self.lazy.is_some();
                let before = (self.delimiter, self.has_header);
                ui.add_enabled_ui(!lazy, |ui| {
                    egui::ComboBox::from_id_salt("delimiter")
                        .selected_text(self.delimiter.label())
                        .show_ui(ui, |ui| {
                            for d in Delimiter::ALL {
                                ui.selectable_value(&mut self.delimiter, d, d.label());
                            }
                        });
                    ui.checkbox(&mut self.has_header, "Header")
                        .on_hover_text("first row is column names");
                });
                if (self.delimiter, self.has_header) != before {
                    self.reopen();
                }

                ui.separator();
                // Large files have no in-memory copy to search, so SQL only.
                if lazy {
                    self.sql_mode = true;
                }
                ui.add_enabled_ui(!lazy, |ui| {
                    ui.selectable_value(&mut self.sql_mode, false, "Search")
                        .on_disabled_hover_text("large file: search with SQL (WHERE …)");
                });
                ui.selectable_value(&mut self.sql_mode, true, "SQL");
                if self.sql_mode {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.sql_input)
                            .desired_width(420.0)
                            .hint_text("SELECT * FROM data WHERE …"),
                    );
                    let run = ui.button("Run ▶").clicked()
                        || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if run {
                        self.run_sql();
                    }
                    if let Some(t) = self.sql_started {
                        ui.spinner();
                        ui.weak(format!("running… {:.1}s", t.elapsed().as_secs_f32()));
                    }
                } else {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .desired_width(220.0)
                            .hint_text("filter all columns"),
                    );
                    if ui.button("✖").on_hover_text("clear search").clicked() {
                        self.query.clear();
                    }
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
                } else if let Some(l) = &self.lazy {
                    let name = l.path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
                    ui.weak(format!(
                        "{name}  ·  [{}]  ·  {} cols  ·  {} rows  ·  large file: not loaded, browse with SQL",
                        l.kind.label(),
                        l.cols.map_or("?".into(), |c| c.to_string()),
                        l.rows.map_or("?".into(), group_digits),
                    ));
                } else {
                    ui.weak("Drag a CSV or Parquet file onto the window, or type a path above.");
                }
                if let Some(err) = &self.error {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                }
                if self.sql_mode {
                    if let Some(res) = &self.sql_result {
                        let took = self
                            .sql_elapsed
                            .map(|d| format!(" in {:.2}s", d.as_secs_f32()))
                            .unwrap_or_default();
                        ui.weak(if res.truncated {
                            format!("  ·  SQL: showing first {} rows{took}", group_digits(SQL_ROW_CAP))
                        } else {
                            format!("  ·  SQL: {} rows{took}", group_digits(res.rows.len()))
                        });
                    }
                    if let Some(err) = &self.sql_error {
                        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                    }
                }
            });
            ui.add_space(2.0);
        });

        self.refresh_filter();

        egui::CentralPanel::default().show(ctx, |ui| {
            // In SQL mode the query result replaces the normal table view.
            if self.sql_mode {
                match &self.sql_result {
                    Some(res) if !res.columns.is_empty() => {
                        render_data_table(
                            ui,
                            &res.columns,
                            res.rows.len(),
                            |_| false,
                            |r| (r + 1).to_string(),
                            |r, c| res.rows[r].get(c).cloned().unwrap_or_default(),
                        );
                    }
                    Some(_) => {
                        ui.weak("(query returned no columns)");
                    }
                    None if self.sql_pending.is_some() => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak("running query…");
                        });
                    }
                    None if self.sql_error.is_none() => {
                        ui.weak(format!(
                            "Write SQL above and press Run — the file is table `{0}`.  \
                             e.g.  SELECT * FROM {0} LIMIT 100",
                            SqlEngine::TABLE,
                        ));
                    }
                    None => {}
                }
                return;
            }

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

            render_data_table(
                ui,
                names,
                filtered.len(),
                |c| is_numeric_type(&types[c]),
                |r| (filtered[r] + 1).to_string(),
                |r, c| match &fmts {
                    Some(f) => f[c].value(filtered[r]).to_string(),
                    None => table.cell(filtered[r], c),
                },
            );
        });
    }
}

/// Render a virtualised, horizontally-scrollable grid. Shared by the normal
/// table view and the SQL result view; callers supply the column names and
/// closures to fetch each gutter label / cell value and to flag numeric columns.
fn render_data_table(
    ui: &mut egui::Ui,
    names: &[String],
    nrows: usize,
    is_num: impl Fn(usize) -> bool,
    gutter: impl Fn(usize) -> String,
    cell: impl Fn(usize, usize) -> String,
) {
    let ncols = names.len();
    let row_height = 18.0;
    // egui_extras tables only scroll vertically on their own, so wrap the whole
    // thing in a horizontal scroll area to pan wide tables left/right. The table
    // keeps its own vertical *virtual* scrolling, so millions of rows stay cheap.
    //
    // Inside the horizontal scroll area the available height is unbounded, so we
    // capture the panel height up front and pin the table's vertical scroll
    // viewport to it — otherwise the vertical scrollbar never appears. Leave room
    // for the (frozen) header row so the last data rows aren't clipped off.
    let header_height = 22.0;
    let viewport_height = (ui.available_height() - header_height).max(50.0);
    egui::ScrollArea::horizontal()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut builder = TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .vscroll(true)
                .max_scroll_height(viewport_height)
                .min_scrolled_height(0.0)
                .auto_shrink([false, false])
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::auto().at_least(40.0)); // row-number gutter
            for _ in 0..ncols {
                builder = builder
                    .column(Column::initial(140.0).at_least(40.0).clip(true).resizable(true));
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
                    body.rows(row_height, nrows, |mut row| {
                        let r = row.index();
                        row.col(|ui| {
                            ui.weak(gutter(r));
                        });
                        for c in 0..ncols {
                            row.col(|ui| {
                                cell_ui(ui, &cell(r, c), is_num(c));
                            });
                        }
                    });
                });
        });
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

    fn sample_parquet(rows: i64) -> PathBuf {
        use arrow::array::{Int64Array, StringArray};
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);

        let mut p = std::env::temp_dir();
        p.push(format!(
            "tessera_gui_{}_{}.parquet",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
        ]));
        let ids: Vec<i64> = (0..rows).collect();
        let names: Vec<String> = ids.iter().map(|i| format!("name{i}")).collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(ids)),
                Arc::new(StringArray::from(names)),
            ],
        )
        .unwrap();
        let file = std::fs::File::create(&p).unwrap();
        let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        p
    }

    /// Block until the background query finishes (tests only).
    fn wait_sql(app: &mut TesseraGui) {
        for _ in 0..500 {
            if !app.poll_sql() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("query did not finish");
    }

    #[test]
    fn large_parquet_opens_in_sql_mode_without_loading() {
        let path = sample_parquet(50);
        let mut app = TesseraGui::new(None);
        app.limits.max_rows = 10; // treat 50 rows as "large"
        app.open(&path);

        // Nothing was materialised; only the footer's shape is known.
        assert!(app.table.is_none());
        let lazy = app.lazy.as_ref().expect("lazy mode");
        assert_eq!((lazy.rows, lazy.cols), (Some(50), Some(2)));
        assert!(app.sql_mode);

        // The preview (the file's first rows) is shown right away.
        assert!(app.sql_pending.is_none());
        let preview = app.sql_result.as_ref().unwrap();
        assert_eq!(preview.rows.len(), 50);
        assert_eq!(preview.rows[0][0], "0");

        // Searching is done with SQL over the whole file.
        app.sql_input = "SELECT id FROM data WHERE name LIKE 'name4%' ORDER BY id".into();
        app.run_sql();
        wait_sql(&mut app);
        let res = app.sql_result.as_ref().unwrap();
        assert_eq!(res.rows.len(), 11); // name4, name40..name49
        assert_eq!(res.rows[0], vec!["4".to_string()]);
        assert!(app.sql_error.is_none());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn small_parquet_still_loads_normally() {
        let path = sample_parquet(5);
        let app = TesseraGui::new(Some(path.clone()));
        assert!(app.lazy.is_none());
        assert_eq!(app.table.as_ref().unwrap().num_rows(), 5);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn groups_digits() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(10_000_000), "10,000,000");
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
