use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::is_hf_commit;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub repo_id: String,
    pub revision: String,
    pub files: Vec<String>,
    pub staging: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedRevision {
    pub commit: String,
    pub files: Vec<String>,
    pub staging: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FetchProgress {
    #[serde(default)]
    pub version: u32,
    pub phase: String,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub completed: Option<u64>,
    #[serde(default)]
    pub total: Option<u64>,
}

impl FetchProgress {
    pub fn phase(phase: &str) -> Self {
        Self {
            version: 1,
            phase: phase.into(),
            unit: None,
            completed: None,
            total: None,
        }
    }
}

#[derive(Debug)]
pub enum UpstreamError {
    Io(std::io::Error),
    Unavailable,
    NotFound,
    Unauthorized,
    InvalidOutput,
    Failed,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "upstream I/O error: {error}"),
            Self::Unavailable => write!(formatter, "upstream unavailable"),
            Self::NotFound => write!(formatter, "upstream repository or revision not found"),
            Self::Unauthorized => write!(formatter, "upstream authorization failed"),
            Self::InvalidOutput => write!(formatter, "upstream returned invalid helper output"),
            Self::Failed => write!(formatter, "upstream acquisition failed"),
        }
    }
}

impl std::error::Error for UpstreamError {}

pub trait UpstreamFetcher: Send + Sync {
    fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError>;

    fn fetch_with_progress(
        &self,
        request: &FetchRequest,
        _progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<FetchedRevision, UpstreamError> {
        self.fetch(request)
    }
}

#[derive(Debug, Clone)]
pub struct OfficialHfFetcher {
    pub python: PathBuf,
    pub helper: PathBuf,
}

impl UpstreamFetcher for OfficialHfFetcher {
    fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
        self.fetch_with_progress(request, &|_| {})
    }

    fn fetch_with_progress(
        &self,
        request: &FetchRequest,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<FetchedRevision, UpstreamError> {
        let mut command = Command::new(&self.python);
        command
            .arg(&self.helper)
            .arg("--repo-id")
            .arg(&request.repo_id)
            .arg("--revision")
            .arg(&request.revision)
            .arg("--output")
            .arg(&request.staging)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for file in &request.files {
            command.arg("--file").arg(file);
        }
        let mut child = command.spawn().map_err(UpstreamError::Io)?;
        let stdout = child.stdout.take().ok_or(UpstreamError::InvalidOutput)?;
        let mut result = None;
        for line in BufReader::new(stdout).lines() {
            let line = line.map_err(UpstreamError::Io)?;
            let value: serde_json::Value =
                serde_json::from_str(&line).map_err(|_| UpstreamError::InvalidOutput)?;
            match value.get("type").and_then(|value| value.as_str()) {
                Some("progress") => {
                    let event: FetchProgress =
                        serde_json::from_value(value).map_err(|_| UpstreamError::InvalidOutput)?;
                    if event.version > 1 {
                        return Err(UpstreamError::InvalidOutput);
                    }
                    progress(event);
                }
                Some("result") | None => {
                    result = Some(
                        serde_json::from_value(value).map_err(|_| UpstreamError::InvalidOutput)?,
                    );
                }
                _ => return Err(UpstreamError::InvalidOutput),
            }
        }
        let status = child.wait().map_err(UpstreamError::Io)?;
        if !status.success() {
            return Err(match status.code() {
                Some(10) => UpstreamError::Unavailable,
                Some(11) => UpstreamError::NotFound,
                Some(12) => UpstreamError::Unauthorized,
                _ => UpstreamError::Failed,
            });
        }
        let response: HelperOutput = result.ok_or(UpstreamError::InvalidOutput)?;
        if !is_hf_commit(&response.commit) || response.files.is_empty() {
            return Err(UpstreamError::InvalidOutput);
        }
        Ok(FetchedRevision {
            commit: response.commit,
            files: response.files,
            staging: request.staging.clone(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct HelperOutput {
    #[serde(rename = "type")]
    _kind: Option<String>,
    commit: String,
    files: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_output_contract_is_deserialized() {
        let commit = "a".repeat(40);
        let encoded = format!(r#"{{ "commit":"{commit}","files":["config.json"] }}"#);
        let output: HelperOutput = serde_json::from_slice(encoded.as_bytes()).unwrap();
        assert_eq!(output.commit, commit);
        assert_eq!(output.files, vec!["config.json"]);
    }

    #[test]
    fn helper_commit_contract_rejects_malformed_identities() {
        assert!(is_hf_commit(&"a".repeat(40)));
        assert!(!is_hf_commit(""));
        assert!(!is_hf_commit("aaaaaaaa"));
        assert!(!is_hf_commit(&"g".repeat(40)));
    }

    #[test]
    fn helper_progress_contract_is_deserialized() {
        let event: FetchProgress = serde_json::from_str(
            r#"{"type":"progress","version":1,"phase":"downloading","unit":"bytes","completed":4,"total":10}"#,
        )
        .unwrap();
        assert_eq!(event.total, Some(10));
        assert_eq!(event.completed, Some(4));
        assert_eq!(event.unit.as_deref(), Some("bytes"));
    }

    #[test]
    fn helper_phase_contract_allows_progress_without_a_counter() {
        let event: FetchProgress = serde_json::from_str(
            r#"{"type":"progress","version":1,"phase":"inventorying_snapshot"}"#,
        )
        .unwrap();
        assert_eq!(event, FetchProgress::phase("inventorying_snapshot"));
    }
}
