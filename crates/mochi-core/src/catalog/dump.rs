//! A faithful, read-only view of a catalog's tables, for inspection tools
//! (`mochi dump-index`, plan C14 / next-work-plan K6).
//!
//! Table names come only from `sqlite_schema` of the catalog itself, never
//! from the caller: a requested name that is not in that list is
//! `INVALID_ARGUMENT`. Identifiers are quoted when they are put in a
//! statement, and the statement text is built from nothing else. No SQL is
//! ever accepted from outside.
//!
//! Rows are ordered by every column in turn, which for these tables is the
//! primary key order (every key column leads its table) and is total, so the
//! output is deterministic whatever the page layout. Values keep their
//! SQLite type: integer, text (as the stored bytes, since an archive's text
//! need not be UTF-8), BLOB, or null. The caller decides how to show them.

use crate::error::{ErrorCode, MochiError};

use super::{sql, Catalog};

type Result<T> = crate::error::Result<T>;

/// One stored value, with its SQLite type kept.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DumpValue {
    Null,
    Integer(i64),
    /// The exact stored bytes of a TEXT value (not necessarily UTF-8).
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

/// One table: its columns in declaration order and its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDump {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<DumpValue>>,
}

/// Tables in name order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogDump {
    pub tables: Vec<TableDump>,
}

/// A name as a quoted SQLite identifier.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Catalog {
    /// The names of every table, in name order, from `sqlite_schema`.
    pub fn table_names(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .map_err(sql)?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(sql)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(sql)?;
        Ok(names)
    }

    /// Every table with its row count, in name order, without reading rows.
    pub fn table_counts(&self) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for name in self.table_names()? {
            let n: i64 = self
                .conn
                .query_row(
                    &format!("SELECT count(*) FROM {}", quoted(&name)),
                    [],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            out.push((name, u64::try_from(n).unwrap_or(0)));
        }
        Ok(out)
    }

    /// The rows of `tables` (all of them for `None`), in table-name order
    /// whatever order they were asked for. A name that is not a table of
    /// this catalog is `INVALID_ARGUMENT`, naming the tables that exist.
    pub fn dump(&self, tables: Option<&[String]>) -> Result<CatalogDump> {
        let all = self.table_names()?;
        let wanted: Vec<&String> = match tables {
            None => all.iter().collect(),
            Some(asked) => {
                if let Some(bad) = asked.iter().find(|t| !all.contains(t)) {
                    return Err(MochiError::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "no table {bad:?} in the catalog; its tables are: {}",
                            all.join(", ")
                        ),
                    ));
                }
                all.iter().filter(|t| asked.contains(t)).collect()
            }
        };
        let mut out = Vec::new();
        for name in wanted {
            out.push(self.dump_table(name)?);
        }
        Ok(CatalogDump { tables: out })
    }

    fn dump_table(&self, name: &str) -> Result<TableDump> {
        // Column count first, so the ORDER BY names every column by position.
        let probe = self
            .conn
            .prepare(&format!("SELECT * FROM {}", quoted(name)))
            .map_err(sql)?;
        let columns: Vec<String> = probe.column_names().iter().map(|c| c.to_string()).collect();
        drop(probe);
        let order: Vec<String> = (1..=columns.len()).map(|i| i.to_string()).collect();
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT * FROM {} ORDER BY {}",
                quoted(name),
                order.join(", ")
            ))
            .map_err(sql)?;
        let mut rows = stmt.query([]).map_err(sql)?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(sql)? {
            let mut row = Vec::with_capacity(columns.len());
            for i in 0..columns.len() {
                use rusqlite::types::ValueRef;
                row.push(match r.get_ref(i).map_err(sql)? {
                    ValueRef::Null => DumpValue::Null,
                    ValueRef::Integer(n) => DumpValue::Integer(n),
                    ValueRef::Real(_) => {
                        // STRICT tables here have no REAL column; a catalog
                        // that held one would already have failed `verify`.
                        return Err(MochiError::new(
                            ErrorCode::CatalogInvalid,
                            format!("table {name:?} holds a REAL value"),
                        ));
                    }
                    ValueRef::Text(t) => DumpValue::Text(t.to_vec()),
                    ValueRef::Blob(b) => DumpValue::Blob(b.to_vec()),
                });
            }
            out.push(row);
        }
        Ok(TableDump {
            name: name.to_owned(),
            columns,
            rows: out,
        })
    }
}
