use std::sync::Arc;

use crate::singleflight::SingleFlight;
use crate::upstream::{FetchProgress, FetchRequest, UpstreamError, UpstreamFetcher};
use crate::{is_hf_commit, Archive, ArchiveError, SourceFile};

#[derive(Clone)]
pub struct PullThrough {
    archive: Archive,
    fetcher: Arc<dyn UpstreamFetcher>,
    flights: Arc<SingleFlight<String, String, PullThroughError>>,
    refresh_flights: Arc<SingleFlight<(String, String, bool), RefreshResult, PullThroughError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullThroughError {
    UpstreamUnavailable,
    UpstreamNotFound,
    UpstreamUnauthorized,
    UpstreamInvalidOutput,
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
            Self::UpstreamInvalidOutput => "upstream invalid output",
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
        self.ensure_with_progress(repo_id, requested_revision, files, &|_| {})
    }

    pub fn ensure_with_progress(
        &self,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<String, PullThroughError> {
        if let Ok(commit) = self.archive.resolve_ref(repo_id, requested_revision) {
            if self.revision_is_ready(repo_id, &commit, files) {
                return Ok(commit);
            }
        }
        if self.revision_is_ready(repo_id, requested_revision, files) {
            return Ok(requested_revision.to_string());
        }

        let key = format!("{repo_id}@{requested_revision}");
        let repo_id = repo_id.to_string();
        let requested_revision = requested_revision.to_string();
        let files = files.to_vec();
        let this = self.clone();
        self.flights.run(key, move || {
            this.fetch_and_publish(&repo_id, &requested_revision, &files, progress)
        })
    }

    pub fn refresh(
        &self,
        repo_id: &str,
        reference: &str,
        dry_run: bool,
    ) -> Result<RefreshResult, PullThroughError> {
        self.refresh_with_progress(repo_id, reference, dry_run, &|_| {})
    }

    pub fn refresh_with_progress(
        &self,
        repo_id: &str,
        reference: &str,
        dry_run: bool,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<RefreshResult, PullThroughError> {
        let key = (repo_id.to_string(), reference.to_string(), dry_run);
        self.refresh_flights.run(key, || {
            // Joined callers receive the same final result. Progress belongs to the
            // leader callback; followers remain in their acquiring phase until the
            // shared operation completes.
            self.refresh_once(repo_id, reference, dry_run, progress)
        })
    }

    fn refresh_once(
        &self,
        repo_id: &str,
        reference: &str,
        dry_run: bool,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<RefreshResult, PullThroughError> {
        let previous = self.archive.resolve_ref(repo_id, reference).ok();
        let staging = self
            .archive
            .create_fetch_staging()
            .map_err(PullThroughError::from)?;
        let fetched = self
            .fetcher
            .fetch_with_progress(
                &FetchRequest {
                    repo_id: repo_id.into(),
                    revision: reference.into(),
                    files: Vec::new(),
                    staging: staging.clone(),
                },
                progress,
            )
            .map_err(|error| {
                let _ = std::fs::remove_dir_all(&staging);
                PullThroughError::from(error)
            })?;
        if dry_run {
            let _ = std::fs::remove_dir_all(&staging);
            return Ok(RefreshResult {
                previous,
                proposed: fetched.commit,
                published: false,
            });
        }
        if !self.revision_is_ready(repo_id, &fetched.commit, &[]) {
            let files = fetched
                .files
                .iter()
                .map(|path| SourceFile {
                    path: path.clone(),
                    source: staging.join(path),
                })
                .collect();
            match self
                .archive
                .publish_revision_from_directory(crate::SourcePublishRequest {
                    repo_id: repo_id.into(),
                    requested_revision: reference.into(),
                    commit: fetched.commit.clone(),
                    source_root: staging.clone(),
                    files,
                }) {
                Ok(_) => {}
                Err(ArchiveError::AlreadyPublished(_))
                    if self.revision_is_ready(repo_id, &fetched.commit, &[]) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let _ = std::fs::remove_dir_all(&staging);
        self.archive
            .update_ref(repo_id, reference, &fetched.commit)
            .map_err(PullThroughError::from)?;
        Ok(RefreshResult {
            previous,
            proposed: fetched.commit,
            published: true,
        })
    }

    fn revision_is_ready(&self, repo_id: &str, commit: &str, files: &[String]) -> bool {
        self.archive
            .is_complete_revision(repo_id, commit)
            .unwrap_or(false)
            && files
                .iter()
                .all(|file| self.archive.resolve_file(repo_id, commit, file).is_ok())
    }

    fn fetch_and_publish(
        &self,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<String, PullThroughError> {
        if let Ok(commit) = self.archive.resolve_ref(repo_id, requested_revision) {
            if self.revision_is_ready(repo_id, &commit, files) {
                return Ok(commit);
            }
        }
        let staging = self.archive.create_fetch_staging()?;
        tracing::info!(repo_id = %repo_id, requested_revision = %requested_revision, "upstream fetch start");
        let request = FetchRequest {
            repo_id: repo_id.to_string(),
            revision: requested_revision.to_string(),
            files: Vec::new(),
            staging: staging.clone(),
        };
        let fetched = match self.fetcher.fetch_with_progress(&request, progress) {
            Ok(result) => result,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(error.into());
            }
        };
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
            .publish_revision_from_directory(crate::SourcePublishRequest {
                repo_id: repo_id.to_string(),
                requested_revision: requested_revision.to_string(),
                commit: fetched.commit.clone(),
                source_root: fetched.staging.clone(),
                files: source_files,
            });
        let _ = std::fs::remove_dir_all(&staging);
        match publish {
            Ok(_) => {}
            Err(ArchiveError::AlreadyPublished(_))
                if self.revision_is_ready(repo_id, &fetched.commit, files) => {}
            Err(error) => return Err(error.into()),
        }
        tracing::info!(repo_id = %repo_id, commit = %fetched.commit, "archive publish complete");
        if !is_hf_commit(requested_revision) {
            self.archive
                .update_ref(repo_id, requested_revision, &fetched.commit)?;
        }
        Ok(fetched.commit)
    }
}

impl From<UpstreamError> for PullThroughError {
    fn from(error: UpstreamError) -> Self {
        match error {
            UpstreamError::Unavailable => Self::UpstreamUnavailable,
            UpstreamError::NotFound => Self::UpstreamNotFound,
            UpstreamError::Unauthorized => Self::UpstreamUnauthorized,
            UpstreamError::InvalidOutput => Self::UpstreamInvalidOutput,
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
    use crate::upstream::FetchedRevision;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::time::Duration;

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
}
