use std::sync::Arc;

use crate::singleflight::SingleFlight;
use crate::upstream::{
    FetchProgress, FetchRequest, InvalidOutputReason, UpstreamError, UpstreamFetcher,
};
use crate::{is_hf_commit, Archive, ArchiveError, RepositoryType, SourceFile};

#[derive(Clone)]
pub struct PullThrough {
    archive: Archive,
    fetcher: Arc<dyn UpstreamFetcher>,
    flights: Arc<SingleFlight<(RepositoryType, String, String), String, PullThroughError>>,
    refresh_flights: Arc<SingleFlight<(String, String, bool), RefreshResult, PullThroughError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullThroughError {
    UpstreamUnavailable,
    UpstreamNotFound,
    UpstreamUnauthorized,
    UpstreamInvalidOutput(InvalidOutputReason),
    UpstreamFailed,
    UnsafePath,
    Integrity,
    Storage,
    Conflict,
}

impl std::fmt::Display for PullThroughError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UpstreamUnavailable => "upstream unavailable",
            Self::UpstreamNotFound => "upstream not found",
            Self::UpstreamUnauthorized => "upstream authorization failed",
            Self::UpstreamInvalidOutput(reason) => {
                return write!(formatter, "upstream invalid output: {reason}")
            }
            Self::UpstreamFailed => "upstream acquisition failed",
            Self::UnsafePath => "unsafe archive path",
            Self::Integrity => "archive integrity failure",
            Self::Storage => "archive storage failure",
            Self::Conflict => "archive publication conflict",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PullThroughError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshResult {
    pub previous: Option<String>,
    pub proposed: String,
    pub published: bool,
}

impl PullThrough {
    pub fn new(archive: Archive, fetcher: Arc<dyn UpstreamFetcher>) -> Self {
        Self {
            archive,
            fetcher,
            flights: Arc::new(SingleFlight::new()),
            refresh_flights: Arc::new(SingleFlight::new()),
        }
    }

    pub fn ensure(
        &self,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
    ) -> Result<String, PullThroughError> {
        self.ensure_for_type(RepositoryType::Model, repo_id, requested_revision, files)
    }

    pub fn ensure_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
    ) -> Result<String, PullThroughError> {
        self.ensure_with_progress_for_type(repo_type, repo_id, requested_revision, files, &|_| {})
    }

    pub fn ensure_with_progress(
        &self,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<String, PullThroughError> {
        self.ensure_with_progress_for_type(
            RepositoryType::Model,
            repo_id,
            requested_revision,
            files,
            progress,
        )
    }

    pub fn ensure_with_progress_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<String, PullThroughError> {
        if let Ok(commit) =
            self.archive
                .resolve_ref_for_type(repo_type, repo_id, requested_revision)
        {
            if self.revision_is_ready(repo_type, repo_id, &commit, files) {
                return Ok(commit);
            }
        }
        if self.revision_is_ready(repo_type, repo_id, requested_revision, files) {
            return Ok(requested_revision.to_string());
        }

        let key = (
            repo_type,
            repo_id.to_string(),
            requested_revision.to_string(),
        );
        let repo_id = repo_id.to_string();
        let requested_revision = requested_revision.to_string();
        let files = files.to_vec();
        let this = self.clone();
        self.flights.run(key, move || {
            this.fetch_and_publish(repo_type, &repo_id, &requested_revision, &files, progress)
        })
    }

    pub fn refresh(
        &self,
        repo_id: &str,
        reference: &str,
        dry_run: bool,
    ) -> Result<RefreshResult, PullThroughError> {
        self.refresh_for_type(RepositoryType::Model, repo_id, reference, dry_run)
    }

    pub fn refresh_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        reference: &str,
        dry_run: bool,
    ) -> Result<RefreshResult, PullThroughError> {
        self.refresh_with_progress_for_type(repo_type, repo_id, reference, dry_run, &|_| {})
    }

    pub fn refresh_with_progress(
        &self,
        repo_id: &str,
        reference: &str,
        dry_run: bool,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<RefreshResult, PullThroughError> {
        self.refresh_with_progress_for_type(
            RepositoryType::Model,
            repo_id,
            reference,
            dry_run,
            progress,
        )
    }

    pub fn refresh_with_progress_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        reference: &str,
        dry_run: bool,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<RefreshResult, PullThroughError> {
        let key = (
            format!("{repo_type}:{repo_id}"),
            reference.to_string(),
            dry_run,
        );
        self.refresh_flights.run(key, || {
            // Joined callers receive the same final result. Progress belongs to the
            // leader callback; followers remain in their acquiring phase until the
            // shared operation completes.
            self.refresh_once(repo_type, repo_id, reference, dry_run, progress)
        })
    }

    fn refresh_once(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        reference: &str,
        dry_run: bool,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<RefreshResult, PullThroughError> {
        let previous = self
            .archive
            .resolve_ref_for_type(repo_type, repo_id, reference)
            .ok();
        let staging = self
            .archive
            .acquire_fetch_staging_for_type(repo_type, repo_id, reference, &[])
            .map_err(|error| log_archive_failure(repo_type, repo_id, reference, "stage", error))?;
        progress(FetchProgress::phase(if staging.resumed {
            "resuming_snapshot"
        } else {
            "acquiring_snapshot"
        }));
        tracing::info!(event = "upstream_fetch_started", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %reference, resumed = staging.resumed, operation = "refresh", "upstream fetch started");
        let fetched = self
            .fetcher
            .fetch_with_progress(
                &FetchRequest {
                    repo_type,
                    repo_id: repo_id.into(),
                    revision: reference.into(),
                    files: Vec::new(),
                    staging: staging.path.clone(),
                    resume_commit: staging.resolved_commit.clone(),
                },
                progress,
            )
            .map_err(|error| {
                self.handle_fetch_failure(&staging.path, repo_type, repo_id, reference, &error);
                log_fetch_failure(repo_type, repo_id, reference, "refresh", &error);
                PullThroughError::from(error)
            })?;
        tracing::info!(event = "upstream_fetch_finished", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %reference, commit = %fetched.commit, operation = "refresh", "upstream fetch finished");
        if dry_run {
            let _ = std::fs::remove_dir_all(&staging.path);
            return Ok(RefreshResult {
                previous,
                proposed: fetched.commit,
                published: false,
            });
        }
        if !self.revision_is_ready(repo_type, repo_id, &fetched.commit, &[]) {
            let files = fetched
                .files
                .iter()
                .map(|path| SourceFile {
                    path: path.clone(),
                    source: staging.path.join(path),
                })
                .collect();
            let published = match self
                .archive
                .publish_revision_from_directory_with_progress_for_type(
                    repo_type,
                    crate::SourcePublishRequest {
                        repo_id: repo_id.into(),
                        requested_revision: reference.into(),
                        commit: fetched.commit.clone(),
                        source_root: staging.path.clone(),
                        files,
                    },
                    &|phase| progress(FetchProgress::phase(phase)),
                ) {
                Ok(_) => true,
                Err(ArchiveError::AlreadyPublished(_))
                    if self.revision_is_ready(repo_type, repo_id, &fetched.commit, &[]) =>
                {
                    false
                }
                Err(error) => {
                    return Err(log_archive_failure(
                        repo_type, repo_id, reference, "publish", error,
                    ))
                }
            };
            if published {
                tracing::info!(event = "archive_published", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %reference, commit = %fetched.commit, operation = "refresh", "archive revision published");
            }
        }
        let _ = std::fs::remove_dir_all(&staging.path);
        self.archive
            .update_ref_for_type(repo_type, repo_id, reference, &fetched.commit)
            .map_err(|error| {
                log_archive_failure(repo_type, repo_id, reference, "update_ref", error)
            })?;
        Ok(RefreshResult {
            previous,
            proposed: fetched.commit,
            published: true,
        })
    }

    fn revision_is_ready(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
        files: &[String],
    ) -> bool {
        self.archive
            .is_complete_revision_for_type(repo_type, repo_id, commit)
            .unwrap_or(false)
            && files.iter().all(|file| {
                self.archive
                    .resolve_file_for_type(repo_type, repo_id, commit, file)
                    .is_ok()
            })
    }

    fn fetch_and_publish(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<String, PullThroughError> {
        if let Ok(commit) =
            self.archive
                .resolve_ref_for_type(repo_type, repo_id, requested_revision)
        {
            if self.revision_is_ready(repo_type, repo_id, &commit, files) {
                return Ok(commit);
            }
        }
        let staging = self
            .archive
            .acquire_fetch_staging_for_type(repo_type, repo_id, requested_revision, &[])
            .map_err(|error| {
                log_archive_failure(repo_type, repo_id, requested_revision, "stage", error)
            })?;
        progress(FetchProgress::phase(if staging.resumed {
            "resuming_snapshot"
        } else {
            "acquiring_snapshot"
        }));
        tracing::info!(event = "upstream_fetch_started", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, resumed = staging.resumed, operation = "pull_through", "upstream fetch started");
        let request = FetchRequest {
            repo_type,
            repo_id: repo_id.to_string(),
            revision: requested_revision.to_string(),
            files: Vec::new(),
            staging: staging.path.clone(),
            resume_commit: staging.resolved_commit.clone(),
        };
        let fetched = match self.fetcher.fetch_with_progress(&request, progress) {
            Ok(result) => result,
            Err(error) => {
                self.handle_fetch_failure(
                    &staging.path,
                    repo_type,
                    repo_id,
                    requested_revision,
                    &error,
                );
                log_fetch_failure(
                    repo_type,
                    repo_id,
                    requested_revision,
                    "pull_through",
                    &error,
                );
                return Err(error.into());
            }
        };
        tracing::info!(event = "upstream_fetch_finished", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, commit = %fetched.commit, operation = "pull_through", "upstream fetch finished");
        let source_files = fetched
            .files
            .iter()
            .map(|path| SourceFile {
                path: path.clone(),
                source: fetched.staging.join(path),
            })
            .collect();
        let publish = self
            .archive
            .publish_revision_from_directory_with_progress_for_type(
                repo_type,
                crate::SourcePublishRequest {
                    repo_id: repo_id.to_string(),
                    requested_revision: requested_revision.to_string(),
                    commit: fetched.commit.clone(),
                    source_root: fetched.staging.clone(),
                    files: source_files,
                },
                &|phase| progress(FetchProgress::phase(phase)),
            );
        let _ = std::fs::remove_dir_all(&staging.path);
        let published = match publish {
            Ok(_) => true,
            Err(ArchiveError::AlreadyPublished(_))
                if self.revision_is_ready(repo_type, repo_id, &fetched.commit, files) =>
            {
                false
            }
            Err(error) => {
                return Err(log_archive_failure(
                    repo_type,
                    repo_id,
                    requested_revision,
                    "publish",
                    error,
                ))
            }
        };
        if published {
            tracing::info!(event = "archive_published", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, commit = %fetched.commit, operation = "pull_through", "archive revision published");
        }
        if !is_hf_commit(requested_revision) {
            self.archive
                .update_ref_for_type(repo_type, repo_id, requested_revision, &fetched.commit)
                .map_err(|error| {
                    log_archive_failure(repo_type, repo_id, requested_revision, "update_ref", error)
                })?;
        }
        Ok(fetched.commit)
    }

    fn handle_fetch_failure(
        &self,
        staging: &std::path::Path,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        error: &UpstreamError,
    ) {
        if matches!(
            error,
            UpstreamError::Unavailable
                | UpstreamError::Failed
                | UpstreamError::Io(_)
                | UpstreamError::Storage
        ) && self
            .archive
            .preserve_fetch_staging(staging)
            .is_ok_and(|preserved| preserved)
        {
            tracing::warn!(event = "incomplete_fetch_preserved", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, "preserved interrupted upstream staging for retry");
            return;
        }
        let _ = std::fs::remove_dir_all(staging);
    }
}

fn upstream_error_class(error: &UpstreamError) -> &'static str {
    match error {
        UpstreamError::Unavailable => "unavailable",
        UpstreamError::NotFound => "not_found",
        UpstreamError::Unauthorized => "unauthorized",
        UpstreamError::InvalidOutput(_) => "invalid_output",
        UpstreamError::Storage => "storage",
        UpstreamError::Failed => "failed",
        UpstreamError::Io(_) => "io",
    }
}

fn log_fetch_failure(
    repo_type: RepositoryType,
    repo_id: &str,
    requested_revision: &str,
    operation: &str,
    error: &UpstreamError,
) {
    tracing::warn!(
        event = "upstream_fetch_failed",
        repo_type = %repo_type,
        repo_id = %repo_id,
        requested_revision = %requested_revision,
        operation,
        error_class = upstream_error_class(error),
        "upstream fetch failed"
    );
}

fn log_archive_failure(
    repo_type: RepositoryType,
    repo_id: &str,
    requested_revision: &str,
    operation: &str,
    error: ArchiveError,
) -> PullThroughError {
    match &error {
        ArchiveError::IntegrityMismatch(_) => tracing::warn!(
            event = "archive_verification_failed",
            repo_type = %repo_type,
            repo_id = %repo_id,
            requested_revision = %requested_revision,
            operation,
            error_class = "integrity",
            "archive verification failed"
        ),
        ArchiveError::Io(io_error) => tracing::error!(
            event = "archive_storage_failed",
            repo_type = %repo_type,
            repo_id = %repo_id,
            requested_revision = %requested_revision,
            operation,
            error_class = "storage",
            io_kind = if io_error.kind() == std::io::ErrorKind::StorageFull { "out_of_space" } else { "other" },
            "archive storage operation failed"
        ),
        _ => {}
    }
    error.into()
}

impl From<UpstreamError> for PullThroughError {
    fn from(error: UpstreamError) -> Self {
        match error {
            UpstreamError::Unavailable => Self::UpstreamUnavailable,
            UpstreamError::NotFound => Self::UpstreamNotFound,
            UpstreamError::Unauthorized => Self::UpstreamUnauthorized,
            UpstreamError::InvalidOutput(reason) => Self::UpstreamInvalidOutput(reason),
            UpstreamError::Storage => Self::Storage,
            UpstreamError::Failed | UpstreamError::Io(_) => Self::UpstreamFailed,
        }
    }
}

impl From<ArchiveError> for PullThroughError {
    fn from(error: ArchiveError) -> Self {
        match error {
            ArchiveError::InvalidPath(_) => Self::UnsafePath,
            ArchiveError::IntegrityMismatch(_) => Self::Integrity,
            ArchiveError::AlreadyPublished(_) => Self::Conflict,
            ArchiveError::Io(_) | ArchiveError::ReferencedRevision(_) => Self::Storage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::{FetchedRevision, InvalidOutputReason};
    use std::fs;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::sync::Mutex;
    use std::time::Duration;
    use tracing_subscriber::EnvFilter;

    #[derive(Clone, Default)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for LogWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl LogWriter {
        fn output(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn capture_logs() -> (LogWriter, tracing::subscriber::DefaultGuard) {
        let writer = LogWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_env_filter(EnvFilter::new("info"))
            .with_writer(writer.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (writer, guard)
    }

    struct RefreshFetcher {
        commit: String,
        fail: bool,
    }

    struct AliasedFetcher {
        calls: Arc<AtomicUsize>,
        barrier: Arc<Barrier>,
    }

    struct SlowRefreshFetcher {
        calls: Arc<AtomicUsize>,
        commit: String,
    }

    struct RetryRefreshFetcher {
        calls: Arc<AtomicUsize>,
    }

    impl UpstreamFetcher for RetryRefreshFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(50));
            if call == 0 {
                return Err(UpstreamError::Unavailable);
            }
            fs::write(request.staging.join("config.json"), b"retry").unwrap();
            Ok(FetchedRevision {
                commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                files: vec!["config.json".into()],
                staging: request.staging.clone(),
            })
        }
    }

    impl UpstreamFetcher for SlowRefreshFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(50));
            fs::write(request.staging.join("config.json"), self.commit.as_bytes()).unwrap();
            Ok(FetchedRevision {
                commit: self.commit.clone(),
                files: vec!["config.json".into()],
                staging: request.staging.clone(),
            })
        }
    }

    impl UpstreamFetcher for AliasedFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            fs::write(request.staging.join("config.json"), b"shared").unwrap();
            self.barrier.wait();
            Ok(FetchedRevision {
                commit: "dddddddddddddddddddddddddddddddddddddddd".into(),
                files: vec!["config.json".into()],
                staging: request.staging.clone(),
            })
        }
    }

    impl UpstreamFetcher for RefreshFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            if self.fail {
                return Err(UpstreamError::Unavailable);
            }
            fs::write(request.staging.join("config.json"), self.commit.as_bytes()).unwrap();
            Ok(FetchedRevision {
                commit: self.commit.clone(),
                files: vec!["config.json".into()],
                staging: request.staging.clone(),
            })
        }
    }

    fn published_archive() -> (tempfile::TempDir, Archive) {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        archive
            .publish_revision(crate::PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                files: vec![crate::ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"old".to_vec(),
                }],
            })
            .unwrap();
        archive
            .update_ref(
                "org/model",
                "main",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .unwrap();
        (root, archive)
    }

    struct FakeFetcher {
        calls: Arc<AtomicUsize>,
    }

    impl UpstreamFetcher for FakeFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            fs::write(request.staging.join("config.json"), b"cold").unwrap();
            assert!(request.files.is_empty());
            fs::write(request.staging.join("tokenizer.json"), b"tokenizer").unwrap();
            Ok(FetchedRevision {
                commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                files: vec!["config.json".into(), "tokenizer.json".into()],
                staging: request.staging.clone(),
            })
        }
    }

    #[test]
    fn cold_miss_publishes_and_updates_ref() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(FakeFetcher {
                calls: calls.clone(),
            }),
        );
        assert_eq!(
            pull.ensure("org/model", "main", &[]).unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read(
                archive
                    .revision_path("org/model", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                    .unwrap()
                    .join("config.json")
            )
            .unwrap(),
            b"cold"
        );
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn hexadecimal_short_name_is_updated_as_a_mutable_ref() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(FakeFetcher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );
        assert_eq!(
            pull.ensure("org/model", "deadbeef", &[]).unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            archive.resolve_ref("org/model", "deadbeef").unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn refresh_dry_run_changes_nothing() {
        let (_root, archive) = published_archive();
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(RefreshFetcher {
                commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                fail: false,
            }),
        );
        let result = pull.refresh("org/model", "main", true).unwrap();
        assert_eq!(
            result.previous.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(result.proposed, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert!(!result.published);
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert!(!archive
            .revision_path("org/model", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .unwrap()
            .exists());
    }

    #[test]
    fn refresh_publishes_new_revision_and_preserves_old_revision() {
        let (_root, archive) = published_archive();
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(RefreshFetcher {
                commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                fail: false,
            }),
        );
        pull.refresh("org/model", "main", false).unwrap();
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        assert!(archive
            .is_complete_revision("org/model", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap());
        assert!(archive
            .is_complete_revision("org/model", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .unwrap());
    }

    #[test]
    fn concurrent_same_ref_refreshes_share_one_acquisition() {
        let (_root, archive) = published_archive();
        let calls = Arc::new(AtomicUsize::new(0));
        let pull = Arc::new(PullThrough::new(
            archive.clone(),
            Arc::new(SlowRefreshFetcher {
                calls: calls.clone(),
                commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            }),
        ));
        let start = Arc::new(Barrier::new(3));
        let threads = (0..2)
            .map(|_| {
                let pull = pull.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    pull.refresh("org/model", "main", false)
                })
            })
            .collect::<Vec<_>>();
        start.wait();

        for thread in threads {
            let result = thread.join().unwrap().unwrap();
            assert_eq!(result.proposed, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
            assert!(result.published);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
    }

    #[test]
    fn concurrent_different_ref_refreshes_are_independent_and_converge() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let pull = Arc::new(PullThrough::new(
            archive.clone(),
            Arc::new(AliasedFetcher {
                calls: calls.clone(),
                barrier: Arc::new(Barrier::new(2)),
            }),
        ));
        let threads = ["main", "release"].map(|reference| {
            let pull = pull.clone();
            std::thread::spawn(move || pull.refresh("org/model", reference, false))
        });

        for thread in threads {
            assert_eq!(
                thread.join().unwrap().unwrap().proposed,
                "dddddddddddddddddddddddddddddddddddddddd"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            archive.list_revisions("org/model").unwrap(),
            vec!["dddddddddddddddddddddddddddddddddddddddd"]
        );
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "dddddddddddddddddddddddddddddddddddddddd"
        );
        assert_eq!(
            archive.resolve_ref("org/model", "release").unwrap(),
            "dddddddddddddddddddddddddddddddddddddddd"
        );
    }

    #[test]
    fn concurrent_refresh_failure_is_shared_and_later_call_retries() {
        let (_root, archive) = published_archive();
        let calls = Arc::new(AtomicUsize::new(0));
        let pull = Arc::new(PullThrough::new(
            archive,
            Arc::new(RetryRefreshFetcher {
                calls: calls.clone(),
            }),
        ));
        let start = Arc::new(Barrier::new(3));
        let threads = (0..2)
            .map(|_| {
                let pull = pull.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    pull.refresh("org/model", "main", false)
                })
            })
            .collect::<Vec<_>>();
        start.wait();

        for thread in threads {
            assert_eq!(
                thread.join().unwrap(),
                Err(PullThroughError::UpstreamUnavailable)
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(pull.refresh("org/model", "main", false).is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn failed_refresh_leaves_ref_and_archive_unchanged() {
        let (_root, archive) = published_archive();
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(RefreshFetcher {
                commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                fail: true,
            }),
        );
        assert!(pull.refresh("org/model", "main", false).is_err());
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            archive.list_revisions("org/model").unwrap(),
            vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
        );
    }

    #[test]
    fn cross_alias_publications_converge_on_complete_revision() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let pull = Arc::new(PullThrough::new(
            archive.clone(),
            Arc::new(AliasedFetcher {
                calls: calls.clone(),
                barrier: Arc::new(Barrier::new(3)),
            }),
        ));
        let threads = [
            "main",
            "release",
            "dddddddddddddddddddddddddddddddddddddddd",
        ]
        .into_iter()
        .map(|revision| {
            let pull = pull.clone();
            std::thread::spawn(move || pull.ensure("org/model", revision, &[]))
        })
        .collect::<Vec<_>>();
        for thread in threads {
            assert_eq!(
                thread.join().unwrap().unwrap(),
                "dddddddddddddddddddddddddddddddddddddddd"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            archive.list_revisions("org/model").unwrap(),
            vec!["dddddddddddddddddddddddddddddddddddddddd"]
        );
        assert_eq!(
            archive
                .verify_revision("org/model", "dddddddddddddddddddddddddddddddddddddddd")
                .unwrap(),
            1
        );
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "dddddddddddddddddddddddddddddddddddddddd"
        );
        assert_eq!(
            archive.resolve_ref("org/model", "release").unwrap(),
            "dddddddddddddddddddddddddddddddddddddddd"
        );
    }

    #[test]
    fn publication_conflict_rejects_incomplete_winner() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        fs::create_dir_all(
            archive
                .revision_path("org/model", "dddddddddddddddddddddddddddddddddddddddd")
                .unwrap(),
        )
        .unwrap();
        let pull = PullThrough::new(
            archive,
            Arc::new(RefreshFetcher {
                commit: "dddddddddddddddddddddddddddddddddddddddd".into(),
                fail: false,
            }),
        );
        assert_eq!(
            pull.ensure("org/model", "dddddddddddddddddddddddddddddddddddddddd", &[]),
            Err(PullThroughError::Conflict)
        );
    }

    #[test]
    fn fetch_lifecycle_and_publication_events_have_correlation_fields() {
        let (writer, _guard) = capture_logs();
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let pull = PullThrough::new(
            archive,
            Arc::new(FakeFetcher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );

        pull.ensure("org/model", "main", &[]).unwrap();

        let output = writer.output();
        for event in [
            "upstream_fetch_started",
            "upstream_fetch_finished",
            "archive_published",
        ] {
            assert!(output.contains(event), "missing {event}: {output}");
        }
        assert!(output.contains("org/model"));
        assert!(output.contains("requested_revision"));
        assert!(output.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    }

    struct SensitiveFailureFetcher;

    impl UpstreamFetcher for SensitiveFailureFetcher {
        fn fetch(&self, _request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            Err(UpstreamError::InvalidOutput(
                InvalidOutputReason::EmptySnapshot,
            ))
        }
    }

    struct InterruptedResolvedFetcher;

    impl UpstreamFetcher for InterruptedResolvedFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            crate::record_fetch_resolved_commit(
                &request.staging,
                &request.repo_id,
                &request.revision,
                &request.files,
                "9999999999999999999999999999999999999999",
            )
            .unwrap();
            fs::write(request.staging.join("partial.bin"), b"partial").unwrap();
            Err(UpstreamError::Unavailable)
        }
    }

    struct StorageFailureResolvedFetcher;

    impl UpstreamFetcher for StorageFailureResolvedFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            crate::record_fetch_resolved_commit(
                &request.staging,
                &request.repo_id,
                &request.revision,
                &request.files,
                "8888888888888888888888888888888888888888",
            )
            .unwrap();
            fs::write(request.staging.join("partial.bin"), b"partial").unwrap();
            Err(UpstreamError::Storage)
        }
    }

    #[test]
    fn interrupted_resolved_fetch_emits_safe_preservation_event() {
        let (writer, _guard) = capture_logs();
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let pull = PullThrough::new(archive, Arc::new(InterruptedResolvedFetcher));

        assert_eq!(
            pull.ensure("org/model", "main", &[]),
            Err(PullThroughError::UpstreamUnavailable)
        );

        let output = writer.output();
        assert!(output.contains("incomplete_fetch_preserved"));
        assert!(output.contains("upstream_fetch_failed"));
        assert!(output.contains("org/model"));
        assert!(output.contains("main"));
        assert!(!output.contains("partial.bin"));
    }

    #[test]
    fn helper_staging_storage_failure_is_classified_and_preserved_for_resume() {
        let (writer, _guard) = capture_logs();
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let pull = PullThrough::new(archive, Arc::new(StorageFailureResolvedFetcher));

        assert_eq!(
            pull.ensure("org/model", "main", &[]),
            Err(PullThroughError::Storage)
        );

        let output = writer.output();
        assert!(output.contains("incomplete_fetch_preserved"));
        assert!(output.contains("upstream_fetch_failed"));
        assert!(output.contains("storage"));
        assert!(!output.contains("partial.bin"));
    }

    #[test]
    fn fetch_failure_event_uses_safe_class_without_error_payload() {
        let (writer, _guard) = capture_logs();
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let pull = PullThrough::new(archive, Arc::new(SensitiveFailureFetcher));

        assert_eq!(
            pull.ensure("org/model", "main", &[]),
            Err(PullThroughError::UpstreamInvalidOutput(
                InvalidOutputReason::EmptySnapshot
            ))
        );

        let output = writer.output();
        assert!(output.contains("upstream_fetch_failed"));
        assert!(output.contains("invalid_output"));
        assert!(!output.contains("signed-url-secret"));
        assert!(!output.contains("bearer-secret"));
    }

    struct DuplicateOutputFetcher;

    impl UpstreamFetcher for DuplicateOutputFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            fs::write(request.staging.join("config.json"), b"content").unwrap();
            Ok(FetchedRevision {
                commit: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
                files: vec!["config.json".into(), "config.json".into()],
                staging: request.staging.clone(),
            })
        }
    }

    #[test]
    fn publication_verification_failure_is_structured() {
        let (writer, _guard) = capture_logs();
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let pull = PullThrough::new(archive.clone(), Arc::new(DuplicateOutputFetcher));

        assert_eq!(
            pull.ensure("org/model", "main", &[]),
            Err(PullThroughError::Integrity)
        );
        assert!(archive.list_revisions("org/model").unwrap().is_empty());

        let output = writer.output();
        assert!(output.contains("archive_verification_failed"));
        assert!(output.contains("integrity"));
        assert!(output.contains("org/model"));
    }

    #[test]
    fn disk_full_event_is_safe_and_does_not_change_existing_revision() {
        let (_root, archive) = published_archive();
        let (writer, _guard) = capture_logs();
        let error = ArchiveError::Io(std::io::Error::from_raw_os_error(28));

        assert_eq!(
            log_archive_failure(RepositoryType::Model, "org/model", "main", "publish", error,),
            PullThroughError::Storage
        );
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            archive
                .verify_revision("org/model", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap(),
            1
        );

        let output = writer.output();
        assert!(output.contains("archive_storage_failed"));
        assert!(output.contains("out_of_space"));
        assert!(output.contains("storage"));
    }

    #[test]
    fn staging_io_failure_is_structured_and_preserves_existing_revision() {
        let (root, archive) = published_archive();
        fs::remove_dir_all(root.path().join("tmp")).unwrap();
        fs::write(root.path().join("tmp"), b"injected non-directory").unwrap();
        let (writer, _guard) = capture_logs();
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(FakeFetcher {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );

        assert_eq!(
            pull.ensure("org/another", "main", &[]),
            Err(PullThroughError::Storage)
        );
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            archive
                .verify_revision("org/model", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap(),
            1
        );

        let output = writer.output();
        assert!(output.contains("archive_storage_failed"));
        assert!(output.contains("org/another"));
        assert!(output.contains("storage"));
        assert!(output.contains("other"));
    }

    #[test]
    fn model_and_dataset_acquisitions_with_the_same_id_are_isolated() {
        struct TypedFetcher {
            requests: Arc<Mutex<Vec<RepositoryType>>>,
        }

        impl UpstreamFetcher for TypedFetcher {
            fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
                self.requests.lock().unwrap().push(request.repo_type);
                let (commit, payload) = match request.repo_type {
                    RepositoryType::Model => (
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        b"model".as_slice(),
                    ),
                    RepositoryType::Dataset => (
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        b"dataset".as_slice(),
                    ),
                };
                fs::write(request.staging.join("content.bin"), payload).unwrap();
                Ok(FetchedRevision {
                    commit: commit.into(),
                    files: vec!["content.bin".into()],
                    staging: request.staging.clone(),
                })
            }
        }

        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(TypedFetcher {
                requests: requests.clone(),
            }),
        );

        let model = pull
            .ensure_for_type(RepositoryType::Model, "org/shared", "main", &[])
            .unwrap();
        let dataset = pull
            .ensure_for_type(RepositoryType::Dataset, "org/shared", "main", &[])
            .unwrap();

        assert_ne!(model, dataset);
        assert_eq!(
            *requests.lock().unwrap(),
            vec![RepositoryType::Model, RepositoryType::Dataset]
        );
        assert_eq!(
            fs::read(
                archive
                    .resolve_file_for_type(
                        RepositoryType::Model,
                        "org/shared",
                        &model,
                        "content.bin",
                    )
                    .unwrap()
                    .path,
            )
            .unwrap(),
            b"model"
        );
        assert_eq!(
            fs::read(
                archive
                    .resolve_file_for_type(
                        RepositoryType::Dataset,
                        "org/shared",
                        &dataset,
                        "content.bin",
                    )
                    .unwrap()
                    .path,
            )
            .unwrap(),
            b"dataset"
        );
    }

    #[test]
    fn singleflight_identity_is_not_ambiguous_when_components_contain_at_signs() {
        struct IdentityFetcher {
            calls: Arc<AtomicUsize>,
        }

        impl UpstreamFetcher for IdentityFetcher {
            fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(100));
                let commit = if request.repo_id == "org/a@b" {
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                } else {
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                };
                fs::write(
                    request.staging.join("content.bin"),
                    request.repo_id.as_bytes(),
                )
                .unwrap();
                Ok(FetchedRevision {
                    commit: commit.into(),
                    files: vec!["content.bin".into()],
                    staging: request.staging.clone(),
                })
            }
        }

        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let pull = Arc::new(PullThrough::new(
            archive,
            Arc::new(IdentityFetcher {
                calls: calls.clone(),
            }),
        ));
        let start = Arc::new(Barrier::new(2));
        let first = {
            let pull = pull.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                pull.ensure("org/a@b", "c", &[])
            })
        };
        let second = {
            let pull = pull.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                pull.ensure("org/a", "b@c", &[])
            })
        };

        assert_eq!(
            first.join().unwrap().unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            second.join().unwrap().unwrap(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
