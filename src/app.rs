//! Application state and input handling — the "brain" of the viewer.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use crate::data::{inspect, is_numeric_type, read_head, FileKind, LoadOptions, StreamLimits, Table};
use crate::sql::{SqlEngine, SqlTable};

/// Rows read up front to preview a file that is too big to load.
pub const PREVIEW_ROWS: usize = 1_000;

/// Most rows an SQL result keeps on screen. Use `tessera FILE -q SQL --csv` to
/// get every row of a bigger result.
pub const SQL_ROW_CAP: usize = 100_000;

/// SQL words offered by Tab completion (column names are added per file).
const SQL_WORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "LIKE", "ILIKE", "IN", "IS", "NULL",
    "BETWEEN", "ORDER", "BY", "GROUP", "HAVING", "LIMIT", "OFFSET", "DESC", "ASC",
    "DISTINCT", "COUNT", "SUM", "AVG", "MIN", "MAX", "AS",
];

/// The file that is open — loaded into memory, or only reachable through SQL.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub kind: FileKind,
    /// Known for loaded files and Parquet; `None` for a streamed CSV.
    pub rows: Option<usize>,
    pub cols: Option<usize>,
    /// True when the file was too big to load and is browsed with SQL only.
    pub streamed: bool,
}

/// A finished background query, sent back to the UI thread.
struct SqlDone {
    engine: Option<Arc<SqlEngine>>,
    query: String,
    result: Result<SqlTable, String>,
}

/// State of the SQL prompt (`S`) and of the query results on screen.
#[derive(Default)]
pub struct SqlUi {
    /// The query being edited and the cursor position in it (in characters).
    pub input: String,
    pub cursor: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    /// Completion candidates from the last Tab press, shown under the prompt.
    pub suggestions: Vec<String>,
    /// The query whose result is currently on screen, if any.
    pub shown: Option<String>,
    pub truncated: bool,
    pub elapsed: Option<Duration>,
    pub error: Option<String>,
    /// When the running query started (`None` when idle).
    pub started: Option<Instant>,
    pending: Option<Receiver<SqlDone>>,
    engine: Option<Arc<SqlEngine>>,
}

/// Interaction modes the viewer can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Normal navigation of the grid.
    Normal,
    /// Typing an incremental filter query.
    Search,
    /// Typing a row number to jump to.
    Goto,
    /// Full-screen help overlay.
    Help,
    /// Schema / column overview overlay (with per-column statistics).
    Schema,
    /// Full-cell inspector for the selected cell.
    Cell,
    /// Built-in file browser for opening another file.
    Browser,
    /// Typing an SQL query in the prompt at the bottom of the screen.
    Sql,
}

/// Sort direction applied to the current view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Ascending,
    Descending,
}

/// Aggregate statistics for one column, computed lazily for the schema view.
#[derive(Debug, Clone)]
pub struct ColStats {
    /// Number of non-null values.
    pub count: usize,
    /// Number of null / empty values.
    pub nulls: usize,
    /// Numeric summary, present only for numeric columns with parseable values.
    pub num: Option<NumStats>,
}

#[derive(Debug, Clone, Copy)]
pub struct NumStats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
}

pub struct App {
    /// The table on screen: the loaded file or an SQL result. `None` while a
    /// streamed file's first query is still running (or nothing is open).
    pub table: Option<Table>,
    /// The open file, whether loaded or streamed.
    pub source: Option<Source>,
    /// The loaded file's table, set aside while an SQL result is shown so `Esc`
    /// can return to it.
    saved: Option<Table>,
    /// SQL prompt and query state.
    pub sql: SqlUi,
    /// Load options reused when opening files from the browser.
    pub opts: LoadOptions,
    /// Size limits above which files are streamed instead of loaded.
    pub limits: StreamLimits,
    /// Always stream (`--sql-only`), whatever the file size.
    pub force_stream: bool,

    /// Row indices (into the table) currently visible, honouring filter & sort.
    pub visible: Vec<usize>,
    /// When `Some`, `visible` is a filtered subset and this is the query.
    pub filter: Option<String>,
    /// Active sort, as `(column, direction)`.
    pub sort: Option<(usize, SortDir)>,
    /// Lazily-built lowercase text for each row, used for filtering.
    row_text: Option<Vec<String>>,
    /// Lazily-computed per-column statistics (invalidated when the table swaps).
    stats: Option<Vec<ColStats>>,

    /// Per-column rendering width (content cells, excluding padding).
    pub col_widths: Vec<u16>,

    /// Selection within `visible` (row) and the table columns (col).
    pub sel_row: usize,
    pub sel_col: usize,
    /// Scroll offsets.
    pub row_off: usize,
    pub col_off: usize,

    pub mode: Mode,
    pub input: String,
    pub status: Option<String>,

    /// The file browser state (always present; shown in `Mode::Browser`).
    pub browser: Browser,

    /// Text queued to be copied to the system clipboard by the run loop.
    pending_clip: Option<String>,

    /// Number of data rows that fit in the current viewport (updated on draw).
    pub viewport_rows: usize,
    pub should_quit: bool,
}

impl App {
    /// Build an app around an already-loaded table.
    pub fn new(table: Table, opts: LoadOptions) -> App {
        let cwd = table
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut app = App::bare(opts, cwd);
        app.source = Some(Source {
            path: table.path.clone(),
            kind: table.kind,
            rows: Some(table.num_rows()),
            cols: Some(table.num_cols()),
            streamed: false,
        });
        app.set_table(table);
        app
    }

    /// Build an app with no table loaded, starting in the file browser.
    pub fn browser_only(opts: LoadOptions, cwd: PathBuf) -> App {
        let mut app = App::bare(opts, cwd);
        app.mode = Mode::Browser;
        app
    }

    fn bare(opts: LoadOptions, cwd: PathBuf) -> App {
        App {
            table: None,
            source: None,
            saved: None,
            sql: SqlUi::default(),
            opts,
            limits: StreamLimits::default(),
            force_stream: false,
            visible: Vec::new(),
            filter: None,
            sort: None,
            row_text: None,
            stats: None,
            col_widths: Vec::new(),
            sel_row: 0,
            sel_col: 0,
            row_off: 0,
            col_off: 0,
            mode: Mode::Normal,
            input: String::new(),
            status: None,
            browser: Browser::new(cwd),
            pending_clip: None,
            viewport_rows: 1,
            should_quit: false,
        }
    }

    /// Swap in a freshly loaded table, resetting all view state.
    fn set_table(&mut self, table: Table) {
        let n = table.num_rows();
        self.col_widths = compute_widths(&table);
        self.table = Some(table);
        self.visible = (0..n).collect();
        self.filter = None;
        self.sort = None;
        self.row_text = None;
        self.stats = None;
        self.sel_row = 0;
        self.sel_col = 0;
        self.row_off = 0;
        self.col_off = 0;
        self.input.clear();
    }

    /// Open `path`: small files are loaded into memory; files over the size
    /// limits (or any file with `force_stream`) are *streamed* — never loaded,
    /// only queried with SQL — so they can't exhaust memory. On failure the
    /// error is shown in the file browser instead of quitting.
    pub fn open_path(&mut self, path: PathBuf) {
        let info = match inspect(&path, self.opts.kind, &self.limits) {
            Ok(info) => info,
            Err(e) => return self.fail_open(e),
        };
        if info.stream || self.force_stream {
            self.open_streamed(path, info.kind, info.rows, info.cols);
            return;
        }
        match Table::load(&path, &self.opts) {
            Ok(table) => {
                self.reset_for_new_file();
                self.source = Some(Source {
                    path: path.clone(),
                    kind: table.kind,
                    rows: Some(table.num_rows()),
                    cols: Some(table.num_cols()),
                    streamed: false,
                });
                self.set_table(table);
                self.mode = Mode::Normal;
                self.status = Some(format!("opened {}", file_name(&path)));
            }
            Err(e) => self.fail_open(e),
        }
    }

    fn fail_open(&mut self, e: anyhow::Error) {
        self.browser.error = Some(crate::error_text(&e));
        self.mode = Mode::Browser;
    }

    /// Open a too-big file in SQL mode: only its first rows are read, as a
    /// preview; everything else is reached with SQL queries.
    fn open_streamed(&mut self, path: PathBuf, kind: FileKind, rows: Option<usize>, mut cols: Option<usize>) {
        self.reset_for_new_file();
        self.clear_table();
        self.mode = Mode::Normal;
        match read_head(&path, &self.opts, PREVIEW_ROWS) {
            Ok(head) => {
                cols = cols.or(Some(head.num_cols()));
                self.set_table(head);
                self.status = Some(format!(
                    "large file: showing the first {} rows — press S to search all of it with SQL",
                    group_digits(PREVIEW_ROWS)
                ));
            }
            Err(e) => self.sql.error = Some(crate::error_text(&e)),
        }
        self.source = Some(Source {
            path,
            kind,
            rows,
            cols,
            streamed: true,
        });
    }

    /// True when the table on screen is the preview of a streamed file.
    pub fn showing_preview(&self) -> bool {
        self.sql.shown.is_none() && self.table.is_some() && self.source.as_ref().is_some_and(|s| s.streamed)
    }

    /// Forget everything tied to the previous file (SQL session, results).
    fn reset_for_new_file(&mut self) {
        self.saved = None;
        let history = std::mem::take(&mut self.sql.history);
        self.sql = SqlUi {
            history,
            ..SqlUi::default()
        };
    }

    fn clear_table(&mut self) {
        self.table = None;
        self.visible.clear();
        self.col_widths.clear();
        self.filter = None;
        self.sort = None;
        self.row_text = None;
        self.stats = None;
        self.sel_row = 0;
        self.sel_col = 0;
        self.row_off = 0;
        self.col_off = 0;
    }

    /// True while an SQL query runs in the background.
    pub fn sql_running(&self) -> bool {
        self.sql.pending.is_some()
    }

    /// Whether `Esc` would go back from an SQL result to the loaded file.
    pub fn can_go_back(&self) -> bool {
        self.sql.shown.is_some() && self.saved.is_some()
    }

    /// Column names to offer for completion and hints: the file's own columns
    /// when known, otherwise those of the result on screen.
    pub fn sql_columns(&self) -> Vec<String> {
        self.saved
            .as_ref()
            .or(self.table.as_ref())
            .map(|t| t.column_names().to_vec())
            .unwrap_or_default()
    }

    fn set_sql_input(&mut self, text: String) {
        self.sql.cursor = text.chars().count();
        self.sql.input = text;
    }

    /// Start running the prompt's query on a background thread.
    pub fn run_sql(&mut self) {
        let Some(src) = &self.source else {
            self.sql.error = Some("open a file first".into());
            return;
        };
        let query = self.sql.input.trim().to_string();
        if query.is_empty() {
            return;
        }
        if self.sql.history.last() != Some(&query) {
            self.sql.history.push(query.clone());
        }
        self.sql.hist_pos = None;
        self.sql.suggestions.clear();
        self.sql.error = None;

        let (path, kind) = (src.path.clone(), src.kind);
        let (delimiter, header) = (self.opts.delimiter, self.opts.has_header);
        let engine = self.sql.engine.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let engine = match engine {
                Some(e) => Ok(e),
                None => SqlEngine::new(&path, kind, delimiter, header).map(Arc::new),
            };
            let done = match engine {
                Ok(engine) => SqlDone {
                    result: engine.query_table(&query, SQL_ROW_CAP).map_err(|e| crate::error_text(&e)),
                    engine: Some(engine),
                    query,
                },
                Err(e) => SqlDone {
                    engine: None,
                    query,
                    result: Err(crate::error_text(&e)),
                },
            };
            let _ = tx.send(done);
        });
        self.sql.pending = Some(rx);
        self.sql.started = Some(Instant::now());
    }

    /// Pick up a finished background query, if any. Call regularly from the
    /// event loop; returns true while a query is still running.
    pub fn tick(&mut self) -> bool {
        let Some(rx) = &self.sql.pending else {
            return false;
        };
        let done = match rx.try_recv() {
            Ok(done) => done,
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => SqlDone {
                engine: None,
                query: String::new(),
                result: Err("the query stopped unexpectedly".into()),
            },
        };
        self.sql.pending = None;
        self.sql.elapsed = self.sql.started.take().map(|t| t.elapsed());
        if let Some(engine) = done.engine {
            self.sql.engine = Some(engine);
        }
        match done.result {
            Ok(res) => {
                // Put the loaded file aside (once) so Esc can come back to it.
                if self.sql.shown.is_none() {
                    if let Some(t) = self.table.take() {
                        self.saved = Some(t);
                    }
                }
                self.set_table(res.table);
                self.sql.shown = Some(done.query);
                self.sql.truncated = res.truncated;
                self.sql.error = None;
            }
            Err(e) => {
                // Reopen the prompt so the query can be fixed right away.
                self.sql.error = Some(e);
                self.mode = Mode::Sql;
            }
        }
        false
    }

    /// Leave an SQL result and show the loaded file again.
    fn go_back(&mut self) {
        if let Some(t) = self.saved.take() {
            self.set_table(t);
            self.sql.shown = None;
            self.sql.truncated = false;
            self.status = Some(if self.showing_preview() {
                "back to the preview".into()
            } else {
                "back to the file".into()
            });
        }
    }

    fn open_sql_prompt(&mut self) {
        if self.source.is_none() {
            return;
        }
        if self.sql.input.trim().is_empty() {
            self.set_sql_input(format!("SELECT * FROM {} WHERE ", SqlEngine::TABLE));
        }
        self.sql.suggestions.clear();
        self.mode = Mode::Sql;
    }

    pub fn num_cols(&self) -> usize {
        self.table.as_ref().map_or(0, Table::num_cols)
    }

    pub fn num_rows(&self) -> usize {
        self.table.as_ref().map_or(0, Table::num_rows)
    }

    pub fn visible_rows(&self) -> usize {
        self.visible.len()
    }

    /// The table row index for the current selection, if any rows are visible.
    pub fn current_row(&self) -> Option<usize> {
        self.visible.get(self.sel_row).copied()
    }

    /// Statistics for every column, computed on first request.
    pub fn stats(&mut self) -> &[ColStats] {
        self.ensure_stats();
        self.stats.as_deref().unwrap_or(&[])
    }

    /// Take any text queued for the system clipboard (consumed by the run loop).
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.pending_clip.take()
    }

    // ---- input dispatch -------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        self.status = None;
        // Safety: never sit in a table mode with no file open.
        if self.source.is_none() && self.mode != Mode::Browser {
            self.mode = Mode::Browser;
        }
        match self.mode {
            Mode::Normal => self.on_key_normal(key),
            Mode::Sql => self.on_key_sql(key),
            Mode::Search => self.on_key_search(key),
            Mode::Goto => self.on_key_goto(key),
            Mode::Browser => self.on_key_browser(key),
            Mode::Help | Mode::Schema | Mode::Cell => {
                // Any of these dismiss overlays.
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.mode = Mode::Normal,
                    KeyCode::Char('?') if self.mode == Mode::Help => self.mode = Mode::Normal,
                    KeyCode::Char('i') if self.mode == Mode::Schema => self.mode = Mode::Normal,
                    _ => {}
                }
            }
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // Esc leaves an SQL result first (never quits from one — a large
            // file has no plain view to go back to); otherwise, like q, it quits.
            KeyCode::Esc if self.can_go_back() => self.go_back(),
            KeyCode::Esc if self.sql.shown.is_some() => {
                self.status = Some("press S for a new query, q to quit".into());
            }
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,

            // SQL prompt.
            KeyCode::Char('S') | KeyCode::F(5) => self.open_sql_prompt(),

            // Cursor movement.
            KeyCode::Char('j') | KeyCode::Down => self.move_row(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_row(-1),
            KeyCode::Char('h') | KeyCode::Left => self.move_col(-1),
            KeyCode::Char('l') | KeyCode::Right => self.move_col(1),

            // Paging.
            KeyCode::PageDown => self.move_row(self.viewport_rows as isize),
            KeyCode::PageUp => self.move_row(-(self.viewport_rows as isize)),
            KeyCode::Char('d') if ctrl => self.move_row(self.viewport_rows as isize / 2),
            KeyCode::Char('u') if ctrl => self.move_row(-(self.viewport_rows as isize / 2)),

            // Jump to extremes.
            KeyCode::Char('g') | KeyCode::Home => self.goto_row(0),
            KeyCode::Char('G') | KeyCode::End => self.goto_row(usize::MAX),
            KeyCode::Char('0') | KeyCode::Char('^') => self.goto_col(0),
            KeyCode::Char('$') => self.goto_col(usize::MAX),

            // Column width tweaks.
            KeyCode::Char('<') => self.resize_col(-2),
            KeyCode::Char('>') => self.resize_col(2),

            // Sorting on the current column.
            KeyCode::Char('s') => self.cycle_sort(),

            // Clipboard / export.
            KeyCode::Char('y') => self.copy_cell(),
            KeyCode::Char('Y') => self.copy_row(),
            KeyCode::Char('e') => self.export_view(),

            // Open the file browser.
            KeyCode::Char('o') => self.open_browser(),

            // Overlays / modes.
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.input = self.filter.clone().unwrap_or_default();
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Goto;
                self.input.clear();
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('i') => self.mode = Mode::Schema,
            KeyCode::Enter | KeyCode::Char(' ') => {
                if self.current_row().is_some() {
                    self.mode = Mode::Cell;
                }
            }
            KeyCode::Char('n') => {
                // Clear an active filter quickly.
                if self.filter.is_some() {
                    self.clear_filter();
                }
            }
            _ => {}
        }
    }

    fn on_key_sql(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.sql.input.chars().count();
        if key.code != KeyCode::Tab {
            self.sql.suggestions.clear();
        }
        match key.code {
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.run_sql();
            }
            KeyCode::Char('u') if ctrl => self.set_sql_input(String::new()),
            KeyCode::Left => self.sql.cursor = self.sql.cursor.saturating_sub(1),
            KeyCode::Right => self.sql.cursor = (self.sql.cursor + 1).min(len),
            KeyCode::Home => self.sql.cursor = 0,
            KeyCode::Char('a') if ctrl => self.sql.cursor = 0,
            KeyCode::End => self.sql.cursor = len,
            KeyCode::Char('e') if ctrl => self.sql.cursor = len,
            KeyCode::Backspace => {
                if self.sql.cursor > 0 {
                    let at = byte_index(&self.sql.input, self.sql.cursor - 1);
                    self.sql.input.remove(at);
                    self.sql.cursor -= 1;
                }
            }
            KeyCode::Delete => {
                if self.sql.cursor < len {
                    let at = byte_index(&self.sql.input, self.sql.cursor);
                    self.sql.input.remove(at);
                }
            }
            KeyCode::Up => self.history_step(-1),
            KeyCode::Down => self.history_step(1),
            KeyCode::Tab => self.complete_sql(),
            KeyCode::Char(c) => {
                let at = byte_index(&self.sql.input, self.sql.cursor);
                self.sql.input.insert(at, c);
                self.sql.cursor += 1;
            }
            _ => {}
        }
    }

    /// Walk the query history (Up = older, Down = newer).
    fn history_step(&mut self, delta: isize) {
        let n = self.sql.history.len();
        if n == 0 {
            return;
        }
        let next = match (self.sql.hist_pos, delta < 0) {
            (None, true) => Some(n - 1),
            (None, false) => None,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i + 1 < n => Some(i + 1),
            (Some(_), false) => None,
        };
        self.sql.hist_pos = next;
        let text = next.map(|i| self.sql.history[i].clone()).unwrap_or_default();
        self.set_sql_input(text);
    }

    /// Tab-complete the word before the cursor from the file's column names,
    /// the table name and common SQL words. One match is inserted; several are
    /// listed under the prompt (and their common prefix is filled in).
    fn complete_sql(&mut self) {
        let chars: Vec<char> = self.sql.input.chars().collect();
        let end = self.sql.cursor.min(chars.len());
        let mut start = end;
        while start > 0 && is_ident_char(chars[start - 1]) {
            start -= 1;
        }
        let word: String = chars[start..end].iter().collect();
        if word.is_empty() {
            // Nothing typed yet: just show what's available.
            self.sql.suggestions = self.sql_columns().iter().map(|c| sql_ident(c)).collect();
            return;
        }
        let lower = word.to_lowercase();
        let mut cands: Vec<String> = self
            .sql_columns()
            .iter()
            .filter(|c| c.to_lowercase().starts_with(&lower))
            .map(|c| sql_ident(c))
            .collect();
        if SqlEngine::TABLE.starts_with(&lower) {
            cands.push(SqlEngine::TABLE.to_string());
        }
        cands.extend(
            SQL_WORDS
                .iter()
                .filter(|w| w.to_lowercase().starts_with(&lower))
                .map(|w| w.to_string()),
        );
        cands.dedup();
        let replacement = match cands.len() {
            0 => return,
            1 => format!("{} ", cands[0]),
            _ => {
                let common = common_prefix_ci(&cands);
                self.sql.suggestions = cands;
                if common.chars().count() <= word.chars().count() {
                    return;
                }
                common
            }
        };
        let before: String = chars[..start].iter().collect();
        let after: String = chars[end..].iter().collect();
        self.sql.cursor = before.chars().count() + replacement.chars().count();
        self.sql.input = format!("{before}{replacement}{after}");
    }

    fn on_key_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.clear_filter();
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.input.pop();
                self.apply_filter();
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                self.apply_filter();
            }
            _ => {}
        }
    }

    fn on_key_goto(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                if let Ok(n) = self.input.trim().parse::<usize>() {
                    // 1-based for humans.
                    let target = n.saturating_sub(1);
                    if let Some(pos) = self.visible.iter().position(|&r| r >= target) {
                        self.goto_row(pos);
                    } else {
                        self.goto_row(usize::MAX);
                    }
                } else {
                    self.status = Some(format!("invalid row: {}", self.input));
                }
                self.mode = Mode::Normal;
                self.input.clear();
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => self.input.push(c),
            _ => {}
        }
    }

    fn on_key_browser(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.table.is_some() {
                    self.mode = Mode::Normal;
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.browser.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.browser.move_sel(-1),
            KeyCode::Char('g') | KeyCode::Home => self.browser.sel = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.browser.sel = self.browser.entries.len().saturating_sub(1);
            }
            KeyCode::PageDown => self.browser.move_sel(self.browser.viewport as isize),
            KeyCode::PageUp => self.browser.move_sel(-(self.browser.viewport as isize)),
            KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left => self.browser.go_parent(),
            KeyCode::Char('r') => self.browser.reload(),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right | KeyCode::Char(' ') => {
                if let Some(entry) = self.browser.selected().cloned() {
                    if entry.is_dir {
                        self.browser.enter(entry.path);
                    } else {
                        self.open_path(entry.path);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        if self.mode == Mode::Browser {
            match ev.kind {
                MouseEventKind::ScrollDown => self.browser.move_sel(1),
                MouseEventKind::ScrollUp => self.browser.move_sel(-1),
                _ => {}
            }
            return;
        }
        match ev.kind {
            MouseEventKind::ScrollDown => self.move_row(3),
            MouseEventKind::ScrollUp => self.move_row(-3),
            _ => {}
        }
    }

    fn open_browser(&mut self) {
        self.browser.error = None;
        self.browser.reload();
        self.mode = Mode::Browser;
    }

    // ---- movement helpers ----------------------------------------------

    fn move_row(&mut self, delta: isize) {
        let n = self.visible_rows();
        if n == 0 {
            return;
        }
        let cur = self.sel_row as isize;
        let next = (cur + delta).clamp(0, n as isize - 1);
        self.sel_row = next as usize;
    }

    fn move_col(&mut self, delta: isize) {
        let n = self.num_cols();
        if n == 0 {
            return;
        }
        let cur = self.sel_col as isize;
        let next = (cur + delta).clamp(0, n as isize - 1);
        self.sel_col = next as usize;
    }

    fn goto_row(&mut self, row: usize) {
        let n = self.visible_rows();
        if n == 0 {
            self.sel_row = 0;
        } else {
            self.sel_row = row.min(n - 1);
        }
    }

    fn goto_col(&mut self, col: usize) {
        let n = self.num_cols();
        if n == 0 {
            self.sel_col = 0;
        } else {
            self.sel_col = col.min(n - 1);
        }
    }

    fn resize_col(&mut self, delta: i32) {
        if let Some(w) = self.col_widths.get_mut(self.sel_col) {
            let next = (*w as i32 + delta).clamp(3, 200);
            *w = next as u16;
        }
    }

    // ---- sorting --------------------------------------------------------

    /// Cycle the current column through ascending → descending → unsorted.
    fn cycle_sort(&mut self) {
        if self.num_cols() == 0 {
            return;
        }
        let col = self.sel_col;
        let next = match self.sort {
            Some((c, SortDir::Ascending)) if c == col => Some((col, SortDir::Descending)),
            Some((c, SortDir::Descending)) if c == col => None,
            _ => Some((col, SortDir::Ascending)),
        };
        self.sort = next;
        self.apply_sort();
        self.status = Some(match self.sort {
            Some((_, SortDir::Ascending)) => "sorted ↑".into(),
            Some((_, SortDir::Descending)) => "sorted ↓".into(),
            None => "sort cleared".into(),
        });
    }

    /// Reorder `visible` in place to honour `self.sort`.
    fn apply_sort(&mut self) {
        let Some((col, dir)) = self.sort else {
            // Restore natural (filtered) order.
            self.visible.sort_unstable();
            self.clamp_selection();
            return;
        };
        let Some(table) = &self.table else { return };
        let numeric = is_numeric_type(table.column_types().get(col).map_or("", |s| s.as_str()));

        // Materialise sort keys once so the comparator stays cheap.
        let keys: Vec<SortKey> = self
            .visible
            .iter()
            .map(|&r| {
                // Genuine nulls are detected from the data, not from the
                // rendered string, so an empty-but-present value is not a null.
                if table.is_null(r, col) {
                    SortKey::Null
                } else if numeric {
                    SortKey::numeric(&table.cell(r, col))
                } else {
                    SortKey::Text(table.cell(r, col).to_lowercase())
                }
            })
            .collect();

        let mut idx: Vec<usize> = (0..self.visible.len()).collect();
        idx.sort_by(|&a, &b| {
            let (ka, kb) = (&keys[a], &keys[b]);
            // Nulls always sort last, regardless of ascending/descending.
            match (ka.is_null(), kb.is_null()) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                (false, false) => {
                    let ord = ka.cmp(kb);
                    match dir {
                        SortDir::Ascending => ord,
                        SortDir::Descending => ord.reverse(),
                    }
                }
            }
        });
        self.visible = idx.into_iter().map(|i| self.visible[i]).collect();
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        self.sel_row = self.sel_row.min(self.visible.len().saturating_sub(1));
    }

    // ---- filtering ------------------------------------------------------

    fn ensure_row_text(&mut self) {
        if self.row_text.is_some() {
            return;
        }
        let Some(table) = &self.table else {
            self.row_text = Some(Vec::new());
            return;
        };
        let rows = table.num_rows();
        let formatters = match table.formatters() {
            Ok(f) => f,
            Err(_) => {
                self.row_text = Some(vec![String::new(); rows]);
                return;
            }
        };
        let mut text = Vec::with_capacity(rows);
        for r in 0..rows {
            let mut line = String::new();
            for (c, fmt) in formatters.iter().enumerate() {
                if c > 0 {
                    line.push('\u{1f}');
                }
                line.push_str(&fmt.value(r).to_string());
            }
            text.push(line.to_lowercase());
        }
        self.row_text = Some(text);
    }

    fn ensure_stats(&mut self) {
        if self.stats.is_some() {
            return;
        }
        let Some(table) = &self.table else {
            self.stats = Some(Vec::new());
            return;
        };
        let rows = table.num_rows();
        let cols = table.num_cols();
        let types = table.column_types().to_vec();
        let formatters = match table.formatters() {
            Ok(f) => f,
            Err(_) => {
                self.stats = Some(Vec::new());
                return;
            }
        };
        let mut out = Vec::with_capacity(cols);
        for c in 0..cols {
            let numeric = is_numeric_type(&types[c]);
            let (mut count, mut nulls, mut numok) = (0usize, 0usize, 0usize);
            let (mut min, mut max, mut sum) = (f64::INFINITY, f64::NEG_INFINITY, 0.0f64);
            for r in 0..rows {
                // A genuine null (not an empty-but-present value) is a null.
                if table.is_null(r, c) {
                    nulls += 1;
                    continue;
                }
                count += 1;
                if numeric {
                    let s = formatters[c].value(r).to_string();
                    if let Ok(v) = s.replace(',', "").parse::<f64>() {
                        min = min.min(v);
                        max = max.max(v);
                        sum += v;
                        numok += 1;
                    }
                }
            }
            let num = (numok > 0).then(|| NumStats {
                min,
                max,
                mean: sum / numok as f64,
            });
            out.push(ColStats { count, nulls, num });
        }
        self.stats = Some(out);
    }

    fn apply_filter(&mut self) {
        if self.input.is_empty() {
            self.clear_filter();
            return;
        }
        self.ensure_row_text();
        let needle = self.input.to_lowercase();
        let text = self.row_text.as_ref().expect("row text built");
        self.visible = (0..self.num_rows())
            .filter(|&r| text[r].contains(&needle))
            .collect();
        self.filter = Some(self.input.clone());
        self.apply_sort();
        self.sel_row = 0;
        self.row_off = 0;
    }

    fn clear_filter(&mut self) {
        self.filter = None;
        self.input.clear();
        self.visible = (0..self.num_rows()).collect();
        self.apply_sort();
        self.clamp_selection();
    }

    // ---- clipboard / export --------------------------------------------

    fn copy_cell(&mut self) {
        let (Some(table), Some(row)) = (&self.table, self.current_row()) else {
            return;
        };
        let value = table.cell(row, self.sel_col);
        self.pending_clip = Some(value);
        self.status = Some("copied cell".into());
    }

    fn copy_row(&mut self) {
        let (Some(table), Some(row)) = (&self.table, self.current_row()) else {
            return;
        };
        let line = (0..table.num_cols())
            .map(|c| table.cell(row, c))
            .collect::<Vec<_>>()
            .join("\t");
        self.pending_clip = Some(line);
        self.status = Some("copied row".into());
    }

    /// Write the current (filtered + sorted) view to a CSV file next to the
    /// source, so what you see is what you save. Never overwrites an existing
    /// file: a numbered suffix is added if needed. Rows are streamed to disk
    /// rather than built up in one big string.
    fn export_view(&mut self) {
        let Some(table) = &self.table else { return };
        let cols = table.num_cols();

        let dest = unique_path(&export_path(&table.path));
        match self.write_view(table, cols, &dest) {
            Ok(()) => {
                self.status = Some(format!(
                    "exported {} rows → {}",
                    self.visible.len(),
                    dest.file_name().and_then(|s| s.to_str()).unwrap_or("?")
                ));
            }
            Err(e) => self.status = Some(format!("export failed: {e}")),
        }
    }

    fn write_view(&self, table: &Table, cols: usize, dest: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let file = fs::File::create(dest)?;
        let mut w = std::io::BufWriter::new(file);

        let header = table
            .column_names()
            .iter()
            .map(|s| csv_escape(s))
            .collect::<Vec<_>>()
            .join(",");
        writeln!(w, "{header}")?;
        for &r in &self.visible {
            let line = (0..cols)
                .map(|c| csv_escape(&table.cell(r, c)))
                .collect::<Vec<_>>()
                .join(",");
            writeln!(w, "{line}")?;
        }
        w.flush()
    }
}

/// A comparable sort key that orders nulls last and numbers numerically.
///
/// Integers are kept as `i128` so large values keep full precision — parsing
/// them through `f64` would make e.g. 9007199254740992 and 9007199254740993
/// compare equal.
#[derive(Debug, PartialEq)]
enum SortKey {
    Int(i128),
    Num(f64),
    Text(String),
    Null,
}

impl SortKey {
    /// Build a numeric key from a rendered cell: exact `i128` when possible,
    /// otherwise `f64`, falling back to text for anything unparseable.
    fn numeric(raw: &str) -> SortKey {
        let cleaned = raw.replace(',', "");
        if let Ok(i) = cleaned.parse::<i128>() {
            SortKey::Int(i)
        } else if let Ok(f) = cleaned.parse::<f64>() {
            SortKey::Num(f)
        } else {
            SortKey::Text(raw.to_lowercase())
        }
    }

    fn is_null(&self) -> bool {
        matches!(self, SortKey::Null)
    }

    /// Rank for ordering across kinds: numbers < text < null.
    fn rank(&self) -> u8 {
        match self {
            SortKey::Int(_) | SortKey::Num(_) => 0,
            SortKey::Text(_) => 1,
            SortKey::Null => 2,
        }
    }
}

impl Eq for SortKey {}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::Equal;
        match (self, other) {
            (SortKey::Int(a), SortKey::Int(b)) => a.cmp(b),
            (SortKey::Num(a), SortKey::Num(b)) => a.partial_cmp(b).unwrap_or(Equal),
            // Mixed int/float within one column: compare as floats.
            (SortKey::Int(a), SortKey::Num(b)) => (*a as f64).partial_cmp(b).unwrap_or(Equal),
            (SortKey::Num(a), SortKey::Int(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(Equal),
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
            // Different kinds: order by rank (number < text < null).
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|s| s.to_str()).unwrap_or("?")
}

/// Byte offset of the `ch`-th character (or the end of the string).
fn byte_index(s: &str, ch: usize) -> usize {
    s.char_indices().nth(ch).map_or(s.len(), |(i, _)| i)
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A column name as it must be written in SQL. DataFusion folds unquoted
/// identifiers to lowercase, so names with capitals, spaces or symbols need
/// double quotes.
pub fn sql_ident(name: &str) -> String {
    let plain = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// Longest prefix shared by all candidates (case-insensitive), in the casing
/// of the first one.
fn common_prefix_ci(cands: &[String]) -> String {
    let first: Vec<char> = cands[0].chars().collect();
    let mut n = first.len();
    for c in &cands[1..] {
        let m = c
            .chars()
            .zip(first.iter())
            .take_while(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
            .count();
        n = n.min(m);
    }
    first[..n].iter().collect()
}

/// Format a count with thousands separators, e.g. 10000000 → "10,000,000".
pub fn group_digits(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Quote a CSV field if it contains a comma, quote, or newline.
fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Derive the destination for an exported view: `<stem>.view.csv` beside the source.
pub(crate) fn export_path(src: &Path) -> PathBuf {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("tessera");
    let name = format!("{stem}.view.csv");
    match src.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(name),
        _ => PathBuf::from(name),
    }
}

/// Return `base` if it doesn't exist yet, otherwise the first free
/// `<stem>.N.<ext>` variant — so an export never clobbers an existing file.
pub(crate) fn unique_path(base: &Path) -> PathBuf {
    if !base.exists() {
        return base.to_path_buf();
    }
    let dir = base.parent();
    let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("export");
    let ext = base.extension().and_then(|s| s.to_str()).unwrap_or("csv");
    for n in 1.. {
        let name = format!("{stem}.{n}.{ext}");
        let candidate = match dir {
            Some(d) if !d.as_os_str().is_empty() => d.join(&name),
            _ => PathBuf::from(&name),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("an unused filename always exists")
}

// ---- file browser ------------------------------------------------------

/// One entry in the file browser listing.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

/// A minimal directory browser for opening files from inside the TUI.
pub struct Browser {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub sel: usize,
    pub offset: usize,
    pub error: Option<String>,
    /// Visible row count, kept in sync by the renderer for paging.
    pub viewport: usize,
}

impl Browser {
    pub fn new(cwd: PathBuf) -> Browser {
        let mut b = Browser {
            cwd,
            entries: Vec::new(),
            sel: 0,
            offset: 0,
            error: None,
            viewport: 10,
        };
        b.reload();
        b
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.sel)
    }

    fn move_sel(&mut self, delta: isize) {
        let n = self.entries.len();
        if n == 0 {
            return;
        }
        let cur = self.sel as isize;
        self.sel = (cur + delta).clamp(0, n as isize - 1) as usize;
    }

    fn go_parent(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            self.enter(parent);
        }
    }

    fn enter(&mut self, dir: PathBuf) {
        self.cwd = dir;
        self.sel = 0;
        self.offset = 0;
        self.reload();
    }

    /// Re-read the current directory: parent first, then sub-directories, then
    /// recognised data files — each group sorted by name.
    pub fn reload(&mut self) {
        let mut dirs: Vec<Entry> = Vec::new();
        let mut files: Vec<Entry> = Vec::new();

        match fs::read_dir(&self.cwd) {
            Ok(rd) => {
                for entry in rd.flatten() {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue; // hide dotfiles
                    }
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    if is_dir {
                        dirs.push(Entry { name, path, is_dir: true });
                    } else if FileKind::from_path(&path).is_some() {
                        files.push(Entry { name, path, is_dir: false });
                    }
                }
                self.error = None;
            }
            Err(e) => {
                self.error = Some(format!("cannot read directory: {e}"));
            }
        }

        dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        let mut entries = Vec::with_capacity(dirs.len() + files.len() + 1);
        if let Some(parent) = self.cwd.parent() {
            entries.push(Entry {
                name: "..".to_string(),
                path: parent.to_path_buf(),
                is_dir: true,
            });
        }
        entries.extend(dirs);
        entries.extend(files);

        self.entries = entries;
        self.sel = self.sel.min(self.entries.len().saturating_sub(1));
    }
}

/// Compute a sensible per-column width from the header and a sample of rows.
fn compute_widths(table: &Table) -> Vec<u16> {
    const MAX_W: usize = 48;
    const MIN_W: usize = 3;
    const SAMPLE: usize = 200;

    let cols = table.num_cols();
    let rows = table.num_rows();
    let mut widths = Vec::with_capacity(cols);

    let formatters = table.formatters().ok();
    for c in 0..cols {
        let mut w = table.column_names()[c].chars().count();
        if let Some(fmts) = &formatters {
            let take = rows.min(SAMPLE);
            for r in 0..take {
                let len = fmts[c].value(r).to_string().chars().count();
                if len > w {
                    w = len;
                }
                if w >= MAX_W {
                    break;
                }
            }
        }
        widths.push(w.clamp(MIN_W, MAX_W) as u16);
    }
    widths
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn sort_keys_keep_large_integer_precision() {
        // Beyond 2^53 these are indistinguishable as f64, but must compare
        // correctly as exact integers.
        let a = SortKey::numeric("9007199254740992");
        let b = SortKey::numeric("9007199254740993");
        assert!(matches!(a, SortKey::Int(_)));
        assert_eq!(a.cmp(&b), Ordering::Less);
        assert_ne!(a, b);
    }

    #[test]
    fn nulls_rank_after_values() {
        assert_eq!(SortKey::numeric("1").cmp(&SortKey::Null), Ordering::Less);
        assert_eq!(
            SortKey::Text("z".into()).cmp(&SortKey::Null),
            Ordering::Less
        );
    }

    #[test]
    fn sql_ident_quotes_only_when_needed() {
        assert_eq!(sql_ident("price"), "price");
        assert_eq!(sql_ident("order_id2"), "order_id2");
        assert_eq!(sql_ident("UserName"), "\"UserName\"");
        assert_eq!(sql_ident("first name"), "\"first name\"");
        assert_eq!(sql_ident("2col"), "\"2col\"");
        assert_eq!(sql_ident("売上"), "\"売上\"");
    }

    #[test]
    fn groups_digits() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(12_000_000), "12,000,000");
    }

    #[test]
    fn unique_path_avoids_overwrite() {
        let mut base = std::env::temp_dir();
        base.push(format!("tessera_unique_{}.view.csv", std::process::id()));

        // Nothing there yet → returns the base path.
        assert_eq!(unique_path(&base), base);

        // Once it exists, the next call picks a numbered variant.
        std::fs::write(&base, "x").unwrap();
        let next = unique_path(&base);
        assert_ne!(next, base);
        assert!(next.to_string_lossy().contains(".1."));
        std::fs::remove_file(&base).ok();
    }
}
