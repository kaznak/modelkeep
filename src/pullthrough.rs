use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use crate::singleflight::{Joined, SingleFlight};
use crate::upstream::{
    FetchProgress, FetchRequest, FileSelection, InvalidOutputReason, InventoryRequest,
    UpstreamError, UpstreamFetcher,
};
use crate::{is_hf_commit, Archive, ArchiveError, RepositoryType, SourceFile};

/// Identity of one in-flight acquisition.
///
/// The normalized selection is part of it (ADR-0020 decision 5): two callers
/// asking for the same revision under the same selection share one acquisition,
/// while callers asking for different files must not be answered with a commit
/// whose revision does not hold the file they asked for.
type FlightKey = (RepositoryType, String, String, String);

type AcquisitionFlights =
    SingleFlight<FlightKey, AcquisitionResult, PullThroughError, FetchProgress>;
type RefreshFlights =
    SingleFlight<(String, String, bool), RefreshResult, PullThroughError, FetchProgress>;

#[derive(Clone)]
pub struct PullThrough {
    archive: Archive,
    fetcher: Arc<dyn UpstreamFetcher>,
    flights: Arc<AcquisitionFlights>,
    refresh_flights: Arc<RefreshFlights>,
}

/// What one acquisition did to the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquisitionOutcome {
    /// Every path the selection covers was already archived. Nothing was
    /// transferred, and this is a genuine no-op rather than a failed transfer.
    AlreadyArchived,
    /// A revision was published for the first time.
    Published,
    /// An already published revision gained the paths it did not hold.
    Extended,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionResult {
    pub commit: String,
    pub outcome: AcquisitionOutcome,
}

impl AcquisitionResult {
    /// True when this acquisition moved bytes into the archive.
    pub fn transferred(&self) -> bool {
        !matches!(self.outcome, AcquisitionOutcome::AlreadyArchived)
    }
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

    /// Serve-driven acquisition.
    ///
    /// A published revision that already resolves every requested path is
    /// enough; this path never consults upstream for a warm or offline hit, so
    /// serving an archived file keeps working while upstream is unavailable
    /// (core invariant 8).
    pub fn ensure_with_progress_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<String, PullThroughError> {
        self.ensure_bounded_for_type(
            repo_type,
            repo_id,
            requested_revision,
            files,
            None,
            progress,
        )
        .map(|commit| commit.expect("an acquisition joined without a deadline cannot time out"))
    }

    /// Serve-driven acquisition that gives up waiting at `deadline`.
    ///
    /// `Ok(None)` means the deadline elapsed while the acquisition was still
    /// running (Issue 0069). Nothing is published and nothing is cancelled: the
    /// flight keeps running on its own thread, so already transferred bytes
    /// survive the caller giving up and a later request joins the same flight
    /// instead of starting a second download.
    ///
    /// `deadline: None` waits for the result, which is what a management job
    /// needs in order to record a terminal state.
    pub fn ensure_bounded_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        files: &[String],
        deadline: Option<Instant>,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<Option<String>, PullThroughError> {
        let selection =
            FileSelection::from_paths(files).map_err(|_| PullThroughError::UnsafePath)?;
        let required = selection.required_paths();
        if let Ok(commit) =
            self.archive
                .resolve_ref_for_type(repo_type, repo_id, requested_revision)
        {
            if self.revision_is_ready(repo_type, repo_id, &commit, &required) {
                return Ok(Some(commit));
            }
        }
        if self.revision_is_ready(repo_type, repo_id, requested_revision, &required) {
            return Ok(Some(requested_revision.to_string()));
        }
        self.run_acquisition(
            repo_type,
            repo_id,
            requested_revision,
            &selection,
            false,
            deadline,
            progress,
        )
        .map(|result| result.map(|result| result.commit))
    }

    /// Selection-driven acquisition (ADR-0020 decision 1).
    ///
    /// Unlike the serve-driven path, this reconciles an already published
    /// revision against upstream's file list for the selection before deciding
    /// that there is nothing to do, so a selection a partially covered revision
    /// does not satisfy acquires exactly the paths it lacks.
    pub fn ensure_selected_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        selection: &FileSelection,
    ) -> Result<AcquisitionResult, PullThroughError> {
        self.ensure_selected_with_progress_for_type(
            repo_type,
            repo_id,
            requested_revision,
            selection,
            &|_| {},
        )
    }

    pub fn ensure_selected_with_progress_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        selection: &FileSelection,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<AcquisitionResult, PullThroughError> {
        self.run_acquisition(
            repo_type,
            repo_id,
            requested_revision,
            selection,
            true,
            None,
            progress,
        )
        .map(|result| result.expect("an acquisition joined without a deadline cannot time out"))
    }

    /// Starts or joins the acquisition for this selection.
    ///
    /// The acquisition body runs on the flight's own thread and reports through
    /// the sink the flight provides; the caller's `progress` callback is
    /// invoked by this thread as it observes those events, so a borrowed
    /// callback never outlives its caller.
    #[allow(clippy::too_many_arguments)]
    fn run_acquisition(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        selection: &FileSelection,
        reconcile: bool,
        deadline: Option<Instant>,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<Option<AcquisitionResult>, PullThroughError> {
        let key = (
            repo_type,
            repo_id.to_string(),
            requested_revision.to_string(),
            // Pattern components can hold no control characters, so a newline
            // join cannot make two different selections share one key.
            selection.identity().join("\n"),
        );
        let owned_repo_id = repo_id.to_string();
        let owned_revision = requested_revision.to_string();
        let selection = selection.clone();
        let this = self.clone();
        // The acquisition keeps the starting caller's log destination: its
        // lifecycle events are the operational record of the transfer and must
        // not disappear because it moved off the request thread.
        let dispatch = tracing::dispatcher::get_default(tracing::Dispatch::clone);
        let joined = self.flights.join(
            key,
            move |sink| {
                tracing::dispatcher::with_default(&dispatch, || {
                    if reconcile {
                        this.acquire_reconciled(
                            repo_type,
                            &owned_repo_id,
                            &owned_revision,
                            &selection,
                            sink,
                        )
                    } else {
                        this.fetch_and_publish(
                            repo_type,
                            &owned_repo_id,
                            &owned_revision,
                            &selection,
                            sink,
                        )
                    }
                })
            },
            deadline,
            &|event| progress(event),
        );
        match joined {
            Joined::Completed(result) => result.map(Some),
            Joined::Pending => Ok(None),
            Joined::Abandoned => {
                // Only a bug in the acquisition body can reach this. It is an
                // internal failure, never a miss and never a partial result.
                tracing::error!(event = "acquisition_abandoned", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, "acquisition thread ended without a result");
                Err(PullThroughError::Conflict)
            }
        }
    }

    /// Acquires a selection against a revision that may already be published.
    ///
    /// `complete: true` says every path the manifest lists was fully acquired;
    /// it says nothing about whether the manifest covers the selection
    /// (ADR-0020 decision 2). Treating it as sufficient would turn a prefetch
    /// against a revision published by an earlier single-file request into a
    /// silent no-op. Whether upstream holds a path the archive does not is
    /// upstream's answer (decision 3), so the published revision is reconciled
    /// against upstream's file list for this selection, and only the paths the
    /// manifest lacks are transferred and added by extension (decision 4).
    fn acquire_reconciled(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        selection: &FileSelection,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<AcquisitionResult, PullThroughError> {
        // A revision that is not published yet needs no reconciliation, and must
        // not pay for an extra upstream round trip.
        if self
            .published_commit(repo_type, repo_id, requested_revision)
            .is_none()
        {
            return self.fetch_and_publish(
                repo_type,
                repo_id,
                requested_revision,
                selection,
                progress,
            );
        }
        progress(FetchProgress::phase("resolving_revision"));
        let inventory = self
            .fetcher
            .inventory(&InventoryRequest {
                repo_type,
                repo_id: repo_id.to_string(),
                revision: requested_revision.to_string(),
                files: selection.include().to_vec(),
                exclude: selection.exclude().to_vec(),
            })
            .map_err(|error| {
                log_fetch_failure(repo_type, repo_id, requested_revision, "reconcile", &error);
                PullThroughError::from(error)
            })?;
        // A fetcher that cannot enumerate upstream, or an upstream commit the
        // archive does not hold, leaves nothing to reconcile against.
        let Some(inventory) = inventory
            .filter(|inventory| self.revision_is_published(repo_type, repo_id, &inventory.commit))
        else {
            return self.fetch_and_publish(
                repo_type,
                repo_id,
                requested_revision,
                selection,
                progress,
            );
        };
        let archived = self.archived_paths(repo_type, repo_id, &inventory.commit)?;
        let missing = inventory
            .files
            .iter()
            .filter(|path| !archived.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            tracing::info!(event = "archive_selection_satisfied", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, commit = %inventory.commit, covered = inventory.files.len(), "selection is already archived");
            return Ok(AcquisitionResult {
                commit: inventory.commit,
                outcome: AcquisitionOutcome::AlreadyArchived,
            });
        }
        // Narrowing to the absent paths is what keeps a repeated acquisition
        // from re-downloading what the revision already holds.
        let narrowed =
            FileSelection::from_paths(&missing).map_err(|_| PullThroughError::UnsafePath)?;
        self.fetch_and_publish(repo_type, repo_id, requested_revision, &narrowed, progress)
    }

    /// The published, complete commit this request already resolves to, if any.
    fn published_commit(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
    ) -> Option<String> {
        if let Ok(commit) =
            self.archive
                .resolve_ref_for_type(repo_type, repo_id, requested_revision)
        {
            if self.revision_is_published(repo_type, repo_id, &commit) {
                return Some(commit);
            }
        }
        self.revision_is_published(repo_type, repo_id, requested_revision)
            .then(|| requested_revision.to_string())
    }

    /// The paths a published revision's live manifest lists.
    ///
    /// A manifest that cannot be read or parsed is a storage or integrity
    /// failure, never an empty archive that would trigger a re-download.
    fn archived_paths(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
    ) -> Result<BTreeSet<String>, PullThroughError> {
        let corrupt = || ArchiveError::IntegrityMismatch("manifest file list is unreadable".into());
        let manifest = self
            .archive
            .manifest_for_type(repo_type, repo_id, commit)
            .map_err(|error| log_archive_failure(repo_type, repo_id, commit, "reconcile", error))?;
        let manifest: serde_json::Value = serde_json::from_str(&manifest)
            .map_err(|_| log_archive_failure(repo_type, repo_id, commit, "reconcile", corrupt()))?;
        let files = manifest["files"].as_array().ok_or_else(|| {
            log_archive_failure(repo_type, repo_id, commit, "reconcile", corrupt())
        })?;
        files
            .iter()
            .map(|file| {
                file["path"].as_str().map(str::to_string).ok_or_else(|| {
                    log_archive_failure(repo_type, repo_id, commit, "reconcile", corrupt())
                })
            })
            .collect()
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
        let owned_repo_id = repo_id.to_string();
        let owned_reference = reference.to_string();
        let this = self.clone();
        let dispatch = tracing::dispatcher::get_default(tracing::Dispatch::clone);
        // A refresh always waits for its result: it is driven by a management
        // job whose record must reach a terminal state.
        let joined = self.refresh_flights.join(
            key,
            move |sink| {
                tracing::dispatcher::with_default(&dispatch, || {
                    this.refresh_once(repo_type, &owned_repo_id, &owned_reference, dry_run, sink)
                })
            },
            None,
            &|event| progress(event),
        );
        match joined {
            Joined::Completed(result) => result,
            Joined::Pending => {
                unreachable!("a refresh joined without a deadline cannot time out")
            }
            Joined::Abandoned => {
                tracing::error!(event = "acquisition_abandoned", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %reference, "refresh thread ended without a result");
                Err(PullThroughError::Conflict)
            }
        }
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
                    exclude: Vec::new(),
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

    /// True when this commit already has a live, complete manifest.
    ///
    /// A revision directory that exists without a complete manifest is a
    /// publication conflict, not something to extend.
    fn revision_is_published(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
    ) -> bool {
        self.archive
            .is_complete_revision_for_type(repo_type, repo_id, commit)
            .unwrap_or(false)
    }

    /// Adds the freshly acquired files a published revision does not list.
    ///
    /// Paths the live manifest already holds are skipped, never rewritten, so
    /// no published byte changes (ADR-0020 decision 4).
    fn extend_published_revision(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        fetched: &crate::upstream::FetchedRevision,
        files: Vec<SourceFile>,
    ) -> Result<AcquisitionOutcome, PullThroughError> {
        let extension = self
            .archive
            .extend_revision_from_directory_for_type(
                repo_type,
                crate::RevisionExtensionRequest {
                    repo_id: repo_id.to_string(),
                    commit: fetched.commit.clone(),
                    source_root: fetched.staging.clone(),
                    files,
                },
            )
            .map_err(|error| {
                log_archive_failure(repo_type, repo_id, requested_revision, "extend", error)
            })?;
        if extension.added.is_empty() {
            return Ok(AcquisitionOutcome::AlreadyArchived);
        }
        tracing::info!(event = "archive_extended", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, commit = %fetched.commit, added = extension.added.len(), skipped = extension.skipped.len(), operation = "pull_through", "archive revision extended");
        Ok(AcquisitionOutcome::Extended)
    }

    fn fetch_and_publish(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        selection: &FileSelection,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
    ) -> Result<AcquisitionResult, PullThroughError> {
        let required = selection.required_paths();
        if let Ok(commit) =
            self.archive
                .resolve_ref_for_type(repo_type, repo_id, requested_revision)
        {
            if self.revision_is_ready(repo_type, repo_id, &commit, &required) {
                return Ok(AcquisitionResult {
                    commit,
                    outcome: AcquisitionOutcome::AlreadyArchived,
                });
            }
        }
        // The normalized selection is part of the staging identity, so staging
        // recorded under a different selection is never adopted (ADR-0020
        // decision 5, ADR-0017).
        let staging = self
            .archive
            .acquire_fetch_staging_for_type(
                repo_type,
                repo_id,
                requested_revision,
                &selection.identity(),
            )
            .map_err(|error| {
                log_archive_failure(repo_type, repo_id, requested_revision, "stage", error)
            })?;
        progress(FetchProgress::phase(if staging.resumed {
            "resuming_snapshot"
        } else {
            "acquiring_snapshot"
        }));
        tracing::info!(event = "upstream_fetch_started", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, resumed = staging.resumed, selected = !selection.is_unrestricted(), operation = "pull_through", "upstream fetch started");
        let request = FetchRequest {
            repo_type,
            repo_id: repo_id.to_string(),
            revision: requested_revision.to_string(),
            files: selection.include().to_vec(),
            exclude: selection.exclude().to_vec(),
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
            .collect::<Vec<_>>();
        // A revision's file set may grow but never change (ADR-0020 decision 4).
        // An acquisition whose commit is already published therefore extends
        // that revision instead of re-publishing it, which is what makes a
        // request for an unheld path cost that path rather than the repository.
        let outcome = if self.revision_is_published(repo_type, repo_id, &fetched.commit) {
            self.extend_published_revision(
                repo_type,
                repo_id,
                requested_revision,
                &fetched,
                source_files,
            )
        } else {
            match self
                .archive
                .publish_revision_from_directory_with_progress_for_type(
                    repo_type,
                    crate::SourcePublishRequest {
                        repo_id: repo_id.to_string(),
                        requested_revision: requested_revision.to_string(),
                        commit: fetched.commit.clone(),
                        source_root: fetched.staging.clone(),
                        files: source_files.clone(),
                    },
                    &|phase| progress(FetchProgress::phase(phase)),
                ) {
                Ok(_) => Ok(AcquisitionOutcome::Published),
                // A concurrent acquisition published this commit first; its file
                // set may not cover what this caller asked for.
                Err(ArchiveError::AlreadyPublished(_))
                    if self.revision_is_published(repo_type, repo_id, &fetched.commit) =>
                {
                    self.extend_published_revision(
                        repo_type,
                        repo_id,
                        requested_revision,
                        &fetched,
                        source_files,
                    )
                }
                Err(error) => Err(log_archive_failure(
                    repo_type,
                    repo_id,
                    requested_revision,
                    "publish",
                    error,
                )),
            }
        };
        let _ = std::fs::remove_dir_all(&staging.path);
        let outcome = outcome?;
        if outcome == AcquisitionOutcome::Published {
            tracing::info!(event = "archive_published", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, commit = %fetched.commit, operation = "pull_through", "archive revision published");
        }
        if !is_hf_commit(requested_revision) {
            self.archive
                .update_ref_for_type(repo_type, repo_id, requested_revision, &fetched.commit)
                .map_err(|error| {
                    log_archive_failure(repo_type, repo_id, requested_revision, "update_ref", error)
                })?;
        }
        Ok(AcquisitionResult {
            commit: fetched.commit,
            outcome,
        })
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedFetch {
        files: Vec<String>,
        exclude: Vec<String>,
        resume_commit: Option<String>,
    }

    /// A fetcher that honours the selection it is given, like the real helper.
    ///
    /// It supports exact paths and a single trailing `*`, which is enough to
    /// show that include and exclude patterns reach the acquisition.
    struct SelectiveFetcher {
        commit: String,
        upstream: Vec<(String, Vec<u8>)>,
        requests: Arc<Mutex<Vec<RecordedFetch>>>,
        inventories: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl SelectiveFetcher {
        fn new(commit: &str, upstream: &[(&str, &[u8])]) -> Self {
            Self {
                commit: commit.into(),
                upstream: upstream
                    .iter()
                    .map(|(path, bytes)| ((*path).to_string(), bytes.to_vec()))
                    .collect(),
                requests: Arc::new(Mutex::new(Vec::new())),
                inventories: Arc::new(AtomicUsize::new(0)),
                delay: Duration::from_millis(0),
            }
        }

        fn matches(path: &str, include: &[String], exclude: &[String]) -> bool {
            let hit = |pattern: &String| match pattern.strip_suffix('*') {
                Some(prefix) => path.starts_with(prefix),
                None => path == pattern.as_str(),
            };
            (include.is_empty() || include.iter().any(hit)) && !exclude.iter().any(hit)
        }
    }

    impl UpstreamFetcher for SelectiveFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            self.requests.lock().unwrap().push(RecordedFetch {
                files: request.files.clone(),
                exclude: request.exclude.clone(),
                resume_commit: request.resume_commit.clone(),
            });
            std::thread::sleep(self.delay);
            let mut files = Vec::new();
            for (path, bytes) in &self.upstream {
                if !Self::matches(path, &request.files, &request.exclude) {
                    continue;
                }
                let destination = request.staging.join(path);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(destination, bytes).unwrap();
                files.push(path.clone());
            }
            Ok(FetchedRevision {
                commit: self.commit.clone(),
                files,
                staging: request.staging.clone(),
            })
        }

        fn inventory(
            &self,
            request: &InventoryRequest,
        ) -> Result<Option<crate::upstream::RevisionInventory>, UpstreamError> {
            self.inventories.fetch_add(1, Ordering::SeqCst);
            Ok(Some(crate::upstream::RevisionInventory {
                commit: self.commit.clone(),
                files: self
                    .upstream
                    .iter()
                    .filter(|(path, _)| Self::matches(path, &request.files, &request.exclude))
                    .map(|(path, _)| path.clone())
                    .collect(),
            }))
        }
    }

    fn selection(include: &[&str], exclude: &[&str]) -> FileSelection {
        FileSelection::new(
            &include
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>(),
            &exclude
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn selection_reaches_the_upstream_helper_as_include_and_exclude() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let fetcher = Arc::new(SelectiveFetcher::new(
            "cccccccccccccccccccccccccccccccccccccccc",
            &[
                ("config.json", b"config"),
                ("weights/a.bin", b"a"),
                ("weights/b.bin", b"b"),
            ],
        ));
        let requests = fetcher.requests.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);

        let acquired = pull
            .ensure_selected_for_type(
                RepositoryType::Model,
                "org/model",
                "main",
                &selection(&["weights/*"], &["weights/b.bin"]),
            )
            .unwrap();
        let commit = acquired.commit;

        assert_eq!(acquired.outcome, AcquisitionOutcome::Published);
        let recorded = requests.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].files, vec!["weights/*".to_string()]);
        assert_eq!(recorded[0].exclude, vec!["weights/b.bin".to_string()]);
        assert!(archive
            .resolve_file("org/model", &commit, "weights/a.bin")
            .is_ok());
        assert!(archive
            .resolve_file("org/model", &commit, "weights/b.bin")
            .is_err());
        assert!(archive
            .resolve_file("org/model", &commit, "config.json")
            .is_err());
        assert_eq!(archive.verify_revision("org/model", &commit).unwrap(), 1);
    }

    #[test]
    fn single_file_cold_miss_acquires_and_publishes_only_that_file() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let fetcher = Arc::new(SelectiveFetcher::new(
            "cccccccccccccccccccccccccccccccccccccccc",
            &[
                ("config.json", b"config"),
                ("huge.bin", b"a-very-large-file"),
            ],
        ));
        let requests = fetcher.requests.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);

        let commit = pull
            .ensure("org/model", "main", &["config.json".to_string()])
            .unwrap();

        assert_eq!(
            requests.lock().unwrap()[0].files,
            vec!["config.json".to_string()]
        );
        assert_eq!(archive.verify_revision("org/model", &commit).unwrap(), 1);
        assert_eq!(
            fs::read(
                archive
                    .resolve_file("org/model", &commit, "config.json")
                    .unwrap()
                    .path
            )
            .unwrap(),
            b"config"
        );
        assert!(archive
            .resolve_file("org/model", &commit, "huge.bin")
            .is_err());
    }

    #[test]
    fn unheld_path_extends_the_published_revision_without_refetching_it() {
        let (_root, archive) = published_archive();
        let fetcher = Arc::new(SelectiveFetcher::new(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &[
                ("config.json", b"upstream-config"),
                ("tokenizer.json", b"tokenizer"),
                ("huge.bin", b"a-very-large-file"),
            ],
        ));
        let requests = fetcher.requests.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);

        let commit = pull
            .ensure("org/model", "main", &["tokenizer.json".to_string()])
            .unwrap();

        assert_eq!(commit, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        // Only the requested path was transferred, not the repository.
        assert_eq!(
            requests.lock().unwrap()[0].files,
            vec!["tokenizer.json".to_string()]
        );
        assert_eq!(
            archive.list_revisions("org/model").unwrap(),
            vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
        );
        assert_eq!(
            fs::read(
                archive
                    .resolve_file("org/model", &commit, "tokenizer.json")
                    .unwrap()
                    .path
            )
            .unwrap(),
            b"tokenizer"
        );
    }

    #[test]
    fn extension_leaves_already_archived_files_byte_identical() {
        let (_root, archive) = published_archive();
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(SelectiveFetcher::new(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &[
                    ("config.json", b"upstream-config"),
                    ("tokenizer.json", b"tokenizer"),
                ],
            )),
        );

        let commit = pull
            .ensure("org/model", "main", &["tokenizer.json".to_string()])
            .unwrap();

        assert_eq!(
            fs::read(
                archive
                    .resolve_file("org/model", &commit, "config.json")
                    .unwrap()
                    .path
            )
            .unwrap(),
            b"old"
        );
        assert_eq!(archive.verify_revision("org/model", &commit).unwrap(), 2);
        assert!(archive.is_complete_revision("org/model", &commit).unwrap());
    }

    struct InterruptedSelectionFetcher {
        commit: String,
    }

    impl UpstreamFetcher for InterruptedSelectionFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            crate::record_fetch_resolved_commit(
                &request.staging,
                &request.repo_id,
                &request.revision,
                &request.selection_identity(),
                &self.commit,
            )
            .unwrap();
            fs::write(request.staging.join("partial.bin"), b"partial").unwrap();
            Err(UpstreamError::Unavailable)
        }
    }

    #[test]
    fn staging_recorded_under_a_different_selection_is_not_resumed() {
        let commit = "cccccccccccccccccccccccccccccccccccccccc";
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let interrupted = PullThrough::new(
            archive.clone(),
            Arc::new(InterruptedSelectionFetcher {
                commit: commit.into(),
            }),
        );
        assert_eq!(
            interrupted.ensure_selected_for_type(
                RepositoryType::Model,
                "org/model",
                "main",
                &selection(&["a.bin"], &[]),
            ),
            Err(PullThroughError::UpstreamUnavailable)
        );

        let fetcher = Arc::new(SelectiveFetcher::new(
            commit,
            &[("a.bin", b"aaa"), ("b.bin", b"bbb")],
        ));
        let requests = fetcher.requests.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);

        // A different selection must not adopt the abandoned staging.
        pull.ensure_selected_for_type(
            RepositoryType::Model,
            "org/model",
            "main",
            &selection(&["b.bin"], &[]),
        )
        .unwrap();
        assert_eq!(requests.lock().unwrap()[0].resume_commit, None);

        // The matching selection still resumes it.
        pull.ensure_selected_for_type(
            RepositoryType::Model,
            "org/model",
            "main",
            &selection(&["a.bin"], &[]),
        )
        .unwrap();
        assert_eq!(
            requests.lock().unwrap()[1].resume_commit.as_deref(),
            Some(commit)
        );
        assert!(archive.resolve_file("org/model", commit, "a.bin").is_ok());
        assert!(archive.resolve_file("org/model", commit, "b.bin").is_ok());
        assert!(archive
            .resolve_file("org/model", commit, "partial.bin")
            .is_err());
    }

    #[test]
    fn concurrent_requests_for_the_same_selection_share_one_acquisition() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let mut fetcher = SelectiveFetcher::new(
            "cccccccccccccccccccccccccccccccccccccccc",
            &[("config.json", b"config"), ("tokenizer.json", b"tokenizer")],
        );
        fetcher.delay = Duration::from_millis(50);
        let fetcher = Arc::new(fetcher);
        let requests = fetcher.requests.clone();
        let pull = Arc::new(PullThrough::new(archive.clone(), fetcher));

        let start = Arc::new(Barrier::new(3));
        let threads = (0..2)
            .map(|_| {
                let pull = pull.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    pull.ensure("org/model", "main", &["config.json".to_string()])
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        for thread in threads {
            assert_eq!(
                thread.join().unwrap().unwrap(),
                "cccccccccccccccccccccccccccccccccccccccc"
            );
        }
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_bounded_acquisition_gives_up_waiting_without_cancelling_the_transfer() {
        let commit = "cccccccccccccccccccccccccccccccccccccccc";
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let mut fetcher = SelectiveFetcher::new(commit, &[("config.json", b"config")]);
        fetcher.delay = Duration::from_millis(300);
        let fetcher = Arc::new(fetcher);
        let requests = fetcher.requests.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);
        let files = ["config.json".to_string()];

        // The deadline elapses first, and nothing is published yet.
        assert_eq!(
            pull.ensure_bounded_for_type(
                RepositoryType::Model,
                "org/model",
                "main",
                &files,
                Some(Instant::now() + Duration::from_millis(20)),
                &|_| {},
            ),
            Ok(None)
        );
        assert!(archive.list_revisions("org/model").unwrap().is_empty());

        // A retry joins the transfer that is still running rather than starting
        // a second one, and is answered once it completes.
        assert_eq!(
            pull.ensure_bounded_for_type(
                RepositoryType::Model,
                "org/model",
                "main",
                &files,
                Some(Instant::now() + Duration::from_secs(30)),
                &|_| {},
            ),
            Ok(Some(commit.to_string()))
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(archive
            .resolve_file("org/model", commit, "config.json")
            .is_ok());
    }

    #[test]
    fn archived_file_is_served_offline_while_an_unheld_path_reports_upstream_failure() {
        let (_root, archive) = published_archive();
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(RefreshFetcher {
                commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                fail: true,
            }),
        );

        assert_eq!(
            pull.ensure("org/model", "main", &["config.json".to_string()])
                .unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            fs::read(
                archive
                    .resolve_file(
                        "org/model",
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "config.json"
                    )
                    .unwrap()
                    .path
            )
            .unwrap(),
            b"old"
        );
        // ADR-0020: a path the archive does not hold is upstream's answer, so it
        // fails as upstream-unavailable rather than as a local 404.
        assert_eq!(
            pull.ensure("org/model", "main", &["tokenizer.json".to_string()]),
            Err(PullThroughError::UpstreamUnavailable)
        );
    }

    fn publish_partial_revision(archive: &Archive, repo_id: &str, commit: &str, path: &str) {
        archive
            .publish_revision(crate::PublishRequest {
                repo_id: repo_id.into(),
                requested_revision: "main".into(),
                commit: commit.into(),
                files: vec![crate::ArchiveFile {
                    path: path.into(),
                    bytes: b"readme".to_vec(),
                }],
            })
            .unwrap();
        archive.update_ref(repo_id, "main", commit).unwrap();
    }

    fn quantisation_fetcher(commit: &str) -> SelectiveFetcher {
        SelectiveFetcher::new(
            commit,
            &[
                ("README.md", b"readme-upstream"),
                ("q4/model-00001.gguf", b"q4-one"),
                ("q4/model-00002.gguf", b"q4-two"),
                ("q8/model-00001.gguf", b"q8-one"),
            ],
        )
    }

    #[test]
    fn glob_selection_extends_a_partially_archived_revision_and_never_refetches_it() {
        let commit = "cccccccccccccccccccccccccccccccccccccccc";
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        // An earlier single-file resolve published this revision with the
        // README alone; `complete: true` says nothing about coverage.
        publish_partial_revision(&archive, "org/gguf", commit, "README.md");
        let fetcher = Arc::new(quantisation_fetcher(commit));
        let requests = fetcher.requests.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);
        let wanted = selection(&["q4/*"], &[]);

        let first = pull
            .ensure_selected_for_type(RepositoryType::Model, "org/gguf", "main", &wanted)
            .unwrap();

        assert_eq!(first.commit, commit);
        assert_eq!(first.outcome, AcquisitionOutcome::Extended);
        assert!(first.transferred());
        // Only the paths the manifest lacked were transferred.
        let recorded = requests.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].files,
            vec![
                "q4/model-00001.gguf".to_string(),
                "q4/model-00002.gguf".to_string()
            ]
        );
        assert!(archive
            .resolve_file("org/gguf", commit, "q4/model-00001.gguf")
            .is_ok());
        assert!(archive
            .resolve_file("org/gguf", commit, "q8/model-00001.gguf")
            .is_err());
        assert_eq!(
            fs::read(
                archive
                    .resolve_file("org/gguf", commit, "README.md")
                    .unwrap()
                    .path
            )
            .unwrap(),
            b"readme"
        );
        assert_eq!(archive.list_revisions("org/gguf").unwrap(), vec![commit]);

        // Repeating the same acquisition transfers nothing at all.
        let second = pull
            .ensure_selected_for_type(RepositoryType::Model, "org/gguf", "main", &wanted)
            .unwrap();
        assert_eq!(second.commit, commit);
        assert_eq!(second.outcome, AcquisitionOutcome::AlreadyArchived);
        assert!(!second.transferred());
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn unrestricted_acquisition_of_a_partially_archived_revision_extends_it() {
        let commit = "cccccccccccccccccccccccccccccccccccccccc";
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        publish_partial_revision(&archive, "org/gguf", commit, "README.md");
        let fetcher = Arc::new(quantisation_fetcher(commit));
        let requests = fetcher.requests.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);

        let result = pull
            .ensure_selected_for_type(
                RepositoryType::Model,
                "org/gguf",
                "main",
                &FileSelection::all(),
            )
            .unwrap();

        assert_eq!(result.outcome, AcquisitionOutcome::Extended);
        assert_eq!(
            requests.lock().unwrap()[0].files,
            vec![
                "q4/model-00001.gguf".to_string(),
                "q4/model-00002.gguf".to_string(),
                "q8/model-00001.gguf".to_string()
            ]
        );
        assert_eq!(archive.verify_revision("org/gguf", commit).unwrap(), 4);
    }

    #[test]
    fn a_fully_archived_selection_is_a_no_op_without_any_transfer() {
        let commit = "cccccccccccccccccccccccccccccccccccccccc";
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        publish_partial_revision(&archive, "org/gguf", commit, "README.md");
        let fetcher = Arc::new(SelectiveFetcher::new(commit, &[("README.md", b"readme")]));
        let requests = fetcher.requests.clone();
        let inventories = fetcher.inventories.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);

        let result = pull
            .ensure_selected_for_type(
                RepositoryType::Model,
                "org/gguf",
                "main",
                &FileSelection::all(),
            )
            .unwrap();

        assert_eq!(
            result,
            AcquisitionResult {
                commit: commit.to_string(),
                outcome: AcquisitionOutcome::AlreadyArchived,
            }
        );
        assert!(!result.transferred());
        assert!(requests.lock().unwrap().is_empty());
        // Exactly one metadata round trip, and no acquisition.
        assert_eq!(inventories.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_cold_selection_does_not_pay_for_a_reconciliation_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let fetcher = Arc::new(quantisation_fetcher(
            "cccccccccccccccccccccccccccccccccccccccc",
        ));
        let inventories = fetcher.inventories.clone();
        let pull = PullThrough::new(archive, fetcher);

        let result = pull
            .ensure_selected_for_type(
                RepositoryType::Model,
                "org/gguf",
                "main",
                &selection(&["q4/*"], &[]),
            )
            .unwrap();

        assert_eq!(result.outcome, AcquisitionOutcome::Published);
        assert_eq!(inventories.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_fetcher_without_inventory_support_acquires_with_the_selection_as_given() {
        struct PlainFetcher {
            requests: Arc<Mutex<Vec<Vec<String>>>>,
        }

        impl UpstreamFetcher for PlainFetcher {
            fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
                self.requests.lock().unwrap().push(request.files.clone());
                fs::write(request.staging.join("tokenizer.json"), b"tokenizer").unwrap();
                Ok(FetchedRevision {
                    commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    files: vec!["tokenizer.json".into()],
                    staging: request.staging.clone(),
                })
            }
        }

        let (_root, archive) = published_archive();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let pull = PullThrough::new(
            archive.clone(),
            Arc::new(PlainFetcher {
                requests: requests.clone(),
            }),
        );

        let result = pull
            .ensure_selected_for_type(
                RepositoryType::Model,
                "org/model",
                "main",
                &selection(&["tokenizer.json"], &[]),
            )
            .unwrap();

        assert_eq!(result.outcome, AcquisitionOutcome::Extended);
        assert_eq!(
            *requests.lock().unwrap(),
            vec![vec!["tokenizer.json".to_string()]]
        );
    }

    #[derive(Default)]
    struct OfflineFetcher {
        calls: Arc<AtomicUsize>,
    }

    impl UpstreamFetcher for OfflineFetcher {
        fn fetch(&self, _request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(UpstreamError::Unavailable)
        }

        fn inventory(
            &self,
            _request: &InventoryRequest,
        ) -> Result<Option<crate::upstream::RevisionInventory>, UpstreamError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(UpstreamError::Unavailable)
        }
    }

    #[test]
    fn serving_paths_never_consult_an_unavailable_upstream_for_archived_content() {
        let (_root, archive) = published_archive();
        let fetcher = Arc::new(OfflineFetcher::default());
        let calls = fetcher.calls.clone();
        let pull = PullThrough::new(archive, fetcher);

        // The metadata routes ask without a selection.
        assert_eq!(
            pull.ensure("org/model", "main", &[]).unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        // The file route asks for the archived path.
        assert_eq!(
            pull.ensure("org/model", "main", &["config.json".to_string()])
                .unwrap(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unsafe_selection_patterns_are_rejected_before_any_acquisition() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let fetcher = Arc::new(SelectiveFetcher::new(
            "cccccccccccccccccccccccccccccccccccccccc",
            &[("config.json", b"config")],
        ));
        let requests = fetcher.requests.clone();
        let pull = PullThrough::new(archive, fetcher);

        assert_eq!(
            pull.ensure("org/model", "main", &["../escape".to_string()]),
            Err(PullThroughError::UnsafePath)
        );
        assert!(requests.lock().unwrap().is_empty());
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
