//! Acceptance tools for Baza Studio. Built and tested with the workspace, never packaged.
//!
//! * `--tls-self-test --fixture <file> --report <file>`: database TLS through an SSH tunnel
//!   against caller-owned PostgreSQL and MariaDB fixtures.
//! * `--vault-concurrency-worker --qa-run <uuid> --report <file>`: one of several processes
//!   that open the same owned QA vault for the first time and update it.
//! * `--vault-concurrency-finalize --qa-run <uuid> --expected-workers <count> --report <file>`:
//!   checks what the workers left behind and removes the QA vault and its key.
//!
//! No tool opens an application vault: the vault tools only use `Vault::open_owned` on a
//! directory of their own under the temporary directory.
mod concurrency_qa;
mod tls_qa;

const USAGE: &str = "\
BazaQa --tls-self-test --fixture <file> --report <file>
BazaQa --vault-concurrency-worker --qa-run <uuid> --report <file>
BazaQa --vault-concurrency-finalize --qa-run <uuid> --expected-workers <count> --report <file>";

fn main() -> anyhow::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let report = args
        .windows(2)
        .find(|w| w[0] == "--report")
        .map(|w| std::path::PathBuf::from(&w[1]));
    if args.iter().any(|a| a == "--tls-self-test") {
        let fixture = args
            .windows(2)
            .find(|w| w[0] == "--fixture")
            .ok_or_else(|| anyhow::anyhow!("tls_fixture_path_required"))?;
        return tls_qa::run(
            std::path::Path::new(&fixture[1]),
            report
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("qa_report_required"))?,
        );
    }
    if args
        .iter()
        .any(|a| a == "--vault-concurrency-worker" || a == "--vault-concurrency-finalize")
    {
        let run = args
            .windows(2)
            .find(|w| w[0] == "--qa-run")
            .ok_or_else(|| anyhow::anyhow!("qa_run_required"))?;
        let run = uuid::Uuid::parse_str(&run[1]).map_err(|_| anyhow::anyhow!("qa_run_invalid"))?;
        let report = report
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("qa_report_required"))?;
        if args.iter().any(|a| a == "--vault-concurrency-worker") {
            return concurrency_qa::worker(run, report);
        }
        let expected = args
            .windows(2)
            .find(|w| w[0] == "--expected-workers")
            .ok_or_else(|| anyhow::anyhow!("qa_worker_count_required"))?[1]
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("qa_worker_count_invalid"))?;
        return concurrency_qa::finalize(run, expected, report);
    }
    eprintln!("{USAGE}");
    anyhow::bail!("qa_mode_required")
}
