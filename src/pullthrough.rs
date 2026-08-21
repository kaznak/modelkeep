use std::sync::Arc;

use crate::singleflight::SingleFlight;
use crate::upstream::{FetchRequest, UpstreamError, UpstreamFetcher};
use crate::{Archive, ArchiveError, ArchiveResult, SourceFile};

#[derive(Clone)]
pub struct PullThrough {
    archive: Archive,
    fetcher: Arc<dyn UpstreamFetcher>,
    flights: Arc<SingleFlight<String, String, String>>,
}

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
        }
    }

    pub fn ensure(
        &self,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
    ) -> Result<String, String> {
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
            this.fetch_and_publish(&repo_id, &requested_revision, &files)
                .map_err(|error| error.to_string())
        })
    }

    pub fn refresh(
        &self,
        repo_id: &str,
        reference: &str,
        dry_run: bool,
    ) -> Result<RefreshResult, String> {
        let previous = self.archive.resolve_ref(repo_id, reference).ok();
        let staging = self
            .archive
            .create_fetch_staging()
            .map_err(|e| e.to_string())?;
        let fetched = self
            .fetcher
            .fetch(&FetchRequest {
                repo_id: repo_id.into(),
                revision: reference.into(),
                files: Vec::new(),
                staging: staging.clone(),
            })
            .map_err(|error| {
                let _ = std::fs::remove_dir_all(&staging);
                error.to_string()
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
            self.archive
                .publish_revision_from_directory(crate::SourcePublishRequest {
                    repo_id: repo_id.into(),
                    requested_revision: reference.into(),
                    commit: fetched.commit.clone(),
                    source_root: staging.clone(),
                    files,
                })
                .map_err(|e| e.to_string())?;
        }
        let _ = std::fs::remove_dir_all(&staging);
        self.archive
            .update_ref(repo_id, reference, &fetched.commit)
            .map_err(|e| e.to_string())?;
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
    ) -> ArchiveResult<String> {
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
        let fetched = match self.fetcher.fetch(&request) {
            Ok(result) => result,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(upstream_error(error));
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
        publish?;
        tracing::info!(repo_id = %repo_id, commit = %fetched.commit, "archive publish complete");
        if !is_commit(requested_revision) {
            self.archive
                .update_ref(repo_id, requested_revision, &fetched.commit)?;
        }
        Ok(fetched.commit)
    }
}

fn is_commit(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn upstream_error(error: UpstreamError) -> ArchiveError {
    let message = match error {
        UpstreamError::Unavailable => "upstream unavailable",
        UpstreamError::NotFound => "upstream not found",
        UpstreamError::Unauthorized => "upstream authorization failed",
        UpstreamError::InvalidOutput => "upstream invalid helper output",
        UpstreamError::Failed => "upstream acquisition failed",
        UpstreamError::Io(_) => "upstream I/O failure",
    };
    ArchiveError::InvalidPath(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::FetchedRevision;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RefreshFetcher {
        commit: String,
        fail: bool,
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
                commit: "aaaaaaaa".into(),
                files: vec![crate::ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"old".to_vec(),
                }],
            })
            .unwrap();
        archive.update_ref("org/model", "main", "aaaaaaaa").unwrap();
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
                commit: "aaaaaaaa".into(),
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
        assert_eq!(pull.ensure("org/model", "main", &[]).unwrap(), "aaaaaaaa");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read(
                archive
                    .revision_path("org/model", "aaaaaaaa")
                    .unwrap()
                    .join("config.json")
            )
            .unwrap(),
            b"cold"
        );
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "aaaaaaaa"
        );
    }

    #[test]
    fn refresh_dry_run_changes_nothing() {
        let (_root, archive) = published_archive();
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(RefreshFetcher {
                commit: "bbbbbbbb".into(),
                fail: false,
            }),
        );
        let result = pull.refresh("org/model", "main", true).unwrap();
        assert_eq!(result.previous.as_deref(), Some("aaaaaaaa"));
        assert_eq!(result.proposed, "bbbbbbbb");
        assert!(!result.published);
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "aaaaaaaa"
        );
        assert!(!archive
            .revision_path("org/model", "bbbbbbbb")
            .unwrap()
            .exists());
    }

    #[test]
    fn refresh_publishes_new_revision_and_preserves_old_revision() {
        let (_root, archive) = published_archive();
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(RefreshFetcher {
                commit: "bbbbbbbb".into(),
                fail: false,
            }),
        );
        pull.refresh("org/model", "main", false).unwrap();
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "bbbbbbbb"
        );
        assert!(archive
            .is_complete_revision("org/model", "aaaaaaaa")
            .unwrap());
        assert!(archive
            .is_complete_revision("org/model", "bbbbbbbb")
            .unwrap());
    }

    #[test]
    fn failed_refresh_leaves_ref_and_archive_unchanged() {
        let (_root, archive) = published_archive();
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(RefreshFetcher {
                commit: "bbbbbbbb".into(),
                fail: true,
            }),
        );
        assert!(pull.refresh("org/model", "main", false).is_err());
        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            "aaaaaaaa"
        );
        assert_eq!(
            archive.list_revisions("org/model").unwrap(),
            vec!["aaaaaaaa"]
        );
    }
}
