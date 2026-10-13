//! Explicit disposable-fixture acceptance, never called during normal startup.
use anyhow::{Result, anyhow, ensure};
use baza_core::{
    database::Database,
    drivers::{mysql, postgres},
    model::{Cell, ConnectionProfile, Engine, QueryResult, TableRef, TlsMode},
    sql::{quote_identifier, update_cell},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{ConnectOptions, Connection};
use std::{
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

#[derive(Deserialize)]
struct Fixture {
    run_id: Uuid,
    postgres_port: u16,
    mariadb_port: u16,
    postgres_password: String,
    mariadb_password: String,
    username: String,
    database: String,
    ca_file: PathBuf,
    #[serde(default)]
    host: String,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.postgres_password.zeroize();
        self.mariadb_password.zeroize();
    }
}
struct PrivateProfile(ConnectionProfile);
impl std::ops::Deref for PrivateProfile {
    type Target = ConnectionProfile;
    fn deref(&self) -> &ConnectionProfile {
        &self.0
    }
}
impl Drop for PrivateProfile {
    fn drop(&mut self) {
        self.0.password.zeroize();
    }
}
#[derive(Serialize)]
struct Check {
    name: String,
    pass: bool,
    code: String,
}
#[derive(Serialize)]
struct Report {
    product: &'static str,
    implementation: &'static str,
    fixture_run: Uuid,
    ca_sha256: String,
    checks: Vec<Check>,
    passed: usize,
    failed: usize,
    all_pass: bool,
    limitations: &'static str,
}
fn read_bounded(path: &Path, limit: usize) -> Result<Zeroizing<Vec<u8>>> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| anyhow!("tls_fixture_missing"))?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= limit as u64,
        "tls_fixture_invalid"
    );
    let mut file = std::fs::File::open(path).map_err(|_| anyhow!("tls_fixture_read_failed"))?;
    let mut bytes = Zeroizing::new(Vec::new());
    Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow!("tls_fixture_read_failed"))?;
    ensure!(bytes.len() <= limit, "tls_fixture_invalid");
    Ok(bytes)
}
fn load_fixture(path: &Path) -> Result<Fixture> {
    let bytes = read_bounded(path, 64 * 1024)?;
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    let fixture: Fixture =
        serde_json::from_slice(bytes).map_err(|_| anyhow!("tls_fixture_invalid"))?;
    let parent = path
        .canonicalize()?
        .parent()
        .ok_or_else(|| anyhow!("tls_fixture_invalid"))?
        .to_path_buf();
    let expected = format!("native-db-qa-{}", &fixture.run_id.to_string()[..8]);
    ensure!(
        parent.file_name().and_then(|n| n.to_str()) == Some(&expected),
        "tls_fixture_marker_invalid"
    );
    ensure!(
        fixture.ca_file.is_absolute()
            && fixture.ca_file.canonicalize()?.parent() == Some(parent.as_path()),
        "tls_fixture_ca_scope_invalid"
    );
    ensure!(
        fixture.database == "baza_owned_qa"
            && (fixture.host.is_empty() || fixture.host == "localhost")
            && fixture.postgres_port >= 1024
            && fixture.mariadb_port >= 1024
            && fixture.postgres_port != fixture.mariadb_port
            && !fixture.username.is_empty()
            && fixture.username.len() <= 128
            && !fixture.username.contains('\0')
            && (16..=4096).contains(&fixture.postgres_password.len())
            && (16..=4096).contains(&fixture.mariadb_password.len()),
        "tls_fixture_scope_invalid"
    );
    read_bounded(&fixture.ca_file, 64 * 1024)?;
    Ok(fixture)
}
fn profile(f: &Fixture, engine: Engine) -> PrivateProfile {
    PrivateProfile(ConnectionProfile {
        name: "Owned TLS acceptance".into(),
        engine,
        host: "localhost".into(),
        port: engine.port(),
        database: f.database.clone(),
        username: f.username.clone(),
        password: if engine == postgres::ENGINE {
            f.postgres_password.clone()
        } else {
            f.mariadb_password.clone()
        },
        tls: TlsMode::VerifyFull,
        ca_file: Some(f.ca_file.clone()),
        ..ConnectionProfile::default()
    })
}
fn cell<'a>(result: &'a QueryResult, name: &str, row: usize) -> Result<&'a Cell> {
    let index = result
        .columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| anyhow!("tls_qa_column_missing"))?;
    result
        .rows
        .get(row)
        .and_then(|r| r.get(index))
        .ok_or_else(|| anyhow!("tls_qa_row_missing"))
}
const DECIMAL: &str = "12345678901234567890123456789012345.123456789012345678901234567890";
const JSON: &str = r#"{"huge":340282366920938463463374607431768211457,"items":[true,null,"żółć"]}"#;
fn typed(result: &QueryResult, label: &str) -> Result<()> {
    ensure!(
        result.rows.len() == 2 && !result.truncated,
        "tls_qa_result_shape"
    );
    ensure!(
        cell(result, "signed_value", 0)? == &Cell::Integer("-9223372036854775808".into()),
        "tls_qa_signed_precision"
    );
    ensure!(
        matches!(cell(result,"unsigned_value",0)?,Cell::Integer(s)|Cell::Decimal(s) if s=="18446744073709551615"),
        "tls_qa_unsigned_precision"
    );
    ensure!(
        matches!(cell(result,"precise",0)?,Cell::Decimal(s) if s==DECIMAL),
        "tls_qa_decimal_precision"
    );
    ensure!(
        cell(result, "bytes_value", 0)? == &Cell::Bytes(vec![0, 1, 254, 255]),
        "tls_qa_binary_precision"
    );
    let source = match cell(result, "json_value", 0)? {
        Cell::Json(s) | Cell::Text(s) => s,
        value => {
            let variant = match value {
                Cell::Bytes(_) => "bytes",
                Cell::Unsupported(_) => "unsupported",
                Cell::Null => "null",
                _ => "nontext",
            };
            let info = result
                .columns
                .iter()
                .find(|c| c.name == "json_value")
                .map(|c| c.data_type.as_str())
                .unwrap_or("missing");
            let info = if info.len() <= 64
                && info
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || " ()_".contains(c))
            {
                info
            } else {
                "opaque"
            };
            return Err(anyhow!("tls_qa_json_type:{variant}:{info}"));
        }
    };
    let actual: serde_json::Value =
        serde_json::from_str(source).map_err(|_| anyhow!("tls_qa_json_invalid"))?;
    let expected: serde_json::Value = serde_json::from_str(JSON)?;
    ensure!(
        actual == expected
            && actual["huge"]
                .as_number()
                .is_some_and(|huge| huge.as_str() == "340282366920938463463374607431768211457"),
        "tls_qa_json_precision"
    );
    ensure!(
        cell(result, "label", 0)? == &Cell::Text(label.into())
            && cell(result, "label", 1)? == &Cell::Text("neighbor".into()),
        "tls_qa_cell_isolation"
    );
    ensure!(
        cell(result, "empty_value", 0)? == &Cell::Text(String::new())
            && cell(result, "null_value", 0)? == &Cell::Null,
        "tls_qa_null_empty"
    );
    Ok(())
}
async fn sql(db: &Database, text: &str) -> Result<QueryResult> {
    db.query(text, CancellationToken::new()).await
}
async fn positive(f: &Fixture, engine: Engine, endpoint: u16) -> Result<()> {
    let p = profile(f, engine);
    let db = Database::connect(&p, Some(endpoint)).await?;
    let table = TableRef {
        schema: if engine == postgres::ENGINE {
            "public".into()
        } else {
            f.database.clone()
        },
        name: format!("native_tls_qa_{}", Uuid::new_v4().simple()),
        kind: "table".into(),
    };
    let qualified = format!(
        "{}.{}",
        quote_identifier(db.dialect(), &table.schema)?,
        quote_identifier(db.dialect(), &table.name)?
    );
    let test=async {
        if engine==postgres::ENGINE {
            let ssl=sql(&db,"SELECT ssl FROM pg_stat_ssl WHERE pid=pg_backend_pid()").await?;
            ensure!(ssl.rows.first().and_then(|r|r.first())==Some(&Cell::Bool(true)),"tls_qa_session_not_encrypted");
        } else {
            let ssl=sql(&db,"SHOW SESSION STATUS LIKE 'Ssl_cipher'").await?;
            ensure!(ssl.rows.first().and_then(|r|r.get(1)).is_some_and(|c|!c.display().is_empty()),"tls_qa_session_not_encrypted");
        }
        let (unsigned,binary,json,binary_literal,json_literal)=if engine==postgres::ENGINE {("NUMERIC(20,0)","BYTEA","JSONB","decode('0001feff','hex')",format!("'{JSON}'::jsonb"))}else{("BIGINT UNSIGNED","LONGBLOB","JSON","UNHEX('0001feff')",format!("'{JSON}'"))};
        sql(&db,&format!("CREATE TABLE {qualified}(id BIGINT PRIMARY KEY, signed_value BIGINT NOT NULL, unsigned_value {unsigned} NOT NULL, precise DECIMAL(65,30) NOT NULL, bytes_value {binary} NOT NULL, json_value {json} NOT NULL, label TEXT NOT NULL, empty_value TEXT NOT NULL, null_value TEXT NULL)")).await?;
        sql(&db,&format!("INSERT INTO {qualified} VALUES(2,-9223372036854775808,18446744073709551615,{DECIMAL},{binary_literal},{json_literal},'original','',NULL),(3,1,1,1,{binary_literal},{json_literal},'neighbor','',NULL)")).await?;
        let query=format!("SELECT * FROM {qualified} ORDER BY id");
        let before=sql(&db,&query).await?;
        typed(&before,"original")?;
        let changed="single cell O'Brien żółć \\ slash";
        let update=update_cell(db.dialect(),&table,"label",cell(&before,"label",0)?,&Cell::Text(changed.into()),&[("id".into(),cell(&before,"id",0)?.clone())])?;
        db.mutate_one(&update).await?;
        typed(&sql(&db,&query).await?,changed)?;
        ensure!(db.mutate_one(&update).await.err().is_some_and(|e|e.to_string()=="row_changed_reload_required"),"tls_qa_optimistic_guard");
        typed(&sql(&db,&query).await?,changed)?;
        Ok::<_,anyhow::Error>(())
    }.await;
    // This exact freshly generated name is the only object this runner deletes.
    let cleanup = sql(&db, &format!("DROP TABLE IF EXISTS {qualified}")).await;
    db.close().await;
    ensure!(cleanup.is_ok(), "tls_qa_owned_cleanup_failed");
    test
}
#[derive(Clone, Copy)]
enum Negative {
    Name,
    Issuer,
}
fn is_certificate_error(error: &sqlx::Error, kind: Negative) -> bool {
    let text = match error {
        sqlx::Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidData => e.to_string(),
        sqlx::Error::Tls(e) => e.to_string(),
        _ => return false,
    };
    let text = text.to_lowercase();
    match kind {
        Negative::Name => {
            text.contains("certificate not valid for name") || text.contains("notvalidforname")
        }
        Negative::Issuer => text.contains("unknownissuer") || text.contains("unknown issuer"),
    }
}
async fn negative(f: &Fixture, engine: Engine, port: u16, kind: Negative) -> Result<()> {
    let p = profile(f, engine);
    let host = if matches!(kind, Negative::Name) {
        "does-not-match-certificate.invalid"
    } else {
        "localhost"
    };
    let endpoint = SocketAddr::from(([127, 0, 0, 1], port));
    // This tool speaks to the two SQLx network drivers directly: it checks
    // what their TLS layer refuses, below the driver modules of the core.
    let check = if engine == postgres::ENGINE {
        {
            let mut options = sqlx::postgres::PgConnectOptions::new_without_pgpass()
                .host(host)
                .port(5432)
                .database(&p.database)
                .username(&p.username)
                .password(&p.password)
                .dial_address(endpoint)
                .ssl_mode(sqlx::postgres::PgSslMode::VerifyFull)
                .disable_statement_logging();
            options = if matches!(kind, Negative::Name) {
                options.ssl_root_cert(&f.ca_file)
            } else {
                options.ssl_root_cert_from_pem(Vec::new())
            };
            match options.connect().await {
                Ok(connection) => {
                    let _ = connection.close().await;
                    false
                }
                Err(error) => is_certificate_error(&error, kind),
            }
        }
    } else if engine == mysql::ENGINE {
        {
            let mut options = sqlx::mysql::MySqlConnectOptions::new()
                .host(host)
                .port(3306)
                .database(&p.database)
                .username(&p.username)
                .password(&p.password)
                .dial_address(endpoint)
                .ssl_mode(sqlx::mysql::MySqlSslMode::VerifyIdentity)
                .disable_statement_logging();
            options = if matches!(kind, Negative::Name) {
                options.ssl_ca(&f.ca_file)
            } else {
                options.ssl_ca_from_pem(Vec::new())
            };
            match options.connect().await {
                Ok(connection) => {
                    let _ = connection.close().await;
                    false
                }
                Err(error) => is_certificate_error(&error, kind),
            }
        }
    } else {
        return Err(anyhow!("tls_qa_engine_invalid"));
    };
    ensure!(check, "tls_qa_expected_certificate_refusal_missing");
    Ok(())
}
fn record(report: &mut Report, name: String, result: Result<()>) {
    let pass = result.is_ok();
    let code = match result {
        Ok(()) => "ok".into(),
        Err(error) => error.to_string(),
    };
    report.checks.push(Check { name, pass, code });
}
pub fn run(fixture: &Path, path: &Path) -> Result<()> {
    let f = load_fixture(fixture)?;
    let ca = read_bounded(&f.ca_file, 64 * 1024)?;
    let mut report = Report {
        product: "Baza Studio",
        implementation: "native_rust_sqlx_strict_tls_ssh_transport",
        fixture_run: f.run_id,
        ca_sha256: Sha256::digest(&ca)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        checks: vec![],
        passed: 0,
        failed: 0,
        all_pass: false,
        limitations: "Actual SQLx PostgreSQL/MariaDB TLS through caller-owned loopback SSH forwards. Does not prove remote russh authentication here; that has a separate owned-server acceptance. Only UUID-owned fixture tables are mutated and dropped. No credentials or SQL are included in this report.",
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .build()?;
    runtime.block_on(async {
        for (engine,port,label) in [(postgres::ENGINE,f.postgres_port,"postgres"),(mysql::ENGINE,f.mariadb_port,"mariadb")] {
            record(&mut report,format!("{label}: strict TLS original SAN+CA / typed precision / single-cell update / cleanup"),positive(&f,engine,port).await);
            for (kind,name) in [(Negative::Name,"wrong SAN certificate refusal"),(Negative::Issuer,"untrusted CA certificate refusal")] {
                let result=tokio::time::timeout(Duration::from_secs(20),negative(&f,engine,port,kind)).await.unwrap_or_else(|_|Err(anyhow!("tls_qa_negative_timeout")));
                record(&mut report,format!("{label}: {name}"),result);
            }
            record(&mut report,format!("{label}: verified reconnect and owned table cleanup after refusals"),positive(&f,engine,port).await);
        }
    });
    report.passed = report.checks.iter().filter(|c| c.pass).count();
    report.failed = report.checks.len() - report.passed;
    report.all_pass = report.failed == 0 && report.passed == 8;
    std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    ensure!(report.all_pass, "native_tls_acceptance_failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_and_auth_failures_are_not_certificate_acceptance() {
        let network = sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));
        assert!(!is_certificate_error(&network, Negative::Name));
        assert!(!is_certificate_error(&network, Negative::Issuer));
        assert!(!is_certificate_error(
            &sqlx::Error::PoolTimedOut,
            Negative::Issuer
        ));
    }
    #[test]
    fn certificate_refusal_kinds_cannot_be_interchanged() {
        let name = sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid peer certificate: certificate not valid for name",
        ));
        let issuer = sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid peer certificate: UnknownIssuer",
        ));
        assert!(is_certificate_error(&name, Negative::Name));
        assert!(!is_certificate_error(&name, Negative::Issuer));
        assert!(is_certificate_error(&issuer, Negative::Issuer));
        assert!(!is_certificate_error(&issuer, Negative::Name));
    }
}
