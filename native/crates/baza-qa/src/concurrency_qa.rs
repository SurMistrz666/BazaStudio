//! Disposable multi-process OS-vault acceptance; UUID-only input selects an owned temp fixture.
//! The vault is a QA vault of this tool (`Vault::open_owned`), never an application vault.
use anyhow::{Result, ensure};
use baza_core::Vault;
use std::path::{Path, PathBuf};
use uuid::Uuid;
/// Any fixed vault record works as the shared fixture; its content is synthetic.
const SHARED_RECORD: &str = "connection-groups";
fn paths(run: Uuid) -> (PathBuf, String) {
    (
        std::env::temp_dir().join(format!("BazaQa-concurrency-QA-{run}")),
        format!("BazaStudio.QA.concurrent.{run}"),
    )
}
pub fn worker(run: Uuid, report: &Path) -> Result<()> {
    let (root, scope) = paths(run);
    let vault = Vault::open_owned(&root.join("vault"), &scope)?;
    let identity = vault.update_named(SHARED_RECORD, |old| {
        let id = if let Some(old) = old {
            let value: serde_json::Value = serde_json::from_slice(old)?;
            ensure!(
                value["kind"] == "owned-native-concurrency-fixture",
                "qa_fixture_mismatch"
            );
            Uuid::parse_str(
                value["identity"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("qa_fixture_mismatch"))?,
            )?
        } else {
            Uuid::new_v4()
        };
        Ok((
            serde_json::to_vec(
                &serde_json::json!({"kind":"owned-native-concurrency-fixture","identity":id}),
            )?,
            id,
        ))
    })?;
    let ordinal = vault.update_named("update-state", |old| {
        let current = old
            .map(serde_json::from_slice::<u64>)
            .transpose()?
            .unwrap_or(0);
        ensure!(current < 128, "qa_worker_limit");
        Ok((serde_json::to_vec(&(current + 1))?, current + 1))
    })?;
    std::fs::write(
        report,
        serde_json::to_vec_pretty(
            &serde_json::json!({"passed":true,"run":run,"identity":identity,"ordinal":ordinal,"implementation":"actual_native_current_user_os_vault"}),
        )?,
    )?;
    Ok(())
}
pub fn finalize(run: Uuid, expected: u64, report: &Path) -> Result<()> {
    ensure!((1..=128).contains(&expected), "qa_worker_limit");
    let (root, scope) = paths(run);
    let vault = Vault::open_owned(&root.join("vault"), &scope)?;
    let identity: serde_json::Value = vault
        .load_record(SHARED_RECORD)?
        .ok_or_else(|| anyhow::anyhow!("qa_fixture_missing"))?;
    ensure!(
        identity["kind"] == "owned-native-concurrency-fixture",
        "qa_fixture_mismatch"
    );
    let count = vault
        .load_record::<u64>("update-state")?
        .ok_or_else(|| anyhow::anyhow!("qa_fixture_missing"))?;
    let temporary = std::fs::read_dir(root.join("vault"))?.any(|entry| {
        entry.is_ok_and(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
    });
    ensure!(count == expected && !temporary, "qa_concurrency_failed");
    vault.remove_owned_qa_key()?;
    drop(vault);
    let absolute = std::path::absolute(&root)?;
    let expected_root =
        std::path::absolute(std::env::temp_dir().join(format!("BazaQa-concurrency-QA-{run}")))?;
    ensure!(
        absolute == expected_root && absolute.parent() == Some(std::env::temp_dir().as_path()),
        "qa_cleanup_boundary_invalid"
    );
    std::fs::remove_dir_all(&absolute)?;
    std::fs::write(
        report,
        serde_json::to_vec_pretty(
            &serde_json::json!({"passed":true,"run":run,"identity":identity["identity"],"actual_updates":count,"expected_updates":expected,"orphaned_temporary_files":temporary,"owned_fixture_removed":true}),
        )?,
    )?;
    Ok(())
}
