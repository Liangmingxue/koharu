//! Durable records for asynchronous Gateway operations.

use std::fs;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use koharu_core::{JobStatus, JobSummary};
use serde::{Deserialize, Serialize};

use crate::durable_json;

const JOURNAL_VERSION: &str = "koharu-operation-record.v1";
const JOURNAL_DIR: &str = "gateway-operations";
const RESTART_ERROR: &str = "operation interrupted by process restart; reconcile scene epoch and retry with a new idempotency key";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperationRecord {
    schema_version: String,
    session_dir: String,
    request_hash: String,
    operation: JobSummary,
}

#[derive(Default)]
pub struct OperationJournal {
    lock: Mutex<()>,
}

impl OperationJournal {
    pub fn record_running(
        &self,
        data_root: &Utf8Path,
        session_dir: &str,
        request_hash: &str,
        operation: &JobSummary,
    ) -> Result<()> {
        if operation.status != JobStatus::Running {
            bail!("new operation journal record must be running");
        }
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("operation journal lock poisoned"))?;
        let path = record_path(data_root, &operation.id);
        if path.exists() {
            bail!("operation journal id already exists: {}", operation.id);
        }
        write_record(
            &path,
            &OperationRecord {
                schema_version: JOURNAL_VERSION.to_string(),
                session_dir: session_dir.to_string(),
                request_hash: request_hash.to_string(),
                operation: operation.clone(),
            },
        )
    }

    pub fn record_terminal(
        &self,
        data_root: &Utf8Path,
        operation_id: &str,
        status: JobStatus,
        error: Option<String>,
    ) -> Result<JobSummary> {
        if status == JobStatus::Running {
            bail!("terminal operation status cannot be running");
        }
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("operation journal lock poisoned"))?;
        let path = record_path(data_root, operation_id);
        let mut record = read_record(&path)?;
        if record.operation.id != operation_id {
            bail!("operation journal id binding changed");
        }
        record.operation.status = status;
        record.operation.error = error;
        write_record(&path, &record)?;
        Ok(record.operation)
    }

    /// Restore terminal records and fail closed for operations interrupted by
    /// a process exit. The persisted operation id remains queryable.
    pub fn recover(&self, data_root: &Utf8Path) -> Result<Vec<JobSummary>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("operation journal lock poisoned"))?;
        let root = data_root.join(JOURNAL_DIR);
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = fs::read_dir(root.as_std_path())?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect::<Vec<_>>();
        paths.sort();
        let mut operations = Vec::with_capacity(paths.len());
        for path in paths {
            let path = Utf8PathBuf::from_path_buf(path).map_err(|path| {
                anyhow::anyhow!("operation journal path is not UTF-8: {}", path.display())
            })?;
            let mut record = read_record(&path)?;
            if record.operation.status == JobStatus::Running {
                record.operation.status = JobStatus::Failed;
                record.operation.error = Some(RESTART_ERROR.to_string());
                write_record(&path, &record)?;
            }
            operations.push(record.operation);
        }
        Ok(operations)
    }
}

fn record_path(data_root: &Utf8Path, operation_id: &str) -> Utf8PathBuf {
    let name = blake3::hash(operation_id.as_bytes()).to_hex();
    data_root.join(JOURNAL_DIR).join(format!("{name}.json"))
}

fn read_record(path: &Utf8Path) -> Result<OperationRecord> {
    let bytes =
        fs::read(path.as_std_path()).with_context(|| format!("read operation record {path}"))?;
    let record: OperationRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode operation record {path}"))?;
    if record.schema_version != JOURNAL_VERSION {
        bail!(
            "unsupported operation journal schema {}",
            record.schema_version
        );
    }
    Ok(record)
}

fn write_record(path: &Utf8Path, record: &OperationRecord) -> Result<()> {
    durable_json::write(path.as_std_path(), record)
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;
    use koharu_core::{JobStatus, JobSummary};

    use super::OperationJournal;

    #[test]
    fn restart_converts_an_interrupted_operation_to_explicit_failure() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let journal = OperationJournal::default();
        let running = JobSummary {
            id: "operation-1".to_string(),
            kind: "pipeline".to_string(),
            status: JobStatus::Running,
            error: None,
        };
        journal
            .record_running(root, "/project.khrproj", "blake3:request", &running)
            .unwrap();

        let recovered = OperationJournal::default().recover(root).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].status, JobStatus::Failed);
        assert!(recovered[0].error.as_deref().unwrap().contains("restart"));
        assert_eq!(
            OperationJournal::default().recover(root).unwrap()[0].status,
            JobStatus::Failed
        );
    }
}
