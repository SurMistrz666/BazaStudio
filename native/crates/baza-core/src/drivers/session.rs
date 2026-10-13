//! What a driver implements and what it is asked for: the [`Session`] of an
//! open connection, a page of records ([`PageRequest`]) and one change of
//! one record ([`RowChange`]).
use crate::builder::{Filter, QueryBuilder};
use crate::engine::Dialect;
use crate::model::{Cell, ColumnInfo, QueryResult, TableRef};
use anyhow::Result;
use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;

/// One page of the records of a container, as "Dane tabeli" asks for it.
#[derive(Clone, Debug)]
pub struct PageRequest {
    pub container: TableRef,
    /// All of them must hold.
    pub filters: Vec<Filter>,
    /// Column and whether the order is descending.
    pub order_by: Option<(String, bool)>,
    /// Number of records, between 1 and 20 000.
    pub limit: u32,
    /// Records to skip.
    pub offset: u64,
}
impl PageRequest {
    /// The first 500 records, unfiltered, in the order of the engine.
    pub fn first(container: TableRef) -> Self {
        Self {
            container,
            filters: Vec::new(),
            order_by: None,
            limit: 500,
            offset: 0,
        }
    }
}

/// One change of one record, made in the grid.
#[derive(Clone, Debug)]
pub enum RowChange {
    /// Give `column` the value `new` in the record with this key, but only
    /// while the column still holds `old`: the conflict check.
    UpdateCell {
        table: TableRef,
        column: String,
        old: Cell,
        new: Cell,
        key: Vec<(String, Cell)>,
    },
    /// Add a record. Columns that are not named take their defaults.
    InsertRow {
        table: TableRef,
        values: Vec<(String, Cell)>,
    },
    /// Remove the record with this key.
    DeleteRow {
        table: TableRef,
        key: Vec<(String, Cell)>,
    },
}

fn unsupported<'a, T: Send + 'a>() -> BoxFuture<'a, Result<T>> {
    Box::pin(async { Err(anyhow::anyhow!("operation_unsupported")) })
}

/// An open connection to a database: everything the application does with
/// one. `EngineSpec::connect` returns it.
///
/// The methods without a default are the minimum. The ones with a default
/// are written for an engine that speaks SQL: they build a statement from
/// [`Session::dialect`] and run it through [`Session::execute`] or
/// [`Session::execute_one`]. An engine without SQL overrides them, and
/// leaves out what its `Capabilities` say the interface must not offer.
///
/// Errors are short stable codes (`anyhow!("database_login_rejected")`),
/// never the text of the server, which can repeat an address or an account.
/// A session is shared between tasks: every method takes `&self`.
pub trait Session: Send + Sync {
    /// The dialect in effect: the one of the product, refined by what the
    /// server turned out to be where that matters.
    fn dialect(&self) -> &Dialect;

    /// Run one statement, a script or a command and return the last result
    /// set with its typed cells, or the number of affected records.
    /// Honours `cancel` (`query_cancelled`), ends after 120 seconds
    /// (`query_timeout`) and stops at `sql::MAX_RESULT_ROWS` rows or
    /// `sql::MAX_RESULT_BYTES` bytes with `truncated` set.
    fn execute<'a>(
        &'a self,
        text: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>>;

    /// What the side panel lists: tables and views, collections, key groups.
    /// `TableRef::kind` contains `table` for a container whose records can be
    /// edited and `view` for one that is read only.
    fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>>;

    /// Columns (fields) of a container, with the key that identifies a
    /// record: without a key the grid does not offer editing.
    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>>;

    /// Statistics for the "Monitor" section: one row, a light query that is
    /// repeated every five seconds. The columns `connections`, `active`,
    /// `max_connections` and `tables` have a translated name; `version` and
    /// `database` describe the server under the chart. Any other column is
    /// shown under the name the driver gives it, so name it for people. The
    /// first two columns are the large figures.
    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>>;

    /// End the session and release everything it holds.
    fn close<'a>(&'a self) -> BoxFuture<'a, ()>;

    /// Facts about one container for the information panel of "Obiekty":
    /// one row whose columns are named by keys the interface translates
    /// (`engine`, `row_estimate`, `data_size`, `index_size`, `created`,
    /// `updated`, `collation`, `comment`, `definition` and others in
    /// `drivers/README.md`); sizes in bytes. No row: nothing to tell.
    fn container_details<'a>(
        &'a self,
        _container: &'a TableRef,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async { Ok(QueryResult::default()) })
    }

    /// Run a statement that must change exactly one record, atomically:
    /// when it changes none or several, nothing is changed and the error is
    /// `row_changed_reload_required`. The default [`Session::apply`] uses it.
    fn execute_one<'a>(&'a self, _statement: &'a str) -> BoxFuture<'a, Result<()>> {
        unsupported()
    }

    /// What reading a page looks like as text: the statement shown in the
    /// tab and in the history.
    fn page_text(&self, page: &PageRequest) -> Result<String> {
        QueryBuilder::page(page).sql(self.dialect())
    }

    /// Read one page of a container.
    fn read_page<'a>(
        &'a self,
        page: &'a PageRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let text = self.page_text(page)?;
            self.execute(&text, cancel).await
        })
    }

    /// What a change looks like as text, for the history.
    fn change_text(&self, change: &RowChange) -> Result<String> {
        crate::sql::change_statement(self.dialect(), change)
    }

    /// Make one change of one record, or none at all: a cell with the
    /// conflict check, a new record, a removed record.
    fn apply<'a>(&'a self, change: &'a RowChange) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let statement = self.change_text(change)?;
            self.execute_one(&statement).await
        })
    }

    /// Add many records at once, all or none. The shape was checked by the
    /// caller: the columns exist, are distinct and every row has a value for
    /// each of them. Returns the number of records in `affected`.
    fn import_rows<'a>(
        &'a self,
        _container: &'a TableRef,
        _columns: &'a [String],
        _rows: &'a [Vec<Cell>],
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The least a driver can be: it only records what it is asked to run.
    struct Recorder {
        dialect: Dialect,
        ran: Mutex<Vec<String>>,
        guarded: bool,
    }
    impl Session for Recorder {
        fn dialect(&self) -> &Dialect {
            &self.dialect
        }
        fn execute<'a>(
            &'a self,
            text: &'a str,
            _: CancellationToken,
        ) -> BoxFuture<'a, Result<QueryResult>> {
            Box::pin(async move {
                self.ran.lock().unwrap().push(text.to_owned());
                Ok(QueryResult::default())
            })
        }
        fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn columns<'a>(&'a self, _: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
            Box::pin(async { Ok(QueryResult::default()) })
        }
        fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn execute_one<'a>(&'a self, statement: &'a str) -> BoxFuture<'a, Result<()>> {
            if !self.guarded {
                return unsupported();
            }
            Box::pin(async move {
                self.ran.lock().unwrap().push(format!("one: {statement}"));
                Ok(())
            })
        }
    }

    fn table() -> TableRef {
        TableRef {
            schema: "s".into(),
            name: "t".into(),
            kind: "table".into(),
        }
    }

    /// A driver that speaks SQL gets pages and grid changes from its
    /// dialect: it implements `execute` and `execute_one` and nothing more.
    #[tokio::test]
    async fn the_defaults_build_statements_from_the_dialect() {
        let session: Box<dyn Session> = Box::new(Recorder {
            dialect: Dialect::ANSI,
            ran: Mutex::default(),
            guarded: true,
        });
        let page = PageRequest {
            order_by: Some(("id".into(), true)),
            ..PageRequest::first(table())
        };
        assert_eq!(
            session.page_text(&page).unwrap(),
            "SELECT *\nFROM \"s\".\"t\"\nORDER BY \"id\" DESC\nLIMIT 500 OFFSET 0"
        );
        session
            .read_page(&page, CancellationToken::new())
            .await
            .unwrap();
        let change = RowChange::DeleteRow {
            table: table(),
            key: vec![("id".into(), Cell::Integer("7".into()))],
        };
        assert_eq!(
            session.change_text(&change).unwrap(),
            "DELETE FROM \"s\".\"t\" WHERE \"id\" = 7"
        );
        session.apply(&change).await.unwrap();
    }

    /// What a driver leaves out answers with one stable code, so that a
    /// missing capability flag shows up as a sentence, not as a crash.
    #[tokio::test]
    async fn what_a_driver_leaves_out_is_reported_as_unsupported() {
        let session = Recorder {
            dialect: Dialect::ANSI,
            ran: Mutex::default(),
            guarded: false,
        };
        let change = RowChange::InsertRow {
            table: table(),
            values: Vec::new(),
        };
        assert_eq!(
            session.apply(&change).await.unwrap_err().to_string(),
            "operation_unsupported"
        );
        assert_eq!(
            session
                .import_rows(&table(), &[], &[], CancellationToken::new())
                .await
                .unwrap_err()
                .to_string(),
            "operation_unsupported"
        );
        assert!(session.ran.lock().unwrap().is_empty());
    }
}
