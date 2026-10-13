use anyhow::Result;
use baza_core::{database::Database, sql, *};
use std::path::Path;
use tokio_util::sync::CancellationToken;
pub fn run(report: Option<&Path>) -> Result<()> {
    let root = std::env::temp_dir().join(format!("BazaStudio-native-QA-{}", uuid::Uuid::new_v4()));
    let namespace = format!("BazaStudio.QA.{}", uuid::Uuid::new_v4());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let mut checks = Vec::<serde_json::Value>::new();
    let mut qa_vault = None;
    let result = (|| -> Result<()> {
        std::fs::create_dir_all(&root)?;
        let vault_root = root.join("vault");
        let v = std::sync::Arc::new(Vault::open_owned(&vault_root, &namespace)?);
        qa_vault = Some(v.clone());
        let mut profiles = v.load_profiles()?;
        checks.push(serde_json::json!({"name":"current_os_vault","pass":true}));
        let fixture = root.join("fixture.sqlite");
        std::fs::File::create(&fixture)?;
        let p = ConnectionProfile {
            name: "Native SQLite QA".into(),
            engine: baza_core::drivers::sqlite::ENGINE,
            sqlite_path: Some(fixture),
            password: "OWN_QA_SECRET_SENTINEL_42".into(),
            ..ConnectionProfile::default()
        };
        profiles.connections.push(p.clone());
        v.save_profiles(&profiles)?;
        checks.push(serde_json::json!({"name":"encrypted_profile_roundtrip","pass":v.load_profiles()?.connections[0].password==p.password}));
        let disk = std::fs::read_dir(&vault_root)?
            .filter_map(|f| f.ok())
            .filter_map(|f| std::fs::read(f.path()).ok())
            .flatten()
            .collect::<Vec<_>>();
        checks.push(serde_json::json!({"name":"no_plaintext_secret_on_disk","pass":!disk.windows(p.password.len()).any(|b|b==p.password.as_bytes())}));
        rt.block_on(async{let db=Database::connect(&p,None).await?;db.query("CREATE TABLE customers(id INTEGER PRIMARY KEY,name TEXT,amount TEXT,note TEXT); INSERT INTO customers VALUES(2,'Alice','18446744073709551615',NULL); INSERT INTO customers VALUES(3,'Bob','0','other');",CancellationToken::new()).await?;checks.push(serde_json::json!({"name":"real_sqlite_create_insert","pass":true}));let tables=db.tables().await?;let cols=db.columns(&tables[0]).await?;checks.push(serde_json::json!({"name":"actual_database_metadata","pass":tables[0].name=="customers"&&cols[0].primary_key&&cols.len()==4}));let q=db.query("SELECT * FROM customers WHERE id=2",CancellationToken::new()).await?;checks.push(serde_json::json!({"name":"filtered_result_exact_integer_text_and_null","pass":q.rows.len()==1&&q.rows[0][2].display()=="18446744073709551615"&&matches!(q.rows[0][3],Cell::Null)}));let update=sql::update_cell(db.dialect(),&tables[0],"name",&q.rows[0][1],&Cell::Text("Alice edited".into()),&[("id".into(),q.rows[0][0].clone())])?;db.mutate_one(&update).await?;let edited=db.query("SELECT * FROM customers ORDER BY id",CancellationToken::new()).await?;checks.push(serde_json::json!({"name":"single_cell_edit_preserves_other_columns_and_rows","pass":edited.rows[0][1].display()=="Alice edited"&&edited.rows[0][2]==q.rows[0][2]&&edited.rows[1][1].display()=="Bob"}));checks.push(serde_json::json!({"name":"optimistic_stale_edit_rejected","pass":db.mutate_one(&update).await.is_err()}));let c=CancellationToken::new();c.cancel();checks.push(serde_json::json!({"name":"query_cancellation","pass":db.query("SELECT 1",c).await.is_err()}));let export=crate::transfer::export(&edited,crate::transfer::Format::Json,db.dialect(),Some(&tables[0]))?;let (_,rows)=crate::transfer::import_rows(&export,crate::transfer::Format::Json)?;checks.push(serde_json::json!({"name":"typed_json_export_import","pass":rows==edited.rows}));db.close().await;Ok::<_,anyhow::Error>(())})?;
        let h = HistoryRecord {
            id: uuid::Uuid::new_v4(),
            connection_id: p.id,
            sql: "SELECT 42".into(),
            status: "completed".into(),
            started_at: chrono::Utc::now().to_rfc3339(),
            duration_ms: 1,
            rows: 1,
            error: None,
        };
        v.append_history(&h)?;
        checks.push(serde_json::json!({"name":"encrypted_query_history","pass":v.load_history()?.iter().any(|r|r.id==h.id&&r.sql=="SELECT 42")}));
        checks.push(serde_json::json!({"name":"plaintext_sql_copy_default_off","pass":!vault_root.join("native-sql-history").exists()}));
        let saved = serde_json::json!([{"id":uuid::Uuid::new_v4(),"name":"Owned QA saved query","connection":p.id,"sql":"SELECT 42 AS OWN_SAVED_SQL_SENTINEL"}]);
        v.save_named("saved-queries", &serde_json::to_vec(&saved)?)?;
        let roundtrip = v
            .load_named("saved-queries")?
            .ok_or_else(|| anyhow::anyhow!("saved_query_missing"))?;
        checks.push(serde_json::json!({"name":"encrypted_saved_query_library","pass":serde_json::from_slice::<serde_json::Value>(&roundtrip)?==saved&&!std::fs::read(vault_root.join("native-saved-queries.bzvault"))?.windows(b"OWN_SAVED_SQL_SENTINEL".len()).any(|b|b==b"OWN_SAVED_SQL_SENTINEL")}));
        checks.push(serde_json::json!({"name":"local_application_lock_optional_default_off","pass":!baza_core::local_lock::LocalLock::open(v.clone())?.status()?.enabled}));
        Ok(())
    })();
    let passed = result.is_ok() && checks.iter().all(|c| c["pass"] == true);
    let body = serde_json::json!({"product":"Baza Studio","implementation":"native_rust_sqlx","version":env!("CARGO_PKG_VERSION"),"passed":passed,"check_count":checks.len(),"checks":checks,"error":result.err().map(|e|e.to_string()),"limitations":"Owned SQLite and current OS vault only; native renderer and remote PostgreSQL/MariaDB are separate acceptance."});
    if let Some(p) = report {
        std::fs::write(p, serde_json::to_vec_pretty(&body)?)?;
    }
    println!(
        "Baza Studio native self-test: {} ({} checks)",
        if passed { "PASS" } else { "FAIL" },
        checks.len()
    );
    if let Some(vault) = qa_vault.take() {
        let _ = vault.remove_owned_qa_key();
        drop(vault);
    }
    let _ = std::fs::remove_dir_all(&root);
    anyhow::ensure!(passed, "native_self_test_failed");
    Ok(())
}
