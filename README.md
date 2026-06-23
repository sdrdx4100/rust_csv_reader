# Tessera

**An ultimate terminal viewer for CSV and Parquet files**, written in Rust.

Tessera opens tabular data of any size in a fast, keyboard-driven TUI. CSV and
Parquet inputs are normalised through [Apache Arrow](https://arrow.apache.org/),
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

Requires a recent stable Rust toolchain (edition 2021, Rust ≥ 1.80).

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
`.csv`/`.parquet` file onto the window. Type in the search box to filter rows
across every column. (Building the GUI needs the usual desktop libraries —
OpenGL plus X11/Wayland on Linux; nothing extra on Windows or macOS.)

## Usage

```sh
tessera                      # no file? opens the built-in file browser
tessera data.csv
tessera data.parquet
tessera --type csv mystery_file
tessera --delimiter ';' euro.csv
tessera --delimiter '\t' --no-header data.tsv

# try the bundled sample
tessera samples/people.csv
```

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
| `o` | Open another file (file browser) |
| `y` / `Y` | Copy current cell / row to the clipboard |
| `e` | Export the current view to `<name>.view.csv` |
| `?` | Toggle help |
| `q` / `Esc` / `Ctrl-c` | Quit |

In the **file browser**: `↑`/`↓` (or `j`/`k`) move, `Enter` opens a file or
enters a folder, `Backspace` goes up a directory, and `q`/`Esc` returns to the
table (or quits if none is open).

## How it works

| Layer | File | Responsibility |
| --- | --- | --- |
| Data | `src/data.rs` | Load CSV/Parquet into a single Arrow `RecordBatch`; type-aware cell formatting. |
| State | `src/app.rs` | Selection, scrolling, filtering and all input handling. |
| View | `src/ui.rs` | Hand-rolled grid rendering with a frozen header and overlays. |
| Entry | `src/main.rs` | CLI parsing and terminal lifecycle. |

The whole file is loaded into memory as one concatenated batch, giving O(1)
random access to any cell and instant scrolling.

## Development

```sh
cargo test     # unit + headless-render tests
cargo clippy   # lints (clean)
cargo run -- samples/people.csv
```

## License

MIT
