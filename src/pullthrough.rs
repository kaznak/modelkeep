use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::singleflight::{Joined, SingleFlight};
use crate::upstream::{
    CancelOutcome, Cancellation, FetchProgress, FetchRequest, FileSelection, HelperFailureClass,
    InvalidOutputReason, InventoryRequest, SanitizedReason, UpstreamError, UpstreamFetcher,
    UpstreamRepositoryFiles,
};
use crate::{
    bounded_archive_detail, is_hf_commit, Archive, ArchiveError, FetchStagingError, RepositoryType,
    SourceFile, UpstreamFile,
};

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

/// What one acquisition reports while it resolves, before it can transfer.
pub const RESOLVE_PHASE: &str = "resolving_revision";

/// What one acquisition reports while it waits for a transfer slot (ADR-0021).
///
/// A management job in this phase is still `queued`: it holds no slot, has moved
/// no bytes, and is cancellable.
pub const TRANSFER_WAIT_PHASE: &str = "waiting_for_transfer_slot";

/// How many transferring acquisitions may run at once across all repositories.
///
/// ADR-0021 decision 5 sets two rather than one: a limit of one would queue a
/// single small file behind a multi-day transfer of an unrelated repository.
pub const DEFAULT_MAX_TRANSFERRING_ACQUISITIONS: usize = 2;

const MAX_TRANSFERRING_ACQUISITIONS_VARIABLE: &str = "MODELKEEP_MAX_TRANSFERRING_ACQUISITIONS";

/// How often a waiter at the gate rechecks its cancellation token.
///
/// The gate and the cancellation token have separate locks, so a waiter polls
/// rather than being woken by the token. The interval only bounds how long a
/// cancelled waiter stays parked; it never delays admission, which is signalled
/// by the gate's own condition variable.
const GATE_CANCEL_POLL: Duration = Duration::from_millis(50);

/// Reads the effective transferring-acquisition limit.
pub fn max_transferring_acquisitions_from_env() -> Result<usize, String> {
    max_transferring_acquisitions_from_value(std::env::var(MAX_TRANSFERRING_ACQUISITIONS_VARIABLE))
}

fn max_transferring_acquisitions_from_value(
    value: Result<String, std::env::VarError>,
) -> Result<usize, String> {
    match value {
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_MAX_TRANSFERRING_ACQUISITIONS),
        Err(error) => Err(format!(
            "invalid {MAX_TRANSFERRING_ACQUISITIONS_VARIABLE}: {error}"
        )),
        Ok(value) => match value.trim().parse::<usize>() {
            Ok(limit) if limit >= 1 => Ok(limit),
            // Zero is rejected rather than read as "unlimited": the point of
            // the setting is to bound concurrent transfers (ADR-0021).
            _ => Err(format!(
                "invalid {MAX_TRANSFERRING_ACQUISITIONS_VARIABLE}: expected a whole number of at least 1"
            )),
        },
    }
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

type RepositoryKey = (RepositoryType, String);

/// Serializes transferring acquisitions (ADR-0021).
///
/// At most one transfer runs per repository, keyed by repository type and
/// repository ID, and at most `limit` run across all repositories. Waiting is
/// FIFO among the tickets that could run: a ticket whose repository is busy is
/// skipped rather than blocking a free slot for an unrelated repository, which
/// is what keeps different repositories concurrent up to the limit.
///
/// The gate covers transferring invocations only. Metadata and resolve-only
/// invocations are never gated, so an acquisition cannot block on a metadata
/// call it makes itself (ADR-0021 decision 3).
struct TransferGate {
    limit: usize,
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    /// The repositories that hold a slot. One repository holds at most one, so
    /// this set's size is also the global transfer count.
    transferring: BTreeSet<RepositoryKey>,
    waiting: VecDeque<u64>,
    granted: BTreeSet<u64>,
    repository_of: BTreeMap<u64, RepositoryKey>,
    next_ticket: u64,
}

/// One held transfer slot. Dropping it releases the slot and admits the next
/// waiter, whether the acquisition succeeded, failed, or was cancelled.
struct TransferPermit<'gate> {
    gate: &'gate TransferGate,
    repository: RepositoryKey,
}

impl Drop for TransferPermit<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().expect("transfer gate lock poisoned");
        state.transferring.remove(&self.repository);
        TransferGate::promote(&mut state, self.gate.limit);
        drop(state);
        self.gate.changed.notify_all();
    }
}

impl TransferGate {
    fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            state: Mutex::new(GateState::default()),
            changed: Condvar::new(),
        }
    }

    /// Admits as many waiting tickets as the per-repository and global rules
    /// allow, oldest first.
    fn promote(state: &mut GateState, limit: usize) {
        let mut index = 0;
        while index < state.waiting.len() && state.transferring.len() < limit {
            let ticket = state.waiting[index];
            let Some(repository) = state.repository_of.get(&ticket).cloned() else {
                state.waiting.remove(index);
                continue;
            };
            if state.transferring.contains(&repository) {
                index += 1;
                continue;
            }
            state.transferring.insert(repository);
            state.granted.insert(ticket);
            state.waiting.remove(index);
        }
    }

    /// Waits for a transfer slot for `repository`.
    ///
    /// Returns how long the caller waited, or `None` when it was admitted
    /// immediately. A cancellation while waiting is reported as
    /// [`PullThroughError::Cancelled`] and transfers nothing.
    fn acquire(
        &self,
        repository: RepositoryKey,
        cancel: &Cancellation,
    ) -> Result<(TransferPermit<'_>, Option<Duration>), PullThroughError> {
        let mut state = self.state.lock().expect("transfer gate lock poisoned");
        let ticket = state.next_ticket;
        state.next_ticket += 1;
        state.repository_of.insert(ticket, repository.clone());
        state.waiting.push_back(ticket);
        Self::promote(&mut state, self.limit);
        let started = Instant::now();
        let mut waited = false;
        loop {
            if state.granted.remove(&ticket) {
                state.repository_of.remove(&ticket);
                drop(state);
                return Ok((
                    TransferPermit {
                        gate: self,
                        repository,
                    },
                    waited.then(|| started.elapsed()),
                ));
            }
            if cancel.is_cancelled() {
                state.waiting.retain(|waiting| *waiting != ticket);
                state.repository_of.remove(&ticket);
                Self::promote(&mut state, self.limit);
                drop(state);
                self.changed.notify_all();
                return Err(PullThroughError::Cancelled);
            }
            waited = true;
            let (next, _timeout) = self
                .changed
                .wait_timeout(state, GATE_CANCEL_POLL)
                .expect("transfer gate lock poisoned");
            state = next;
        }
    }
}

/// What one acquisition is doing right now, for the in-flight views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcquisitionState {
    Resolving,
    Waiting,
    Transferring,
}

impl AcquisitionState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Resolving => "resolving",
            Self::Waiting => "waiting_for_transfer_slot",
            Self::Transferring => "transferring",
        }
    }
}

/// One acquisition that is in flight right now.
///
/// There is deliberately no field naming who asked for it. The download data
/// plane carries no principal (ADR-0015 keeps identity on the management plane
/// only), so any attribution ModelKeep could report here would be a guess, and
/// a partial hint is worse than none for an operational decision.
#[derive(Debug, Clone, Serialize)]
pub struct AcquisitionSnapshot {
    pub id: String,
    pub repo_type: RepositoryType,
    pub repo_id: String,
    pub requested_revision: String,
    /// The normalized selection this acquisition runs under. Both empty means
    /// the whole repository.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// `pull_through` or `refresh`, the same vocabulary the event stream uses.
    pub operation: &'static str,
    pub state: &'static str,
    pub phase: String,
    pub transferred_bytes: u64,
    pub total_bytes: Option<u64>,
    pub started_at: u64,
    pub cancelled: bool,
}

/// The in-flight view the Admin API and the admin UI report.
#[derive(Debug, Clone, Serialize)]
pub struct AcquisitionsView {
    /// The effective global transfer limit (ADR-0021 decision 5).
    pub transfer_limit: usize,
    /// How many acquisitions hold a transfer slot.
    pub transferring: usize,
    /// How many are waiting for one.
    pub waiting: usize,
    pub items: Vec<AcquisitionSnapshot>,
}

#[derive(Debug)]
struct LiveAcquisition {
    state: AcquisitionState,
    phase: String,
    transferred_bytes: u64,
    total_bytes: Option<u64>,
}

#[derive(Debug)]
struct AcquisitionEntry {
    id: String,
    repo_type: RepositoryType,
    repo_id: String,
    requested_revision: String,
    include: Vec<String>,
    exclude: Vec<String>,
    operation: &'static str,
    started_at: u64,
    cancel: Arc<Cancellation>,
    live: Mutex<LiveAcquisition>,
}

impl AcquisitionEntry {
    fn snapshot(&self) -> AcquisitionSnapshot {
        let live = self.live.lock().expect("acquisition lock poisoned");
        AcquisitionSnapshot {
            id: self.id.clone(),
            repo_type: self.repo_type,
            repo_id: self.repo_id.clone(),
            requested_revision: self.requested_revision.clone(),
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            operation: self.operation,
            state: live.state.as_str(),
            phase: live.phase.clone(),
            transferred_bytes: live.transferred_bytes,
            total_bytes: live.total_bytes,
            started_at: self.started_at,
            cancelled: self.cancel.is_cancelled(),
        }
    }
}

/// Every acquisition that is in flight, whether a management job or a client
/// request started it (Issue 0076).
#[derive(Default)]
struct AcquisitionRegistry {
    state: Mutex<RegistryState>,
}

#[derive(Default)]
struct RegistryState {
    next_id: u64,
    entries: BTreeMap<String, Arc<AcquisitionEntry>>,
}

impl AcquisitionRegistry {
    fn register(
        self: &Arc<Self>,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        selection: &FileSelection,
        operation: &'static str,
    ) -> AcquisitionHandle {
        let mut state = self.state.lock().expect("acquisition registry poisoned");
        state.next_id += 1;
        let id = format!("acq-{:012}", state.next_id);
        let entry = Arc::new(AcquisitionEntry {
            id: id.clone(),
            repo_type,
            repo_id: repo_id.to_string(),
            requested_revision: requested_revision.to_string(),
            include: selection.include().to_vec(),
            exclude: selection.exclude().to_vec(),
            operation,
            started_at: unix_timestamp(),
            cancel: Arc::new(Cancellation::new()),
            live: Mutex::new(LiveAcquisition {
                state: AcquisitionState::Resolving,
                phase: RESOLVE_PHASE.to_string(),
                transferred_bytes: 0,
                total_bytes: None,
            }),
        });
        state.entries.insert(id, Arc::clone(&entry));
        drop(state);
        AcquisitionHandle {
            registry: Arc::clone(self),
            entry,
        }
    }

    fn view(&self, transfer_limit: usize) -> AcquisitionsView {
        let state = self.state.lock().expect("acquisition registry poisoned");
        let items = state
            .entries
            .values()
            .map(|entry| entry.snapshot())
            .collect::<Vec<_>>();
        drop(state);
        AcquisitionsView {
            transfer_limit,
            transferring: items
                .iter()
                .filter(|item| item.state == AcquisitionState::Transferring.as_str())
                .count(),
            waiting: items
                .iter()
                .filter(|item| item.state == AcquisitionState::Waiting.as_str())
                .count(),
            items,
        }
    }

    fn entry(&self, id: &str) -> Option<Arc<AcquisitionEntry>> {
        self.state
            .lock()
            .expect("acquisition registry poisoned")
            .entries
            .get(id)
            .map(Arc::clone)
    }

    fn matching(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        include: &[String],
        exclude: &[String],
        operation: &str,
    ) -> Option<Arc<AcquisitionEntry>> {
        self.state
            .lock()
            .expect("acquisition registry poisoned")
            .entries
            .values()
            .find(|entry| {
                entry.repo_type == repo_type
                    && entry.repo_id == repo_id
                    && entry.requested_revision == requested_revision
                    && entry.include == include
                    && entry.exclude == exclude
                    && entry.operation == operation
            })
            .map(Arc::clone)
    }

    fn remove(&self, id: &str) {
        self.state
            .lock()
            .expect("acquisition registry poisoned")
            .entries
            .remove(id);
    }
}

/// The registration one acquisition holds for as long as it runs.
///
/// Dropping it deregisters the acquisition, including when the acquisition
/// thread panics, so the in-flight view cannot accumulate acquisitions that
/// ended.
struct AcquisitionHandle {
    registry: Arc<AcquisitionRegistry>,
    entry: Arc<AcquisitionEntry>,
}

impl Drop for AcquisitionHandle {
    fn drop(&mut self) {
        self.registry.remove(&self.entry.id);
    }
}

impl AcquisitionHandle {
    fn cancellation(&self) -> &Cancellation {
        &self.entry.cancel
    }

    fn set_state(&self, state: AcquisitionState) {
        let mut live = self.entry.live.lock().expect("acquisition lock poisoned");
        live.state = state;
        if state != AcquisitionState::Transferring {
            live.phase = state_phase(state).to_string();
        }
    }

    /// Records what a progress event says about this acquisition.
    ///
    /// Only a strictly higher byte count counts as transferred, for the same
    /// reason the cold-miss liveness events do it: a repeated counter says the
    /// transfer is alive, not that it advanced.
    fn observe(&self, event: &FetchProgress) {
        let mut live = self.entry.live.lock().expect("acquisition lock poisoned");
        live.phase = event.phase.clone();
        if event.unit.as_deref() == Some("bytes") {
            if let Some(completed) = event.completed {
                live.transferred_bytes = live.transferred_bytes.max(completed);
            }
            if live.total_bytes.is_none() {
                live.total_bytes = event.total;
            }
        }
    }
}

fn state_phase(state: AcquisitionState) -> &'static str {
    match state {
        AcquisitionState::Resolving => RESOLVE_PHASE,
        AcquisitionState::Waiting => TRANSFER_WAIT_PHASE,
        AcquisitionState::Transferring => "transferring",
    }
}

#[derive(Clone)]
pub struct PullThrough {
    archive: Archive,
    fetcher: Arc<dyn UpstreamFetcher>,
    flights: Arc<AcquisitionFlights>,
    refresh_flights: Arc<RefreshFlights>,
    acquisitions: Arc<AcquisitionRegistry>,
    gate: Arc<TransferGate>,
    resolved_refs: Arc<Mutex<ResolvedRefs>>,
}

/// What upstream said each mutable ref resolves to, keyed by repository type,
/// repository, and ref name.
type ResolvedRefs = BTreeMap<(RepositoryType, String, String), String>;

/// How many observed ref resolutions are remembered at once.
///
/// The map exists to learn the handful of refs real traffic asks about, so it is
/// bounded: a client asking about unbounded ref names must not grow it without
/// limit. Beyond the bound a new observation is dropped, which costs a ref the
/// archive learns later rather than any correctness.
const MAX_RESOLVED_REFS: usize = 1024;

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

/// Why a pull-through request could not be answered.
///
/// This type used to be `Copy` and payload-free, and that absence of a payload
/// was how it was kept clear of credentials. That guarantee has moved
/// (Issue 0084): what keeps a reason credential-free is now that the helper
/// sanitizes it where the exception and its context are known, that ModelKeep
/// bounds it as untrusted input, and that the only way to obtain a
/// [`SanitizedReason`] is to derive it from an [`UpstreamError`]. Depending on
/// the payload's absence instead was weaker, because any later change could have
/// added one and lost the property without anything noticing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullThroughError {
    UpstreamUnavailable,
    UpstreamNotFound,
    UpstreamUnauthorized,
    UpstreamInvalidOutput(InvalidOutputReason),
    /// An acquisition that failed for a reason the class alone does not give.
    ///
    /// The reason is present whenever the failure came from upstream, which is
    /// every path ModelKeep itself produces; `None` is what a caller that has no
    /// `UpstreamError` to derive one from reports.
    UpstreamFailed(Option<SanitizedReason>),
    UnsafePath,
    Integrity,
    Storage,
    /// Another operation already holds the work this acquisition needed: a
    /// published revision it would have published over.
    Conflict,
    /// Fetch staging for this identity is held by a running acquisition
    /// (Issue 0083).
    ///
    /// This is a different subsystem from publication: nothing was being
    /// published, and a revision of this repository need not exist. It has its
    /// own class so an operator is not sent to look at the archive.
    StagingConflict,
    /// The acquisition was stopped on request (Issue 0076).
    ///
    /// It is an interruption, not a miss and not a failure of upstream: nothing
    /// was published, the archive is unchanged, and staging is left resumable.
    Cancelled,
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
            // The reason already begins with ModelKeep's description of the
            // class, so it replaces the fixed wording rather than decorating it.
            Self::UpstreamFailed(Some(reason)) => return formatter.write_str(reason.as_str()),
            Self::UpstreamFailed(None) => "upstream acquisition failed",
            Self::UnsafePath => "unsafe archive path",
            Self::Integrity => "archive integrity failure",
            Self::Storage => "archive storage failure",
            Self::Conflict => "archive publication conflict",
            Self::StagingConflict => "fetch staging is held by a running acquisition",
            Self::Cancelled => "acquisition cancelled",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PullThroughError {}

/// What reconciling a selection against upstream and the archive concluded.
enum Reconciliation {
    /// Nothing archived to reconcile against; acquire the selection as asked.
    NotPublished,
    /// The archive already holds every path the selection covers.
    Satisfied(AcquisitionResult),
    /// Only these paths are missing.
    Missing(FileSelection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshResult {
    pub previous: Option<String>,
    pub proposed: String,
    pub published: bool,
}

impl PullThrough {
    pub fn new(archive: Archive, fetcher: Arc<dyn UpstreamFetcher>) -> Self {
        Self::with_transfer_limit(archive, fetcher, DEFAULT_MAX_TRANSFERRING_ACQUISITIONS)
    }

    /// Builds a pull-through whose transfers are bounded by `transfer_limit`
    /// across all repositories (ADR-0021 decision 5).
    pub fn with_transfer_limit(
        archive: Archive,
        fetcher: Arc<dyn UpstreamFetcher>,
        transfer_limit: usize,
    ) -> Self {
        Self {
            archive,
            fetcher,
            flights: Arc::new(SingleFlight::new()),
            refresh_flights: Arc::new(SingleFlight::new()),
            acquisitions: Arc::new(AcquisitionRegistry::default()),
            gate: Arc::new(TransferGate::new(transfer_limit)),
            resolved_refs: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// The effective global transfer limit.
    pub fn transfer_limit(&self) -> usize {
        self.gate.limit
    }

    /// Every acquisition in flight, with what holds each transfer slot and what
    /// is waiting for one (Issue 0076, Issue 0077).
    pub fn in_flight_acquisitions(&self) -> AcquisitionsView {
        self.acquisitions.view(self.gate.limit)
    }

    /// Stops the in-flight acquisition with this identifier.
    ///
    /// `None` means no acquisition is in flight under that identifier, which is
    /// different from one that had already finished.
    pub fn cancel_acquisition(&self, id: &str) -> Option<CancelOutcome> {
        // The acquisition itself emits `acquisition_cancelled` once it stops, so
        // the event records what actually happened rather than what was asked.
        Some(self.acquisitions.entry(id)?.cancel.cancel())
    }

    /// Stops the acquisition a management job is running, addressed by the
    /// job's own target and normalized selection.
    ///
    /// Single-flight means one acquisition can serve a job and any number of
    /// client requests for the same work, so cancelling here stops that shared
    /// acquisition and therefore also the clients waiting on it. That is a
    /// property of sharing the transfer, not a separate policy.
    pub fn cancel_matching_acquisition(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        include: &[String],
        exclude: &[String],
        operation: &str,
    ) -> Option<CancelOutcome> {
        let entry = self.acquisitions.matching(
            repo_type,
            repo_id,
            requested_revision,
            include,
            exclude,
            operation,
        )?;
        Some(entry.cancel.cancel())
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
                // Registration happens inside the flight body, so exactly one
                // record exists per acquisition however many callers share it,
                // and it is visible and cancellable from the moment the work
                // starts rather than once it reaches upstream.
                let handle = this.acquisitions.register(
                    repo_type,
                    &owned_repo_id,
                    &owned_revision,
                    &selection,
                    "pull_through",
                );
                let observed = |event: FetchProgress| {
                    handle.observe(&event);
                    sink(event);
                };
                tracing::dispatcher::with_default(&dispatch, || {
                    if reconcile {
                        this.acquire_reconciled(
                            repo_type,
                            &owned_repo_id,
                            &owned_revision,
                            &selection,
                            &observed,
                            &handle,
                        )
                    } else {
                        this.fetch_and_publish(
                            repo_type,
                            &owned_repo_id,
                            &owned_revision,
                            &selection,
                            &observed,
                            &handle,
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
        handle: &AcquisitionHandle,
    ) -> Result<AcquisitionResult, PullThroughError> {
        // The reconciliation round trip is resolve-only and is made before any
        // transfer slot is held (ADR-0021 decision 3), so a selection a published
        // revision already covers is answered without ever queueing behind
        // another repository's transfer. That is also what keeps an acquisition
        // from waiting on a metadata call of its own.
        let selection = match self.reconcile_selection(
            repo_type,
            repo_id,
            requested_revision,
            selection,
            progress,
            handle,
        )? {
            Reconciliation::Satisfied(result) => return Ok(result),
            Reconciliation::Missing(narrowed) => narrowed,
            Reconciliation::NotPublished => selection.clone(),
        };
        self.fetch_and_publish_selection(
            repo_type,
            repo_id,
            requested_revision,
            &selection,
            true,
            progress,
            handle,
        )
    }

    /// What upstream and the archive together say about a selection.
    ///
    /// `NotPublished` means there is nothing archived to reconcile against, so
    /// the selection is acquired exactly as asked.
    fn reconcile_selection(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        selection: &FileSelection,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
        handle: &AcquisitionHandle,
    ) -> Result<Reconciliation, PullThroughError> {
        // A revision that is not published yet needs no reconciliation, and must
        // not pay for an extra upstream round trip.
        let Some(published) = self.published_commit(repo_type, repo_id, requested_revision) else {
            return Ok(Reconciliation::NotPublished);
        };
        if handle.cancellation().is_cancelled() {
            return Err(PullThroughError::Cancelled);
        }
        if let Some(reconciliation) = self.reconcile_from_record(
            repo_type,
            repo_id,
            requested_revision,
            &published,
            selection,
        )? {
            return Ok(reconciliation);
        }
        handle.set_state(AcquisitionState::Resolving);
        progress(FetchProgress::phase(RESOLVE_PHASE));
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
            return Ok(Reconciliation::NotPublished);
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
            return Ok(Reconciliation::Satisfied(AcquisitionResult {
                commit: inventory.commit,
                outcome: AcquisitionOutcome::AlreadyArchived,
            }));
        }
        // Narrowing to the absent paths is what keeps a repeated acquisition
        // from re-downloading what the revision already holds.
        Ok(Reconciliation::Missing(
            FileSelection::from_paths(&missing).map_err(|_| PullThroughError::UnsafePath)?,
        ))
    }

    /// Reconciles a selection against the revision's own recorded upstream file
    /// list, with no upstream round trip (Issue 0074).
    ///
    /// The list of an immutable commit cannot go stale, so where a record exists
    /// the resolve-only call buys nothing the archive does not already know.
    /// `None` means this cannot be answered locally and the upstream round trip
    /// stands. Three conditions must hold, and each is about answering the same
    /// question upstream would rather than a cheaper one:
    ///
    /// * the request names the commit itself, so no mutable ref can have moved
    ///   upstream since. A `main` upstream has advanced must still be resolved
    ///   upstream, or a prefetch would report a stale revision as satisfied;
    /// * the selection excludes nothing and its includes are concrete paths, so
    ///   no pattern has to be matched by anything but the official client
    ///   (ADR-0020 decision 1). An unrestricted selection qualifies as well,
    ///   because it covers exactly the recorded list;
    /// * the revision actually has a record. One published before this existed,
    ///   or imported from a client cache, keeps the upstream path.
    fn reconcile_from_record(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        published: &str,
        selection: &FileSelection,
    ) -> Result<Option<Reconciliation>, PullThroughError> {
        if !is_hf_commit(requested_revision) || requested_revision != published {
            return Ok(None);
        }
        if !selection.exclude().is_empty()
            || selection.required_paths().len() != selection.include().len()
        {
            return Ok(None);
        }
        let recorded = self
            .archive
            .upstream_files_for_type(repo_type, repo_id, published)
            .map_err(|error| {
                log_archive_failure(repo_type, repo_id, requested_revision, "reconcile", error)
            })?;
        let Some(recorded) = recorded else {
            return Ok(None);
        };
        let upstream = recorded
            .into_iter()
            .map(|file| file.path)
            .collect::<BTreeSet<_>>();
        let covered = if selection.is_unrestricted() {
            upstream.iter().cloned().collect::<Vec<_>>()
        } else {
            selection
                .include()
                .iter()
                .filter(|path| upstream.contains(*path))
                .cloned()
                .collect::<Vec<_>>()
        };
        let archived = self.archived_paths(repo_type, repo_id, published)?;
        let missing = covered
            .iter()
            .filter(|path| !archived.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            tracing::info!(event = "archive_selection_satisfied", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, commit = %published, covered = covered.len(), "selection is already archived");
            return Ok(Some(Reconciliation::Satisfied(AcquisitionResult {
                commit: published.to_string(),
                outcome: AcquisitionOutcome::AlreadyArchived,
            })));
        }
        Ok(Some(Reconciliation::Missing(
            FileSelection::from_paths(&missing).map_err(|_| PullThroughError::UnsafePath)?,
        )))
    }

    /// Asks upstream what a revision contains, without acquiring it.
    ///
    /// This is what lets a client's own file filter narrow a first acquisition
    /// (Issue 0074): the metadata routes answer from upstream's file list, and
    /// the per-file requests that follow acquire only what the client asks for.
    /// Nothing is transferred, nothing is published, and nothing is cached in the
    /// archive, so a metadata answer never becomes archived state on its own.
    ///
    /// `Ok(None)` means this fetcher cannot report upstream's per-file metadata,
    /// so the caller keeps the older behavior of acquiring and answering from the
    /// archive. It is deliberately not an empty answer: ModelKeep must not report
    /// a repository as empty because it could not enumerate it.
    pub fn upstream_metadata_for_type(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
    ) -> Result<Option<UpstreamRepositoryFiles>, PullThroughError> {
        let reported = self
            .fetcher
            .repository_files(&InventoryRequest {
                repo_type,
                repo_id: repo_id.to_string(),
                revision: requested_revision.to_string(),
                files: Vec::new(),
                exclude: Vec::new(),
            })
            .map_err(|error| {
                log_fetch_failure(repo_type, repo_id, requested_revision, "metadata", &error);
                PullThroughError::from(error)
            })?;
        let Some(reported) = reported else {
            return Ok(None);
        };
        tracing::info!(event = "upstream_metadata_answered", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, commit = %reported.commit, files = reported.files.len(), "answered repository metadata from upstream without acquiring");
        if !is_hf_commit(requested_revision) {
            self.remember_resolved_ref(repo_type, repo_id, requested_revision, &reported.commit);
            // An archive that already holds the revision — an imported cache, or
            // one acquired by commit — can adopt the name immediately.
            self.adopt_resolved_refs(repo_type, repo_id, &reported.commit);
        }
        Ok(Some(reported))
    }

    /// Remembers what upstream said a mutable ref resolves to.
    ///
    /// A supported client resolves a ref through the metadata routes and then
    /// requests every file by commit, so nothing in the requests that follow
    /// names the ref. Answering metadata without acquiring would therefore stop
    /// the archive from ever learning what `main` resolved to, and a revision
    /// mirrored by an ordinary `hf download` would be downloadable only by
    /// commit — not by the name it was mirrored under, which is the offline
    /// guarantee of core invariant 8.
    ///
    /// This is in-memory only: a metadata answer writes nothing to the archive,
    /// and the observation is applied only once a revision for that commit
    /// exists.
    fn remember_resolved_ref(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        reference: &str,
        commit: &str,
    ) {
        let mut resolved = self.resolved_refs.lock().expect("resolved refs poisoned");
        let key = (repo_type, repo_id.to_string(), reference.to_string());
        if resolved.len() >= MAX_RESOLVED_REFS && !resolved.contains_key(&key) {
            return;
        }
        resolved.insert(key, commit.to_string());
    }

    /// Records the names upstream gave this commit, for names the archive lacks.
    ///
    /// Only ever creates a ref the archive does not have. It never moves one, so
    /// a stale observation cannot walk a ref backwards and an explicit refresh
    /// (ADR-0012) stays the only thing that advances one. A published revision is
    /// never touched either way.
    fn adopt_resolved_refs(&self, repo_type: RepositoryType, repo_id: &str, commit: &str) {
        let candidates = {
            let resolved = self.resolved_refs.lock().expect("resolved refs poisoned");
            resolved
                .iter()
                .filter(|((entry_type, entry_repo, _), resolved_commit)| {
                    *entry_type == repo_type
                        && entry_repo == repo_id
                        && resolved_commit.as_str() == commit
                })
                .map(|((_, _, reference), _)| reference.clone())
                .collect::<Vec<_>>()
        };
        for reference in candidates {
            if self
                .archive
                .resolve_ref_for_type(repo_type, repo_id, &reference)
                .is_ok()
            {
                continue;
            }
            if let Err(error) = self
                .archive
                .update_ref_for_type(repo_type, repo_id, &reference, commit)
            {
                let _ = log_archive_failure(repo_type, repo_id, &reference, "update_ref", error);
            }
        }
    }

    /// Records the commit's upstream file list beside the revision it describes.
    ///
    /// Deliberately not fatal to an acquisition that already published: the
    /// revision is serving, the record adds no manifest entry and no published
    /// byte, and it is reconstructible from upstream. A failure leaves the
    /// revision in the "does not know its upstream file list" state, which is
    /// the state every revision published before this change is in.
    fn record_upstream_files(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        commit: &str,
        files: &[UpstreamFile],
    ) {
        if files.is_empty() {
            return;
        }
        match self
            .archive
            .record_upstream_files_for_type(repo_type, repo_id, commit, files)
        {
            Ok(true) => {
                tracing::info!(event = "upstream_file_list_recorded", repo_type = %repo_type, repo_id = %repo_id, commit = %commit, files = files.len(), "recorded the upstream file list of an immutable commit");
            }
            // Already recorded: the list of an immutable commit cannot change.
            Ok(false) => {}
            Err(error) => {
                let _ =
                    log_archive_failure(repo_type, repo_id, commit, "record_upstream_files", error);
            }
        }
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
                let handle = this.acquisitions.register(
                    repo_type,
                    &owned_repo_id,
                    &owned_reference,
                    &FileSelection::all(),
                    "refresh",
                );
                let observed = |event: FetchProgress| {
                    handle.observe(&event);
                    sink(event);
                };
                tracing::dispatcher::with_default(&dispatch, || {
                    this.refresh_once(
                        repo_type,
                        &owned_repo_id,
                        &owned_reference,
                        dry_run,
                        &observed,
                        &handle,
                    )
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

    #[allow(clippy::too_many_arguments)]
    fn refresh_once(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        reference: &str,
        dry_run: bool,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
        handle: &AcquisitionHandle,
    ) -> Result<RefreshResult, PullThroughError> {
        let previous = self
            .archive
            .resolve_ref_for_type(repo_type, repo_id, reference)
            .ok();
        let cancel = handle.cancellation();
        // A refresh transfers, so it holds a slot like any other transferring
        // acquisition; a dry run transfers too, because it must fetch to see
        // what the ref now resolves to.
        let (_permit, _waited) =
            self.enter_transfer_gate(repo_type, repo_id, reference, "refresh", progress, handle)?;
        if cancel.is_cancelled() {
            return Err(PullThroughError::Cancelled);
        }
        let staging = self
            .archive
            .acquire_fetch_staging_for_type(repo_type, repo_id, reference, &[])
            .map_err(|error| fetch_staging_failure(repo_type, repo_id, reference, error))?;
        progress(FetchProgress::phase(if staging.resumed {
            "resuming_snapshot"
        } else {
            "acquiring_snapshot"
        }));
        tracing::info!(event = "upstream_fetch_started", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %reference, resumed = staging.resumed, operation = "refresh", "upstream fetch started");
        let fetched = self
            .fetcher
            .fetch_cancellable(
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
                cancel,
            )
            .map_err(|error| {
                self.handle_fetch_failure(&staging.path, repo_type, repo_id, reference, &error);
                self.report_fetch_failure(repo_type, repo_id, reference, "refresh", &error, handle);
                PullThroughError::from(error)
            })?;
        tracing::info!(event = "upstream_fetch_finished", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %reference, commit = %fetched.commit, operation = "refresh", "upstream fetch finished");
        // Read while staging still exists: publication consumes it.
        let recorded_files =
            crate::staged_upstream_files(&staging.path, repo_type, &fetched.commit);
        if !cancel.commit() {
            if self
                .archive
                .preserve_fetch_staging(&staging.path)
                .is_ok_and(|preserved| preserved)
            {
                tracing::warn!(event = "incomplete_fetch_preserved", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %reference, "preserved interrupted upstream staging for retry");
            }
            log_acquisition_cancelled(&handle.entry);
            return Err(PullThroughError::Cancelled);
        }
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
        self.record_upstream_files(repo_type, repo_id, &fetched.commit, &recorded_files);
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
        handle: &AcquisitionHandle,
    ) -> Result<AcquisitionResult, PullThroughError> {
        self.fetch_and_publish_selection(
            repo_type,
            repo_id,
            requested_revision,
            selection,
            false,
            progress,
            handle,
        )
    }

    /// Transfers a selection and publishes what it produced.
    ///
    /// `reconcile` says this acquisition came from the selection-driven path, so
    /// if it had to wait for a transfer slot its selection is reconciled again
    /// before anything is transferred. That second look is where ADR-0021's
    /// saving actually happens: by the time a queued acquisition runs, the one
    /// ahead of it may have archived the paths they share, and only the remainder
    /// is worth the uplink.
    #[allow(clippy::too_many_arguments)]
    fn fetch_and_publish_selection(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        selection: &FileSelection,
        reconcile: bool,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
        handle: &AcquisitionHandle,
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
        let cancel = handle.cancellation();
        // This is the transferring invocation, so it is what the gate covers
        // (ADR-0021 decision 3). The slot is taken before staging is created, so
        // the number of partial snapshots on disk is bounded by the same limit.
        let (_permit, waited) = self.enter_transfer_gate(
            repo_type,
            repo_id,
            requested_revision,
            "pull_through",
            progress,
            handle,
        )?;
        if cancel.is_cancelled() {
            return Err(PullThroughError::Cancelled);
        }
        let narrowed;
        let selection = if reconcile && waited.is_some() {
            match self.reconcile_selection(
                repo_type,
                repo_id,
                requested_revision,
                selection,
                progress,
                handle,
            )? {
                Reconciliation::Satisfied(result) => return Ok(result),
                Reconciliation::Missing(remaining) => {
                    narrowed = remaining;
                    &narrowed
                }
                Reconciliation::NotPublished => selection,
            }
        } else {
            selection
        };
        handle.set_state(AcquisitionState::Transferring);
        // The normalized selection is part of the staging identity: staging
        // recorded under a different restricted selection is never adopted,
        // while staging left by an unrestricted acquisition is, because it
        // already covered these paths at this commit and resuming it is what
        // keeps an interrupted large fetch from starting over (ADR-0020
        // decision 5, ADR-0017). The acquisition still runs under this
        // request's selection, so only what the helper reports is published.
        let staging = self
            .archive
            .acquire_fetch_staging_for_type(
                repo_type,
                repo_id,
                requested_revision,
                &selection.identity(),
            )
            .map_err(|error| {
                fetch_staging_failure(repo_type, repo_id, requested_revision, error)
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
        let fetched = match self.fetcher.fetch_cancellable(&request, progress, cancel) {
            Ok(result) => result,
            Err(error) => {
                self.handle_fetch_failure(
                    &staging.path,
                    repo_type,
                    repo_id,
                    requested_revision,
                    &error,
                );
                self.report_fetch_failure(
                    repo_type,
                    repo_id,
                    requested_revision,
                    "pull_through",
                    &error,
                    handle,
                );
                return Err(error.into());
            }
        };
        tracing::info!(event = "upstream_fetch_finished", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, commit = %fetched.commit, operation = "pull_through", "upstream fetch finished");
        // Read while staging still exists: publication consumes it.
        let recorded_files =
            crate::staged_upstream_files(&staging.path, repo_type, &fetched.commit);
        // The one place cancellation and completion are decided against each
        // other: after this claim succeeds the acquisition publishes and a later
        // cancel is answered "already finished", and if it fails a cancel won and
        // nothing is published. Staging stays resumable either way, so the bytes
        // already transferred are not thrown away (ADR-0017).
        if !cancel.commit() {
            if self
                .archive
                .preserve_fetch_staging(&staging.path)
                .is_ok_and(|preserved| preserved)
            {
                tracing::warn!(event = "incomplete_fetch_preserved", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, "preserved interrupted upstream staging for retry");
            }
            log_acquisition_cancelled(&handle.entry);
            return Err(PullThroughError::Cancelled);
        }
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
        // The revision exists from here on, so the commit's upstream file list
        // has somewhere to live. It is recorded after publication rather than
        // staged with the payload, so no published revision is ever rewritten
        // and an interrupted acquisition cannot leave a list describing a
        // revision that was never published.
        self.record_upstream_files(repo_type, repo_id, &fetched.commit, &recorded_files);
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
        // A client that resolved a ref on the metadata route asks for every file
        // by commit, so this is where the archive learns the name it was asked
        // about.
        self.adopt_resolved_refs(repo_type, repo_id, &fetched.commit);
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
                // A cancellation is an interruption, not a deletion: the bytes it
                // already moved stay resumable so a later acquisition with the
                // same or a narrower selection adopts them (ADR-0017).
                | UpstreamError::Cancelled
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

    /// Waits for a transfer slot, reporting the wait so a management job stays
    /// `queued` and the in-flight views show what is blocked (ADR-0021).
    fn enter_transfer_gate(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        operation: &'static str,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
        handle: &AcquisitionHandle,
    ) -> Result<(TransferPermit<'_>, Option<Duration>), PullThroughError> {
        handle.set_state(AcquisitionState::Waiting);
        progress(FetchProgress::phase(TRANSFER_WAIT_PHASE));
        let view = self.in_flight_acquisitions();
        tracing::info!(event = "transfer_slot_waiting", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, operation, transferring = view.transferring, waiting = view.waiting, transfer_limit = view.transfer_limit, "acquisition is waiting for a transfer slot");
        let result = self
            .gate
            .acquire((repo_type, repo_id.to_string()), handle.cancellation());
        match result {
            Ok((permit, waited)) => {
                handle.set_state(AcquisitionState::Transferring);
                tracing::info!(event = "transfer_slot_admitted", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %requested_revision, operation, waited_ms = waited.map_or(0, |waited| waited.as_millis()), transfer_limit = self.gate.limit, "acquisition holds a transfer slot");
                Ok((permit, waited))
            }
            Err(error) => {
                log_acquisition_cancelled(&handle.entry);
                Err(error)
            }
        }
    }

    /// Reports a failed fetch, distinguishing an interruption from an upstream
    /// failure so a cancellation is never logged as one.
    fn report_fetch_failure(
        &self,
        repo_type: RepositoryType,
        repo_id: &str,
        requested_revision: &str,
        operation: &str,
        error: &UpstreamError,
        handle: &AcquisitionHandle,
    ) {
        if matches!(error, UpstreamError::Cancelled) {
            log_acquisition_cancelled(&handle.entry);
            return;
        }
        log_fetch_failure(repo_type, repo_id, requested_revision, operation, error);
    }
}

fn log_acquisition_cancelled(entry: &AcquisitionEntry) {
    let snapshot = entry.snapshot();
    tracing::warn!(
        event = "acquisition_cancelled",
        repo_type = %snapshot.repo_type,
        repo_id = %snapshot.repo_id,
        requested_revision = %snapshot.requested_revision,
        operation = snapshot.operation,
        acquisition_id = %snapshot.id,
        acquired_bytes = snapshot.transferred_bytes,
        "acquisition was cancelled on request"
    );
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
        // A class the helper established for itself (Issue 0084). A reported
        // class and the same class carried by a bare variant report the same
        // name, so the event's vocabulary does not depend on which path
        // produced it.
        UpstreamError::HelperFailure(failure) => failure.class().as_str(),
        // A cancelled acquisition is reported by `acquisition_cancelled`, not as
        // an upstream failure; this arm exists so the mapping stays total.
        UpstreamError::Cancelled => "cancelled",
    }
}

fn log_fetch_failure(
    repo_type: RepositoryType,
    repo_id: &str,
    requested_revision: &str,
    operation: &str,
    error: &UpstreamError,
) {
    // `safe_reason` is the sanitized reason the helper reported for itself
    // (Issue 0084), or ModelKeep's own description when the failure did not come
    // from one. Raw helper output still never reaches a log: helper stderr stays
    // discarded, and the reported diagnostic was sanitized in the helper, where
    // the exception and its context are known.
    tracing::warn!(
        event = "upstream_fetch_failed",
        repo_type = %repo_type,
        repo_id = %repo_id,
        requested_revision = %requested_revision,
        operation,
        error_class = upstream_error_class(error),
        safe_reason = %error.safe_reason(),
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
        ArchiveError::AlreadyPublished(published) => tracing::warn!(
            event = "archive_already_published",
            repo_type = %repo_type,
            repo_id = %repo_id,
            requested_revision = %requested_revision,
            operation,
            error_class = "conflict",
            // The directory's name is the commit that already exists, which is
            // the one thing `requested_revision` cannot tell an operator when a
            // mutable ref resolved onto an archived revision. The archive root
            // is left out, as `fetch_staging_conflict` leaves it out.
            published = %bounded_archive_detail(
                published
                    .file_name()
                    .map(|name| name.to_string_lossy())
                    .unwrap_or_default()
                    .as_ref()
            ),
            "archive revision is already published"
        ),
        ArchiveError::InvalidPath(offending) => tracing::warn!(
            event = "archive_unsafe_path",
            repo_type = %repo_type,
            repo_id = %repo_id,
            requested_revision = %requested_revision,
            operation,
            error_class = "unsafe_path",
            // Naming the path is the whole diagnosis, and it is also the one
            // field here built from text a request chose.
            unsafe_path = %bounded_archive_detail(offending),
            "archive path is not safe"
        ),
        ArchiveError::ReferencedRevision(references) => tracing::warn!(
            event = "archive_revision_referenced",
            repo_type = %repo_type,
            repo_id = %repo_id,
            requested_revision = %requested_revision,
            operation,
            error_class = "referenced",
            reference_count = references.len(),
            references = %bounded_archive_detail(&references.join(",")),
            "archive revision is still referenced"
        ),
    }
    error.into()
}

/// Answers a failed staging acquisition (Issue 0083).
///
/// A staging collision is already reported where it is detected, with the
/// `staging_conflict` class the archive gives it; nothing is logged twice here.
/// The request-facing answer stays at the coarser granularity the management API
/// defines for a job record, as it does for an upstream failure whose finer
/// class lives in `upstream_fetch_failed.error_class`.
fn fetch_staging_failure(
    repo_type: RepositoryType,
    repo_id: &str,
    requested_revision: &str,
    error: FetchStagingError,
) -> PullThroughError {
    match error {
        FetchStagingError::InFlight(_) => PullThroughError::StagingConflict,
        FetchStagingError::Archive(error) => {
            log_archive_failure(repo_type, repo_id, requested_revision, "stage", error)
        }
    }
}

impl From<UpstreamError> for PullThroughError {
    fn from(error: UpstreamError) -> Self {
        // This conversion is the one place a reason enters management state, and
        // it can only take the one `SanitizedReason::of` derives from the error
        // it is converting (Issue 0084).
        match &error {
            UpstreamError::Unavailable => Self::UpstreamUnavailable,
            UpstreamError::NotFound => Self::UpstreamNotFound,
            UpstreamError::Unauthorized => Self::UpstreamUnauthorized,
            UpstreamError::InvalidOutput(reason) => Self::UpstreamInvalidOutput(*reason),
            UpstreamError::Storage => Self::Storage,
            UpstreamError::Failed | UpstreamError::Io(_) => {
                Self::UpstreamFailed(Some(SanitizedReason::of(&error)))
            }
            // A reported class decides the request-facing answer (Issue 0084).
            // A class that already names what happened maps onto the answer that
            // already means it — rate limiting is an upstream that will not serve
            // this acquisition now, which is what `UpstreamUnavailable` means to a
            // client. The class that names nothing on its own is the one that
            // carries the reason.
            UpstreamError::HelperFailure(failure) => match failure.class() {
                HelperFailureClass::Unavailable | HelperFailureClass::RateLimited => {
                    Self::UpstreamUnavailable
                }
                HelperFailureClass::NotFound => Self::UpstreamNotFound,
                HelperFailureClass::Unauthorized => Self::UpstreamUnauthorized,
                HelperFailureClass::ClientFailure => {
                    Self::UpstreamFailed(Some(SanitizedReason::of(&error)))
                }
            },
            UpstreamError::Cancelled => Self::Cancelled,
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
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

    /// A fetcher whose callers all resolve to one commit.
    ///
    /// It used to rendezvous inside `fetch` on a barrier, which asserted that
    /// several acquisitions for one repository were transferring at once.
    /// ADR-0021 forbids exactly that, so the rendezvous moved outside the
    /// transfer: it waits until `expect_in_flight` acquisitions are registered,
    /// which they are before any of them can hold a transfer slot. Every
    /// assertion the affected tests make is unchanged; only the synchronisation
    /// that required simultaneous transfers is.
    struct AliasedFetcher {
        calls: Arc<AtomicUsize>,
        pull: Arc<std::sync::OnceLock<Arc<PullThrough>>>,
        expect_in_flight: usize,
        all_registered: std::sync::atomic::AtomicBool,
    }

    impl AliasedFetcher {
        fn shared(expect_in_flight: usize) -> (Arc<Self>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    calls: calls.clone(),
                    pull: Arc::new(std::sync::OnceLock::new()),
                    expect_in_flight,
                    all_registered: std::sync::atomic::AtomicBool::new(false),
                }),
                calls,
            )
        }

        /// Blocks the first transfer until every expected acquisition is
        /// registered in flight.
        ///
        /// Registration happens before an acquisition can be short-circuited by
        /// another one publishing, so this latch is what keeps the aliases'
        /// fetch count deterministic now that they transfer one at a time. It is
        /// one-shot: later transfers proceed immediately, because the earlier
        /// ones have already deregistered.
        fn await_registrations(&self) {
            if self.all_registered.load(Ordering::SeqCst) {
                return;
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let in_flight = self
                    .pull
                    .get()
                    .map_or(0, |pull| pull.in_flight_acquisitions().items.len());
                if in_flight >= self.expect_in_flight {
                    self.all_registered.store(true, Ordering::SeqCst);
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "only {in_flight} acquisition(s) registered; expected {}",
                    self.expect_in_flight
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
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
            self.await_registrations();
            self.calls.fetch_add(1, Ordering::SeqCst);
            fs::write(request.staging.join("config.json"), b"shared").unwrap();
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
        let (fetcher, calls) = AliasedFetcher::shared(2);
        let pull = Arc::new(PullThrough::new(archive.clone(), fetcher.clone()));
        assert!(fetcher.pull.set(pull.clone()).is_ok());
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
        let (fetcher, calls) = AliasedFetcher::shared(3);
        let pull = Arc::new(PullThrough::new(archive.clone(), fetcher.clone()));
        assert!(fetcher.pull.set(pull.clone()).is_ok());
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

    /// A fetcher that fails the test if an acquisition ever reaches upstream.
    struct UnreachedFetcher;

    impl UpstreamFetcher for UnreachedFetcher {
        fn fetch(&self, _request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            panic!("a refused acquisition must not reach upstream");
        }
    }

    /// Issue 0083: a running acquisition is still refused, and the refusal says
    /// staging rather than publication all the way out to the caller.
    #[test]
    fn a_live_staging_lease_is_refused_as_a_staging_collision() {
        let (writer, _guard) = capture_logs();
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        // The identity is held by an acquisition whose lease has not expired.
        let running = archive
            .acquire_fetch_staging("org/model", "main", &[])
            .unwrap();
        let pull = PullThrough::new(archive, Arc::new(UnreachedFetcher));

        let error = pull.ensure("org/model", "main", &[]).unwrap_err();

        assert_eq!(error, PullThroughError::StagingConflict);
        // Nothing was published and no revision of this repository exists.
        assert!(!error.to_string().contains("publi"), "{error}");
        let output = writer.output();
        assert!(output.contains("fetch_staging_conflict"));
        assert!(output.contains("staging_conflict"));
        assert!(!output.contains("archive_published"));
        // The refused request left the running acquisition's staging alone.
        assert!(running.path.is_dir());
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

    struct ReportedFailureFetcher(UpstreamError);

    impl UpstreamFetcher for ReportedFailureFetcher {
        fn fetch(&self, _request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            Err(match &self.0 {
                UpstreamError::HelperFailure(failure) => {
                    UpstreamError::HelperFailure(failure.clone())
                }
                UpstreamError::InvalidOutput(reason) => UpstreamError::InvalidOutput(*reason),
                other => panic!("unsupported fixture error {other:?}"),
            })
        }
    }

    /// Issue 0084: the class and the sanitized reason the helper established both
    /// reach the structured event, so the failure that took a day of
    /// investigation is legible in one log read.
    #[test]
    fn fetch_failure_event_carries_the_reported_class_and_reason() {
        let (writer, _guard) = capture_logs();
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let failure = crate::upstream::HelperFailure::reported(
            HelperFailureClass::ClientFailure,
            Some("PermissionError"),
            Some("PermissionError: [Errno 13] Permission denied: '/hf-home/hub'"),
        );
        let pull = PullThrough::new(
            archive,
            Arc::new(ReportedFailureFetcher(UpstreamError::HelperFailure(
                failure,
            ))),
        );

        let error = pull.ensure("org/model", "main", &[]).unwrap_err();
        // The reason survives into the error value, which is what the job record
        // is built from (Issue 0084).
        let PullThroughError::UpstreamFailed(Some(reason)) = &error else {
            panic!("the reason was dropped: {error:?}");
        };
        assert_eq!(
            reason.as_str(),
            "upstream client failure: PermissionError: [Errno 13] Permission denied: '/hf-home/hub'"
        );
        assert_eq!(error.to_string(), reason.as_str());

        let output = writer.output();
        assert!(output.contains("upstream_fetch_failed"), "{output}");
        assert!(
            output.contains(r#""error_class":"client_failure""#),
            "{output}"
        );
        assert!(!output.contains(r#""error_class":"failed""#), "{output}");
        assert!(
            output
                .contains("upstream client failure: PermissionError: [Errno 13] Permission denied"),
            "{output}"
        );
    }

    #[test]
    fn each_reported_class_is_recorded_under_its_own_name() {
        for (class, expected_name, expected_error) in [
            (
                HelperFailureClass::Unavailable,
                "unavailable",
                PullThroughError::UpstreamUnavailable,
            ),
            (
                HelperFailureClass::NotFound,
                "not_found",
                PullThroughError::UpstreamNotFound,
            ),
            (
                HelperFailureClass::Unauthorized,
                "unauthorized",
                PullThroughError::UpstreamUnauthorized,
            ),
            (
                HelperFailureClass::RateLimited,
                "rate_limited",
                PullThroughError::UpstreamUnavailable,
            ),
            (
                HelperFailureClass::ClientFailure,
                "client_failure",
                PullThroughError::UpstreamFailed(Some(SanitizedReason::of(
                    &UpstreamError::HelperFailure(crate::upstream::HelperFailure::from_class(
                        HelperFailureClass::ClientFailure,
                    )),
                ))),
            ),
        ] {
            let (writer, guard) = capture_logs();
            let root = tempfile::tempdir().unwrap();
            let archive = Archive::new(root.path()).unwrap();
            let pull = PullThrough::new(
                archive,
                Arc::new(ReportedFailureFetcher(UpstreamError::HelperFailure(
                    crate::upstream::HelperFailure::from_class(class),
                ))),
            );

            assert_eq!(pull.ensure("org/model", "main", &[]), Err(expected_error));

            let output = writer.output();
            assert!(
                output.contains(&format!(r#""error_class":"{expected_name}""#)),
                "{class:?} was not recorded as {expected_name}: {output}"
            );
            drop(guard);
        }
    }

    /// A helper that failed without reporting why is reported as the helper's own
    /// contract failure, which is a different thing an operator does a different
    /// thing about than an upstream failure whose class nobody established.
    #[test]
    fn a_helper_that_reported_nothing_is_a_contract_failure() {
        let (writer, _guard) = capture_logs();
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let pull = PullThrough::new(
            archive,
            Arc::new(ReportedFailureFetcher(UpstreamError::InvalidOutput(
                InvalidOutputReason::MissingFailureEvent,
            ))),
        );

        assert_eq!(
            pull.ensure("org/model", "main", &[]),
            Err(PullThroughError::UpstreamInvalidOutput(
                InvalidOutputReason::MissingFailureEvent
            ))
        );

        let output = writer.output();
        assert!(
            output.contains(r#""error_class":"invalid_output""#),
            "{output}"
        );
        assert!(
            output.contains("helper failed without reporting a failure event"),
            "{output}"
        );
        assert!(!output.contains("upstream acquisition failed"), "{output}");
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

    /// Every `ArchiveError` variant leaves a trace (Issue 0085).
    ///
    /// Driven through `log_archive_failure` directly rather than through a
    /// scenario per variant, because what is under test is that no variant
    /// reaches the function without being logged - and the `match` is now
    /// exhaustive, so a new variant cannot compile into silence.
    #[test]
    fn every_archive_failure_variant_is_logged_with_its_class() {
        let cases: Vec<(ArchiveError, &str, &str)> = vec![
            (
                ArchiveError::IntegrityMismatch("digest mismatch".into()),
                "archive_verification_failed",
                "integrity",
            ),
            (
                ArchiveError::Io(std::io::Error::other("disk gone")),
                "archive_storage_failed",
                "storage",
            ),
            (
                ArchiveError::AlreadyPublished(std::path::PathBuf::from(
                    "/archive/models/org/model/revisions/abc123",
                )),
                "archive_already_published",
                "conflict",
            ),
            (
                ArchiveError::InvalidPath("../escape".into()),
                "archive_unsafe_path",
                "unsafe_path",
            ),
            (
                ArchiveError::ReferencedRevision(vec!["main".into(), "dev".into()]),
                "archive_revision_referenced",
                "referenced",
            ),
        ];

        for (error, event, class) in cases {
            let (writer, _guard) = capture_logs();
            log_archive_failure(RepositoryType::Model, "org/model", "main", "publish", error);
            let output = writer.output();
            assert!(output.contains(event), "{event} missing from {output}");
            assert!(
                output.contains(&format!("\"error_class\":\"{class}\"")),
                "{class} missing from {output}"
            );
            assert!(
                output.contains("\"operation\":\"publish\""),
                "operation missing from {output}"
            );
            assert!(
                output.contains("org/model"),
                "repo_id missing from {output}"
            );
        }
    }

    /// The detail each new arm adds is what makes the log answer the question,
    /// so it is asserted rather than left to the event name.
    #[test]
    fn an_archive_failure_names_what_it_failed_on() {
        let (writer, _guard) = capture_logs();
        log_archive_failure(
            RepositoryType::Model,
            "org/model",
            "main",
            "publish",
            ArchiveError::AlreadyPublished(std::path::PathBuf::from(
                "/archive/models/org/model/revisions/abc123",
            )),
        );
        let output = writer.output();
        assert!(output.contains("\"published\":\"abc123\""), "{output}");
        // The archive root is not what an operator needs and is not reported.
        assert!(!output.contains("/archive/models"), "{output}");

        let (writer, _guard) = capture_logs();
        log_archive_failure(
            RepositoryType::Model,
            "org/model",
            "main",
            "publish",
            ArchiveError::InvalidPath("../escape".into()),
        );
        assert!(
            writer.output().contains("\"unsafe_path\":\"../escape\""),
            "{}",
            writer.output()
        );

        let (writer, _guard) = capture_logs();
        log_archive_failure(
            RepositoryType::Model,
            "org/model",
            "main",
            "delete",
            ArchiveError::ReferencedRevision(vec!["main".into(), "dev".into()]),
        );
        let output = writer.output();
        assert!(output.contains("\"references\":\"main,dev\""), "{output}");
        assert!(output.contains("\"reference_count\":2"), "{output}");
    }

    /// A crafted name cannot forge a log record or flood one (Issue 0085).
    #[test]
    fn archive_detail_is_bounded_and_single_line() {
        let (writer, _guard) = capture_logs();
        log_archive_failure(
            RepositoryType::Model,
            "org/model",
            "main",
            "publish",
            ArchiveError::InvalidPath(format!("a\nb\t{}", "x".repeat(400))),
        );
        let output = writer.output();
        assert!(output.contains("...[truncated]"), "{output}");
        // One record, and the injected newline did not start a second one.
        assert_eq!(output.lines().count(), 1, "{output}");
        assert!(!output.contains("a\\nb"), "{output}");
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
        /// Whether this helper reports upstream's per-file metadata, as the
        /// production helper does. Off by default, which is how a helper from
        /// before Issue 0074 behaves.
        reporting: bool,
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
                reporting: false,
            }
        }

        /// The same upstream, reported by a helper that carries the commit's
        /// per-file metadata.
        fn reporting(mut self) -> Self {
            self.reporting = true;
            self
        }

        /// Upstream's own per-file values for the whole commit.
        fn reported(&self) -> Vec<UpstreamFile> {
            self.upstream
                .iter()
                .enumerate()
                .map(|(index, (path, bytes))| UpstreamFile {
                    path: path.clone(),
                    size: Some(bytes.len() as u64),
                    blob_id: Some(format!("{:040x}", 0xb10b0000u64 + index as u64)),
                    lfs_sha256: None,
                })
                .collect()
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
            if self.reporting {
                crate::write_staged_upstream_files(
                    &request.staging,
                    request.repo_type,
                    &request.repo_id,
                    &self.commit,
                    &self.reported(),
                )
                .unwrap();
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

        fn repository_files(
            &self,
            _request: &InventoryRequest,
        ) -> Result<Option<crate::upstream::UpstreamRepositoryFiles>, UpstreamError> {
            if !self.reporting {
                return Ok(None);
            }
            self.inventories.fetch_add(1, Ordering::SeqCst);
            Ok(Some(crate::upstream::UpstreamRepositoryFiles {
                commit: self.commit.clone(),
                files: self.reported(),
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

    const RECORDED_COMMIT: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn recorded_upstream(archive: &Archive, commit: &str) -> Option<Vec<UpstreamFile>> {
        archive
            .upstream_files_for_type(RepositoryType::Model, "org/model", commit)
            .unwrap()
    }

    /// Issue 0074: reconciling a selection against a revision that knows its
    /// upstream file list is a local set operation, so it costs no upstream
    /// round trip. The control half of the test is the same sequence against a
    /// helper that reports no list, which still pays for one call per
    /// reconciliation.
    #[test]
    fn reconciliation_uses_the_recorded_file_list_instead_of_an_upstream_round_trip() {
        let upstream: [(&str, &[u8]); 3] = [
            ("config.json", b"config"),
            ("weights/a.bin", b"a"),
            ("weights/b.bin", b"b"),
        ];
        let requested = selection(&["config.json", "weights/a.bin"], &[]);

        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let fetcher = Arc::new(SelectiveFetcher::new(RECORDED_COMMIT, &upstream).reporting());
        let inventories = fetcher.inventories.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);

        // A single-file cold miss publishes the revision and records the list.
        pull.ensure("org/model", RECORDED_COMMIT, &["config.json".to_string()])
            .unwrap();
        assert!(recorded_upstream(&archive, RECORDED_COMMIT).is_some());
        let after_publication = inventories.load(Ordering::SeqCst);

        // The paths the revision lacks are found locally and acquired.
        let extended = pull
            .ensure_selected_for_type(
                RepositoryType::Model,
                "org/model",
                RECORDED_COMMIT,
                &requested,
            )
            .unwrap();
        assert_eq!(extended.outcome, AcquisitionOutcome::Extended);
        // Repeating it is answered locally as well.
        let again = pull
            .ensure_selected_for_type(
                RepositoryType::Model,
                "org/model",
                RECORDED_COMMIT,
                &requested,
            )
            .unwrap();
        assert_eq!(again.outcome, AcquisitionOutcome::AlreadyArchived);
        assert_eq!(inventories.load(Ordering::SeqCst), after_publication);
        assert!(archive
            .resolve_file("org/model", RECORDED_COMMIT, "weights/a.bin")
            .is_ok());
        assert!(archive
            .resolve_file("org/model", RECORDED_COMMIT, "weights/b.bin")
            .is_err());

        // Control: no recorded list, so each reconciliation asks upstream.
        let control_root = tempfile::tempdir().unwrap();
        let control_archive = Archive::new(control_root.path()).unwrap();
        let control_fetcher = Arc::new(SelectiveFetcher::new(RECORDED_COMMIT, &upstream));
        let control_inventories = control_fetcher.inventories.clone();
        let control = PullThrough::new(control_archive.clone(), control_fetcher);
        control
            .ensure("org/model", RECORDED_COMMIT, &["config.json".to_string()])
            .unwrap();
        assert!(recorded_upstream(&control_archive, RECORDED_COMMIT).is_none());
        for _ in 0..2 {
            control
                .ensure_selected_for_type(
                    RepositoryType::Model,
                    "org/model",
                    RECORDED_COMMIT,
                    &requested,
                )
                .unwrap();
        }
        assert_eq!(control_inventories.load(Ordering::SeqCst), 2);
    }

    /// The record is internal archive state: it carries no manifest entry, and
    /// every consistency check treats it as ModelKeep's own state rather than as
    /// an unexpected file in the revision.
    #[test]
    fn the_recorded_file_list_is_internal_state_and_not_a_manifest_entry() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let fetcher = Arc::new(
            SelectiveFetcher::new(
                RECORDED_COMMIT,
                &[("config.json", b"config"), ("weights/a.bin", b"a")],
            )
            .reporting(),
        );
        let pull = PullThrough::new(archive.clone(), fetcher);
        pull.ensure("org/model", RECORDED_COMMIT, &["config.json".to_string()])
            .unwrap();

        let revision = archive.revision_path("org/model", RECORDED_COMMIT).unwrap();
        assert!(revision.join(crate::UPSTREAM_FILES_FILE).is_file());
        let manifest: serde_json::Value =
            serde_json::from_str(&archive.manifest("org/model", RECORDED_COMMIT).unwrap()).unwrap();
        assert_eq!(
            manifest["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|file| file["path"].as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            ["config.json"]
        );
        // The revision still verifies: the record is not an unexpected file.
        assert_eq!(
            archive
                .verify_revision("org/model", RECORDED_COMMIT)
                .unwrap(),
            1
        );
        assert!(archive.audit().unwrap().failures.is_empty());
        let report = archive.self_check();
        assert_eq!(report.status(), "clean", "{report:?}");
        // The recorded list covers the whole commit, not the archived subset.
        assert_eq!(
            recorded_upstream(&archive, RECORDED_COMMIT)
                .unwrap()
                .into_iter()
                .map(|file| file.path)
                .collect::<Vec<_>>(),
            ["config.json", "weights/a.bin"]
        );
    }

    /// A mutable ref must still be resolved upstream even where a record exists:
    /// the record says what a commit contains, never which commit a ref names.
    #[test]
    fn a_mutable_ref_is_still_resolved_upstream_when_a_record_exists() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let fetcher = Arc::new(
            SelectiveFetcher::new(RECORDED_COMMIT, &[("config.json", b"config")]).reporting(),
        );
        let inventories = fetcher.inventories.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);
        pull.ensure("org/model", "main", &["config.json".to_string()])
            .unwrap();
        assert!(recorded_upstream(&archive, RECORDED_COMMIT).is_some());
        let before = inventories.load(Ordering::SeqCst);

        pull.ensure_selected_for_type(
            RepositoryType::Model,
            "org/model",
            "main",
            &selection(&["config.json"], &[]),
        )
        .unwrap();

        assert_eq!(inventories.load(Ordering::SeqCst), before + 1);
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

    /// Drives an unrestricted acquisition to a resumable checkpoint and loses
    /// it, which is what a crash during a metadata-route fetch leaves behind.
    fn interrupt_unrestricted_fetch(
        archive: &Archive,
        repo_id: &str,
        revision: &str,
        commit: &str,
    ) {
        let interrupted = PullThrough::new(
            archive.clone(),
            Arc::new(InterruptedSelectionFetcher {
                commit: commit.into(),
            }),
        );
        assert_eq!(
            interrupted.ensure_selected_for_type(
                RepositoryType::Model,
                repo_id,
                revision,
                &FileSelection::all(),
            ),
            Err(PullThroughError::UpstreamUnavailable)
        );
    }

    fn manifest_paths(archive: &Archive, repo_id: &str, commit: &str) -> Vec<String> {
        let manifest: serde_json::Value =
            serde_json::from_str(&archive.manifest(repo_id, commit).unwrap()).unwrap();
        manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap().to_string())
            .collect()
    }

    /// The crash-recovery path of ADR-0017: an interrupted unrestricted fetch
    /// leaves partial bytes, and the narrow request that follows resumes them
    /// instead of paying for the transfer again.
    #[test]
    fn unrestricted_staging_is_resumed_by_a_narrower_request() {
        let commit = "cccccccccccccccccccccccccccccccccccccccc";
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        interrupt_unrestricted_fetch(&archive, "org/model", "main", commit);

        let fetcher = Arc::new(SelectiveFetcher::new(
            commit,
            &[("a.bin", b"aaa"), ("b.bin", b"bbb")],
        ));
        let requests = fetcher.requests.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);
        pull.ensure_selected_for_type(
            RepositoryType::Model,
            "org/model",
            "main",
            &selection(&["a.bin"], &[]),
        )
        .unwrap();

        let recorded = requests.lock().unwrap();
        assert_eq!(recorded[0].resume_commit.as_deref(), Some(commit));
        // The acquisition still runs under the requesting selection.
        assert_eq!(recorded[0].files, vec!["a.bin".to_string()]);
    }

    /// Adopting wider staging must not widen what gets published: only the
    /// files the helper reported reach the manifest.
    #[test]
    fn a_resumed_unrestricted_staging_publishes_only_reported_files() {
        let commit = "cccccccccccccccccccccccccccccccccccccccc";
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        interrupt_unrestricted_fetch(&archive, "org/model", "main", commit);

        let fetcher = Arc::new(SelectiveFetcher::new(
            commit,
            &[("a.bin", b"aaa"), ("b.bin", b"bbb")],
        ));
        let requests = fetcher.requests.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);
        pull.ensure_selected_for_type(
            RepositoryType::Model,
            "org/model",
            "main",
            &selection(&["a.bin"], &[]),
        )
        .unwrap();

        assert_eq!(
            requests.lock().unwrap()[0].resume_commit.as_deref(),
            Some(commit)
        );
        assert_eq!(
            manifest_paths(&archive, "org/model", commit),
            vec!["a.bin".to_string()]
        );
        // The leftover the abandoned unrestricted fetch wrote is not published.
        assert!(archive
            .resolve_file("org/model", commit, "partial.bin")
            .is_err());
        assert!(archive.resolve_file("org/model", commit, "b.bin").is_err());
        assert!(archive.is_complete_revision("org/model", commit).unwrap());
        assert_eq!(archive.verify_revision("org/model", commit).unwrap(), 1);
    }

    /// Breadth is the only relaxed dimension: another repository or another
    /// requested revision is still a different staging identity.
    #[test]
    fn unrestricted_staging_for_another_identity_is_not_resumed() {
        let commit = "cccccccccccccccccccccccccccccccccccccccc";
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        interrupt_unrestricted_fetch(&archive, "org/other", "main", commit);
        interrupt_unrestricted_fetch(&archive, "org/model", "dev", commit);

        let fetcher = Arc::new(SelectiveFetcher::new(commit, &[("a.bin", b"aaa")]));
        let requests = fetcher.requests.clone();
        let pull = PullThrough::new(archive.clone(), fetcher);
        pull.ensure_selected_for_type(
            RepositoryType::Model,
            "org/model",
            "main",
            &selection(&["a.bin"], &[]),
        )
        .unwrap();
        assert_eq!(requests.lock().unwrap()[0].resume_commit, None);
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

    // ------------------------------------------------------------------
    // Issue 0076 (cancel a running acquisition) and Issue 0077 / ADR-0021
    // (serialize transferring acquisitions).
    // ------------------------------------------------------------------

    /// A rendezvous a fetcher waits on so a test can act mid-transfer.
    #[derive(Default)]
    struct Hold {
        state: Mutex<HoldState>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct HoldState {
        released: bool,
        arrived: usize,
    }

    impl Hold {
        /// Waits to be released. `false` means the acquisition was cancelled
        /// while it waited, which is how a fetcher that is mid-transfer notices.
        fn wait(&self, cancel: &Cancellation) -> bool {
            let mut state = self.state.lock().unwrap();
            state.arrived += 1;
            self.changed.notify_all();
            while !state.released {
                if cancel.is_cancelled() {
                    return false;
                }
                let (next, _timeout) = self
                    .changed
                    .wait_timeout(state, Duration::from_millis(10))
                    .unwrap();
                state = next;
            }
            true
        }

        fn arrived(&self) -> usize {
            self.state.lock().unwrap().arrived
        }

        fn await_arrival(&self, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if self.arrived() >= count {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "only {} fetch(es) reached the hold; expected {count}",
                    self.arrived()
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        fn release(&self) {
            self.state.lock().unwrap().released = true;
            self.changed.notify_all();
        }
    }

    /// A fetcher that measures the bytes it moves and can be interrupted.
    ///
    /// It writes the selected files one at a time and skips any that staging
    /// already holds, which is the supported client's `local_dir` resume
    /// behaviour that ADR-0017 relies on. `transferred` therefore counts only
    /// bytes that actually crossed the link, which is what the byte-level
    /// acceptance criteria of Issues 0076 and 0077 are measured against.
    struct MeasuredFetcher {
        commit: String,
        upstream: Vec<(String, Vec<u8>)>,
        transferred: Arc<AtomicU64>,
        transfers: Arc<Mutex<Vec<String>>>,
        requests: Arc<Mutex<Vec<RecordedFetch>>>,
        inventories: Arc<AtomicUsize>,
        /// After this many files of one fetch have moved, wait on the hold.
        hold_after: usize,
        hold: Option<Arc<Hold>>,
        /// Cancel the acquisition after the transfer is complete but before it
        /// returns, which puts the cancellation exactly in the window between a
        /// finished transfer and the publication claim.
        cancel_at_the_end: bool,
    }

    impl MeasuredFetcher {
        fn new(commit: &str, upstream: &[(&str, usize)]) -> Self {
            Self {
                commit: commit.into(),
                upstream: upstream
                    .iter()
                    .map(|(path, size)| ((*path).to_string(), vec![b'x'; *size]))
                    .collect(),
                transferred: Arc::new(AtomicU64::new(0)),
                transfers: Arc::new(Mutex::new(Vec::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
                inventories: Arc::new(AtomicUsize::new(0)),
                hold_after: 0,
                hold: None,
                cancel_at_the_end: false,
            }
        }

        fn holding(mut self, hold: &Arc<Hold>, hold_after: usize) -> Self {
            self.hold = Some(Arc::clone(hold));
            self.hold_after = hold_after;
            self
        }

        fn cancelling_at_the_end(mut self) -> Self {
            self.cancel_at_the_end = true;
            self
        }

        fn matches(path: &str, include: &[String], exclude: &[String]) -> bool {
            let hit = |pattern: &String| match pattern.strip_suffix('*') {
                Some(prefix) => path.starts_with(prefix),
                None => path == pattern.as_str(),
            };
            (include.is_empty() || include.iter().any(hit)) && !exclude.iter().any(hit)
        }

        fn transferred(&self) -> u64 {
            self.transferred.load(Ordering::SeqCst)
        }

        fn transfers_of(&self, path: &str) -> usize {
            self.transfers
                .lock()
                .unwrap()
                .iter()
                .filter(|moved| moved.as_str() == path)
                .count()
        }
    }

    impl UpstreamFetcher for MeasuredFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            self.fetch_cancellable(request, &|_| {}, &Cancellation::new())
        }

        fn fetch_cancellable(
            &self,
            request: &FetchRequest,
            progress: &(dyn Fn(FetchProgress) + Send + Sync),
            cancel: &Cancellation,
        ) -> Result<FetchedRevision, UpstreamError> {
            self.requests.lock().unwrap().push(RecordedFetch {
                files: request.files.clone(),
                exclude: request.exclude.clone(),
                resume_commit: request.resume_commit.clone(),
            });
            // The official helper records its resolved commit into staging; a
            // fetcher that skips this leaves staging unresumable (ADR-0017).
            crate::record_fetch_resolved_commit_for_type(
                &request.staging,
                request.repo_type,
                &request.repo_id,
                &request.revision,
                &request.selection_identity(),
                &self.commit,
            )
            .map_err(|_| UpstreamError::Storage)?;
            let mut files = Vec::new();
            let mut moved_files = 0usize;
            let mut moved_bytes = 0u64;
            for (path, bytes) in &self.upstream {
                if !Self::matches(path, &request.files, &request.exclude) {
                    continue;
                }
                files.push(path.clone());
                let destination = request.staging.join(path);
                if destination.exists() {
                    // Already in staging: a resumed download does not re-fetch it.
                    continue;
                }
                if cancel.is_cancelled() {
                    return Err(UpstreamError::Cancelled);
                }
                if let Some(hold) = &self.hold {
                    if moved_files == self.hold_after && !hold.wait(cancel) {
                        return Err(UpstreamError::Cancelled);
                    }
                }
                if cancel.is_cancelled() {
                    return Err(UpstreamError::Cancelled);
                }
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(destination, bytes).unwrap();
                moved_files += 1;
                moved_bytes += bytes.len() as u64;
                self.transferred
                    .fetch_add(bytes.len() as u64, Ordering::SeqCst);
                self.transfers.lock().unwrap().push(path.clone());
                progress(FetchProgress {
                    version: 1,
                    phase: "downloading".into(),
                    unit: Some("bytes".into()),
                    completed: Some(moved_bytes),
                    total: None,
                });
            }
            if files.is_empty() {
                return Err(UpstreamError::InvalidOutput(
                    InvalidOutputReason::EmptySnapshot,
                ));
            }
            if self.cancel_at_the_end {
                assert_eq!(cancel.cancel(), CancelOutcome::Cancelled);
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

    const MEASURED_COMMIT: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn await_in_flight(pull: &PullThrough, state: &str, count: usize) -> AcquisitionsView {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let view = pull.in_flight_acquisitions();
            if view.items.iter().filter(|item| item.state == state).count() >= count {
                return view;
            }
            assert!(
                Instant::now() < deadline,
                "expected {count} acquisition(s) in state {state}, saw {:?}",
                view.items
                    .iter()
                    .map(|item| (item.repo_id.clone(), item.state))
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn cancellation_and_completion_are_mutually_exclusive() {
        // The one decision point: whichever of the two claims it first wins, and
        // the other is told what happened rather than being hidden as an error.
        let completed = Cancellation::new();
        assert!(completed.commit());
        assert_eq!(completed.cancel(), CancelOutcome::AlreadyFinished);
        assert!(!completed.is_cancelled());

        let cancelled = Cancellation::new();
        assert_eq!(cancelled.cancel(), CancelOutcome::Cancelled);
        assert_eq!(cancelled.cancel(), CancelOutcome::AlreadyCancelled);
        assert!(!cancelled.commit());
        assert!(cancelled.is_cancelled());
    }

    #[test]
    fn a_client_driven_acquisition_is_listable_and_cancellable_with_waiters_attached() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let hold = Arc::new(Hold::default());
        let fetcher = Arc::new(
            MeasuredFetcher::new(MEASURED_COMMIT, &[("a.bin", 100), ("b.bin", 200)])
                .holding(&hold, 1),
        );
        let pull = Arc::new(PullThrough::new(
            archive.clone(),
            Arc::clone(&fetcher) as Arc<_>,
        ));

        // Two client requests for the same work share one acquisition, so one of
        // them is a waiter attached to the other's flight.
        let waiters = (0..2)
            .map(|_| {
                let pull = Arc::clone(&pull);
                std::thread::spawn(move || pull.ensure("org/model", "main", &[]))
            })
            .collect::<Vec<_>>();
        hold.await_arrival(1);
        let view = await_in_flight(&pull, "transferring", 1);
        assert_eq!(view.items.len(), 1, "one shared acquisition: {view:?}");
        let listed = view.items[0].clone();
        assert_eq!(listed.repo_id, "org/model");
        assert_eq!(listed.requested_revision, "main");
        assert!(listed.include.is_empty() && listed.exclude.is_empty());
        assert_eq!(listed.operation, "pull_through");
        assert_eq!(listed.transferred_bytes, 100);

        assert_eq!(
            pull.cancel_acquisition(&listed.id),
            Some(CancelOutcome::Cancelled)
        );

        // Every waiter is answered with the interruption rather than a hang, a
        // miss, or a success that delivers nothing.
        for waiter in waiters {
            assert_eq!(waiter.join().unwrap(), Err(PullThroughError::Cancelled));
        }
        assert_eq!(fetcher.requests.lock().unwrap().len(), 1);
        assert!(archive.list_revisions("org/model").unwrap().is_empty());
        assert!(pull.in_flight_acquisitions().items.is_empty());
        assert_eq!(pull.cancel_acquisition(&listed.id), None);

        // A later request starts a new acquisition, which is the documented
        // behaviour: cancellation is an interruption, not a cooldown.
        hold.release();
        assert_eq!(
            pull.ensure("org/model", "main", &[]).unwrap(),
            MEASURED_COMMIT
        );
    }

    #[test]
    fn a_cancelled_acquisition_publishes_nothing_and_leaves_resumable_staging() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let hold = Arc::new(Hold::default());
        let fetcher = Arc::new(
            MeasuredFetcher::new(MEASURED_COMMIT, &[("a.bin", 100), ("b.bin", 200)])
                .holding(&hold, 1),
        );
        let pull = Arc::new(PullThrough::new(archive.clone(), fetcher));
        let acquiring = {
            let pull = Arc::clone(&pull);
            std::thread::spawn(move || pull.ensure("org/model", "main", &[]))
        };
        hold.await_arrival(1);
        let view = await_in_flight(&pull, "transferring", 1);
        assert_eq!(
            pull.cancel_acquisition(&view.items[0].id),
            Some(CancelOutcome::Cancelled)
        );
        assert_eq!(acquiring.join().unwrap(), Err(PullThroughError::Cancelled));

        // Nothing partial is observable: no revision, no ref, no manifest.
        assert!(archive.list_revisions("org/model").unwrap().is_empty());
        assert!(archive.resolve_ref("org/model", "main").is_err());
        assert!(!archive
            .revision_path("org/model", MEASURED_COMMIT)
            .unwrap()
            .exists());
        // The bytes it did move are kept, as resumable staging rather than as
        // archive content (ADR-0017): a cancel is an interruption, not a delete.
        let staged = fs::read_dir(root.path().join("tmp"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("fetch-abandoned-"))
            .collect::<Vec<_>>();
        assert_eq!(
            staged.len(),
            1,
            "expected preserved staging, saw {staged:?}"
        );
        assert!(root
            .path()
            .join("tmp")
            .join(&staged[0])
            .join("a.bin")
            .exists());
    }

    #[test]
    fn a_resumed_acquisition_after_a_cancellation_transfers_fewer_bytes_than_a_fresh_one() {
        let upstream: &[(&str, usize)] = &[("a.bin", 1000), ("b.bin", 2000)];

        // The number a fresh start costs, measured rather than assumed.
        let fresh_root = tempfile::tempdir().unwrap();
        let fresh_archive = Archive::new(fresh_root.path()).unwrap();
        let fresh_fetcher = Arc::new(MeasuredFetcher::new(MEASURED_COMMIT, upstream));
        let fresh = PullThrough::new(fresh_archive, Arc::clone(&fresh_fetcher) as Arc<_>);
        assert_eq!(
            fresh.ensure("org/model", "main", &[]).unwrap(),
            MEASURED_COMMIT
        );
        let fresh_bytes = fresh_fetcher.transferred();
        assert_eq!(fresh_bytes, 3000);

        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let hold = Arc::new(Hold::default());
        let fetcher = Arc::new(MeasuredFetcher::new(MEASURED_COMMIT, upstream).holding(&hold, 1));
        let pull = Arc::new(PullThrough::new(
            archive.clone(),
            Arc::clone(&fetcher) as Arc<_>,
        ));
        let acquiring = {
            let pull = Arc::clone(&pull);
            std::thread::spawn(move || pull.ensure("org/model", "main", &[]))
        };
        hold.await_arrival(1);
        let view = await_in_flight(&pull, "transferring", 1);
        assert_eq!(
            pull.cancel_acquisition(&view.items[0].id),
            Some(CancelOutcome::Cancelled)
        );
        assert_eq!(acquiring.join().unwrap(), Err(PullThroughError::Cancelled));
        let cancelled_bytes = fetcher.transferred();
        assert_eq!(cancelled_bytes, 1000);

        // The retry adopts that staging and pays only for what is missing.
        hold.release();
        assert_eq!(
            pull.ensure("org/model", "main", &[]).unwrap(),
            MEASURED_COMMIT
        );
        let resumed_bytes = fetcher.transferred() - cancelled_bytes;
        assert_eq!(resumed_bytes, 2000);
        assert!(
            resumed_bytes < fresh_bytes,
            "resumed transfer {resumed_bytes} was not cheaper than a fresh {fresh_bytes}"
        );
        assert_eq!(
            fetcher.requests.lock().unwrap()[1].resume_commit.as_deref(),
            Some(MEASURED_COMMIT),
            "the retry did not resume the recorded commit"
        );
        assert_eq!(fetcher.transfers_of("a.bin"), 1);
        assert!(archive
            .is_complete_revision("org/model", MEASURED_COMMIT)
            .unwrap());
    }

    #[test]
    fn a_narrower_acquisition_after_a_cancellation_also_resumes_the_staging() {
        let upstream: &[(&str, usize)] = &[("a.bin", 1000), ("b.bin", 2000)];
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let hold = Arc::new(Hold::default());
        let fetcher = Arc::new(MeasuredFetcher::new(MEASURED_COMMIT, upstream).holding(&hold, 1));
        let pull = Arc::new(PullThrough::new(
            archive.clone(),
            Arc::clone(&fetcher) as Arc<_>,
        ));
        let acquiring = {
            let pull = Arc::clone(&pull);
            std::thread::spawn(move || pull.ensure("org/model", "main", &[]))
        };
        hold.await_arrival(1);
        let view = await_in_flight(&pull, "transferring", 1);
        assert_eq!(
            pull.cancel_acquisition(&view.items[0].id),
            Some(CancelOutcome::Cancelled)
        );
        assert_eq!(acquiring.join().unwrap(), Err(PullThroughError::Cancelled));
        assert_eq!(fetcher.transferred(), 1000);

        // A narrower selection adopts the unrestricted staging and needs nothing
        // more, because the one path it asks for is already there.
        hold.release();
        assert_eq!(
            pull.ensure("org/model", "main", &["a.bin".to_string()])
                .unwrap(),
            MEASURED_COMMIT
        );
        assert_eq!(fetcher.transferred(), 1000, "a resumed path was re-fetched");
    }

    #[test]
    fn a_cancellation_that_lands_after_the_transfer_still_publishes_nothing() {
        // The hard half of the race, made deterministic: the cancellation lands
        // in the window between a finished transfer and the publication claim.
        // `commit` is the only thing that decides it, so the acquisition must
        // publish nothing, report the interruption, and keep its bytes.
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let fetcher = Arc::new(
            MeasuredFetcher::new(MEASURED_COMMIT, &[("a.bin", 100)]).cancelling_at_the_end(),
        );
        let pull = PullThrough::new(archive.clone(), Arc::clone(&fetcher) as Arc<_>);
        assert_eq!(
            pull.ensure("org/model", "main", &[]),
            Err(PullThroughError::Cancelled)
        );
        assert_eq!(fetcher.transferred(), 100, "the transfer did not complete");
        assert!(archive.list_revisions("org/model").unwrap().is_empty());
        assert!(archive.resolve_ref("org/model", "main").is_err());
        assert!(!archive
            .revision_path("org/model", MEASURED_COMMIT)
            .unwrap()
            .exists());
        // The completed transfer is kept as resumable staging, not discarded.
        let staged = fs::read_dir(root.path().join("tmp"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("fetch-abandoned-"))
            .collect::<Vec<_>>();
        assert_eq!(
            staged.len(),
            1,
            "expected preserved staging, saw {staged:?}"
        );
    }

    #[test]
    fn a_cancel_that_races_completion_leaves_consistent_state() {
        // Every iteration ends in exactly one of two states: published and
        // completed, or reported cancelled and nothing published. Never both,
        // and never neither.
        for iteration in 0..24u64 {
            let root = tempfile::tempdir().unwrap();
            let archive = Archive::new(root.path()).unwrap();
            let fetcher = Arc::new(MeasuredFetcher::new(MEASURED_COMMIT, &[("a.bin", 64)]));
            let pull = Arc::new(PullThrough::new(archive.clone(), fetcher));
            let acquiring = {
                let pull = Arc::clone(&pull);
                std::thread::spawn(move || pull.ensure("org/model", "main", &[]))
            };
            // Sweep the cancel across the acquisition's whole lifetime so it
            // lands before, during, and after the publication claim.
            std::thread::sleep(Duration::from_micros(iteration * 120));
            let cancelled = pull
                .in_flight_acquisitions()
                .items
                .first()
                .and_then(|item| pull.cancel_acquisition(&item.id));
            let result = acquiring.join().unwrap();
            let published = archive
                .is_complete_revision("org/model", MEASURED_COMMIT)
                .unwrap_or(false);
            match result {
                Ok(commit) => {
                    assert_eq!(commit, MEASURED_COMMIT);
                    assert!(
                        published,
                        "iteration {iteration} completed without publishing"
                    );
                    assert_ne!(
                        cancelled,
                        Some(CancelOutcome::Cancelled),
                        "iteration {iteration} reported a cancellation and published"
                    );
                }
                Err(PullThroughError::Cancelled) => {
                    assert!(
                        !published,
                        "iteration {iteration} was cancelled and published anyway"
                    );
                }
                Err(other) => panic!("iteration {iteration} failed unexpectedly: {other:?}"),
            }
        }
    }

    #[test]
    fn cancelling_one_acquisition_leaves_another_repositorys_acquisition_alone() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let hold = Arc::new(Hold::default());
        let fetcher =
            Arc::new(MeasuredFetcher::new(MEASURED_COMMIT, &[("a.bin", 100)]).holding(&hold, 0));
        let pull = Arc::new(PullThrough::new(archive.clone(), fetcher));
        let threads = ["org/first", "org/second"].map(|repo_id| {
            let pull = Arc::clone(&pull);
            std::thread::spawn(move || (repo_id, pull.ensure(repo_id, "main", &[])))
        });
        hold.await_arrival(2);
        let view = await_in_flight(&pull, "transferring", 2);
        let doomed = view
            .items
            .iter()
            .find(|item| item.repo_id == "org/first")
            .expect("the first repository is in flight");
        assert_eq!(
            pull.cancel_acquisition(&doomed.id),
            Some(CancelOutcome::Cancelled)
        );
        hold.release();
        for thread in threads {
            let (repo_id, result) = thread.join().unwrap();
            if repo_id == "org/first" {
                assert_eq!(result, Err(PullThroughError::Cancelled));
            } else {
                assert_eq!(result.unwrap(), MEASURED_COMMIT);
            }
        }
        assert!(archive.list_revisions("org/first").unwrap().is_empty());
        assert!(archive
            .is_complete_revision("org/second", MEASURED_COMMIT)
            .unwrap());
    }

    #[test]
    fn a_second_acquisition_for_one_repository_waits_for_the_first_to_finish() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let hold = Arc::new(Hold::default());
        let fetcher = Arc::new(
            MeasuredFetcher::new(MEASURED_COMMIT, &[("a.bin", 100), ("b.bin", 200)])
                .holding(&hold, 0),
        );
        let pull = Arc::new(PullThrough::new(
            archive.clone(),
            Arc::clone(&fetcher) as Arc<_>,
        ));
        let first = {
            let pull = Arc::clone(&pull);
            std::thread::spawn(move || {
                pull.ensure_selected_for_type(
                    RepositoryType::Model,
                    "org/model",
                    "main",
                    &selection(&["a.bin"], &[]),
                )
            })
        };
        hold.await_arrival(1);
        await_in_flight(&pull, "transferring", 1);
        let second = {
            let pull = Arc::clone(&pull);
            std::thread::spawn(move || {
                pull.ensure_selected_for_type(
                    RepositoryType::Model,
                    "org/model",
                    "main",
                    &selection(&["b.bin"], &[]),
                )
            })
        };
        // The second acquisition is registered and visible, but it has not
        // started transferring: exactly one repository holds a slot.
        let view = await_in_flight(&pull, "waiting_for_transfer_slot", 1);
        assert_eq!(view.transferring, 1);
        assert_eq!(view.waiting, 1);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            hold.arrived(),
            1,
            "a second transfer started for one repository"
        );
        assert_eq!(fetcher.requests.lock().unwrap().len(), 1);

        hold.release();
        assert!(first.join().unwrap().unwrap().transferred());
        assert!(second.join().unwrap().unwrap().transferred());
        assert_eq!(fetcher.requests.lock().unwrap().len(), 2);
        assert_eq!(fetcher.transferred(), 300);
    }

    #[test]
    fn two_overlapping_selections_transfer_each_shared_file_once() {
        let upstream: &[(&str, usize)] = &[("a.bin", 100), ("shared.bin", 400), ("b.bin", 200)];

        // What a fresh acquisition of the second selection costs on its own.
        let fresh_root = tempfile::tempdir().unwrap();
        let fresh_fetcher = Arc::new(MeasuredFetcher::new(MEASURED_COMMIT, upstream));
        let fresh = PullThrough::new(
            Archive::new(fresh_root.path()).unwrap(),
            Arc::clone(&fresh_fetcher) as Arc<_>,
        );
        fresh
            .ensure_selected_for_type(
                RepositoryType::Model,
                "org/model",
                "main",
                &selection(&["b.bin", "shared.bin"], &[]),
            )
            .unwrap();
        let fresh_bytes = fresh_fetcher.transferred();
        assert_eq!(fresh_bytes, 600);

        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let hold = Arc::new(Hold::default());
        let fetcher = Arc::new(MeasuredFetcher::new(MEASURED_COMMIT, upstream).holding(&hold, 0));
        let pull = Arc::new(PullThrough::new(
            archive.clone(),
            Arc::clone(&fetcher) as Arc<_>,
        ));
        let first = {
            let pull = Arc::clone(&pull);
            std::thread::spawn(move || {
                pull.ensure_selected_for_type(
                    RepositoryType::Model,
                    "org/model",
                    "main",
                    &selection(&["a.bin", "shared.bin"], &[]),
                )
            })
        };
        hold.await_arrival(1);
        await_in_flight(&pull, "transferring", 1);
        let second = {
            let pull = Arc::clone(&pull);
            std::thread::spawn(move || {
                pull.ensure_selected_for_type(
                    RepositoryType::Model,
                    "org/model",
                    "main",
                    &selection(&["b.bin", "shared.bin"], &[]),
                )
            })
        };
        await_in_flight(&pull, "waiting_for_transfer_slot", 1);
        hold.release();
        assert_eq!(
            first.join().unwrap().unwrap().outcome,
            AcquisitionOutcome::Published
        );
        assert_eq!(
            second.join().unwrap().unwrap().outcome,
            AcquisitionOutcome::Extended
        );

        // Measured, not asserted: the shared file crossed the link once, and the
        // second acquisition paid only for what the archive did not hold.
        assert_eq!(fetcher.transfers_of("shared.bin"), 1);
        assert_eq!(fetcher.transferred(), 700);
        let second_bytes = fetcher.transferred() - 500;
        assert_eq!(second_bytes, 200);
        assert!(
            second_bytes < fresh_bytes,
            "the second selection cost {second_bytes}, not less than a fresh {fresh_bytes}"
        );
    }

    #[test]
    fn different_repositories_run_concurrently_up_to_the_limit() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let hold = Arc::new(Hold::default());
        let fetcher =
            Arc::new(MeasuredFetcher::new(MEASURED_COMMIT, &[("a.bin", 10)]).holding(&hold, 0));
        let pull = Arc::new(PullThrough::new(archive, fetcher));
        assert_eq!(pull.transfer_limit(), DEFAULT_MAX_TRANSFERRING_ACQUISITIONS);
        let threads = ["org/one", "org/two", "org/three"].map(|repo_id| {
            let pull = Arc::clone(&pull);
            std::thread::spawn(move || pull.ensure(repo_id, "main", &[]))
        });
        // Two transfer, the third waits: the default limit is two.
        hold.await_arrival(2);
        let view = await_in_flight(&pull, "waiting_for_transfer_slot", 1);
        assert_eq!(view.transfer_limit, 2);
        assert_eq!(view.transferring, 2);
        assert_eq!(view.waiting, 1);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            hold.arrived(),
            2,
            "a third repository transferred past the limit"
        );
        hold.release();
        for thread in threads {
            assert_eq!(thread.join().unwrap().unwrap(), MEASURED_COMMIT);
        }
    }

    #[test]
    fn a_limit_of_one_makes_a_second_repository_wait() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let hold = Arc::new(Hold::default());
        let fetcher =
            Arc::new(MeasuredFetcher::new(MEASURED_COMMIT, &[("a.bin", 10)]).holding(&hold, 0));
        let pull = Arc::new(PullThrough::with_transfer_limit(archive, fetcher, 1));
        assert_eq!(pull.transfer_limit(), 1);
        let threads = ["org/one", "org/two"].map(|repo_id| {
            let pull = Arc::clone(&pull);
            std::thread::spawn(move || pull.ensure(repo_id, "main", &[]))
        });
        hold.await_arrival(1);
        let view = await_in_flight(&pull, "waiting_for_transfer_slot", 1);
        assert_eq!(view.transferring, 1);
        assert_eq!(view.waiting, 1);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            hold.arrived(),
            1,
            "a second repository transferred under a limit of one"
        );
        hold.release();
        for thread in threads {
            assert_eq!(thread.join().unwrap().unwrap(), MEASURED_COMMIT);
        }
    }

    #[test]
    fn a_resolve_only_reconciliation_is_not_blocked_by_a_running_transfer() {
        // The sharpest risk in ADR-0021: if the gate covered metadata calls, the
        // Issue 0070 reconciliation an acquisition makes about its own repository
        // would wait for a slot that is already held, and deadlock. The limit is
        // one so no slot is available at all while the transfer runs.
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let fetcher = Arc::new(MeasuredFetcher::new(
            MEASURED_COMMIT,
            &[("a.bin", 100), ("b.bin", 200)],
        ));
        let seeded =
            PullThrough::with_transfer_limit(archive.clone(), Arc::clone(&fetcher) as Arc<_>, 1);
        // Archive the revision first, so the reconciliation path is the one under
        // test rather than a first publication.
        seeded
            .ensure_selected_for_type(
                RepositoryType::Model,
                "org/model",
                "main",
                &selection(&["a.bin"], &[]),
            )
            .unwrap();
        assert!(archive
            .is_complete_revision("org/model", MEASURED_COMMIT)
            .unwrap());

        // A transfer for the same repository now holds the only slot.
        let blocking = Arc::new(Hold::default());
        let holding_fetcher = Arc::new(
            MeasuredFetcher::new(MEASURED_COMMIT, &[("a.bin", 100), ("b.bin", 200)])
                .holding(&blocking, 0),
        );
        let gated = Arc::new(PullThrough::with_transfer_limit(
            archive.clone(),
            Arc::clone(&holding_fetcher) as Arc<_>,
            1,
        ));
        let transferring = {
            let gated = Arc::clone(&gated);
            std::thread::spawn(move || {
                gated.ensure_selected_for_type(
                    RepositoryType::Model,
                    "org/model",
                    "main",
                    &selection(&["b.bin"], &[]),
                )
            })
        };
        blocking.await_arrival(1);
        await_in_flight(&gated, "transferring", 1);

        // The reconciliation resolves against upstream and answers from the
        // archive while that transfer still holds the slot.
        let inventories_before = holding_fetcher.inventories.load(Ordering::SeqCst);
        let started = Instant::now();
        let reconciled = gated
            .ensure_selected_for_type(
                RepositoryType::Model,
                "org/model",
                "main",
                &selection(&["a.bin"], &[]),
            )
            .unwrap();
        assert_eq!(reconciled.outcome, AcquisitionOutcome::AlreadyArchived);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the reconciliation waited {:?} behind the gate",
            started.elapsed()
        );
        assert!(
            holding_fetcher.inventories.load(Ordering::SeqCst) > inventories_before,
            "the reconciliation did not make its resolve-only call"
        );
        assert_eq!(
            gated.in_flight_acquisitions().transferring,
            1,
            "the running transfer was disturbed"
        );
        blocking.release();
        assert!(transferring.join().unwrap().unwrap().transferred());
    }

    #[test]
    fn identical_requests_are_collapsed_by_single_flight_rather_than_serialized() {
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let hold = Arc::new(Hold::default());
        let fetcher =
            Arc::new(MeasuredFetcher::new(MEASURED_COMMIT, &[("a.bin", 100)]).holding(&hold, 0));
        let pull = Arc::new(PullThrough::with_transfer_limit(
            archive,
            Arc::clone(&fetcher) as Arc<_>,
            1,
        ));
        let callers = (0..4)
            .map(|_| {
                let pull = Arc::clone(&pull);
                std::thread::spawn(move || {
                    pull.ensure_selected_for_type(
                        RepositoryType::Model,
                        "org/model",
                        "main",
                        &selection(&["a.bin"], &[]),
                    )
                })
            })
            .collect::<Vec<_>>();
        hold.await_arrival(1);
        let view = await_in_flight(&pull, "transferring", 1);
        // One acquisition, holding the one slot, with nothing queued behind it:
        // identical work is still collapsed and never queues behind itself, even
        // with a limit of one.
        assert_eq!(view.items.len(), 1, "identical requests were not collapsed");
        assert_eq!(view.waiting, 0);
        hold.release();
        for caller in callers {
            assert_eq!(caller.join().unwrap().unwrap().commit, MEASURED_COMMIT);
        }
        assert_eq!(fetcher.requests.lock().unwrap().len(), 1);
        assert_eq!(fetcher.transferred(), 100);
    }

    #[test]
    fn a_cancelled_refresh_is_reported_and_publishes_nothing() {
        let (_root, archive) = published_archive();
        let hold = Arc::new(Hold::default());
        let fetcher = Arc::new(
            MeasuredFetcher::new(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &[("a.bin", 100)],
            )
            .holding(&hold, 0),
        );
        let pull = Arc::new(PullThrough::new(archive.clone(), fetcher));
        let refreshing = {
            let pull = Arc::clone(&pull);
            std::thread::spawn(move || pull.refresh("org/model", "main", false))
        };
        hold.await_arrival(1);
        let view = await_in_flight(&pull, "transferring", 1);
        assert_eq!(view.items[0].operation, "refresh");
        assert_eq!(
            pull.cancel_acquisition(&view.items[0].id),
            Some(CancelOutcome::Cancelled)
        );
        assert_eq!(refreshing.join().unwrap(), Err(PullThroughError::Cancelled));
        // The old revision and the ref are untouched.
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
    fn a_cancelled_acquisition_is_reported_as_an_interruption_not_an_upstream_failure() {
        let (writer, _guard) = capture_logs();
        let root = tempfile::tempdir().unwrap();
        let archive = Archive::new(root.path()).unwrap();
        let hold = Arc::new(Hold::default());
        let fetcher =
            Arc::new(MeasuredFetcher::new(MEASURED_COMMIT, &[("a.bin", 100)]).holding(&hold, 0));
        let pull = Arc::new(PullThrough::new(archive, fetcher));
        // The acquisition inherits the starting caller's log destination, so this
        // thread is the one that must start it for the captured subscriber to see
        // the events; the cancellation comes from another thread, as it does in
        // the deployment.
        let cancelling = {
            let pull = Arc::clone(&pull);
            let hold = Arc::clone(&hold);
            std::thread::spawn(move || {
                hold.await_arrival(1);
                let view = await_in_flight(&pull, "transferring", 1);
                pull.cancel_acquisition(&view.items[0].id)
            })
        };
        assert_eq!(
            pull.ensure("org/model", "main", &[]),
            Err(PullThroughError::Cancelled)
        );
        assert_eq!(cancelling.join().unwrap(), Some(CancelOutcome::Cancelled));
        let output = writer.output();
        assert!(
            output.contains("\"event\":\"acquisition_cancelled\""),
            "missing acquisition_cancelled: {output}"
        );
        assert!(
            !output.contains("\"event\":\"upstream_fetch_failed\""),
            "a cancellation was reported as an upstream failure: {output}"
        );
        assert!(output.contains("\"event\":\"incomplete_fetch_preserved\""));
        assert!(output.contains("\"event\":\"transfer_slot_waiting\""));
        assert!(output.contains("\"event\":\"transfer_slot_admitted\""));
    }

    #[test]
    fn the_transfer_limit_setting_defaults_to_two_and_rejects_unusable_values() {
        assert_eq!(
            max_transferring_acquisitions_from_value(Err(std::env::VarError::NotPresent)).unwrap(),
            2
        );
        assert_eq!(
            max_transferring_acquisitions_from_value(Ok(" 5 ".into())).unwrap(),
            5
        );
        for rejected in ["0", "-1", "two", ""] {
            assert!(
                max_transferring_acquisitions_from_value(Ok(rejected.into())).is_err(),
                "{rejected:?} was accepted as a transfer limit"
            );
        }
    }
}
