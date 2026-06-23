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
- **Typed columns** with schema inference for CSV; numeric columns are
  right-aligned automatically.
- **Frozen header row** and a **row-number gutter** that stay put while you
  scroll in both directions.
- **Vim-style and arrow navigation**, paging, half-paging, jump-to-edges, and
  mouse-wheel scrolling.
- **Incremental filter** (`/`) across all columns, with a live match count.
- **Go to row** (`:`) by number.
- **Cell inspector** (`Enter`) for values too wide for the grid.
- **Schema overview** (`i`) listing every column and its type.
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

## Usage

```sh
tessera data.csv
tessera data.parquet
tessera --type csv mystery_file
tessera --delimiter ';' euro.csv
tessera --delimiter '\t' --no-header data.tsv

# try the bundled sample
tessera samples/people.csv
```

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
| `i` | Schema / column overview |
| `<` / `>` | Shrink / grow the current column |
| `/` | Incremental filter across all columns |
| `n` | Clear the active filter |
| `:` | Go to a row number |
| `?` | Toggle help |
| `q` / `Esc` / `Ctrl-c` | Quit |

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
