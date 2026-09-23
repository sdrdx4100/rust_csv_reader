//! Data loading and the in-memory columnar table model.
//!
//! Both CSV and Parquet inputs are normalised into Apache Arrow
//! [`RecordBatch`]es and concatenated into a single batch so that the UI can
//! perform O(1) random access into any cell. Cell rendering is delegated to
//! Arrow's [`ArrayFormatter`], which gives correct, type-aware string output
//! for every Arrow data type (integers, floats, dates, timestamps, decimals,
//! lists, structs, …) for free.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, SchemaRef};
use arrow::record_batch::RecordBatch;
use arrow::util::display::{ArrayFormatter, FormatOptions};

/// How the input file should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Csv,
    Parquet,
}

impl FileKind {
    pub fn label(self) -> &'static str {
        match self {
            FileKind::Csv => "CSV",
            FileKind::Parquet => "Parquet",
        }
    }

    /// Best-effort detection from a file extension.
    pub fn from_path(path: &Path) -> Option<FileKind> {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("csv") | Some("tsv") | Some("txt") => Some(FileKind::Csv),
            Some("parquet") | Some("pq") | Some("parq") => Some(FileKind::Parquet),
            _ => None,
        }
    }
}

/// Options that influence how a file is parsed.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub kind: Option<FileKind>,
    pub delimiter: u8,
    pub has_header: bool,
    /// Number of rows sampled for CSV schema inference.
    pub infer_rows: usize,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            kind: None,
            delimiter: b',',
            has_header: true,
            infer_rows: 1024,
        }
    }
}

/// A fully-materialised, immutable view over the loaded file.
pub struct Table {
    pub path: PathBuf,
    pub kind: FileKind,
    /// Every row of the file in a single concatenated batch.
    batch: RecordBatch,
    column_names: Vec<String>,
    column_types: Vec<String>,
}

impl Table {
    /// Load `path` according to `opts`, auto-detecting the format when needed.
    pub fn load(path: &Path, opts: &LoadOptions) -> Result<Table> {
        let kind = detect_kind(path, opts.kind).ok_or_else(|| {
            anyhow!(
                "could not determine file type for {}; pass --type csv|parquet",
                path.display()
            )
        })?;

        let (schema, batches) = match kind {
            FileKind::Csv => load_csv(path, opts)?,
            FileKind::Parquet => load_parquet(path)?,
        };
        Table::from_batches(path, kind, schema, batches)
    }

    /// Build a table from already-decoded batches (e.g. an SQL query result).
    /// `path` and `kind` describe the file the data came from.
    pub fn from_batches(
        path: &Path,
        kind: FileKind,
        schema: SchemaRef,
        batches: Vec<RecordBatch>,
    ) -> Result<Table> {
        let batch = if batches.is_empty() {
            RecordBatch::new_empty(schema.clone())
        } else {
            concat_batches(&schema, &batches)
                .context("failed to concatenate record batches")?
        };

        let column_names = schema.fields().iter().map(|f| f.name().clone()).collect();
        let column_types = schema
            .fields()
            .iter()
            .map(|f| friendly_type(f.data_type()))
            .collect();

        Ok(Table {
            path: path.to_path_buf(),
            kind,
            batch,
            column_names,
            column_types,
        })
    }

    pub fn num_rows(&self) -> usize {
        self.batch.num_rows()
    }

    pub fn num_cols(&self) -> usize {
        self.batch.num_columns()
    }

    pub fn column_names(&self) -> &[String] {
        &self.column_names
    }

    pub fn column_types(&self) -> &[String] {
        &self.column_types
    }

    /// Whether the cell at `(row, col)` is a genuine null (as opposed to an
    /// empty-but-present value). Used so sorting and statistics can tell the
    /// two apart.
    pub fn is_null(&self, row: usize, col: usize) -> bool {
        self.batch.column(col).is_null(row)
    }

    /// Construct a display formatter for every column. Formatters borrow the
    /// underlying arrays, so they must not outlive the table.
    pub fn formatters(&self) -> Result<Vec<ArrayFormatter<'_>>> {
        let opts = FormatOptions::default()
            .with_null("")
            .with_display_error(true);
        self.batch
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &opts).map_err(Into::into))
            .collect()
    }

    /// Format a single cell to a string. Cheap enough for per-frame use over
    /// the handful of visible cells.
    pub fn cell(&self, row: usize, col: usize) -> String {
        let opts = FormatOptions::default().with_null("");
        match ArrayFormatter::try_new(self.batch.column(col).as_ref(), &opts) {
            Ok(fmt) => fmt.value(row).to_string(),
            Err(_) => String::from("<err>"),
        }
    }
}

/// Work out how to interpret `path`: an explicit `forced` kind wins, then the
/// file extension, then the file's leading bytes.
pub fn detect_kind(path: &Path, forced: Option<FileKind>) -> Option<FileKind> {
    forced
        .or_else(|| FileKind::from_path(path))
        .or_else(|| sniff_kind(path))
}

/// Row and column counts of a Parquet file, read from its footer metadata only.
/// Cheap even for files with tens of millions of rows, since no data is decoded.
pub fn parquet_shape(path: &Path) -> Result<(usize, usize)> {
    let (rows, cols, _) = parquet_footer(path)?;
    Ok((rows, cols))
}

/// Rows, columns and total *uncompressed* data size, from the Parquet footer.
fn parquet_footer(path: &Path) -> Result<(usize, usize, u64)> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let file = File::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .context("failed to open Parquet file")?;
    let meta = builder.metadata();
    let rows = meta.file_metadata().num_rows().max(0) as usize;
    let bytes = meta
        .row_groups()
        .iter()
        .map(|rg| rg.total_byte_size().max(0) as u64)
        .sum();
    Ok((rows, builder.schema().fields().len(), bytes))
}

/// Size limits above which a file is *streamed* — never loaded into memory,
/// only queried with SQL — so opening it can't exhaust RAM on a small machine.
#[derive(Debug, Clone, Copy)]
pub struct StreamLimits {
    /// Parquet: more rows than this streams.
    pub max_rows: usize,
    /// Parquet: more uncompressed data than this streams (catches wide files).
    pub max_parquet_bytes: u64,
    /// CSV: a file larger than this on disk streams (its row count is unknown
    /// without reading it all).
    pub max_csv_bytes: u64,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            max_rows: 2_000_000,
            max_parquet_bytes: 512 * 1024 * 1024,
            max_csv_bytes: 256 * 1024 * 1024,
        }
    }
}

/// What can be learned about a file cheaply, before deciding how to open it.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub kind: FileKind,
    /// Known for Parquet (from the footer); `None` for CSV.
    pub rows: Option<usize>,
    pub cols: Option<usize>,
    /// True when the file is too big to load and should be opened in SQL mode.
    pub stream: bool,
}

/// Look at `path` without loading its data and decide whether to stream it.
pub fn inspect(path: &Path, forced: Option<FileKind>, limits: &StreamLimits) -> Result<FileInfo> {
    let kind = detect_kind(path, forced).ok_or_else(|| {
        anyhow!("could not determine file type for {}", path.display())
    })?;
    match kind {
        FileKind::Parquet => {
            let (rows, cols, bytes) = parquet_footer(path)?;
            Ok(FileInfo {
                kind,
                rows: Some(rows),
                cols: Some(cols),
                stream: rows > limits.max_rows || bytes > limits.max_parquet_bytes,
            })
        }
        FileKind::Csv => {
            let len = std::fs::metadata(path)
                .with_context(|| format!("failed to open {}", path.display()))?
                .len();
            Ok(FileInfo {
                kind,
                rows: None,
                cols: None,
                stream: len > limits.max_csv_bytes,
            })
        }
    }
}

/// Peek at the first bytes of a file to recognise the Parquet magic header.
fn sniff_kind(path: &Path) -> Option<FileKind> {
    use std::io::Read;
    let mut file = File::open(path).ok()?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;
    if &magic == b"PAR1" {
        Some(FileKind::Parquet)
    } else {
        // Fall back to CSV for anything that looks like text.
        Some(FileKind::Csv)
    }
}

fn load_csv(path: &Path, opts: &LoadOptions) -> Result<(SchemaRef, Vec<RecordBatch>)> {
    read_csv(path, opts, None)
}

/// Read a CSV file — all of it, or only the first `limit` rows.
fn read_csv(
    path: &Path,
    opts: &LoadOptions,
    limit: Option<usize>,
) -> Result<(SchemaRef, Vec<RecordBatch>)> {
    use arrow::csv::reader::Format;
    use arrow::datatypes::{Field, Schema};

    let format = Format::default()
        .with_header(opts.has_header)
        .with_delimiter(opts.delimiter);

    let mut file = File::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let (schema, _) = format
        .infer_schema(&mut file, Some(opts.infer_rows))
        .context("failed to infer CSV schema")?;
    let schema = Arc::new(schema);

    // Type inference only samples the first `infer_rows` rows, so a column that
    // looks numeric early can still contain text further down and break the
    // typed read. If that happens, fall back to reading every column as text so
    // the file always opens.
    match read_csv_with_schema(path, &format, schema.clone(), limit) {
        Ok(batches) => Ok((schema, batches)),
        Err(_) => {
            let string_schema = Arc::new(Schema::new(
                schema
                    .fields()
                    .iter()
                    .map(|f| Field::new(f.name(), DataType::Utf8, true))
                    .collect::<Vec<_>>(),
            ));
            let batches = read_csv_with_schema(path, &format, string_schema.clone(), limit)
                .context("failed to read CSV data")?;
            Ok((string_schema, batches))
        }
    }
}

/// Read the batches of a CSV file with a fixed schema (up to `limit` rows),
/// opening the file fresh.
fn read_csv_with_schema(
    path: &Path,
    format: &arrow::csv::reader::Format,
    schema: SchemaRef,
    limit: Option<usize>,
) -> Result<Vec<RecordBatch>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut builder = arrow::csv::ReaderBuilder::new(schema).with_format(format.clone());
    if let Some(n) = limit {
        builder = builder.with_batch_size(n.clamp(1, 8192));
    }
    let reader = builder.build(file).context("failed to build CSV reader")?;
    let mut out = Vec::new();
    let mut got = 0usize;
    for batch in reader {
        let batch = batch.context("failed to read CSV data")?;
        match limit {
            Some(n) if got + batch.num_rows() >= n => {
                out.push(batch.slice(0, n - got));
                break;
            }
            _ => {
                got += batch.num_rows();
                out.push(batch);
            }
        }
    }
    Ok(out)
}

/// Read only the first `n` rows of a file, in file order. Used to preview
/// files that are too big to load; reads just enough of the file for `n` rows.
pub fn read_head(path: &Path, opts: &LoadOptions, n: usize) -> Result<Table> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let kind = detect_kind(path, opts.kind)
        .ok_or_else(|| anyhow!("could not determine file type for {}", path.display()))?;
    let (schema, batches) = match kind {
        FileKind::Csv => read_csv(path, opts, Some(n))?,
        FileKind::Parquet => {
            let file = File::open(path)
                .with_context(|| format!("failed to open {}", path.display()))?;
            let builder = ParquetRecordBatchReaderBuilder::try_new(file)
                .context("failed to open Parquet file")?;
            let schema = builder.schema().clone();
            let reader = builder
                .with_limit(n)
                .with_batch_size(n.clamp(1, 8192))
                .build()
                .context("failed to build Parquet reader")?;
            let batches = reader
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("failed to read Parquet data")?;
            (schema, batches)
        }
    };
    Table::from_batches(path, kind, schema, batches)
}

fn load_parquet(path: &Path) -> Result<(SchemaRef, Vec<RecordBatch>)> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let file = File::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .context("failed to open Parquet file")?;
    let schema = builder.schema().clone();
    let reader = builder.build().context("failed to build Parquet reader")?;
    let batches = reader
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to read Parquet data")?;
    Ok((schema, batches))
}

/// Whether a [`friendly_type`] string denotes a right-alignable numeric column.
pub fn is_numeric_type(ty: &str) -> bool {
    matches!(ty, "int" | "uint" | "float") || ty.starts_with("decimal")
}

/// A short, human-readable rendering of an Arrow data type for the schema view.
fn friendly_type(dt: &DataType) -> String {
    match dt {
        DataType::Boolean => "bool".into(),
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => "int".into(),
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => "uint".into(),
        DataType::Float16 | DataType::Float32 | DataType::Float64 => "float".into(),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "string".into(),
        DataType::Date32 | DataType::Date64 => "date".into(),
        DataType::Timestamp(_, _) => "timestamp".into(),
        DataType::Time32(_) | DataType::Time64(_) => "time".into(),
        DataType::Decimal128(p, s) | DataType::Decimal256(p, s) => {
            format!("decimal({p},{s})")
        }
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => "binary".into(),
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(_, _) => "list".into(),
        DataType::Struct(_) => "struct".into(),
        DataType::Map(_, _) => "map".into(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("tessera_test_{}_{}", std::process::id(), name));
        p
    }

    #[test]
    fn loads_csv_with_inferred_types() {
        let path = temp_path("basic.csv");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "id,name,score").unwrap();
        writeln!(f, "1,alice,3.5").unwrap();
        writeln!(f, "2,bob,7.0").unwrap();
        writeln!(f, "3,carol,").unwrap();
        f.flush().unwrap();

        let table = Table::load(&path, &LoadOptions::default()).unwrap();
        assert_eq!(table.num_rows(), 3);
        assert_eq!(table.num_cols(), 3);
        assert_eq!(table.column_names(), &["id", "name", "score"]);
        assert_eq!(table.column_types()[0], "int");
        assert_eq!(table.column_types()[2], "float");
        assert_eq!(table.cell(0, 1), "alice");
        // A null cell renders as an empty string.
        assert_eq!(table.cell(2, 2), "");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn honours_custom_delimiter_and_no_header() {
        let path = temp_path("tsv.tsv");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "a\tb\tc").unwrap();
        writeln!(f, "d\te\tf").unwrap();
        f.flush().unwrap();

        let opts = LoadOptions {
            kind: Some(FileKind::Csv),
            delimiter: b'\t',
            has_header: false,
            ..Default::default()
        };
        let table = Table::load(&path, &opts).unwrap();
        assert_eq!(table.num_rows(), 2);
        assert_eq!(table.num_cols(), 3);
        assert_eq!(table.cell(0, 0), "a");
        assert_eq!(table.cell(1, 2), "f");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn round_trips_parquet() {
        use arrow::array::{Int64Array, StringArray};
        use arrow::datatypes::{Field, Schema};
        use parquet::arrow::ArrowWriter;

        let path = temp_path("data.parquet");
        let schema = Arc::new(Schema::new(vec![
            Field::new("n", DataType::Int64, false),
            Field::new("label", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![10, 20, 30])),
                Arc::new(StringArray::from(vec![Some("x"), None, Some("z")])),
            ],
        )
        .unwrap();
        {
            let file = File::create(&path).unwrap();
            let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
        }

        let table = Table::load(&path, &LoadOptions::default()).unwrap();
        assert_eq!(table.kind, FileKind::Parquet);
        assert_eq!(table.num_rows(), 3);
        assert_eq!(table.column_types(), &["int", "string"]);
        assert_eq!(table.cell(0, 0), "10");
        assert_eq!(table.cell(1, 1), "");
        assert_eq!(table.cell(2, 1), "z");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn detects_kind_without_extension() {
        let path = temp_path("noext");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "x,y").unwrap();
        writeln!(f, "1,2").unwrap();
        f.flush().unwrap();

        let table = Table::load(&path, &LoadOptions::default()).unwrap();
        assert_eq!(table.kind, FileKind::Csv);
        assert_eq!(table.num_cols(), 2);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn csv_falls_back_to_strings_when_inference_is_wrong() {
        // "v" looks numeric in the sampled rows, then turns out to hold text.
        let path = temp_path("mixed.csv");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "id,v").unwrap();
        writeln!(f, "1,10").unwrap();
        writeln!(f, "2,20").unwrap();
        writeln!(f, "3,abc").unwrap();
        f.flush().unwrap();

        // Only sample the first 2 data rows so inference guesses int for "v".
        let opts = LoadOptions {
            infer_rows: 2,
            ..Default::default()
        };
        let table = Table::load(&path, &opts).unwrap();
        assert_eq!(table.num_rows(), 3);
        // The whole column fell back to strings, so the text row survives.
        assert_eq!(table.column_types()[1], "string");
        assert_eq!(table.cell(2, 1), "abc");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reads_parquet_shape_from_footer() {
        use arrow::array::Int64Array;
        use arrow::datatypes::{Field, Schema};
        use parquet::arrow::ArrowWriter;

        let path = temp_path("shape.parquet");
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int64, false),
            Field::new("b", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from((0..7).collect::<Vec<i64>>())),
                Arc::new(Int64Array::from((0..7).collect::<Vec<i64>>())),
            ],
        )
        .unwrap();
        {
            let file = File::create(&path).unwrap();
            let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
        }
        assert_eq!(parquet_shape(&path).unwrap(), (7, 2));
        assert_eq!(detect_kind(&path, None), Some(FileKind::Parquet));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn inspect_decides_when_to_stream() {
        let path = temp_path("inspect.csv");
        std::fs::write(&path, "a,b\n1,2\n3,4\n").unwrap();

        let roomy = StreamLimits::default();
        let info = inspect(&path, None, &roomy).unwrap();
        assert_eq!(info.kind, FileKind::Csv);
        assert!(!info.stream);

        // Any CSV bigger than the limit is streamed instead of loaded.
        let tight = StreamLimits {
            max_csv_bytes: 4,
            ..Default::default()
        };
        assert!(inspect(&path, None, &tight).unwrap().stream);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_head_returns_first_rows_in_order() {
        use arrow::array::Int64Array;
        use arrow::datatypes::{Field, Schema};
        use parquet::arrow::ArrowWriter;
        use parquet::file::properties::WriterProperties;

        // Several row groups, so "first rows" really means the file's start.
        let path = temp_path("head.parquet");
        let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false)]));
        let props = WriterProperties::builder().set_max_row_group_row_count(Some(100)).build();
        {
            let file = File::create(&path).unwrap();
            let mut w = ArrowWriter::try_new(file, schema.clone(), Some(props)).unwrap();
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(Int64Array::from((0..1000).collect::<Vec<i64>>()))],
            )
            .unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
        }
        let head = read_head(&path, &LoadOptions::default(), 250).unwrap();
        assert_eq!(head.num_rows(), 250);
        assert_eq!(head.cell(0, 0), "0");
        assert_eq!(head.cell(249, 0), "249");
        std::fs::remove_file(&path).ok();

        let path = temp_path("head.csv");
        let mut text = String::from("a,b\n");
        for i in 0..100 {
            text.push_str(&format!("{i},x{i}\n"));
        }
        std::fs::write(&path, text).unwrap();
        let head = read_head(&path, &LoadOptions::default(), 10).unwrap();
        assert_eq!(head.num_rows(), 10);
        assert_eq!(head.cell(9, 1), "x9");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn distinguishes_null_from_empty_string() {
        use arrow::array::StringArray;
        use arrow::datatypes::{Field, Schema};
        use parquet::arrow::ArrowWriter;

        let path = temp_path("nulls.parquet");
        let schema = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, true)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(StringArray::from(vec![Some("x"), Some(""), None]))],
        )
        .unwrap();
        {
            let file = File::create(&path).unwrap();
            let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
        }

        let table = Table::load(&path, &LoadOptions::default()).unwrap();
        // Row 1 is an empty-but-present value; row 2 is a genuine null. Both
        // render as "", but only row 2 reports as null.
        assert_eq!(table.cell(1, 0), "");
        assert_eq!(table.cell(2, 0), "");
        assert!(!table.is_null(0, 0));
        assert!(!table.is_null(1, 0));
        assert!(table.is_null(2, 0));

        std::fs::remove_file(&path).ok();
    }
}
