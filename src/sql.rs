//! Optional SQL querying powered by [DataFusion].
//!
//! DataFusion reads the source file itself (CSV or Parquet) and exposes it as a
//! table named `data`, so users can write queries like
//! `SELECT * FROM data WHERE amount > 100 ORDER BY amount DESC`. Results are
//! eagerly stringified into a plain grid, which keeps DataFusion's own (older)
//! Arrow version entirely contained in this module — the rest of the app keeps
//! using its own Arrow build.
//!
//! [DataFusion]: https://datafusion.apache.org/

use std::path::Path;

use anyhow::{anyhow, Result};
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::prelude::{CsvReadOptions, ParquetReadOptions, SessionContext};

use crate::data::FileKind;

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
}

impl SqlEngine {
    /// The SQL table name the source file is registered under.
    pub const TABLE: &'static str = "data";

    /// Register `path` (interpreted as `kind`) as the `data` table.
    pub fn new(path: &Path, kind: FileKind) -> Result<SqlEngine> {
        let rt = tokio::runtime::Runtime::new()?;
        let ctx = SessionContext::new();
        let p = path
            .to_str()
            .ok_or_else(|| anyhow!("path is not valid UTF-8"))?;
        rt.block_on(async {
            match kind {
                FileKind::Csv => {
                    ctx.register_csv(Self::TABLE, p, CsvReadOptions::new()).await
                }
                FileKind::Parquet => {
                    ctx.register_parquet(Self::TABLE, p, ParquetReadOptions::default())
                        .await
                }
            }
        })?;
        Ok(SqlEngine { rt, ctx })
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
