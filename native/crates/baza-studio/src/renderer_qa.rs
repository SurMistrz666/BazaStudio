//! A normal native-window exit is not a renderer acceptance result.
use anyhow::{Result, ensure};
use std::{io::Read, path::Path, time::Duration};
use uuid::Uuid;

fn bounded_read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open(path).map_err(|_| anyhow::anyhow!("qa_result_missing"))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= limit as u64,
        "qa_result_invalid"
    );
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "qa_result_invalid");
    Ok(bytes)
}

fn fields(
    value: &serde_json::Value,
    run: Uuid,
    dimensions: (u32, u32),
    time_limit: Duration,
) -> Result<()> {
    // The sample table has 2000 rows and the grid shows its first page. A
    // run on a server named by its runner (`BAZA_QA_CONNECTION`) shows
    // whatever that table holds: one row is enough there.
    let rows = if value["qa_connection"].is_string() {
        1
    } else {
        500
    };
    ensure!(
        value["product"] == "Baza Studio"
            && value["implementation"] == "native_rust_egui_wgpu"
            && value["qa_run"] == run.to_string()
            && value["renderer_screenshot"] == true
            && value["frame_completed"] == true
            && value["vault_opened"] == true
            && value["actual_sqlite_rows_rendered"]
                .as_u64()
                .is_some_and(|v| v >= rows)
            && value["owned_fixture"] == true
            && value.get("failure").is_none()
            && value["viewport_width"].as_u64() == Some(dimensions.0 as u64)
            && value["viewport_height"].as_u64() == Some(dimensions.1 as u64)
            && value["elapsed_ms"]
                .as_u64()
                .is_some_and(|v| u128::from(v) <= time_limit.as_millis()),
        "native_renderer_acceptance_failed"
    );
    Ok(())
}

/// Accept the report and the screenshot of one renderer self-test. The run
/// must have finished within `time_limit`: 40 s, plus the time its QA script
/// was allowed.
pub fn validate(report: &Path, run: Uuid, time_limit: Duration) -> Result<()> {
    let value: serde_json::Value = serde_json::from_slice(&bounded_read(report, 64 * 1024)?)
        .map_err(|_| anyhow::anyhow!("qa_result_invalid"))?;
    let png = bounded_read(&report.with_extension("png"), 16 * 1024 * 1024)?;
    ensure!(
        png.len() >= 33 && png.starts_with(b"\x89PNG\r\n\x1a\n") && &png[12..16] == b"IHDR",
        "qa_screenshot_invalid"
    );
    let width = u32::from_be_bytes(png[16..20].try_into()?);
    let height = u32::from_be_bytes(png[20..24].try_into()?);
    ensure!(
        width >= 320
            && height >= 240
            && width <= 8192
            && height <= 8192
            && (width as u64) * (height as u64) <= 16_777_216,
        "qa_screenshot_invalid"
    );
    fields(&value, run, (width, height), time_limit)?;
    let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .map_err(|_| anyhow::anyhow!("qa_screenshot_invalid"))?;
    ensure!(
        decoded.width() == width && decoded.height() == height,
        "qa_screenshot_invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const LIMIT: Duration = Duration::from_secs(40);
    fn report(run: Uuid) -> serde_json::Value {
        serde_json::json!({"product":"Baza Studio", "implementation":"native_rust_egui_wgpu",
            "qa_run":run.to_string(), "renderer_screenshot":true,"frame_completed":true,
            "vault_opened":true,"actual_sqlite_rows_rendered":500,"owned_fixture":true,"viewport_width":320,
            "viewport_height":240,"elapsed_ms":2500})
    }
    #[test]
    fn normal_exit_without_report_is_not_a_pass() {
        assert!(
            validate(
                &std::env::temp_dir().join(format!("missing-{}.json", Uuid::new_v4())),
                Uuid::new_v4(),
                LIMIT
            )
            .is_err()
        );
    }
    #[test]
    fn stale_and_incomplete_reports_are_refused() {
        let run = Uuid::new_v4();
        let value = report(run);
        assert!(fields(&value, Uuid::new_v4(), (320, 240), LIMIT).is_err());
        assert!(fields(&value, run, (320, 240), LIMIT).is_ok());
        for key in ["renderer_screenshot", "frame_completed", "vault_opened"] {
            let mut value = report(run);
            value[key] = false.into();
            assert!(fields(&value, run, (320, 240), LIMIT).is_err());
        }
        let mut value = report(run);
        value["actual_sqlite_rows_rendered"] = 0.into();
        assert!(fields(&value, run, (320, 240), LIMIT).is_err());
        assert!(fields(&report(run), run, (321, 240), LIMIT).is_err());
        // A run is refused when it took longer than it was allowed.
        let mut slow = report(run);
        slow["elapsed_ms"] = 41_000.into();
        assert!(fields(&slow, run, (320, 240), LIMIT).is_err());
        assert!(fields(&slow, run, (320, 240), LIMIT + Duration::from_secs(2)).is_ok());
    }
    #[test]
    fn a_fresh_report_requires_a_real_decodable_png() {
        let root = std::env::temp_dir().join(format!("BazaStudio-report-QA-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("report.json");
        let run = Uuid::new_v4();
        std::fs::write(&path, serde_json::to_vec(&report(run)).unwrap()).unwrap();
        assert!(validate(&path, run, LIMIT).is_err());
        image::save_buffer_with_format(
            path.with_extension("png"),
            &vec![40u8; 320 * 240 * 4],
            320,
            240,
            image::ColorType::Rgba8,
            image::ImageFormat::Png,
        )
        .unwrap();
        assert!(validate(&path, run, LIMIT).is_ok());
        std::fs::write(path.with_extension("png"), b"not an image").unwrap();
        assert!(validate(&path, run, LIMIT).is_err());
        std::fs::remove_file(path.with_extension("png")).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
