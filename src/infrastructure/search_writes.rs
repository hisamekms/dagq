//! Search adapter write entry (ADR-t3982-1 decision 3, ADR-t3986-1 decision 6).
//! Call after writing the source row, in that same execution-port transaction.
//! SQLite's triggers already did the work. Import can use the same per-row entry.
use super::sql_exec::{SqlDialect, SqlWrite};
use anyhow::Result;

/// The source row to copy. A goal update carries its acceptance before the
/// write; `None` means insertion, so importing a version never increments it.
/// Call once per source mutation; integrated events are idempotent by run ID.
#[derive(Clone, Copy)]
pub enum SearchWrite<'a> {
    Task(i64),
    Goal {
        id: i64,
        previous_acceptance: Option<&'a str>,
    },
    Note(i64),
    Integrated(i64),
    Commit(i64),
}

pub fn maintain(tx: &impl SqlWrite, change: SearchWrite<'_>) -> Result<()> {
    if tx.dialect() == SqlDialect::Sqlite {
        return Ok(());
    }
    super::postgres::search_write::maintain(tx, change)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::sql_exec::{SqlRead, SqlResult, SqlRow, SqlValue};
    struct NoSql;
    impl SqlRead for NoSql {
        fn dialect(&self) -> SqlDialect {
            SqlDialect::Sqlite
        }
        fn visit_rows(
            &self,
            _: &str,
            _: &[SqlValue],
            _: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
        ) -> SqlResult<()> {
            panic!("SQLite must not read")
        }
    }
    impl SqlWrite for NoSql {
        fn after_commit(&self, _: Box<dyn FnOnce()>) -> SqlResult<()> {
            panic!("SQLite must not register a commit action")
        }
        fn execute(&self, _: &str, _: &[SqlValue]) -> SqlResult<usize> {
            panic!("SQLite must not write")
        }
        fn insert_id(&self, _: &str, _: &[SqlValue]) -> SqlResult<i64> {
            panic!("SQLite must not insert")
        }
        fn insert_id_if_inserted(&self, _: &str, _: &[SqlValue]) -> SqlResult<Option<i64>> {
            panic!("SQLite must not insert")
        }
    }
    #[test]
    fn sqlite_executes_nothing_for_every_source_change() {
        for change in [
            SearchWrite::Task(1),
            SearchWrite::Goal {
                id: 1,
                previous_acceptance: None,
            },
            SearchWrite::Goal {
                id: 1,
                previous_acceptance: Some("old"),
            },
            SearchWrite::Note(1),
            SearchWrite::Integrated(1),
            SearchWrite::Commit(1),
        ] {
            maintain(&NoSql, change).unwrap();
        }
    }
}
