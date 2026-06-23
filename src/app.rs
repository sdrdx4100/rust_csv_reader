//! Application state and input handling — the "brain" of the viewer.

use std::fs;
use std::path::{Path, PathBuf};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use crate::data::{is_numeric_type, FileKind, LoadOptions, Table};

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
    /// The currently open table, or `None` when only the file browser is shown.
    pub table: Option<Table>,
    /// Load options reused when opening files from the browser.
    pub opts: LoadOptions,

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
            opts,
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

    /// Load `path` with the current options; on success switch to it, otherwise
    /// surface the error in the browser.
    pub fn open_path(&mut self, path: PathBuf) {
        match Table::load(&path, &self.opts) {
            Ok(table) => {
                self.set_table(table);
                self.mode = Mode::Normal;
                self.status = Some(format!(
                    "opened {}",
                    path.file_name().and_then(|s| s.to_str()).unwrap_or("?")
                ));
            }
            Err(e) => {
                self.browser.error = Some(format!("{e:#}"));
            }
        }
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
        // Safety: never sit in a table mode without a table loaded.
        if self.table.is_none() && self.mode != Mode::Browser {
            self.mode = Mode::Browser;
        }
        match self.mode {
            Mode::Normal => self.on_key_normal(key),
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
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,

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
                let raw = table.cell(r, col);
                if raw.is_empty() {
                    SortKey::Null
                } else if numeric {
                    match raw.replace(',', "").parse::<f64>() {
                        Ok(v) => SortKey::Num(v),
                        Err(_) => SortKey::Text(raw.to_lowercase()),
                    }
                } else {
                    SortKey::Text(raw.to_lowercase())
                }
            })
            .collect();

        let mut idx: Vec<usize> = (0..self.visible.len()).collect();
        idx.sort_by(|&a, &b| {
            let ord = keys[a].cmp(&keys[b]);
            match dir {
                SortDir::Ascending => ord,
                SortDir::Descending => ord.reverse(),
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
                let s = formatters[c].value(r).to_string();
                if s.is_empty() {
                    nulls += 1;
                    continue;
                }
                count += 1;
                if numeric {
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
    /// source, so what you see is what you save.
    fn export_view(&mut self) {
        let Some(table) = &self.table else { return };
        let cols = table.num_cols();
        let names = table.column_names();

        let mut out = String::new();
        out.push_str(
            &names
                .iter()
                .map(|s| csv_escape(s))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
        for &r in &self.visible {
            let line = (0..cols)
                .map(|c| csv_escape(&table.cell(r, c)))
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&line);
            out.push('\n');
        }

        let dest = export_path(&table.path);
        match fs::write(&dest, out) {
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
}

/// A comparable sort key that orders nulls last and numbers numerically.
#[derive(PartialEq)]
enum SortKey {
    Num(f64),
    Text(String),
    Null,
}

impl SortKey {
    fn rank(&self) -> u8 {
        match self {
            SortKey::Num(_) => 0,
            SortKey::Text(_) => 1,
            SortKey::Null => 2,
        }
    }
}

impl Eq for SortKey {}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (SortKey::Num(a), SortKey::Num(b)) => a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
            // Different kinds: order by rank (num < text < null).
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
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
