//! What the three SQLx drivers (PostgreSQL, MySQL, SQLite) have in common:
//! the loop that runs a statement, the guarded single-row change, the bulk
//! insert and the mapping of SQLx errors to result codes.
//!
//! The pools of the three drivers are different types without a common
//! trait that covers what is needed here, so the shared code is three macros
//! that are expanded once per driver.
use crate::model::{Cell, ResultColumn};

/// Why a connection was not opened. The answers a person can act on have
/// codes of their own; the server's text is never passed on, because it can
/// repeat the user name and the address.
///
/// * `database_login_rejected`: the user name or the password is wrong
///   (SQLSTATE class 28).
/// * `database_not_found`: the server has no database with that name
///   (PostgreSQL 3D000, MySQL and MariaDB error 1049).
/// * `database_access_denied`: the user may not use that database (MySQL and
///   MariaDB error 1044, PostgreSQL 42501).
/// * `database_certificate_rejected`: the TLS certificate of the server did
///   not pass verification, for example a self-signed one, which current
///   MariaDB servers present out of the box. The TLS library reports it as an
///   I/O error, which would otherwise read as a lost network connection.
/// * `database_tls_unsupported`: encryption was asked for and the server has
///   none.
pub(super) fn connect_error(e: sqlx::Error) -> anyhow::Error {
    let tls = match &e {
        sqlx::Error::Tls(inner) if inner.to_string().contains("does not support TLS") => {
            Some("database_tls_unsupported")
        }
        sqlx::Error::Tls(inner) if inner.to_string().contains("certificate") => {
            Some("database_certificate_rejected")
        }
        sqlx::Error::Io(inner)
            if inner.kind() == std::io::ErrorKind::InvalidData
                && inner.to_string().contains("certificate") =>
        {
            Some("database_certificate_rejected")
        }
        _ => None,
    };
    if let Some(code) = tls {
        return anyhow::anyhow!(code);
    }
    if let sqlx::Error::Database(d) = &e {
        let state = d.code().unwrap_or_default().into_owned();
        let number = d
            .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            .map(sqlx::mysql::MySqlDatabaseError::number);
        let code = match (state.as_str(), number) {
            (state, _) if state.starts_with("28") => Some("database_login_rejected"),
            ("3D000", _) | (_, Some(1049)) => Some("database_not_found"),
            ("42501", _) | (_, Some(1044)) => Some("database_access_denied"),
            _ => None,
        };
        if let Some(code) = code {
            return anyhow::anyhow!(code);
        }
    }
    safe_db_error(e)
}

/// A failure of a statement as a code, with the SQLSTATE when there is one.
pub(super) fn safe_db_error(e: sqlx::Error) -> anyhow::Error {
    let summary = match &e {
        sqlx::Error::Database(d) => {
            if let Some(code) = d.code() {
                format!("database_error [{}]", code)
            } else {
                "database_error".into()
            }
        }
        sqlx::Error::Io(_) => "database_network_error".into(),
        sqlx::Error::Tls(_) => "database_tls_error".into(),
        sqlx::Error::PoolTimedOut => "database_connection_timeout".into(),
        _ => "database_operation_failed".into(),
    };
    anyhow::anyhow!(summary)
}

/// A column that is an expression, such as `COUNT(*)`, has no declared type
/// in SQLite: all the driver can say about it is `NULL`. Such a column is
/// named after the first value it holds; one that only holds NULL keeps no
/// type name at all. Returns whether a column is still waiting for a value.
pub(super) fn name_untyped(columns: &mut [ResultColumn], cells: &[Cell]) -> bool {
    let mut waiting = false;
    for (column, cell) in columns.iter_mut().zip(cells) {
        if !column.data_type.eq_ignore_ascii_case("NULL") {
            continue;
        }
        let name = match cell {
            Cell::Integer(_) => "INTEGER",
            Cell::Float(_) => "REAL",
            Cell::Decimal(_) => "NUMERIC",
            Cell::Text(_) | Cell::Json(_) => "TEXT",
            Cell::Bytes(_) => "BLOB",
            Cell::Bool(_) => "BOOLEAN",
            Cell::Null | Cell::Unsupported(_) => {
                waiting = true;
                continue;
            }
        };
        column.data_type = name.into();
    }
    waiting
}

/// A decimal as the server stores it: every digit of its scale is shown, so
/// `0.00` of a `DECIMAL(12,2)` column does not turn into `0` next to `318.00`.
pub(super) fn decimal_text(value: &bigdecimal::BigDecimal) -> String {
    let (_, scale) = value.as_bigint_and_exponent();
    match usize::try_from(scale) {
        Ok(digits) if digits > 0 => format!("{value:.digits$}"),
        _ => value.to_string(),
    }
}

/// Size of a row as it counts towards `sql::MAX_RESULT_BYTES`.
pub(super) fn row_bytes(cells: &[Cell]) -> usize {
    cells
        .iter()
        .map(|cell| match cell {
            Cell::Bytes(value) => value.len(),
            _ => cell.display().len(),
        })
        .sum()
}

/// Run `$sql` on a connection of `$pool` and evaluate to the `QueryResult` of
/// its last result set. To be used inside an `async` block that returns
/// `anyhow::Result<QueryResult>`.
///
/// `$session` is handed to the three hooks of the driver:
///
/// * `$setup(session, &mut connection, cancel)` prepares the connection and
///   returns what `$abort` needs to stop its statement from outside, of
///   whatever type suits the driver;
/// * `$abort(session, handle)` stops the statement of a connection that was
///   cancelled, ran out of time or returned more than a result may hold;
/// * `$cleanup(session, &mut connection)` undoes `$setup` after a statement
///   that ended by itself.
///
/// `$decode(&row)` turns a row into typed cells.
macro_rules! run_statement {
    ($session:expr, $pool:expr, $sql:expr, $cancel:expr, $decode:path, $setup:path, $cleanup:path, $abort:path) => {{
        use futures_util::StreamExt as _;
        use sqlx::{Column as _, Row as _, TypeInfo as _};
        use $crate::drivers::sqlx_support::{name_untyped, row_bytes, safe_db_error};
        use $crate::model::{QueryResult, ResultColumn};
        use $crate::sql::{MAX_RESULT_BYTES, MAX_RESULT_ROWS};

        let cancel: tokio_util::sync::CancellationToken = $cancel;
        let started = std::time::Instant::now();
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(120));
        tokio::pin!(deadline);
        let mut connection = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(anyhow::anyhow!("query_cancelled")),
            _ = &mut deadline => return Err(anyhow::anyhow!("query_timeout")),
            result = $pool.acquire() => result.map_err(safe_db_error)?,
        };
        let backend = $setup($session, &mut *connection, cancel.clone()).await?;
        let mut result = QueryResult::default();
        let mut bytes = 0usize;
        // A new result set starts: its columns replace the previous ones.
        let mut fresh = true;
        let mut untyped = true;
        let mut abort = false;
        let mut failure = None;
        {
            let mut stream = sqlx::raw_sql(sqlx::AssertSqlSafe($sql)).fetch_many(&mut *connection);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        abort = true;
                        failure = Some(anyhow::anyhow!("query_cancelled"));
                        break;
                    }
                    _ = &mut deadline => {
                        abort = true;
                        failure = Some(anyhow::anyhow!("query_timeout"));
                        break;
                    }
                    item = stream.next() => match item {
                        None => break,
                        Some(Err(error)) => {
                            failure = Some(safe_db_error(error));
                            break;
                        }
                        Some(Ok(sqlx::Either::Left(done))) => {
                            result.affected = result.affected.saturating_add(done.rows_affected());
                            fresh = true;
                        }
                        Some(Ok(sqlx::Either::Right(row))) => {
                            if fresh {
                                result.columns = row
                                    .columns()
                                    .iter()
                                    .map(|column| ResultColumn {
                                        name: column.name().into(),
                                        data_type: column.type_info().name().into(),
                                    })
                                    .collect();
                                result.rows.clear();
                                bytes = 0;
                                fresh = false;
                                untyped = true;
                            }
                            let cells = $decode(&row);
                            if untyped {
                                untyped = name_untyped(&mut result.columns, &cells);
                            }
                            let size = row_bytes(&cells);
                            if result.rows.len() >= MAX_RESULT_ROWS
                                || bytes.saturating_add(size) > MAX_RESULT_BYTES
                            {
                                result.truncated = true;
                                abort = true;
                                break;
                            }
                            bytes += size;
                            result.rows.push(cells);
                        }
                    }
                }
            }
        }
        if abort {
            // The statement may still be running on the server. Stop it from
            // another connection and never give this one back to the pool.
            cancel.cancel();
            let connection = connection.detach();
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(2), $abort($session, backend))
                    .await;
            let _ = sqlx::Connection::close_hard(connection).await;
        } else {
            $cleanup($session, &mut *connection).await?;
        }
        if let Some(error) = failure {
            return Err(error);
        }
        for column in &mut result.columns {
            if column.data_type.eq_ignore_ascii_case("NULL") {
                column.data_type.clear();
            }
        }
        result.duration_ms = started.elapsed().as_millis() as u64;
        result
    }};
}
pub(super) use run_statement;

/// Run `$statement` in a transaction of `$pool` that is committed only when
/// exactly one row was changed. To be used inside an `async` block that
/// returns `anyhow::Result<()>`.
macro_rules! execute_exactly_one {
    ($pool:expr, $statement:expr) => {{
        use $crate::drivers::sqlx_support::safe_db_error;
        let mut transaction = $pool.begin().await.map_err(safe_db_error)?;
        let changed = sqlx::raw_sql(sqlx::AssertSqlSafe($statement))
            .execute(&mut *transaction)
            .await
            .map_err(safe_db_error)?
            .rows_affected();
        if changed != 1 {
            transaction.rollback().await.map_err(safe_db_error)?;
            anyhow::bail!("row_changed_reload_required");
        }
        transaction.commit().await.map_err(safe_db_error)?;
    }};
}
pub(super) use execute_exactly_one;

/// Insert `$rows` into `$table` in one transaction of `$pool`, one `INSERT`
/// per row with literals of `$dialect`, and evaluate to the `QueryResult`
/// with the number of rows. To be used inside an `async` block that returns
/// `anyhow::Result<QueryResult>`.
macro_rules! insert_rows {
    ($pool:expr, $dialect:expr, $table:expr, $columns:expr, $rows:expr, $cancel:expr) => {{
        use $crate::drivers::sqlx_support::safe_db_error;
        use $crate::sql::{MAX_SQL_BYTES, literal, qualified, quote_identifier};
        let dialect: &$crate::engine::Dialect = $dialect;
        let cancel: tokio_util::sync::CancellationToken = $cancel;
        let target = qualified(dialect, $table)?;
        let names = $columns
            .iter()
            .map(|column| quote_identifier(dialect, column))
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(", ");
        let started = std::time::Instant::now();
        let mut transaction = $pool.begin().await.map_err(safe_db_error)?;
        let mut affected = 0u64;
        for row in $rows {
            if cancel.is_cancelled() {
                transaction.rollback().await.map_err(safe_db_error)?;
                anyhow::bail!("query_cancelled");
            }
            let statement = format!(
                "INSERT INTO {target} ({names}) VALUES ({})",
                row.iter()
                    .map(|cell| literal(dialect, cell))
                    .collect::<anyhow::Result<Vec<_>>>()?
                    .join(", ")
            );
            anyhow::ensure!(statement.len() <= MAX_SQL_BYTES, "import_row_too_large");
            affected += sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
                .execute(&mut *transaction)
                .await
                .map_err(safe_db_error)?
                .rows_affected();
        }
        transaction.commit().await.map_err(safe_db_error)?;
        $crate::model::QueryResult {
            affected,
            duration_ms: started.elapsed().as_millis() as u64,
            ..Default::default()
        }
    }};
}
pub(super) use insert_rows;

/// Read a list of containers from a result whose rows are
/// `schema, name, kind`.
pub(super) fn containers_of(result: &crate::model::QueryResult) -> Vec<crate::model::TableRef> {
    result
        .rows
        .iter()
        .filter(|row| row.len() >= 3)
        .map(|row| crate::model::TableRef {
            schema: row[0].display(),
            name: row[1].display(),
            kind: row[2].display().to_lowercase(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decimals_keep_the_digits_of_their_scale() {
        for text in [
            "0.00",
            "318.00",
            "-0.50",
            "9840.15",
            "0",
            "42",
            "0.000000001",
            "123456789012345678901234567890.1200",
        ] {
            let value: bigdecimal::BigDecimal = text.parse().unwrap();
            assert_eq!(decimal_text(&value), text);
            // The text is still the same number for a literal.
            assert_eq!(
                decimal_text(&value)
                    .parse::<bigdecimal::BigDecimal>()
                    .unwrap(),
                value
            );
        }
        // A negative scale has no fractional digits to keep.
        let large = bigdecimal::BigDecimal::new(12.into(), -3);
        assert_eq!(
            decimal_text(&large)
                .parse::<bigdecimal::BigDecimal>()
                .unwrap(),
            large
        );
    }
}
