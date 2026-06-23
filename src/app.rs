//! Application state and input handling — the "brain" of the viewer.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use crate::data::Table;

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
    /// Schema / column overview overlay.
    Schema,
    /// Full-cell inspector for the selected cell.
    Cell,
}

pub struct App {
    pub table: Table,

    /// Row indices (into the table) currently visible, honouring any filter.
    pub visible: Vec<usize>,
    /// When `Some`, `visible` is a filtered subset and this is the query.
    pub filter: Option<String>,
    /// Lazily-built lowercase text for each row, used for filtering.
    row_text: Option<Vec<String>>,

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

    /// Number of data rows that fit in the current viewport (updated on draw).
    pub viewport_rows: usize,
    pub should_quit: bool,
}

impl App {
    pub fn new(table: Table) -> App {
        let n = table.num_rows();
        let col_widths = compute_widths(&table);
        App {
            table,
            visible: (0..n).collect(),
            filter: None,
            row_text: None,
            col_widths,
            sel_row: 0,
            sel_col: 0,
            row_off: 0,
            col_off: 0,
            mode: Mode::Normal,
            input: String::new(),
            status: None,
            viewport_rows: 1,
            should_quit: false,
        }
    }

    pub fn visible_rows(&self) -> usize {
        self.visible.len()
    }

    /// The table row index for the current selection, if any rows are visible.
    pub fn current_row(&self) -> Option<usize> {
        self.visible.get(self.sel_row).copied()
    }

    // ---- input dispatch -------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) {
        self.status = None;
        match self.mode {
            Mode::Normal => self.on_key_normal(key),
            Mode::Search => self.on_key_search(key),
            Mode::Goto => self.on_key_goto(key),
            Mode::Help | Mode::Schema | Mode::Cell => {
                // Any of these dismiss overlays.
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => self.mode = Mode::Normal,
                    KeyCode::Char('?') if self.mode == Mode::Help => self.mode = Mode::Normal,
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

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        match ev.kind {
            MouseEventKind::ScrollDown => self.move_row(3),
            MouseEventKind::ScrollUp => self.move_row(-3),
            _ => {}
        }
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
        let n = self.table.num_cols();
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
        let n = self.table.num_cols();
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

    // ---- filtering ------------------------------------------------------

    fn ensure_row_text(&mut self) {
        if self.row_text.is_some() {
            return;
        }
        let rows = self.table.num_rows();
        let cols = self.table.num_cols();
        let formatters = match self.table.formatters() {
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
        let _ = cols;
        self.row_text = Some(text);
    }

    fn apply_filter(&mut self) {
        if self.input.is_empty() {
            self.clear_filter();
            return;
        }
        self.ensure_row_text();
        let needle = self.input.to_lowercase();
        let text = self.row_text.as_ref().expect("row text built");
        self.visible = (0..self.table.num_rows())
            .filter(|&r| text[r].contains(&needle))
            .collect();
        self.filter = Some(self.input.clone());
        self.sel_row = 0;
        self.row_off = 0;
    }

    fn clear_filter(&mut self) {
        self.filter = None;
        self.input.clear();
        self.visible = (0..self.table.num_rows()).collect();
        self.sel_row = self.sel_row.min(self.visible.len().saturating_sub(1));
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
