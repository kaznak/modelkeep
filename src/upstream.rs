use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use serde::Deserialize;

use crate::{is_hf_commit, record_fetch_resolved_commit_for_type, RepositoryType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub repo_type: RepositoryType,
    pub repo_id: String,
    pub revision: String,
    /// Include patterns handed to the official client as `allow_patterns`.
    /// Empty means the whole repository.
    pub files: Vec<String>,
    /// Exclude patterns handed to the official client as `ignore_patterns`.
    pub exclude: Vec<String>,
    pub staging: PathBuf,
    pub resume_commit: Option<String>,
}

impl FetchRequest {
    /// The selection component of this acquisition's staging identity.
    pub fn selection_identity(&self) -> Vec<String> {
        selection_identity(&self.files, &self.exclude)
    }
}

/// Prefix distinguishing an exclude pattern inside a staging identity.
///
/// A leading `!` is rejected by [`FileSelection`] normalization, so an include
/// pattern can never collide with the encoded form of an exclude pattern.
const EXCLUDE_IDENTITY_PREFIX: &str = "!";

/// Encodes an include/exclude selection as the staging-identity file list.
///
/// A selection without exclude patterns encodes to its include list unchanged,
/// so staging recorded before exclude patterns existed stays adoptable.
pub fn selection_identity(include: &[String], exclude: &[String]) -> Vec<String> {
    if exclude.is_empty() {
        return include.to_vec();
    }
    let mut identity = include.to_vec();
    identity.extend(
        exclude
            .iter()
            .map(|pattern| format!("{EXCLUDE_IDENTITY_PREFIX}{pattern}")),
    );
    identity
}

/// A rejected acquisition selection pattern.
///
/// The offending pattern is deliberately not carried: selection patterns are
/// untrusted input that must not be echoed back into logs or management state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsafeSelectionPattern;

impl std::fmt::Display for UnsafeSelectionPattern {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("unsafe acquisition selection pattern")
    }
}

impl std::error::Error for UnsafeSelectionPattern {}

/// A validated, normalized include/exclude selection for one acquisition.
///
/// ADR-0020 decision 1: ModelKeep validates and normalizes the patterns as
/// untrusted input and delegates matching itself to the official client's
/// `allow_patterns` / `ignore_patterns`. ADR-0020 decision 5: the normalized
/// selection is part of acquisition identity, never of the published manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSelection {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl FileSelection {
    /// The whole repository: ADR-0020 decision 6 keeps this the default.
    pub fn all() -> Self {
        Self::default()
    }

    /// A selection naming exactly the requested paths.
    pub fn from_paths(paths: &[String]) -> Result<Self, UnsafeSelectionPattern> {
        Self::new(paths, &[])
    }

    pub fn new(include: &[String], exclude: &[String]) -> Result<Self, UnsafeSelectionPattern> {
        Ok(Self {
            include: normalize_patterns(include)?,
            exclude: normalize_patterns(exclude)?,
        })
    }

    pub fn include(&self) -> &[String] {
        &self.include
    }

    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }

    /// True when this selection restricts nothing.
    pub fn is_unrestricted(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// The include entries that name one concrete path rather than a pattern.
    ///
    /// Only these can be checked against a published manifest; a pattern's
    /// coverage is upstream's answer, not the archive's (ADR-0020 decision 3).
    pub fn required_paths(&self) -> Vec<String> {
        self.include
            .iter()
            .filter(|pattern| !is_glob_pattern(pattern))
            .cloned()
            .collect()
    }

    /// The selection component of the fetch staging identity.
    pub fn identity(&self) -> Vec<String> {
        selection_identity(&self.include, &self.exclude)
    }
}

fn is_glob_pattern(value: &str) -> bool {
    value.ends_with('/') || value.contains(['*', '?', '[', ']'])
}

fn normalize_patterns(patterns: &[String]) -> Result<Vec<String>, UnsafeSelectionPattern> {
    let mut normalized = std::collections::BTreeSet::new();
    for pattern in patterns {
        if !is_safe_selection_pattern(pattern) {
            return Err(UnsafeSelectionPattern);
        }
        normalized.insert(pattern.clone());
    }
    Ok(normalized.into_iter().collect())
}

/// Rejects a selection pattern that could escape the archive root, address
/// ModelKeep's internal state, or make a staging identity ambiguous.
fn is_safe_selection_pattern(pattern: &str) -> bool {
    if pattern.is_empty()
        || pattern.starts_with(EXCLUDE_IDENTITY_PREFIX)
        || pattern.starts_with('/')
        || pattern.contains('\\')
        || pattern
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return false;
    }
    // A trailing `/` is the official client's directory form and expands to
    // `<pattern>*`; it is the one empty trailing component that is allowed.
    let body = pattern.strip_suffix('/').unwrap_or(pattern);
    if body.is_empty() {
        return false;
    }
    let mut components = body.split('/');
    let first = components.next().unwrap_or_default();
    if first.starts_with(".modelkeep-") {
        return false;
    }
    std::iter::once(first)
        .chain(components)
        .all(|component| !matches!(component, "" | "." | ".." | ".cache"))
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
    InvalidOutput(InvalidOutputReason),
    Storage,
    Failed,
}

/// A credential-safe description of a rejected fetch-helper contract.
///
/// This deliberately is not an arbitrary string: helper stdout and stderr can
/// contain credentials or signed URLs and must never reach logs or management
/// state through an error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidOutputReason {
    StdoutUnavailable,
    NonJsonLine,
    MalformedProgress,
    UnsupportedProgressVersion,
    MalformedResolved,
    MalformedResolvedCommit,
    StagingMetadataUpdateFailed,
    MalformedResult,
    UnsupportedEventType,
    MissingResult,
    MalformedResultCommit,
    ResumeCommitMismatch,
    EmptySnapshot,
}

impl InvalidOutputReason {
    pub const ALL: [Self; 13] = [
        Self::StdoutUnavailable,
        Self::NonJsonLine,
        Self::MalformedProgress,
        Self::UnsupportedProgressVersion,
        Self::MalformedResolved,
        Self::MalformedResolvedCommit,
        Self::StagingMetadataUpdateFailed,
        Self::MalformedResult,
        Self::UnsupportedEventType,
        Self::MissingResult,
        Self::MalformedResultCommit,
        Self::ResumeCommitMismatch,
        Self::EmptySnapshot,
    ];

    pub const fn safe_reason(self) -> &'static str {
        match self {
            Self::StdoutUnavailable => "helper stdout was unavailable",
            Self::NonJsonLine => "helper emitted a non-JSON line",
            Self::MalformedProgress => "helper emitted a malformed progress event",
            Self::UnsupportedProgressVersion => "helper emitted an unsupported progress version",
            Self::MalformedResolved => "helper emitted a malformed resolved event",
            Self::MalformedResolvedCommit => "helper resolved a malformed commit identity",
            Self::StagingMetadataUpdateFailed => "fetch staging metadata update failed",
            Self::MalformedResult => "helper emitted a malformed result event",
            Self::UnsupportedEventType => "helper emitted an unsupported event type",
            Self::MissingResult => "helper exited successfully without a result event",
            Self::MalformedResultCommit => "helper returned a malformed commit identity",
            Self::ResumeCommitMismatch => "helper result did not match the resumed commit",
            Self::EmptySnapshot => "helper returned an empty snapshot",
        }
    }
}

impl std::fmt::Display for InvalidOutputReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.safe_reason())
    }
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "upstream I/O error: {error}"),
            Self::Unavailable => write!(formatter, "upstream unavailable"),
            Self::NotFound => write!(formatter, "upstream repository or revision not found"),
            Self::Unauthorized => write!(formatter, "upstream authorization failed"),
            Self::InvalidOutput(reason) => {
                write!(
                    formatter,
                    "upstream returned invalid helper output: {reason}"
                )
            }
            Self::Storage => write!(formatter, "fetch staging storage failure"),
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
            .arg("--repo-type")
            .arg(request.repo_type.to_string())
            .arg("--repo-id")
            .arg(&request.repo_id)
            .arg("--revision")
            .arg(request.resume_commit.as_ref().unwrap_or(&request.revision))
            .arg("--output")
            .arg(&request.staging)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for file in &request.files {
            command.arg("--file").arg(file);
        }
        for pattern in &request.exclude {
            command.arg("--exclude").arg(pattern);
        }
        let mut child = command.spawn().map_err(UpstreamError::Io)?;
        let Some(stdout) = child.stdout.take() else {
            terminate_and_reap(&mut child);
            return Err(UpstreamError::InvalidOutput(
                InvalidOutputReason::StdoutUnavailable,
            ));
        };
        let parsed = (|| {
            let mut result = None;
            for line in BufReader::new(stdout).lines() {
                let line = line.map_err(UpstreamError::Io)?;
                let value: serde_json::Value = serde_json::from_str(&line)
                    .map_err(|_| UpstreamError::InvalidOutput(InvalidOutputReason::NonJsonLine))?;
                match value.get("type").and_then(|value| value.as_str()) {
                    Some("progress") => {
                        let event: FetchProgress = serde_json::from_value(value).map_err(|_| {
                            UpstreamError::InvalidOutput(InvalidOutputReason::MalformedProgress)
                        })?;
                        if event.version > 1 {
                            return Err(UpstreamError::InvalidOutput(
                                InvalidOutputReason::UnsupportedProgressVersion,
                            ));
                        }
                        progress(event);
                    }
                    Some("resolved") => {
                        let commit = value.get("commit").and_then(|value| value.as_str()).ok_or(
                            UpstreamError::InvalidOutput(InvalidOutputReason::MalformedResolved),
                        )?;
                        if !is_hf_commit(commit) {
                            return Err(UpstreamError::InvalidOutput(
                                InvalidOutputReason::MalformedResolvedCommit,
                            ));
                        }
                        record_fetch_resolved_commit_for_type(
                            &request.staging,
                            request.repo_type,
                            &request.repo_id,
                            &request.revision,
                            &request.selection_identity(),
                            commit,
                        )
                        .map_err(|error| match error {
                            crate::ArchiveError::Io(_) => UpstreamError::Storage,
                            _ => UpstreamError::InvalidOutput(
                                InvalidOutputReason::StagingMetadataUpdateFailed,
                            ),
                        })?;
                    }
                    Some("result") => {
                        result = Some(serde_json::from_value(value).map_err(|_| {
                            UpstreamError::InvalidOutput(InvalidOutputReason::MalformedResult)
                        })?);
                    }
                    // Only explicitly typed events belong to the helper protocol.
                    // The supported client and its transports may emit credential-free
                    // JSON diagnostics while reopening an interrupted local_dir. Treating
                    // an untyped object as a legacy result made those diagnostics fatal.
                    // A helper that emits only diagnostics still fails safely below with
                    // MissingResult.
                    None => continue,
                    _ => {
                        return Err(UpstreamError::InvalidOutput(
                            InvalidOutputReason::UnsupportedEventType,
                        ))
                    }
                }
            }
            Ok(result)
        })();
        let result = match parsed {
            Ok(result) => result,
            Err(error) => {
                terminate_and_reap(&mut child);
                return Err(error);
            }
        };
        let status = child.wait().map_err(UpstreamError::Io)?;
        if !status.success() {
            return Err(match status.code() {
                Some(10) => UpstreamError::Unavailable,
                Some(11) => UpstreamError::NotFound,
                Some(12) => UpstreamError::Unauthorized,
                _ => UpstreamError::Failed,
            });
        }
        let response: HelperOutput = result.ok_or(UpstreamError::InvalidOutput(
            InvalidOutputReason::MissingResult,
        ))?;
        if !is_hf_commit(&response.commit) {
            return Err(UpstreamError::InvalidOutput(
                InvalidOutputReason::MalformedResultCommit,
            ));
        }
        if request
            .resume_commit
            .as_deref()
            .is_some_and(|commit| commit != response.commit)
        {
            return Err(UpstreamError::InvalidOutput(
                InvalidOutputReason::ResumeCommitMismatch,
            ));
        }
        if response.files.is_empty() {
            return Err(UpstreamError::InvalidOutput(
                InvalidOutputReason::EmptySnapshot,
            ));
        }
        Ok(FetchedRevision {
            commit: response.commit,
            files: response.files,
            staging: request.staging.clone(),
        })
    }
}

fn terminate_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
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
    use crate::{Archive, FETCH_STAGING_FILE};
    use std::fs;
    use std::time::{Duration, Instant};

    fn run_helper(
        script: &str,
        resume_commit: Option<String>,
        mutate_staging: impl FnOnce(&std::path::Path),
    ) -> UpstreamError {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let staging = archive
            .acquire_fetch_staging("public/model", "main", &[])
            .unwrap();
        mutate_staging(&staging.path);
        let helper = directory.path().join("helper.sh");
        fs::write(&helper, format!("#!/bin/sh\n{script}\n")).unwrap();
        OfficialHfFetcher {
            python: "sh".into(),
            helper,
        }
        .fetch(&FetchRequest {
            repo_type: RepositoryType::Model,
            repo_id: "public/model".into(),
            revision: "main".into(),
            files: Vec::new(),
            exclude: Vec::new(),
            staging: staging.path,
            resume_commit,
        })
        .unwrap_err()
    }

    #[test]
    fn helper_output_contract_is_deserialized() {
        let commit = "a".repeat(40);
        let encoded = format!(r#"{{ "commit":"{commit}","files":["config.json"] }}"#);
        let output: HelperOutput = serde_json::from_slice(encoded.as_bytes()).unwrap();
        assert_eq!(output.commit, commit);
        assert_eq!(output.files, vec!["config.json"]);
    }

    #[test]
    fn selection_patterns_are_validated_as_untrusted_input() {
        for pattern in [
            "",
            "../escape",
            "/etc/passwd",
            "a/../b",
            "weights/./a",
            ".modelkeep-manifest.json",
            ".cache/blob",
            "a/.cache/b",
            "a//b",
            "a\\b",
            "!negated",
            "a\nb",
            "/",
        ] {
            assert_eq!(
                FileSelection::new(&[pattern.to_string()], &[]),
                Err(UnsafeSelectionPattern),
                "include {pattern:?} was accepted"
            );
            assert_eq!(
                FileSelection::new(&[], &[pattern.to_string()]),
                Err(UnsafeSelectionPattern),
                "exclude {pattern:?} was accepted"
            );
        }
        for pattern in [
            "config.json",
            "weights/*",
            "weights/",
            "*.safetensors",
            "a/b/c.bin",
            "Qwen3-Q4_K_M/?.gguf",
        ] {
            assert!(
                FileSelection::new(&[pattern.to_string()], &[]).is_ok(),
                "safe pattern {pattern:?} was rejected"
            );
        }
    }

    #[test]
    fn normalized_selection_identity_separates_include_from_exclude() {
        let selection = FileSelection::new(
            &["b.bin".into(), "a.bin".into(), "b.bin".into()],
            &["z.bin".into()],
        )
        .unwrap();
        assert_eq!(selection.include(), ["a.bin".to_string(), "b.bin".into()]);
        assert_eq!(
            selection.identity(),
            vec!["a.bin".to_string(), "b.bin".into(), "!z.bin".into()]
        );
        assert_eq!(
            FileSelection::new(&["a.bin".into(), "b.bin".into()], &["z.bin".into()])
                .unwrap()
                .identity(),
            selection.identity()
        );
        assert_ne!(
            FileSelection::new(&["a.bin".into()], &[])
                .unwrap()
                .identity(),
            FileSelection::new(&[], &["a.bin".into()])
                .unwrap()
                .identity()
        );
        // An unrestricted acquisition keeps the identity it had before
        // selections existed, so staging written earlier stays adoptable.
        assert!(FileSelection::all().identity().is_empty());
        assert!(FileSelection::all().is_unrestricted());
        assert_eq!(
            FileSelection::new(
                &["weights/*".into(), "config.json".into(), "dir/".into()],
                &[]
            )
            .unwrap()
            .required_paths(),
            vec!["config.json".to_string()]
        );
    }

    #[test]
    fn helper_invocation_carries_include_and_exclude_patterns() {
        let commit = "a".repeat(40);
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let selection = FileSelection::new(&["config.json".into()], &["weights/*".into()]).unwrap();
        let staging = archive
            .acquire_fetch_staging("public/model", "main", &selection.identity())
            .unwrap();
        let arguments = directory.path().join("arguments");
        let helper = directory.path().join("helper.sh");
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\nfor argument in \"$@\"; do echo \"$argument\" >> '{}'; done\necho '{{\"type\":\"result\",\"commit\":\"{commit}\",\"files\":[\"config.json\"]}}'\n",
                arguments.display()
            ),
        )
        .unwrap();

        let fetched = OfficialHfFetcher {
            python: "sh".into(),
            helper,
        }
        .fetch(&FetchRequest {
            repo_type: RepositoryType::Model,
            repo_id: "public/model".into(),
            revision: "main".into(),
            files: selection.include().to_vec(),
            exclude: selection.exclude().to_vec(),
            staging: staging.path,
            resume_commit: None,
        })
        .unwrap();

        assert_eq!(fetched.files, vec!["config.json"]);
        let recorded = fs::read_to_string(arguments).unwrap();
        let recorded = recorded.lines().collect::<Vec<_>>();
        assert!(
            recorded
                .windows(2)
                .any(|pair| pair == ["--file", "config.json"]),
            "include pattern missing from {recorded:?}"
        );
        assert!(
            recorded
                .windows(2)
                .any(|pair| pair == ["--exclude", "weights/*"]),
            "exclude pattern missing from {recorded:?}"
        );
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

    #[test]
    fn invalid_output_reason_is_safe_and_actionable() {
        let error = UpstreamError::InvalidOutput(InvalidOutputReason::EmptySnapshot);
        assert_eq!(
            error.to_string(),
            "upstream returned invalid helper output: helper returned an empty snapshot"
        );
    }

    #[test]
    fn every_invalid_output_reason_is_fixed_and_credential_safe() {
        assert_eq!(InvalidOutputReason::ALL.len(), 13);
        for reason in InvalidOutputReason::ALL {
            let safe = reason.safe_reason();
            assert!(!safe.is_empty());
            assert_eq!(safe, reason.to_string());
            assert!(!safe.contains("http://"));
            assert!(!safe.contains("https://"));
            assert!(!safe.contains("token="));
            assert!(!safe.contains("Bearer "));
            assert!(!safe.contains('\n'));
        }
    }

    #[test]
    fn actual_helpers_cover_every_reachable_contract_rejection() {
        let commit_a = "a".repeat(40);
        let commit_b = "b".repeat(40);
        let cases = vec![
            ("echo not-json".into(), None, InvalidOutputReason::NonJsonLine),
            (
                "echo '{\"type\":\"progress\",\"phase\":7}'".into(),
                None,
                InvalidOutputReason::MalformedProgress,
            ),
            (
                "echo '{\"type\":\"progress\",\"version\":2,\"phase\":\"download\"}'".into(),
                None,
                InvalidOutputReason::UnsupportedProgressVersion,
            ),
            (
                "echo '{\"type\":\"resolved\"}'".into(),
                None,
                InvalidOutputReason::MalformedResolved,
            ),
            (
                "echo '{\"type\":\"resolved\",\"commit\":\"bad\"}'".into(),
                None,
                InvalidOutputReason::MalformedResolvedCommit,
            ),
            (
                "echo '{\"type\":\"result\",\"commit\":7,\"files\":[]}'".into(),
                None,
                InvalidOutputReason::MalformedResult,
            ),
            (
                "echo '{\"type\":\"unsupported\"}'".into(),
                None,
                InvalidOutputReason::UnsupportedEventType,
            ),
            (
                "echo '{\"resumed\":true,\"transport\":\"diagnostic\"}'".into(),
                None,
                InvalidOutputReason::MissingResult,
            ),
            ("exit 0".into(), None, InvalidOutputReason::MissingResult),
            (
                "echo '{\"type\":\"result\",\"commit\":\"bad\",\"files\":[\"config.json\"]}'".into(),
                None,
                InvalidOutputReason::MalformedResultCommit,
            ),
            (
                format!("echo '{{\"type\":\"result\",\"commit\":\"{commit_b}\",\"files\":[\"config.json\"]}}'"),
                Some(commit_a.clone()),
                InvalidOutputReason::ResumeCommitMismatch,
            ),
            (
                format!("echo '{{\"type\":\"result\",\"commit\":\"{commit_a}\",\"files\":[]}}'"),
                None,
                InvalidOutputReason::EmptySnapshot,
            ),
        ];
        for (script, resume_commit, expected) in cases {
            let error = run_helper(&script, resume_commit, |_| {});
            assert!(
                matches!(error, UpstreamError::InvalidOutput(reason) if reason == expected),
                "expected {expected:?}, got {error:?}"
            );
            assert_eq!(
                error.to_string(),
                format!("upstream returned invalid helper output: {expected}")
            );
        }

        let resolved = format!(
            "echo '{{\"type\":\"resolved\",\"commit\":\"{}\"}}'",
            "a".repeat(40)
        );
        let error = run_helper(&resolved, None, |staging| {
            fs::write(staging.join(FETCH_STAGING_FILE), b"not-json").unwrap();
        });
        assert!(matches!(
            error,
            UpstreamError::InvalidOutput(InvalidOutputReason::StagingMetadataUpdateFailed)
        ));
    }

    #[test]
    fn untyped_json_diagnostic_is_ignored_before_typed_result() {
        let commit = "a".repeat(40);
        let script = format!(
            "echo '{{\"resumed\":true,\"transport\":\"diagnostic\"}}'; echo '{{\"type\":\"result\",\"commit\":\"{commit}\",\"files\":[\"config.json\"]}}'"
        );
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let staging = archive
            .acquire_fetch_staging("public/model", "main", &[])
            .unwrap();
        let helper = directory.path().join("helper.sh");
        fs::write(&helper, format!("#!/bin/sh\n{script}\n")).unwrap();
        let mut permissions = fs::metadata(&helper).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
            fs::set_permissions(&helper, permissions).unwrap();
        }
        let fetcher = OfficialHfFetcher {
            python: "/bin/sh".into(),
            helper,
        };
        let fetched = fetcher
            .fetch(&FetchRequest {
                repo_type: RepositoryType::Model,
                repo_id: "public/model".into(),
                revision: "main".into(),
                files: vec![],
                exclude: Vec::new(),
                staging: staging.path,
                resume_commit: Some(commit.clone()),
            })
            .unwrap();
        assert_eq!(fetched.commit, commit);
        assert_eq!(fetched.files, vec!["config.json"]);
    }

    #[test]
    fn parser_failure_terminates_and_reaps_a_long_running_helper() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let staging = archive
            .acquire_fetch_staging("public/model", "main", &[])
            .unwrap();
        let helper = directory.path().join("helper.sh");
        let pid_file = directory.path().join("helper.pid");
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\necho $$ > '{}'\necho not-json\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .unwrap();
        let started = Instant::now();
        let error = OfficialHfFetcher {
            python: "sh".into(),
            helper,
        }
        .fetch(&FetchRequest {
            repo_type: RepositoryType::Model,
            repo_id: "public/model".into(),
            revision: "main".into(),
            files: Vec::new(),
            exclude: Vec::new(),
            staging: staging.path,
            resume_commit: None,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            UpstreamError::InvalidOutput(InvalidOutputReason::NonJsonLine)
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid = fs::read_to_string(pid_file).unwrap();
        assert!(
            !std::path::Path::new("/proc").join(pid.trim()).exists(),
            "helper process {pid:?} was not reaped"
        );
    }

    #[test]
    fn staging_metadata_io_failure_is_storage_not_invalid_output() {
        let commit = "a".repeat(40);
        let script =
            format!("echo '{{\"type\":\"resolved\",\"commit\":\"{commit}\"}}'\nexec sleep 30");
        let error = run_helper(&script, None, |staging| {
            fs::remove_file(staging.join(FETCH_STAGING_FILE)).unwrap();
            fs::create_dir(staging.join(FETCH_STAGING_FILE)).unwrap();
        });
        assert!(matches!(error, UpstreamError::Storage));
    }
}
