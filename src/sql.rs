//! SQL querying powered by [DataFusion].
//!
//! DataFusion reads the source file itself (CSV or Parquet) and exposes it as a
//! table named `data`, so users can write queries like
//! `SELECT * FROM data WHERE amount > 100 ORDER BY amount DESC`. The file is
//! never loaded whole: each query streams through it. Results come back as a
//! typed [`Table`] (handed across DataFusion's own Arrow version via the Arrow
//! IPC format), as display strings for the GUI, as pretty text, or streamed as
//! CSV.
//!
//! [DataFusion]: https://datafusion.apache.org/

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::prelude::{CsvReadOptions, ParquetReadOptions, SessionContext};
use futures::StreamExt;

use crate::data::{FileKind, Table};

/// A query result converted into the app's own [`Table`], so it can be shown,
/// sorted, filtered and exported like any loaded file.
pub struct SqlTable {
    pub table: Table,
    /// True when more rows matched than `max_rows` and the rest were dropped.
    pub truncated: bool,
}

/// A fully-materialised, display-ready query result.
pub struct SqlResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// True when more rows matched than the display cap and were dropped.
    pub truncated: bool,
}

/// A DataFusion session with the open file registered as table `data`.
pub struct SqlEngine {
    rt: tokio::runtime::Runtime,
    ctx: SessionContext,
    path: PathBuf,
    kind: FileKind,
}

impl SqlEngine {
    /// The SQL table name the source file is registered under.
    pub const TABLE: &'static str = "data";

    /// Register `path` (interpreted as `kind`) as the `data` table. For CSV the
    /// `delimiter` and `has_header` settings must match how the file is being
    /// viewed, so SQL sees the same columns.
    pub fn new(path: &Path, kind: FileKind, delimiter: u8, has_header: bool) -> Result<SqlEngine> {
        let rt = tokio::runtime::Runtime::new()?;
        let ctx = SessionContext::new();
        let p = path
            .to_str()
            .ok_or_else(|| anyhow!("path is not valid UTF-8"))?;
        rt.block_on(async {
            match kind {
                FileKind::Csv => {
                    let opts = CsvReadOptions::new()
                        .delimiter(delimiter)
                        .has_header(has_header);
                    ctx.register_csv(Self::TABLE, p, opts).await
                }
                FileKind::Parquet => {
                    ctx.register_parquet(Self::TABLE, p, ParquetReadOptions::default())
                        .await
                }
            }
        })?;
        Ok(SqlEngine {
            rt,
            ctx,
            path: path.to_path_buf(),
            kind,
        })
    }

    /// Run `sql` and return at most `max_rows` rows as a typed [`Table`].
    ///
    /// DataFusion uses its own Arrow version, so the batches are handed over
    /// through the (version-stable) Arrow IPC stream format rather than
    /// stringified — column types, numeric alignment and sorting all survive.
    pub fn query_table(&self, sql: &str, max_rows: usize) -> Result<SqlTable> {
        use datafusion::arrow::ipc::writer::StreamWriter;

        let (schema, batches) = self.rt.block_on(async {
            let df = self.ctx.sql(sql).await?;
            let schema = std::sync::Arc::new(df.schema().as_arrow().clone());
            // One extra row tells us whether the result was capped.
            let batches = df.limit(0, Some(max_rows + 1))?.collect().await?;
            Ok::<_, anyhow::Error>((schema, batches))
        })?;

        let mut buf = Vec::new();
        {
            let mut w = StreamWriter::try_new(&mut buf, &schema)?;
            for b in &batches {
                w.write(b)?;
            }
            w.finish()?;
        }
        let reader = arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(buf), None)
            .context("failed to decode query result")?;
        let schema = reader.schema();

        let mut out = Vec::new();
        let mut kept = 0usize;
        let mut truncated = false;
        for b in reader {
            let b = b?;
            if kept + b.num_rows() > max_rows {
                out.push(b.slice(0, max_rows - kept));
                truncated = true;
                break;
            }
            kept += b.num_rows();
            out.push(b);
        }
        Ok(SqlTable {
            table: Table::from_batches(&self.path, self.kind, schema, out)?,
            truncated,
        })
    }

    /// Run `sql` and render up to `max_rows` rows as a boxed text table.
    /// Returns the rendered text and whether rows were left out.
    pub fn pretty(&self, sql: &str, max_rows: usize) -> Result<(String, bool)> {
        self.rt.block_on(async {
            let df = self.ctx.sql(sql).await?;
            let batches = df.limit(0, Some(max_rows + 1))?.collect().await?;
            let total: usize = batches.iter().map(|b| b.num_rows()).sum();
            let mut kept = Vec::new();
            let mut n = 0;
            for b in batches {
                let take = b.num_rows().min(max_rows - n);
                if take == 0 {
                    break;
                }
                kept.push(b.slice(0, take));
                n += take;
            }
            let text = datafusion::arrow::util::pretty::pretty_format_batches(&kept)?.to_string();
            Ok((text, total > max_rows))
        })
    }

    /// Run `sql` and stream every result row to `out` as CSV (with a header),
    /// batch by batch, so arbitrarily large results never sit in memory.
    /// Returns the number of rows written.
    pub fn write_csv(&self, sql: &str, out: impl Write) -> Result<usize> {
        self.rt.block_on(async {
            let df = self.ctx.sql(sql).await?;
            let mut stream = df.execute_stream().await?;
            let mut w = datafusion::arrow::csv::WriterBuilder::new()
                .with_header(true)
                .build(out);
            let mut rows = 0;
            while let Some(batch) = stream.next().await {
                let batch = batch?;
                rows += batch.num_rows();
                w.write(&batch)?;
            }
            Ok(rows)
        })
    }

    /// Run `sql`, returning at most `max_rows` rows for display.
    pub fn query(&self, sql: &str, max_rows: usize) -> Result<SqlResult> {
        self.rt.block_on(async {
            let df = self.ctx.sql(sql).await?;
            let columns: Vec<String> = df
                .schema()
                .fields()
                .iter()
                .map(|f| f.name().to_string())
                .collect();

            // Pull one extra row so we can tell whether the result was capped.
            let batches = df.limit(0, Some(max_rows + 1))?.collect().await?;

            let opts = FormatOptions::default().with_null("");
            let mut rows: Vec<Vec<String>> = Vec::new();
            let mut truncated = false;
            'outer: for batch in &batches {
                let fmts = batch
                    .columns()
                    .iter()
                    .map(|c| ArrayFormatter::try_new(c.as_ref(), &opts))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                for r in 0..batch.num_rows() {
                    if rows.len() >= max_rows {
                        truncated = true;
                        break 'outer;
                    }
                    rows.push(fmts.iter().map(|f| f.value(r).to_string()).collect());
                }
            }

            Ok(SqlResult {
                columns,
                rows,
                truncated,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sample_csv() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "tessera_sql_{}_{}.csv",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "id,name,score").unwrap();
        for i in 0..20 {
            writeln!(f, "{i},name{i},{}", i * 10).unwrap();
        }
        f.flush().unwrap();
        p
    }

    #[test]
    fn runs_select_with_filter_and_order() {
        let path = sample_csv();
        let engine = SqlEngine::new(&path, FileKind::Csv, b',', true).unwrap();

        let res = engine
            .query(
                "SELECT id, score FROM data WHERE score >= 150 ORDER BY score DESC",
                1000,
            )
            .unwrap();
        assert_eq!(res.columns, vec!["id".to_string(), "score".to_string()]);
        // scores 150..190 → ids 15..19, five rows, highest first.
        assert_eq!(res.rows.len(), 5);
        assert_eq!(res.rows[0], vec!["19".to_string(), "190".to_string()]);
        assert!(!res.truncated);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reports_truncation_past_the_cap() {
        let path = sample_csv();
        let engine = SqlEngine::new(&path, FileKind::Csv, b',', true).unwrap();
        let res = engine.query("SELECT * FROM data", 5).unwrap();
        assert_eq!(res.rows.len(), 5);
        assert!(res.truncated);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn query_table_keeps_types_and_caps_rows() {
        let path = sample_csv();
        let engine = SqlEngine::new(&path, FileKind::Csv, b',', true).unwrap();

        let res = engine
            .query_table("SELECT id, name FROM data ORDER BY id DESC", 5)
            .unwrap();
        assert!(res.truncated);
        assert_eq!(res.table.num_rows(), 5);
        assert_eq!(res.table.column_names(), &["id", "name"]);
        // The integer column arrives as a real integer, not text.
        assert_eq!(res.table.column_types()[0], "int");
        assert_eq!(res.table.cell(0, 0), "19");

        let all = engine.query_table("SELECT COUNT(*) AS n FROM data", 5).unwrap();
        assert!(!all.truncated);
        assert_eq!(all.table.cell(0, 0), "20");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn pretty_and_csv_output() {
        let path = sample_csv();
        let engine = SqlEngine::new(&path, FileKind::Csv, b',', true).unwrap();

        let (text, more) = engine.pretty("SELECT id FROM data ORDER BY id", 3).unwrap();
        assert!(more);
        assert!(text.contains("| id |"), "{text}");
        assert!(text.contains("| 2  |"), "{text}");
        assert!(!text.contains("| 3  |"), "{text}");

        let mut buf = Vec::new();
        let n = engine
            .write_csv("SELECT id, score FROM data WHERE id < 3 ORDER BY id", &mut buf)
            .unwrap();
        assert_eq!(n, 3);
        assert_eq!(String::from_utf8(buf).unwrap(), "id,score\n0,0\n1,10\n2,20\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn surfaces_query_errors() {
        let path = sample_csv();
        let engine = SqlEngine::new(&path, FileKind::Csv, b',', true).unwrap();
        assert!(engine.query("SELECT * FROM nonexistent", 10).is_err());
        std::fs::remove_file(&path).ok();
    }
}
