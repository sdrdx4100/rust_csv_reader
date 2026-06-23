//! Data loading and the in-memory columnar table model.
//!
//! Both CSV and Parquet inputs are normalised into Apache Arrow
//! [`RecordBatch`]es and concatenated into a single batch so that the UI can
//! perform O(1) random access into any cell. Cell rendering is delegated to
//! Arrow's [`ArrayFormatter`], which gives correct, type-aware string output
//! for every Arrow data type (integers, floats, dates, timestamps, decimals,
//! lists, structs, …) for free.

use std::fs::File;
use std::io::{Seek, SeekFrom};
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
        let kind = opts
            .kind
            .or_else(|| FileKind::from_path(path))
            .or_else(|| sniff_kind(path))
            .ok_or_else(|| {
                anyhow!(
                    "could not determine file type for {}; pass --type csv|parquet",
                    path.display()
                )
            })?;

        let (schema, batches) = match kind {
            FileKind::Csv => load_csv(path, opts)?,
            FileKind::Parquet => load_parquet(path)?,
        };

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

        let _ = schema;
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
    use arrow::csv::reader::Format;

    let format = Format::default()
        .with_header(opts.has_header)
        .with_delimiter(opts.delimiter);

    let mut file = File::open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let (schema, _) = format
        .infer_schema(&mut file, Some(opts.infer_rows))
        .context("failed to infer CSV schema")?;
    file.seek(SeekFrom::Start(0))?;

    let schema = Arc::new(schema);
    let reader = arrow::csv::ReaderBuilder::new(schema.clone())
        .with_format(format)
        .build(file)
        .context("failed to build CSV reader")?;

    let batches = reader
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to read CSV data")?;
    Ok((schema, batches))
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
}
