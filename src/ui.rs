//! Terminal rendering. The data grid is drawn by hand (rather than via the
//! built-in `Table` widget) so we get precise control over horizontal
//! scrolling, a frozen header row, a row-number gutter and per-cell styling.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{group_digits, sql_ident, App, ColStats, Mode, SortDir, PREVIEW_ROWS, SQL_ROW_CAP};
use crate::sql::SqlEngine;
use crate::data::is_numeric_type;

// A restrained 256-colour palette; degrades gracefully on most terminals.
const C_HEADER_BG: Color = Color::Indexed(24);
const C_HEADER_FG: Color = Color::Indexed(231);
const C_SELCOL_BG: Color = Color::Indexed(31);
const C_ROW_BG: Color = Color::Indexed(236);
const C_ZEBRA_BG: Color = Color::Indexed(234);
const C_CELL_BG: Color = Color::Indexed(25);
const C_GUTTER_FG: Color = Color::Indexed(244);
const C_GUTTER_SEL: Color = Color::Indexed(214);
const C_STATUS_BG: Color = Color::Indexed(238);
const C_STATUS_FG: Color = Color::Indexed(231);
const C_ACCENT: Color = Color::Indexed(39);
const C_DIM: Color = Color::Indexed(245);

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // The file browser (and the state with no file open) takes the screen.
    if app.mode == Mode::Browser || app.source.is_none() {
        render_browser(f, app, area);
        return;
    }

    // The SQL prompt opens as a 4-line panel above the status bar.
    let sql_h = if app.mode == Mode::Sql { 4 } else { 0 };
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(sql_h),
        Constraint::Length(1),
    ])
    .split(area);

    render_title(f, app, chunks[0]);
    if app.table.is_some() {
        render_grid(f, app, chunks[1]);
    } else {
        render_placeholder(f, app, chunks[1]);
    }
    if app.mode == Mode::Sql {
        render_sql_panel(f, app, chunks[2]);
    }
    render_status(f, app, chunks[3]);

    match app.mode {
        Mode::Help => render_help(f, area),
        Mode::Schema => render_schema(f, app, area),
        Mode::Cell => render_cell(f, app, area),
        _ => {}
    }
}

fn render_title(f: &mut Frame, app: &App, area: Rect) {
    let Some(src) = app.source.as_ref() else {
        return;
    };
    let name = src.path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    let rows = src.rows.map_or("?".to_string(), group_digits);
    let cols = src.cols.map_or("?".to_string(), |c| c.to_string());
    let mut spans = vec![
        Span::styled(" Tessera ", Style::default().fg(Color::Black).bg(C_ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(format!("[{}]", src.kind.label()), Style::default().fg(C_ACCENT)),
        Span::raw("  "),
        Span::styled(format!("{rows} rows × {cols} cols"), Style::default().fg(C_DIM)),
    ];
    if src.streamed {
        spans.push(Span::styled("  not loaded (SQL mode)", Style::default().fg(C_GUTTER_SEL)));
    }
    // What the grid is showing: the file, its preview, or an SQL result.
    if let (Some(_), Some(t)) = (&app.sql.shown, &app.table) {
        let mut info = format!("  │ result {} rows", group_digits(t.num_rows()));
        if app.sql.truncated {
            info.push_str(&format!(" (first {})", group_digits(SQL_ROW_CAP)));
        }
        if let Some(d) = app.sql.elapsed {
            info.push_str(&format!(" · {:.2}s", d.as_secs_f32()));
        }
        spans.push(Span::styled(info, Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)));
    } else if app.showing_preview() {
        spans.push(Span::styled(
            format!("  │ preview: first {} rows", group_digits(PREVIEW_ROWS)),
            Style::default().fg(C_ACCENT),
        ));
    }
    if let Some(t) = app.sql.started {
        spans.push(Span::styled(
            format!("  ⏳ running query… {:.1}s", t.elapsed().as_secs_f32()),
            Style::default().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Shown in place of the grid while a streamed file has no result yet.
fn render_placeholder(f: &mut Frame, app: &App, area: Rect) {
    let msg = if app.sql_running() {
        "  ⏳ running the query… (the file is read on demand, nothing is loaded)".to_string()
    } else if let Some(err) = &app.sql.error {
        format!("  {}  — press S to query the file with SQL.", one_line(err))
    } else {
        "  Press S to query the file with SQL.".to_string()
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(C_DIM)))), area);
}

/// The SQL prompt: the query being edited, plus a line of hints, completion
/// candidates or the last error.
fn render_sql_panel(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(format!(" SQL — this file is the table `{}` ", SqlEngine::TABLE))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Query line with a block cursor, scrolled so the cursor stays visible.
    let chars: Vec<char> = app.sql.input.chars().collect();
    let cur = app.sql.cursor.min(chars.len());
    let width = inner.width.saturating_sub(3) as usize; // "> " + cursor cell
    let start = cur.saturating_sub(width.saturating_sub(1));
    let end = (start + width).min(chars.len());
    let before: String = chars[start..cur].iter().collect();
    let at: String = chars.get(cur).map_or(" ".to_string(), |c| c.to_string());
    let after: String = if cur < end { chars[cur + 1..end].iter().collect() } else { String::new() };
    let query_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(before, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(at, Style::default().bg(C_ACCENT).fg(Color::Black)),
        Span::styled(after, Style::default().add_modifier(Modifier::BOLD)),
    ]);

    let info_line = if let Some(err) = &app.sql.error {
        Line::from(Span::styled(format!("error: {}", one_line(err)), Style::default().fg(Color::Red)))
    } else if !app.sql.suggestions.is_empty() {
        Line::from(vec![
            Span::styled("Tab: ", Style::default().fg(C_DIM)),
            Span::styled(app.sql.suggestions.join("  "), Style::default().fg(C_GUTTER_SEL)),
        ])
    } else {
        let cols = app.sql_columns();
        let cols = if cols.is_empty() {
            "(Tab to list columns)".to_string()
        } else {
            cols.iter().map(|c| sql_ident(c)).collect::<Vec<_>>().join(", ")
        };
        Line::from(vec![
            Span::styled("Enter run · Esc close · Tab complete · ↑↓ history · Ctrl-u clear   ", Style::default().fg(C_DIM)),
            Span::styled(format!("columns: {cols}"), Style::default().fg(C_GUTTER_FG)),
        ])
    };
    f.render_widget(Paragraph::new(vec![query_line, info_line]), inner);
}

/// Collapse a (possibly multi-line) error message onto one line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn render_grid(f: &mut Frame, app: &mut App, area: Rect) {
    if area.height < 2 || area.width < 4 {
        return;
    }
    let num_cols = app.num_cols();
    let num_vis = app.visible_rows();

    // Gutter width: enough for the largest 1-based row number, plus a space.
    let max_label = app.num_rows().max(1);
    let gutter = (digits(max_label) + 1).max(4);

    // --- vertical scroll: keep the selected row in view --------------------
    let data_h = area.height.saturating_sub(1) as usize; // minus header row
    app.viewport_rows = data_h.max(1);
    if num_vis > 0 {
        if app.sel_row < app.row_off {
            app.row_off = app.sel_row;
        }
        if app.sel_row >= app.row_off + app.viewport_rows {
            app.row_off = app.sel_row + 1 - app.viewport_rows;
        }
        if app.row_off > num_vis.saturating_sub(1) {
            app.row_off = num_vis.saturating_sub(1);
        }
    } else {
        app.row_off = 0;
    }

    // --- horizontal scroll: keep the selected column in view ---------------
    if num_cols > 0 {
        if app.sel_col < app.col_off {
            app.col_off = app.sel_col;
        }
        let avail = area.width.saturating_sub(gutter);
        loop {
            let last = last_visible_col(&app.col_widths, app.col_off, avail, num_cols);
            if app.sel_col <= last || app.col_off >= app.sel_col {
                break;
            }
            app.col_off += 1;
        }
    }

    let avail = area.width.saturating_sub(gutter);
    let visible_cols = visible_col_list(&app.col_widths, app.col_off, avail, num_cols);

    // All reads below borrow the table immutably; scroll mutation is done.
    let table = app.table.as_ref().expect("table present in grid render");
    let sort = app.sort;

    // --- header row --------------------------------------------------------
    let mut header_spans = Vec::with_capacity(visible_cols.len() + 1);
    header_spans.push(Span::styled(
        fit_right("#", gutter as usize - 1) + " ",
        Style::default().bg(C_HEADER_BG).fg(C_HEADER_FG).add_modifier(Modifier::BOLD),
    ));
    let names = table.column_names();
    for &c in &visible_cols {
        let w = app.col_widths[c] as usize;
        let bg = if c == app.sel_col { C_SELCOL_BG } else { C_HEADER_BG };
        let arrow = match sort {
            Some((sc, SortDir::Ascending)) if sc == c => "↑",
            Some((sc, SortDir::Descending)) if sc == c => "↓",
            _ => "",
        };
        let label = if arrow.is_empty() {
            fit(names[c].as_str(), w)
        } else {
            // Reserve one column for the arrow so it never gets clipped away.
            fit(&format!("{}{}", names[c], arrow), w)
        };
        header_spans.push(Span::styled(
            label + " ",
            Style::default().bg(bg).fg(C_HEADER_FG).add_modifier(Modifier::BOLD),
        ));
    }
    let header_area = Rect { height: 1, ..area };
    f.render_widget(
        Paragraph::new(Line::from(header_spans)).style(Style::default().bg(C_HEADER_BG)),
        header_area,
    );

    // --- data rows ---------------------------------------------------------
    let types = table.column_types();
    let mut lines = Vec::with_capacity(app.viewport_rows);
    for i in 0..app.viewport_rows {
        let view_row = app.row_off + i;
        if view_row >= num_vis {
            break;
        }
        let table_row = app.visible[view_row];
        let selected_line = view_row == app.sel_row;

        let mut spans = Vec::with_capacity(visible_cols.len() + 1);
        let gstyle = if selected_line {
            Style::default().fg(C_GUTTER_SEL).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_GUTTER_FG)
        };
        spans.push(Span::styled(fit_right(&(table_row + 1).to_string(), gutter as usize - 1) + " ", gstyle));

        for &c in &visible_cols {
            let w = app.col_widths[c] as usize;
            let raw = table.cell(table_row, c);
            let numeric = is_numeric_type(&types[c]);
            let text = if numeric { fit_right(&raw, w) } else { fit(&raw, w) };
            let mut style = Style::default();
            if selected_line && c == app.sel_col {
                style = style.bg(C_CELL_BG).fg(Color::White).add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(text + " ", style));
        }

        let mut line = Line::from(spans);
        if selected_line {
            line = line.style(Style::default().bg(C_ROW_BG));
        } else if view_row % 2 == 1 {
            // Subtle zebra striping for readability across wide rows.
            line = line.style(Style::default().bg(C_ZEBRA_BG));
        }
        lines.push(line);
    }

    if num_vis == 0 {
        let msg = if app.filter.is_some() {
            "no rows match the current filter"
        } else {
            "(empty)"
        };
        lines.push(Line::from(Span::styled(msg, Style::default().fg(C_DIM))));
    }

    let body_area = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    f.render_widget(Paragraph::new(lines), body_area);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let base = Style::default().bg(C_STATUS_BG).fg(C_STATUS_FG);
    let line = match app.mode {
        Mode::Search => Line::from(vec![
            Span::styled(" filter ", Style::default().bg(C_ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD)),
            Span::raw(" /"),
            Span::styled(app.input.clone(), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("█", Style::default().fg(C_ACCENT)),
            Span::styled("   Esc clear · Enter keep", Style::default().fg(C_DIM)),
        ]),
        Mode::Goto => Line::from(vec![
            Span::styled(" go to ", Style::default().bg(C_ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD)),
            Span::raw(" :"),
            Span::styled(app.input.clone(), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("█", Style::default().fg(C_ACCENT)),
            Span::styled("   Enter jump · Esc cancel", Style::default().fg(C_DIM)),
        ]),
        _ => {
            let row_disp = if app.visible_rows() == 0 {
                "0/0".to_string()
            } else {
                format!("{}/{}", app.sel_row + 1, app.visible_rows())
            };
            let col_name = app
                .table
                .as_ref()
                .and_then(|t| t.column_names().get(app.sel_col))
                .map(|s| s.as_str())
                .unwrap_or("-");
            let mut spans = vec![
                Span::styled(" row ", Style::default().fg(C_DIM)),
                Span::styled(row_disp, Style::default().add_modifier(Modifier::BOLD)),
                Span::styled("  col ", Style::default().fg(C_DIM)),
                Span::styled(
                    format!("{}/{} {}", app.sel_col + 1, app.num_cols(), col_name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some((_, dir)) = app.sort {
                let a = if dir == SortDir::Ascending { "↑" } else { "↓" };
                spans.push(Span::styled(format!("  sort {a}"), Style::default().fg(C_GUTTER_SEL)));
            }
            if let Some(q) = &app.filter {
                spans.push(Span::styled(
                    format!("  filter “{}” → {} rows", q, app.visible_rows()),
                    Style::default().fg(C_ACCENT),
                ));
            }
            if let Some(msg) = &app.status {
                spans.push(Span::styled(format!("  {msg}"), Style::default().fg(Color::Yellow)));
            }
            if app.can_go_back() {
                let to = if app.source.as_ref().is_some_and(|s| s.streamed) { "preview" } else { "file" };
                spans.push(Span::styled(format!("   Esc back to {to}"), Style::default().fg(C_ACCENT)));
            }
            spans.push(Span::styled(
                "   S sql  /find  s sort  o open  y copy  e export  ? help",
                Style::default().fg(C_DIM),
            ));
            Line::from(spans)
        }
    };
    f.render_widget(Paragraph::new(line).style(base), area);
}

fn render_browser(f: &mut Frame, app: &mut App, area: Rect) {
    f.render_widget(Clear, area);

    let title = format!(" Open file · {} ", app.browser.cwd.display());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_ACCENT));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Reserve the last inner row for a hint / error line.
    let list_h = inner.height.saturating_sub(1) as usize;
    app.browser.viewport = list_h.max(1);

    // Keep the selection within the visible window.
    let n = app.browser.entries.len();
    if app.browser.sel < app.browser.offset {
        app.browser.offset = app.browser.sel;
    } else if list_h > 0 && app.browser.sel >= app.browser.offset + list_h {
        app.browser.offset = app.browser.sel + 1 - list_h;
    }
    if app.browser.offset > n.saturating_sub(1) {
        app.browser.offset = n.saturating_sub(1);
    }

    let mut lines = Vec::with_capacity(list_h);
    if n == 0 {
        lines.push(Line::from(Span::styled(
            "  (no sub-folders or CSV/Parquet files here — Backspace to go up)",
            Style::default().fg(C_DIM),
        )));
    }
    for i in 0..list_h {
        let idx = app.browser.offset + i;
        if idx >= n {
            break;
        }
        let entry = &app.browser.entries[idx];
        let selected = idx == app.browser.sel;
        let marker = if selected { "▸ " } else { "  " };
        let label = if entry.is_dir {
            format!("{}{}/", marker, entry.name)
        } else {
            format!("{}{}", marker, entry.name)
        };
        let mut style = if entry.is_dir {
            Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_HEADER_FG)
        };
        if selected {
            style = style.bg(C_ROW_BG).add_modifier(Modifier::BOLD);
        }
        lines.push(Line::from(Span::styled(label, style)));
    }

    let list_area = Rect { height: inner.height.saturating_sub(1), ..inner };
    f.render_widget(Paragraph::new(lines), list_area);

    let hint = if let Some(err) = &app.browser.error {
        Line::from(Span::styled(format!("  {err}"), Style::default().fg(Color::Yellow)))
    } else {
        Line::from(Span::styled(
            "  ↑↓ move · Enter open · Backspace up · q close",
            Style::default().fg(C_DIM),
        ))
    };
    let hint_area = Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner };
    f.render_widget(Paragraph::new(hint), hint_area);
}

fn render_help(f: &mut Frame, area: Rect) {
    let lines = vec![
        section("Movement"),
        kv("h j k l / arrows", "move cursor by cell"),
        kv("g / G", "first / last row"),
        kv("0 / $", "first / last column"),
        kv("PgUp / PgDn", "page up / down"),
        kv("Ctrl-u / Ctrl-d", "half page up / down"),
        kv("mouse wheel", "scroll rows"),
        Line::raw(""),
        section("View"),
        kv("Enter / Space", "inspect full cell value"),
        kv("i", "schema + column statistics"),
        kv("s", "sort by column (asc → desc → off)"),
        kv("< / >", "shrink / grow current column"),
        Line::raw(""),
        section("Find"),
        kv("/", "incremental filter (all columns)"),
        kv("n", "clear active filter"),
        kv(":", "go to row number"),
        Line::raw(""),
        section("SQL (the file is table `data`)"),
        kv("S / F5", "open the SQL prompt — Enter runs it"),
        kv("Tab  ↑↓", "complete names · query history"),
        kv("Esc", "back from a result to the file"),
        kv("e.g.", "SELECT * FROM data WHERE name LIKE '%foo%'"),
        kv("", "SELECT city, COUNT(*) FROM data GROUP BY city"),
        Line::raw(""),
        section("Files & data"),
        kv("o", "open another file (browser)"),
        kv("y / Y", "copy cell / row to clipboard"),
        kv("e", "export current view to CSV"),
        Line::raw(""),
        section("Other"),
        kv("? ", "toggle this help"),
        kv("q / Ctrl-c", "quit (Esc too, outside a result)"),
        Line::raw(""),
        Line::from(Span::styled("  press any key to close ", Style::default().fg(C_DIM))),
    ];
    let popup = centered_rect(70, 96, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_ACCENT));
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_schema(f: &mut Frame, app: &mut App, area: Rect) {
    let stats: Vec<ColStats> = app.stats().to_vec();
    let Some(table) = app.table.as_ref() else {
        return;
    };
    let names = table.column_names();
    let types = table.column_types();

    let mut lines = Vec::with_capacity(names.len() + 1);
    lines.push(Line::from(vec![
        Span::styled(format!("  {:>3}  ", "#"), Style::default().fg(C_DIM)),
        Span::styled(format!("{:<22}", "column"), Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!("{:<11}", "type"), Style::default().add_modifier(Modifier::BOLD)),
        Span::styled("stats", Style::default().add_modifier(Modifier::BOLD)),
    ]));
    for (i, (n, t)) in names.iter().zip(types).enumerate() {
        let sel = i == app.sel_col;
        let style = if sel {
            Style::default().fg(C_GUTTER_SEL).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let stat = stats.get(i).map(format_stats).unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("  {:>3}  ", i + 1), Style::default().fg(C_DIM)),
            Span::styled(format!("{:<22}", truncate(n, 22)), style),
            Span::styled(format!("{:<11}", truncate(t, 11)), Style::default().fg(C_ACCENT)),
            Span::styled(stat, Style::default().fg(C_DIM)),
        ]));
    }
    let popup = centered_rect(78, 82, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(format!(" Schema · {} columns × {} rows ", names.len(), table.num_rows()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_ACCENT));
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// One-line statistics summary for a column.
fn format_stats(s: &ColStats) -> String {
    let mut out = format!("n={} nulls={}", s.count, s.nulls);
    if let Some(num) = &s.num {
        out.push_str(&format!(
            "  min={} max={} mean={}",
            trim_num(num.min),
            trim_num(num.max),
            trim_num(num.mean),
        ));
    }
    out
}

/// Render a float compactly: drop the decimal part when it is a whole number.
fn trim_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.3}")
    }
}

fn render_cell(f: &mut Frame, app: &App, area: Rect) {
    let (Some(table), Some(table_row)) = (app.table.as_ref(), app.current_row()) else {
        return;
    };
    let col = app.sel_col;
    let name = table.column_names().get(col).cloned().unwrap_or_default();
    let ty = table.column_types().get(col).cloned().unwrap_or_default();
    let value = table.cell(table_row, col);
    let shown = if value.is_empty() { "(null)".to_string() } else { value };

    let header = Line::from(vec![
        Span::styled(name, Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(format!("[{ty}]"), Style::default().fg(C_DIM)),
        Span::raw("  "),
        Span::styled(format!("row {}", table_row + 1), Style::default().fg(C_DIM)),
    ]);
    let body = vec![
        header,
        Line::raw(""),
        Line::raw(shown),
        Line::raw(""),
        Line::from(Span::styled("y copy · any key close", Style::default().fg(C_DIM))),
    ];
    let popup = centered_rect(70, 60, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(" Cell ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_ACCENT));
    f.render_widget(
        Paragraph::new(body).block(block).wrap(Wrap { trim: false }),
        popup,
    );
}

// ---- small rendering helpers -------------------------------------------

fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {title}"),
        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    ))
}

fn kv(key: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {key:<18}"), Style::default().fg(C_GUTTER_SEL)),
        Span::styled(desc.to_string(), Style::default()),
    ])
}

fn digits(mut n: usize) -> u16 {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

/// Fit `s` into exactly `width` columns, left-aligned, padding with spaces and
/// truncating long values with an ellipsis.
fn fit(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        let mut out = String::with_capacity(width);
        out.push_str(s);
        out.extend(std::iter::repeat_n(' ', width - count));
        out
    } else if width == 0 {
        String::new()
    } else if width == 1 {
        "…".to_string()
    } else {
        let mut out: String = s.chars().take(width - 1).collect();
        out.push('…');
        out
    }
}

/// Like [`fit`] but right-aligned, for numeric columns.
fn fit_right(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count <= width {
        let mut out = String::with_capacity(width);
        out.extend(std::iter::repeat_n(' ', width - count));
        out.push_str(s);
        out
    } else {
        fit(s, width)
    }
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else if width <= 1 {
        "…".to_string()
    } else {
        let mut out: String = s.chars().take(width - 1).collect();
        out.push('…');
        out
    }
}

/// Index of the last column that fits starting from `start`.
#[allow(clippy::needless_range_loop)]
fn last_visible_col(widths: &[u16], start: usize, avail: u16, num_cols: usize) -> usize {
    let mut used = 0u16;
    let mut last = start;
    for c in start..num_cols {
        let w = widths[c] + 1;
        if used + w > avail && c > start {
            break;
        }
        used = used.saturating_add(w);
        last = c;
    }
    last
}

#[allow(clippy::needless_range_loop)]
fn visible_col_list(widths: &[u16], start: usize, avail: u16, num_cols: usize) -> Vec<usize> {
    let mut used = 0u16;
    let mut cols = Vec::new();
    for c in start..num_cols {
        let w = widths[c] + 1;
        if used + w > avail && !cols.is_empty() {
            break;
        }
        used = used.saturating_add(w);
        cols.push(c);
    }
    cols
}

/// A `Rect` centred within `area`, sized as a percentage of it.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{LoadOptions, Table};
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use std::io::Write;

    /// A 50-row CSV on disk (kept, since SQL reads the file itself) and an app
    /// with it opened. The caller deletes the file.
    fn file_app(stream: bool) -> (App, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tessera_sqlui_{}_{}.csv",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "id,name,score").unwrap();
        for i in 0..50 {
            writeln!(f, "{i},name{i},{}", i * 10).unwrap();
        }
        f.flush().unwrap();

        let mut app = App::browser_only(LoadOptions::default(), std::env::temp_dir());
        if stream {
            app.limits.max_csv_bytes = 1; // anything counts as "too big"
        }
        app.open_path(p.clone());
        (app, p)
    }

    fn key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
    }

    fn ctrl(app: &mut App, c: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn type_str(app: &mut App, text: &str) {
        for c in text.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    /// Wait for the background query to finish, redrawing like the real loop.
    fn wait_sql(app: &mut App) {
        for _ in 0..500 {
            draw(app);
            if !app.tick() {
                draw(app);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("query did not finish");
    }

    /// Replace the prompt's text with `sql` and run it.
    fn run_query(app: &mut App, sql: &str) {
        key(app, KeyCode::Char('S'));
        assert_eq!(app.mode, Mode::Sql);
        draw(app);
        ctrl(app, 'u');
        type_str(app, sql);
        key(app, KeyCode::Enter);
        wait_sql(app);
    }

    #[test]
    fn big_file_opens_in_sql_mode_without_loading() {
        let (mut app, path) = file_app(true);
        let src = app.source.clone().expect("source");
        assert!(src.streamed);
        assert_eq!(app.mode, Mode::Normal);
        // Only a preview of the first rows was read; no query is running.
        assert!(app.showing_preview());
        assert!(!app.sql_running());
        assert_eq!(app.num_rows(), 50);
        draw(&mut app);

        // Searching the whole file is an SQL query.
        run_query(&mut app, "SELECT id, name FROM data WHERE score >= 450 ORDER BY id");
        assert!(app.sql.error.is_none(), "{:?}", app.sql.error);
        assert_eq!(app.num_rows(), 5);
        assert_eq!(app.table.as_ref().unwrap().cell(0, 0), "45");

        // Esc goes back from the result to the preview (and doesn't quit).
        assert!(app.can_go_back());
        key(&mut app, KeyCode::Esc);
        assert!(!app.should_quit);
        assert!(app.showing_preview());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn sql_result_on_loaded_file_and_back() {
        let (mut app, path) = file_app(false);
        assert!(!app.source.as_ref().unwrap().streamed);
        assert_eq!(app.num_rows(), 50);

        // The prompt starts from a helpful template.
        key(&mut app, KeyCode::Char('S'));
        assert_eq!(app.sql.input, "SELECT * FROM data WHERE ");
        type_str(&mut app, "id < 3");
        key(&mut app, KeyCode::Enter);
        wait_sql(&mut app);
        assert_eq!(app.num_rows(), 3);
        assert!(app.can_go_back());

        // Results behave like any table: e.g. sort them.
        key(&mut app, KeyCode::Char('s'));
        draw(&mut app);

        key(&mut app, KeyCode::Esc);
        assert!(!app.should_quit);
        assert_eq!(app.num_rows(), 50);
        assert!(!app.can_go_back());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn sql_error_reopens_the_prompt() {
        let (mut app, path) = file_app(false);
        run_query(&mut app, "SELECT nope FROM data");
        assert!(app.sql.error.is_some());
        assert_eq!(app.mode, Mode::Sql);
        // The table on screen is untouched.
        assert_eq!(app.num_rows(), 50);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tab_completes_columns_table_and_keywords() {
        let (mut app, path) = file_app(false);
        key(&mut app, KeyCode::Char('S'));
        ctrl(&mut app, 'u');
        type_str(&mut app, "SELECT na");
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.sql.input, "SELECT name ");
        type_str(&mut app, "FROM da");
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.sql.input, "SELECT name FROM data ");
        type_str(&mut app, "wh");
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.sql.input, "SELECT name FROM data WHERE ");

        // Several matches are listed instead of guessed.
        type_str(&mut app, "s");
        key(&mut app, KeyCode::Tab);
        assert!(app.sql.suggestions.contains(&"score".to_string()));
        assert!(app.sql.suggestions.contains(&"SELECT".to_string()));
        draw(&mut app);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn prompt_editing_and_history() {
        let (mut app, path) = file_app(false);
        run_query(&mut app, "SELECT COUNT(*) FROM data");
        assert_eq!(app.table.as_ref().unwrap().cell(0, 0), "50");

        key(&mut app, KeyCode::Char('S'));
        ctrl(&mut app, 'u');
        assert_eq!(app.sql.input, "");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.sql.input, "SELECT COUNT(*) FROM data");

        // Cursor editing in the middle of the line.
        key(&mut app, KeyCode::Home);
        key(&mut app, KeyCode::Delete);
        type_str(&mut app, "s");
        assert_eq!(app.sql.input, "sELECT COUNT(*) FROM data");
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unreadable_file_shows_error_instead_of_exiting() {
        let mut p = std::env::temp_dir();
        p.push(format!("tessera_missing_{}.parquet", std::process::id()));
        let mut app = App::browser_only(LoadOptions::default(), std::env::temp_dir());
        app.open_path(p);
        assert_eq!(app.mode, Mode::Browser);
        assert!(app.browser.error.is_some());
        draw(&mut app);
    }

    fn sample_app() -> App {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tessera_ui_{}_{}.csv",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "id,name,score").unwrap();
        for i in 0..50 {
            writeln!(f, "{i},name{i},{}.5", i * 2).unwrap();
        }
        f.flush().unwrap();
        let table = Table::load(&p, &LoadOptions::default()).unwrap();
        std::fs::remove_file(&p).ok();
        App::new(table, LoadOptions::default())
    }

    fn draw(app: &mut App) {
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
    }

    #[test]
    fn renders_and_navigates_without_panic() {
        let mut app = sample_app();
        draw(&mut app);

        // Walk to the bottom and across columns.
        for _ in 0..60 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
            app.on_key(KeyEvent::from(KeyCode::Char('l')));
            draw(&mut app);
        }
        assert!(app.sel_row < app.visible_rows());
        assert!(app.sel_col < app.num_cols());

        // Overlays should render too.
        for key in ['?', 'i'] {
            app.on_key(KeyEvent::from(KeyCode::Char(key)));
            draw(&mut app);
            app.on_key(KeyEvent::from(KeyCode::Esc));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter)); // cell inspector
        draw(&mut app);
    }

    #[test]
    fn filter_narrows_visible_rows() {
        let mut app = sample_app();
        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "name1".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        draw(&mut app);
        // "name1", "name10".."name19" → 11 matches.
        assert_eq!(app.visible_rows(), 11);
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.visible_rows(), 50);
    }

    #[test]
    fn sort_orders_numeric_column() {
        let mut app = sample_app();
        // Sort by the "score" column (index 2): ascending then descending.
        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        app.on_key(KeyEvent::from(KeyCode::Char('s'))); // ascending
        draw(&mut app);
        assert_eq!(app.visible.first().copied(), Some(0));
        app.on_key(KeyEvent::from(KeyCode::Char('s'))); // descending
        draw(&mut app);
        assert_eq!(app.visible.first().copied(), Some(49));
        app.on_key(KeyEvent::from(KeyCode::Char('s'))); // off → natural order
        draw(&mut app);
        assert_eq!(app.visible.first().copied(), Some(0));
    }

    #[test]
    fn export_writes_filtered_view_to_csv() {
        let mut app = sample_app();
        let dest = crate::app::export_path(&app.table.as_ref().unwrap().path);

        // Filter to the 11 "name1*" rows, then export.
        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "name1".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.on_key(KeyEvent::from(KeyCode::Char('e')));

        let written = std::fs::read_to_string(&dest).unwrap();
        let line_count = written.lines().count();
        assert_eq!(line_count, 12); // header + 11 rows
        assert!(written.starts_with("id,name,score"));
        std::fs::remove_file(&dest).ok();
    }

    #[test]
    fn browser_renders_and_lists_entries() {
        let mut app = sample_app();
        app.on_key(KeyEvent::from(KeyCode::Char('o')));
        assert_eq!(app.mode, Mode::Browser);
        draw(&mut app);
        // Esc returns to the table since one is loaded.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn renders_in_tiny_viewport() {
        let mut app = sample_app();
        let backend = TestBackend::new(3, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
    }
}
