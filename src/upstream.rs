use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

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
}

#[derive(Debug, Clone)]
pub struct OfficialHfFetcher {
    pub python: PathBuf,
    pub helper: PathBuf,
}

impl UpstreamFetcher for OfficialHfFetcher {
    fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
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
        let output = command.output().map_err(UpstreamError::Io)?;
        if !output.status.success() {
            return Err(match output.status.code() {
                Some(10) => UpstreamError::Unavailable,
                Some(11) => UpstreamError::NotFound,
                Some(12) => UpstreamError::Unauthorized,
                _ => UpstreamError::Failed,
            });
        }
        let response: HelperOutput =
            serde_json::from_slice(&output.stdout).map_err(|_| UpstreamError::InvalidOutput)?;
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

fn is_hf_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug, Deserialize)]
struct HelperOutput {
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
}
