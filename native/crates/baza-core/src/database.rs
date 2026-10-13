//! An open database, whatever its engine: the one type the application
//! holds for a connection. It finds the driver in the registry, checks what
//! every driver would have to check, and hands the work to the driver's
//! [`Session`].
use crate::drivers::{PageRequest, RowChange, Session};
use crate::engine::{Capabilities, ConnectRequest, Dialect, Engine, ProductSpec};
use crate::{model::*, sql::*};
use anyhow::{Result, ensure};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Database {
    session: Arc<dyn Session>,
    engine: Engine,
    product: &'static ProductSpec,
}
impl Database {
    /// Open the database of a profile. `forwarded_port` is the loopback end
    /// of an SSH tunnel to the server: the driver dials it, while the host
    /// of the profile stays the name of the server for TLS.
    pub async fn connect(profile: &ConnectionProfile, forwarded_port: Option<u16>) -> Result<Self> {
        profile.validate()?;
        let engine = profile.engine;
        let spec = engine.spec();
        ensure!(spec.available, "engine_unsupported");
        let product = profile.product_spec();
        let request = ConnectRequest {
            profile,
            product,
            forwarded: forwarded_port
                .map(|port| std::net::SocketAddr::from(([127, 0, 0, 1], port))),
        };
        let session = (spec.connect)(&request).await?;
        Ok(Self {
            session,
            engine,
            product,
        })
    }
    /// The engine family of the connection.
    pub fn engine(&self) -> Engine {
        self.engine
    }
    /// The product the profile was saved for.
    pub fn product(&self) -> &'static ProductSpec {
        self.product
    }
    /// What the interface may offer for this connection.
    pub fn capabilities(&self) -> &'static Capabilities {
        self.engine.spec().capabilities_of(self.product)
    }
    /// The SQL dialect in effect, as the driver settled it after connecting.
    pub fn dialect(&self) -> &Dialect {
        self.session.dialect()
    }
    pub async fn close(&self) {
        self.session.close().await
    }
    /// Run a statement, a script or a command.
    pub async fn query(&self, sql: &str, cancel: CancellationToken) -> Result<QueryResult> {
        ensure!(
            !sql.trim().is_empty() && sql.len() <= MAX_SQL_BYTES && !sql.contains('\0'),
            "sql_invalid"
        );
        self.session.execute(sql, cancel).await
    }
    /// Run a statement that must change exactly one row.
    pub async fn mutate_one(&self, sql: &str) -> Result<()> {
        self.session.execute_one(sql).await
    }
    /// What reading a page of a container looks like as text.
    pub fn page_text(&self, page: &PageRequest) -> Result<String> {
        self.session.page_text(page)
    }
    /// Read one page of a container.
    pub async fn read_page(
        &self,
        page: &PageRequest,
        cancel: CancellationToken,
    ) -> Result<QueryResult> {
        self.session.read_page(page, cancel).await
    }
    /// What a change of one record looks like as text, for the history.
    pub fn change_text(&self, change: &RowChange) -> Result<String> {
        self.session.change_text(change)
    }
    /// Make one change of one record, or none at all.
    pub async fn apply(&self, change: &RowChange) -> Result<()> {
        self.session.apply(change).await
    }
    pub async fn import_rows(
        &self,
        table: &TableRef,
        columns: &[String],
        rows: &[Vec<Cell>],
        cancel: CancellationToken,
    ) -> Result<QueryResult> {
        ensure!(
            !columns.is_empty() && columns.len() <= 128 && rows.len() <= 20_000,
            "import_shape_invalid"
        );
        let actual = self.columns(table).await?;
        ensure!(
            columns.iter().all(|c| actual.iter().any(|a| &a.name == c)),
            "import_column_unknown"
        );
        let unique = columns.iter().collect::<std::collections::HashSet<_>>();
        ensure!(
            unique.len() == columns.len() && rows.iter().all(|r| r.len() == columns.len()),
            "import_shape_invalid"
        );
        self.session.import_rows(table, columns, rows, cancel).await
    }
    /// Tables and views, or what the engine has in their place.
    pub async fn tables(&self) -> Result<Vec<TableRef>> {
        self.session.containers().await
    }
    pub async fn columns(&self, t: &TableRef) -> Result<Vec<ColumnInfo>> {
        self.session.columns(t).await
    }
    pub async fn server_stats(&self) -> Result<QueryResult> {
        self.session.server_stats().await
    }
    /// Facts about one container for the information panel.
    pub async fn container_details(&self, t: &TableRef) -> Result<QueryResult> {
        self.session.container_details(t).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::sqlite;
    #[tokio::test]
    async fn expression_columns_are_named_after_their_values_never_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("types.sqlite");
        std::fs::File::create(&path).unwrap();
        let p = ConnectionProfile {
            name: "Types".into(),
            engine: sqlite::ENGINE,
            sqlite_path: Some(path),
            ..ConnectionProfile::default()
        };
        let db = Database::connect(&p, None).await.unwrap();
        db.query(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, status TEXT, amount REAL); INSERT INTO t VALUES (1,'a',1.5),(2,'a',NULL),(3,NULL,2.5)",
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let types = |result: &QueryResult| {
            result
                .columns
                .iter()
                .map(|column| column.data_type.clone())
                .collect::<Vec<_>>()
        };
        // What the query builder runs: a grouped column and an aggregate.
        let grouped = db
            .query(
                "SELECT status, COUNT(status) AS result, SUM(amount) AS total, 'x' || status AS label, NULL AS blank FROM t GROUP BY status ORDER BY status",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        // The first row holds NULLs for `status` and `label`; the type comes
        // from the first row that has a value. A column of NULLs has none.
        assert_eq!(types(&grouped), ["TEXT", "INTEGER", "REAL", "TEXT", ""]);
        assert!(
            types(&grouped)
                .iter()
                .all(|name| !name.eq_ignore_ascii_case("null"))
        );
        // Declared types are left alone.
        let plain = db
            .query("SELECT id, status FROM t", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(types(&plain), ["INTEGER", "TEXT"]);
        db.close().await;
    }
    #[tokio::test]
    async fn sqlite_real_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.sqlite");
        std::fs::File::create(&path).unwrap();
        let p = ConnectionProfile {
            id: uuid::Uuid::new_v4(),
            name: "QA".into(),
            engine: sqlite::ENGINE,
            sqlite_path: Some(path),
            ..ConnectionProfile::default()
        };
        let db = Database::connect(&p, None).await.unwrap();
        db.query("CREATE TABLE customers(id INTEGER PRIMARY KEY,name TEXT, amount TEXT); INSERT INTO customers VALUES(2,'old', '18446744073709551615');",CancellationToken::new()).await.unwrap();
        let ts = db.tables().await.unwrap();
        assert_eq!(ts[0].name, "customers");
        let cs = db.columns(&ts[0]).await.unwrap();
        assert!(cs[0].primary_key);
        let r = db
            .query(
                "SELECT * FROM customers WHERE id=2",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(r.rows[0][2], Cell::Text("18446744073709551615".into()));
        let u = update_cell(
            db.dialect(),
            &ts[0],
            "name",
            &r.rows[0][1],
            &Cell::Text("new'; DROP TABLE customers;--".into()),
            &[("id".into(), r.rows[0][0].clone())],
        )
        .unwrap();
        db.mutate_one(&u).await.unwrap();
        assert!(db.mutate_one(&u).await.is_err());
        let r = db
            .query(
                "SELECT name FROM customers WHERE id=2",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.rows[0][0],
            Cell::Text("new'; DROP TABLE customers;--".into())
        );
        db.close().await;
    }
    #[tokio::test]
    async fn cancellation_is_real() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("q.sqlite");
        std::fs::File::create(&p).unwrap();
        let cp = ConnectionProfile {
            name: "Cancel".into(),
            engine: sqlite::ENGINE,
            sqlite_path: Some(p),
            ..ConnectionProfile::default()
        };
        let db = Database::connect(&cp, None).await.unwrap();
        let c = CancellationToken::new();
        c.cancel();
        assert_eq!(
            db.query("SELECT 1", c).await.unwrap_err().to_string(),
            "query_cancelled"
        );
        assert_eq!(
            db.query("SELECT 2", CancellationToken::new())
                .await
                .unwrap()
                .rows[0][0],
            Cell::Integer("2".into())
        );
        db.close().await;
    }
}

#[cfg(test)]
mod acceptance_tests {
    use super::*;
    use crate::drivers::sqlite;
    use std::time::{Duration, Instant};
    async fn fixture() -> (tempfile::TempDir, Database, TableRef) {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("acceptance.sqlite");
        std::fs::File::create(&path).unwrap();
        let p = ConnectionProfile {
            name: "Owned acceptance".into(),
            engine: sqlite::ENGINE,
            sqlite_path: Some(path),
            ..Default::default()
        };
        let db = Database::connect(&p, None).await.unwrap();
        db.query(
            "CREATE TABLE q(id INTEGER PRIMARY KEY,name TEXT);INSERT INTO q VALUES(1,'existing')",
            CancellationToken::new(),
        )
        .await
        .unwrap();
        (
            d,
            db,
            TableRef {
                schema: "main".into(),
                name: "q".into(),
                kind: "table".into(),
            },
        )
    }
    #[tokio::test]
    async fn running_sqlite_query_interrupts_and_pool_recovers() {
        let (_d, db, _) = fixture().await;
        let token = CancellationToken::new();
        let c = token.clone();
        let d = db.clone();
        let task = tokio::spawn(async move {
            d.query("WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<1000000000) SELECT sum(n) FROM c",c).await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let now = Instant::now();
        token.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err().to_string(), "query_cancelled");
        assert!(now.elapsed() < Duration::from_secs(2));
        assert_eq!(
            db.query("SELECT 3", CancellationToken::new())
                .await
                .unwrap()
                .rows[0][0],
            Cell::Integer("3".into())
        );
        db.close().await;
    }
    #[tokio::test]
    async fn import_is_atomic_on_conflict() {
        let (_d, db, t) = fixture().await;
        let rows = vec![
            vec![Cell::Integer("2".into()), Cell::Text("new".into())],
            vec![Cell::Integer("1".into()), Cell::Text("collision".into())],
        ];
        assert!(
            db.import_rows(
                &t,
                &["id".into(), "name".into()],
                &rows,
                CancellationToken::new()
            )
            .await
            .is_err()
        );
        let q = db
            .query("SELECT * FROM q", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(q.rows.len(), 1);
        assert_eq!(q.rows[0][1], Cell::Text("existing".into()));
        assert!(
            db.import_rows(
                &t,
                &["id".into(), "id".into()],
                &rows,
                CancellationToken::new()
            )
            .await
            .is_err()
        );
        db.close().await;
    }
    #[tokio::test]
    async fn empty_strings_null_unicode_and_quotes_roundtrip() {
        let (_d, db, t) = fixture().await;
        let values = [
            Cell::Text("".into()),
            Cell::Null,
            Cell::Text("Zażółć 🦀 ' \\".into()),
        ];
        for (i, v) in values.iter().enumerate() {
            db.import_rows(
                &t,
                &["id".into(), "name".into()],
                &[vec![Cell::Integer((i + 2).to_string()), v.clone()]],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        }
        let q = db
            .query(
                "SELECT name FROM q WHERE id>1 ORDER BY id",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            q.rows.iter().map(|r| r[0].clone()).collect::<Vec<_>>(),
            values
        );
        db.close().await;
    }
    /// The grid changes records through `apply`: a cell with its conflict
    /// check, a new record and a removed one, each as one statement.
    #[tokio::test]
    async fn one_record_changes_through_the_session() {
        let (_d, db, t) = fixture().await;
        let key = vec![("id".to_owned(), Cell::Integer("1".into()))];
        let update = RowChange::UpdateCell {
            table: t.clone(),
            column: "name".into(),
            old: Cell::Text("existing".into()),
            new: Cell::Text("changed".into()),
            key: key.clone(),
        };
        assert_eq!(
            db.change_text(&update).unwrap(),
            "UPDATE \"main\".\"q\" SET \"name\" = 'changed' WHERE \"id\" = 1 AND \"name\" = 'existing'"
        );
        db.apply(&update).await.unwrap();
        // The old value is gone: the same change is a conflict now.
        assert_eq!(
            db.apply(&update).await.unwrap_err().to_string(),
            "row_changed_reload_required"
        );
        db.apply(&RowChange::InsertRow {
            table: t.clone(),
            values: vec![("name".into(), Cell::Text("added".into()))],
        })
        .await
        .unwrap();
        db.apply(&RowChange::InsertRow {
            table: t.clone(),
            values: vec![],
        })
        .await
        .unwrap();
        db.apply(&RowChange::DeleteRow {
            table: t.clone(),
            key,
        })
        .await
        .unwrap();
        let page = db
            .read_page(&PageRequest::first(t.clone()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            page.rows,
            [
                vec![Cell::Integer("2".into()), Cell::Text("added".into())],
                vec![Cell::Integer("3".into()), Cell::Null],
            ]
        );
        assert_eq!(
            db.page_text(&PageRequest::first(t)).unwrap(),
            "SELECT *\nFROM \"main\".\"q\"\nLIMIT 500 OFFSET 0"
        );
        db.close().await;
    }
}
