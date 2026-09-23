# Tessera

**A terminal viewer and SQL explorer for CSV and Parquet files**, written in Rust.

Tessera opens CSV and Parquet files in a fast, keyboard-driven TUI and lets you
search them with SQL. Files that fit comfortably in memory are loaded whole;
files too big for that (10M+ rows is fine) are never loaded — you get a preview
of the first rows and query the rest with SQL, which streams through the file.
Inputs are normalised through [Apache Arrow](https://arrow.apache.org/),
so columns are correctly typed and every value — integers, floats, dates,
timestamps, decimals, nested lists/structs — is rendered with Arrow's
type-aware formatter.

```
┌ Tessera  people.csv  [CSV]  10 rows × 6 cols ──────────────────────────┐
│ #   id  name             department    salary  start_date   active     │
│ 1    1  Alice Johnson    Engineering    95000  2019-03-15   true       │
│ 2    2  Bob Smith        Marketing      72000  2020-07-01   true       │
│ 3    3  Carol Williams   Engineering   110000  2018-01-20   true       │
│ …                                                                      │
│ row 3/10  col 4/6 salary                                       ? help   │
└────────────────────────────────────────────────────────────────────────┘
```

## Features

- **CSV & Parquet** in one tool, auto-detected by extension or content (the
  Parquet `PAR1` magic header). Force it with `--type`.
- **SQL** (`S`): query the file — it is the table `data` — with
  [DataFusion](https://datafusion.apache.org/). Results show up as a normal
  table you can sort, filter, copy and export; `Esc` goes back. Tab completes
  column names, ↑/↓ recalls earlier queries.
- **Huge files**: anything too big to load (see [below](#large-files)) opens in
  SQL mode with a preview instead of running out of memory.
- **One-shot queries** from the shell: `tessera data.parquet -q "SELECT …"`
  prints a table; add `--csv` to stream every row as CSV.
- **Built-in file browser** — run `tessera` with no arguments to pick a file,
  or press `o` any time to open another one without leaving the app.
- **Typed columns** with schema inference for CSV; numeric columns are
  right-aligned automatically.
- **Sort** by any column (`s`) — ascending → descending → off — type-aware so
  numbers sort numerically and nulls sink to the bottom.
- **Column statistics** in the schema view (`i`): non-null/null counts plus
  min, max and mean for numeric columns.
- **Copy & export**: `y`/`Y` copy the current cell/row to the system clipboard
  (via OSC 52, works over SSH); `e` writes the current filtered+sorted view to
  a `.view.csv` next to the source.
- **Frozen header row** and a **row-number gutter** that stay put while you
  scroll in both directions, with subtle zebra striping for readability.
- **Vim-style and arrow navigation**, paging, half-paging, jump-to-edges, and
  mouse-wheel scrolling.
- **Incremental filter** (`/`) across all columns, with a live match count.
- **Go to row** (`:`) by number.
- **Cell inspector** (`Enter`) for values too wide for the grid.
- **Adjustable column widths** (`<` / `>`).
- Robust terminal handling: alternate screen, mouse capture, and a panic hook
  that always restores your terminal.

## Install

```sh
# from a clone of this repository
cargo install --path .

# or just build it
cargo build --release   # binary at target/release/tessera
```

Requires a recent stable Rust toolchain (edition 2021, Rust ≥ 1.86).

## Desktop GUI (optional)

Prefer a window? Tessera ships an optional, deliberately minimal desktop
viewer — `tessera-gui` — built on [egui](https://github.com/emilk/egui). It
shows the same CSV/Parquet data in a plain table with a single search box, and
renders only the rows currently on screen, so opening a **million-row** file
stays smooth.

```sh
# build / run the GUI (it is behind the `gui` feature)
cargo run --release --features gui --bin tessera-gui -- data.parquet

# install it alongside the TUI
cargo install --path . --features gui
```

Pass a file on the command line, type a path in the toolbar, or just drag a
`.csv`/`.parquet` file onto the window. For CSV/TSV, pick the field delimiter
(comma, tab, semicolon or pipe) and toggle whether the first row is a header
from the toolbar — the view (and SQL) reload with those settings. Type in the
search box to filter rows across every column. Numeric columns are
right-aligned; hover a cell to read its full value (handy when it's clipped)
and click it to copy. Wide tables scroll horizontally, and the row grid is
virtualised so million-row files stay smooth. (Building the GUI needs the usual
desktop libraries — OpenGL plus X11/Wayland on Linux; nothing extra on Windows
or macOS.)

Flip the toolbar toggle from **Search** to **SQL** to query the file with
[DataFusion](https://datafusion.apache.org/): the open file is registered as a
table named `data`, so you can write things like

```sql
SELECT name, score FROM data WHERE score > 100 ORDER BY score DESC LIMIT 50
```

Results render in the same grid (capped at 100k rows for display). Queries run
on a background thread, so the window stays responsive during a long scan.

### Very large Parquet files

Files too big to load (see [Large files](#large-files)) are **not loaded into
memory**. The GUI reads only the file's metadata, opens straight into SQL mode
and shows the file's first 1,000 rows as a preview; DataFusion then streams
through the file for each query. Search with `WHERE`, e.g.
`SELECT * FROM data WHERE name LIKE '%foo%'`.

Measured on a 12,000,000-row, 288 MB Parquet file (5 columns, 4-core Linux):

| Query | Time |
| --- | --- |
| open the file | < 1 ms |
| first 1,000 rows (preview) | instant |
| `SELECT COUNT(*) FROM data` | < 0.01 s |
| `… WHERE name = 'user11999999'` (full scan) | 0.11 s |
| `… WHERE name LIKE '%99999%'` | 0.26 s |
| `GROUP BY category` with `COUNT`/`AVG` | 0.09 s |
| `ORDER BY value DESC LIMIT 100` | 0.20 s |

Peak memory stayed around 300 MB. The same size limits as the terminal viewer
apply (see [Large files](#large-files)), so big CSV files open this way too.

Prebuilt `tessera-gui` binaries ship in the **Windows** and **macOS** release
archives alongside the TUI; on Linux, build it from source as above.

## Usage

```sh
tessera                      # no file? opens the built-in file browser
tessera data.csv
tessera data.parquet
tessera --type csv mystery_file
tessera --delimiter ';' euro.csv
tessera --delimiter '\t' --no-header data.tsv
tessera --sql-only big.csv   # never load it; browse with SQL only

# one-shot SQL: print the result and exit (the file is the table `data`)
tessera data.parquet -q "SELECT category, COUNT(*) FROM data GROUP BY category"
tessera data.parquet -q "SELECT * FROM data WHERE price > 100" --csv > hits.csv

# try the bundled sample
tessera samples/people.csv
```

If a file can't be opened, Tessera stays open and shows the error in its file
browser (so on Windows the window doesn't just close).

### SQL in the viewer

Press **`S`** (or `F5`) to open the SQL prompt at the bottom of the screen. The
open file is the table **`data`**; the prompt starts as
`SELECT * FROM data WHERE ` so you only type the condition:

```sql
SELECT * FROM data WHERE name LIKE '%tanaka%'
SELECT * FROM data WHERE amount > 1000 ORDER BY amount DESC
SELECT city, COUNT(*) AS n, AVG(amount) FROM data GROUP BY city ORDER BY n DESC
```

- **Enter** runs the query in the background — the screen stays responsive and
  the title bar shows the elapsed time, then the result's row count and time.
- **Tab** completes column names, `data` and SQL keywords (several matches are
  listed under the prompt). Column names with capitals, spaces or non-ASCII
  characters are inserted in `"double quotes"`, as SQL requires.
- **↑ / ↓** walk through earlier queries; **Ctrl-u** clears the line.
- The prompt lists the file's columns; errors are shown in red and the prompt
  stays open so you can fix the query.
- The result is an ordinary table: sort (`s`), filter (`/`), inspect, copy and
  export (`e`) it. **Esc** returns to the file (or its preview). Results show up
  to 100,000 rows — use `-q … --csv` for more.

### Large files

A file is **not loaded** — only previewed, and queried with SQL — when it is a
Parquet file with more than 2 million rows or more than 512 MB of uncompressed
data, or a CSV file larger than 256 MB (`--sql-only` forces this for any file).
Only the first 1,000 rows are read for the preview; each query streams through
the file.

Measured with the terminal viewer on a 12,000,000-row, 288 MB Parquet file
(5 columns, 4-core Linux), with the process limited to 1 GB of memory:

| | Time | Peak memory |
| --- | --- | --- |
| open (preview of the first 1,000 rows) | instant | 22 MB |
| `GROUP BY category` over all rows | 0.21 s | 111 MB |
| `WHERE name LIKE '%77777%' ORDER BY value DESC` | ≈0.3 s | ≈220 MB |
| loading the whole file (the old behaviour) | 2.5 s | 1.2 GB — aborted under the 1 GB limit |

On Windows you can also drag a `.csv`/`.parquet` file onto `tessera.exe`, or run
it with no arguments and browse to the file from inside the app.

### Options

| Flag | Description |
| --- | --- |
| `-t, --type <csv\|parquet>` | Force the file type instead of auto-detecting. |
| `-d, --delimiter <CHAR>` | CSV field delimiter (`,` default; `\t` and `tab` accepted). |
| `--no-header` | Treat the first CSV row as data, not column names. |

## Keybindings

| Keys | Action |
| --- | --- |
| `h` `j` `k` `l` / arrows | Move the cursor one cell |
| `g` / `G` | Jump to first / last row |
| `0` / `$` | Jump to first / last column |
| `PgUp` / `PgDn` | Page up / down |
| `Ctrl-u` / `Ctrl-d` | Half page up / down |
| mouse wheel | Scroll rows |
| `Enter` / `Space` | Inspect the full cell value |
| `i` | Schema + column statistics |
| `s` | Sort by current column (asc → desc → off) |
| `<` / `>` | Shrink / grow the current column |
| `/` | Incremental filter across all columns |
| `n` | Clear the active filter |
| `:` | Go to a row number |
| `S` / `F5` | SQL prompt (Enter run · Tab complete · ↑↓ history · Esc close) |
| `Esc` | From an SQL result: back to the file / preview |
| `o` | Open another file (file browser) |
| `y` / `Y` | Copy current cell / row to the clipboard |
| `e` | Export the current view to `<name>.view.csv` |
| `?` | Toggle help |
| `q` / `Esc` / `Ctrl-c` | Quit (`Esc` goes back first when showing an SQL result) |

In the **file browser**: `↑`/`↓` (or `j`/`k`) move, `Enter` opens a file or
enters a folder, `Backspace` goes up a directory, and `q`/`Esc` returns to the
table (or quits if none is open).

## How it works

| Layer | File | Responsibility |
| --- | --- | --- |
| Data | `src/data.rs` | Load CSV/Parquet into a single Arrow `RecordBatch` (or just its first rows); decide which files are too big to load; type-aware cell formatting. |
| SQL | `src/sql.rs` | DataFusion session over the file (table `data`); results as typed tables, pretty text or streamed CSV. |
| State | `src/app.rs` | Selection, scrolling, filtering and all input handling. |
| View | `src/ui.rs` | Hand-rolled grid rendering with a frozen header and overlays. |
| Entry | `src/main.rs` | CLI parsing and terminal lifecycle. |

A file that fits is loaded into memory as one concatenated batch, giving O(1)
random access to any cell and instant scrolling. SQL results are moved from
DataFusion's Arrow version to the app's through the Arrow IPC format, so they
keep their column types.

## Development

```sh
cargo test     # unit + headless-render tests
cargo clippy   # lints (clean)
cargo run -- samples/people.csv
```

## License

MIT
