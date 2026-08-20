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
            if self
                .archive
                .revision_path(repo_id, &commit)
                .map(|path| path.is_dir())
                .unwrap_or(false)
            {
                return Ok(commit);
            }
        }
        if self
            .archive
            .revision_path(repo_id, requested_revision)
            .map(|path| path.is_dir())
            .unwrap_or(false)
        {
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

    fn fetch_and_publish(
        &self,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
    ) -> ArchiveResult<String> {
        if let Ok(commit) = self.archive.resolve_ref(repo_id, requested_revision) {
            if self
                .archive
                .revision_path(repo_id, &commit)
                .map(|path| path.is_dir())
                .unwrap_or(false)
            {
                return Ok(commit);
            }
        }
        let staging = self.archive.create_fetch_staging()?;
        tracing::info!(repo_id = %repo_id, requested_revision = %requested_revision, "upstream fetch start");
        let request = FetchRequest {
            repo_id: repo_id.to_string(),
            revision: requested_revision.to_string(),
            files: files.to_vec(),
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

    struct FakeFetcher {
        calls: Arc<AtomicUsize>,
    }

    impl UpstreamFetcher for FakeFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            fs::write(request.staging.join("config.json"), b"cold").unwrap();
            Ok(FetchedRevision {
                commit: "aaaaaaaa".into(),
                files: vec!["config.json".into()],
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
}
