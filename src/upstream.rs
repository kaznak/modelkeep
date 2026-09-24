use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use serde::Deserialize;

use crate::{is_hf_commit, record_fetch_resolved_commit_for_type, RepositoryType, UpstreamFile};

/// What a cancellation request found (Issue 0076).
///
/// Cancelling something that has already finished is reported as such rather
/// than as an error, because "it was already done" and "there is no such
/// acquisition" are different operational answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// This request stopped a running acquisition.
    Cancelled,
    /// The acquisition was already cancelled.
    AlreadyCancelled,
    /// The acquisition had already claimed its publication point, so it
    /// finishes and publishes. Nothing was cancelled and nothing is partial.
    AlreadyFinished,
}

impl CancelOutcome {
    /// The stable name the Admin API and the admin UI report.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::AlreadyCancelled => "already_cancelled",
            Self::AlreadyFinished => "already_finished",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum CancellationPhase {
    #[default]
    Running,
    Cancelled,
    Committing,
}

#[derive(Debug, Default)]
struct CancellationState {
    phase: CancellationPhase,
    child: Option<Child>,
}

/// The token that stops one acquisition, and the owner of its helper process.
///
/// The token owns the running helper because stopping an acquisition means
/// stopping that process: [`Child::kill`] needs `&mut Child`, so the handle
/// lives behind this lock rather than only on the acquisition thread, which is
/// normally blocked reading the helper's pipe. Whoever cancels kills **and
/// reaps** the child under the lock, so no helper is left orphaned and no
/// zombie is left behind when the acquisition thread is woken by the resulting
/// end of file.
///
/// `commit` is the single point where cancellation and completion are decided
/// against each other: exactly one of them wins, so an acquisition is either
/// published or recorded cancelled, never both and never neither.
#[derive(Debug, Default)]
pub struct Cancellation {
    state: Mutex<CancellationState>,
}

/// The helper handed back to the acquisition thread once its output ends.
pub enum ReclaimedChild {
    /// Still owned by this acquisition; it must be waited for as usual.
    Running(Child),
    /// A cancellation already stopped and reaped the helper.
    Reaped,
}

impl Cancellation {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, CancellationState> {
        self.state.lock().expect("cancellation lock poisoned")
    }

    pub fn is_cancelled(&self) -> bool {
        self.state().phase == CancellationPhase::Cancelled
    }

    /// Stops the acquisition, killing and reaping its helper if one is running.
    pub fn cancel(&self) -> CancelOutcome {
        let mut state = self.state();
        match state.phase {
            CancellationPhase::Committing => CancelOutcome::AlreadyFinished,
            CancellationPhase::Cancelled => CancelOutcome::AlreadyCancelled,
            CancellationPhase::Running => {
                state.phase = CancellationPhase::Cancelled;
                if let Some(mut child) = state.child.take() {
                    terminate_and_reap(&mut child);
                }
                CancelOutcome::Cancelled
            }
        }
    }

    /// Claims the right to publish. `false` means a cancellation won the race,
    /// so this acquisition must publish nothing.
    pub fn commit(&self) -> bool {
        let mut state = self.state();
        if state.phase == CancellationPhase::Running {
            state.phase = CancellationPhase::Committing;
            return true;
        }
        false
    }

    /// Hands a freshly spawned helper to the token.
    ///
    /// `false` means the acquisition was cancelled before the helper started;
    /// the child has already been stopped and reaped, so the caller must not
    /// wait for it.
    fn attach(&self, child: Child) -> bool {
        let mut state = self.state();
        if state.phase == CancellationPhase::Cancelled {
            let mut child = child;
            terminate_and_reap(&mut child);
            return false;
        }
        state.child = Some(child);
        true
    }

    /// Takes the helper back once its output has ended.
    fn reclaim(&self) -> ReclaimedChild {
        let mut state = self.state();
        match state.child.take() {
            Some(child) => ReclaimedChild::Running(child),
            None => ReclaimedChild::Reaped,
        }
    }
}

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

/// Asks upstream which paths a selection covers, without transferring any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryRequest {
    pub repo_type: RepositoryType,
    pub repo_id: String,
    pub revision: String,
    pub files: Vec<String>,
    pub exclude: Vec<String>,
}

/// Upstream's answer to an [`InventoryRequest`].
///
/// Unlike an acquisition, an empty file list is a legitimate answer: it means
/// upstream holds nothing matching the selection at this revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionInventory {
    pub commit: String,
    pub files: Vec<String>,
}

/// Every file upstream holds at one immutable commit, with its per-file
/// metadata, obtained without transferring anything.
///
/// Unlike [`RevisionInventory::files`], no selection narrows this: the contents
/// of a commit are a fact about the commit, which is why ModelKeep can record
/// the list and answer from it later (Issue 0074).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRepositoryFiles {
    pub commit: String,
    pub files: Vec<UpstreamFile>,
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

/// The version of the helper failure event this parent understands.
const FAILURE_EVENT_VERSION: u32 = 1;

/// The longest reported diagnostic ModelKeep keeps, in characters.
///
/// The helper bounds its own message; this bound is what makes the parent's
/// state independent of a helper that does not.
const REPORTED_DETAIL_LIMIT: usize = 400;

/// The class of failure the fetch helper reported for itself (Issue 0084).
///
/// The class is the helper's answer, not something the parent infers: only the
/// helper holds the client exception and the context needed to place it. Each
/// class is one an operator acts on differently, which is why there is no class
/// here for a distinction nothing branches on. A failure the helper could not
/// place is reported as [`Self::ClientFailure`] together with the exception
/// type, which is honest, rather than guessed into one of the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperFailureClass {
    /// Upstream could not be reached or could not answer.
    Unavailable,
    /// Upstream answered that the repository, revision or file does not exist.
    NotFound,
    /// Upstream refused the credentials, or the repository is gated for them.
    Unauthorized,
    /// Upstream refused because it was asked too often.
    RateLimited,
    /// The transfer or the official client itself failed, including a failure
    /// the helper could not classify.
    ClientFailure,
}

impl HelperFailureClass {
    pub const ALL: [Self; 5] = [
        Self::Unavailable,
        Self::NotFound,
        Self::Unauthorized,
        Self::RateLimited,
        Self::ClientFailure,
    ];

    /// The stable name the helper protocol and the structured events use.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::NotFound => "not_found",
            Self::Unauthorized => "unauthorized",
            Self::RateLimited => "rate_limited",
            Self::ClientFailure => "client_failure",
        }
    }

    /// ModelKeep's own description of the class, used when the helper reported
    /// no diagnostic of its own.
    const fn safe_reason(self) -> &'static str {
        match self {
            Self::Unavailable => "upstream unavailable",
            Self::NotFound => "upstream repository or revision not found",
            Self::Unauthorized => "upstream authorization failed",
            Self::RateLimited => "upstream rate limited the acquisition",
            Self::ClientFailure => "upstream client failure",
        }
    }

    /// The class a reported name denotes, or `None` for a name this parent does
    /// not implement. An unknown class is a helper contract failure rather than
    /// a class silently treated as something else.
    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == value)
    }
}

/// A failure the fetch helper classified and sanitized for itself.
///
/// The diagnostic text originates in the helper because that is where the
/// exception and its context are known; ModelKeep does not re-derive that
/// judgement by pattern matching text it did not raise. What the parent does is
/// bound the report structurally as untrusted input: unprintable characters are
/// removed so a report cannot forge a log record, whitespace is collapsed so it
/// cannot span lines, and the length is capped so it cannot flood them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperFailure {
    class: HelperFailureClass,
    exception: Option<String>,
    message: Option<String>,
}

impl HelperFailure {
    /// A class with no diagnostic of its own: what an exit code alone reports.
    pub fn from_class(class: HelperFailureClass) -> Self {
        Self {
            class,
            exception: None,
            message: None,
        }
    }

    /// The helper's own report, bounded as untrusted input.
    pub fn reported(
        class: HelperFailureClass,
        exception: Option<&str>,
        message: Option<&str>,
    ) -> Self {
        Self {
            class,
            exception: exception.and_then(bounded_detail),
            message: message.and_then(bounded_detail),
        }
    }

    pub fn class(&self) -> HelperFailureClass {
        self.class
    }

    /// The exception type the helper named, when it named one.
    pub fn exception(&self) -> Option<&str> {
        self.exception.as_deref()
    }

    /// The helper's sanitized diagnostic, when it reported one.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// The one-line reason an operator reads: ModelKeep's description of the
    /// class, plus whatever the helper was able to say about this failure.
    pub fn safe_reason(&self) -> String {
        let class = self.class.safe_reason();
        match (self.message.as_deref(), self.exception.as_deref()) {
            (Some(message), _) => format!("{class}: {message}"),
            (None, Some(exception)) => format!("{class}: {exception}"),
            (None, None) => class.to_string(),
        }
    }
}

/// One reported string reduced to a bounded, single-line, printable form.
///
/// This is deliberately not a search for credentials. Recognizing a secret
/// requires the context the helper has and the parent does not, so the helper
/// sanitizes; the parent only refuses to store something unbounded or something
/// that could forge a log record.
fn bounded_detail(value: &str) -> Option<String> {
    let printable: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let collapsed = printable
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(REPORTED_DETAIL_LIMIT)
        .collect::<String>();
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
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
    /// The helper reported why it failed (Issue 0084).
    ///
    /// This carries the class the helper established and the diagnostic it
    /// sanitized. It is what replaced the bucket that used to turn every
    /// unclassified helper exit into [`Self::Failed`]; the bare variants above
    /// remain the vocabulary of fetchers that are not the official helper, and
    /// of the exit codes the helper contract defined before this event existed.
    HelperFailure(HelperFailure),
    /// The acquisition was stopped on request (Issue 0076). This is an
    /// interruption, not an upstream failure and never a miss.
    Cancelled,
}

impl UpstreamError {
    /// The credential-safe one-line reason for a structured event.
    ///
    /// An I/O failure is reported by kind rather than by message: the kind is
    /// what an operator acts on, and the message would carry local paths that
    /// no event needs.
    pub fn safe_reason(&self) -> String {
        match self {
            Self::Io(error) => format!("upstream I/O error: {:?}", error.kind()),
            Self::HelperFailure(failure) => failure.safe_reason(),
            other => other.to_string(),
        }
    }
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
    /// The helper failed without reporting a failure event (Issue 0084).
    ///
    /// A helper that fails owes the parent a reason on its protocol channel.
    /// Reporting the missing reason as the helper's own contract failure is what
    /// keeps it from being mistaken for an upstream failure whose class nobody
    /// established.
    MissingFailureEvent,
    /// The helper reported a failure event this parent cannot read, including
    /// one naming a class it does not implement or announcing a version it does
    /// not understand.
    MalformedFailure,
}

impl InvalidOutputReason {
    pub const ALL: [Self; 15] = [
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
        Self::MissingFailureEvent,
        Self::MalformedFailure,
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
            Self::MissingFailureEvent => "helper failed without reporting a failure event",
            Self::MalformedFailure => "helper emitted a malformed failure event",
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
            Self::HelperFailure(failure) => formatter.write_str(&failure.safe_reason()),
            Self::Cancelled => write!(formatter, "upstream acquisition cancelled"),
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

    /// A transferring fetch that can be stopped while it runs (Issue 0076).
    ///
    /// The default implementation ignores the token, which keeps every existing
    /// fetcher valid: a fetcher that cannot be interrupted still runs to its
    /// own end, and the caller observes the cancellation at the next boundary
    /// instead of mid-transfer.
    fn fetch_cancellable(
        &self,
        request: &FetchRequest,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
        _cancel: &Cancellation,
    ) -> Result<FetchedRevision, UpstreamError> {
        self.fetch_with_progress(request, progress)
    }

    /// Resolves the commit and the upstream paths a selection covers, without
    /// transferring anything.
    ///
    /// `Ok(None)` means this fetcher cannot enumerate upstream. The caller then
    /// acquires with the selection exactly as given instead of reconciling an
    /// already published revision against it.
    fn inventory(
        &self,
        _request: &InventoryRequest,
    ) -> Result<Option<RevisionInventory>, UpstreamError> {
        Ok(None)
    }

    /// Resolves the commit and upstream's per-file metadata for every file it
    /// holds there, without transferring anything (Issue 0074).
    ///
    /// This is what answers repository metadata for a revision the archive has
    /// never seen, so that a client's own file filter can narrow the first
    /// acquisition. `Ok(None)` means this fetcher cannot report upstream's
    /// per-file metadata; the caller then acquires the revision and answers from
    /// the archive, because reporting a repository as empty because it could not
    /// be enumerated would be worse.
    fn repository_files(
        &self,
        _request: &InventoryRequest,
    ) -> Result<Option<UpstreamRepositoryFiles>, UpstreamError> {
        Ok(None)
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
        self.run_fetch(request, progress, &Cancellation::new())
    }

    fn fetch_cancellable(
        &self,
        request: &FetchRequest,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
        cancel: &Cancellation,
    ) -> Result<FetchedRevision, UpstreamError> {
        self.run_fetch(request, progress, cancel)
    }

    fn inventory(
        &self,
        request: &InventoryRequest,
    ) -> Result<Option<RevisionInventory>, UpstreamError> {
        let resolved = self.run_inventory(request)?;
        // An empty list is a legitimate answer here: it means upstream holds
        // nothing matching this selection, not that a transfer produced nothing.
        Ok(Some(RevisionInventory {
            commit: resolved.commit,
            files: resolved.files,
        }))
    }

    fn repository_files(
        &self,
        request: &InventoryRequest,
    ) -> Result<Option<UpstreamRepositoryFiles>, UpstreamError> {
        let resolved = self.run_inventory(request)?;
        // A helper that reported no per-file metadata cannot answer metadata;
        // saying so is not the same as saying the repository is empty.
        if resolved.repository_files.is_empty() {
            return Ok(None);
        }
        Ok(Some(UpstreamRepositoryFiles {
            commit: resolved.commit,
            files: resolved.repository_files,
        }))
    }
}

impl OfficialHfFetcher {
    fn run_fetch(
        &self,
        request: &FetchRequest,
        progress: &(dyn Fn(FetchProgress) + Send + Sync),
        cancel: &Cancellation,
    ) -> Result<FetchedRevision, UpstreamError> {
        if cancel.is_cancelled() {
            return Err(UpstreamError::Cancelled);
        }
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
        // From here the token owns the helper, so a cancellation arriving while
        // this thread is blocked on the pipe stops and reaps it rather than
        // leaving it transferring with nobody waiting.
        if !cancel.attach(child) {
            return Err(UpstreamError::Cancelled);
        }
        let parsed = (|| {
            let mut result = None;
            let mut failure = None;
            for line in BufReader::new(stdout).lines() {
                let line = line.map_err(UpstreamError::Io)?;
                let value: serde_json::Value = serde_json::from_str(&line)
                    .map_err(|_| UpstreamError::InvalidOutput(InvalidOutputReason::NonJsonLine))?;
                match value.get("type").and_then(|value| value.as_str()) {
                    // The helper's own reason for failing (Issue 0084). Reading
                    // it does not end the loop: the pipe is drained to end of
                    // file as always, because a helper that is still writing
                    // must never be left blocked on it.
                    Some("failure") => {
                        let reported = parse_failure_event(value)?;
                        // The first reported failure wins, so a later one cannot
                        // overwrite the reason the acquisition actually failed
                        // for.
                        failure = failure.or(Some(reported));
                    }
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
            Ok((result, failure))
        })();
        // The helper's output ended. Either a cancellation already reaped it, or
        // this thread owns it again and is responsible for reaping it.
        let ReclaimedChild::Running(mut child) = cancel.reclaim() else {
            return Err(UpstreamError::Cancelled);
        };
        if cancel.is_cancelled() {
            terminate_and_reap(&mut child);
            return Err(UpstreamError::Cancelled);
        }
        let (result, failure) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                terminate_and_reap(&mut child);
                return Err(error);
            }
        };
        let status = child.wait().map_err(UpstreamError::Io)?;
        // A failure the helper reported for itself is authoritative and fails
        // closed: a helper that said it failed did not succeed, whatever it then
        // exited with, and its own reason is better than any the exit code
        // carries.
        if let Some(failure) = failure {
            return Err(UpstreamError::HelperFailure(failure));
        }
        if !status.success() {
            return Err(helper_exit_failure(status.code()));
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
        // The commit's upstream file list is recorded into staging, from where
        // the acquisition installs it beside the revision it describes. Failing
        // to write it leaves the revision without a recorded file list, which is
        // the state of every revision published before this existed; it is not a
        // reason to fail an acquisition whose payload is complete.
        let response = response.sanitized();
        let _ = crate::write_staged_upstream_files(
            &request.staging,
            request.repo_type,
            &request.repo_id,
            &response.commit,
            &response.repository_files,
        );
        Ok(FetchedRevision {
            commit: response.commit,
            files: response.files,
            staging: request.staging.clone(),
        })
    }

    /// A resolve-only invocation. It transfers nothing, is never gated
    /// (ADR-0021 decision 3), and owns no staging, so it needs no cancellation
    /// token: it is the call a running acquisition may make about itself.
    fn run_inventory(&self, request: &InventoryRequest) -> Result<HelperOutput, UpstreamError> {
        let mut command = Command::new(&self.python);
        command
            .arg(&self.helper)
            .arg("--repo-type")
            .arg(request.repo_type.to_string())
            .arg("--repo-id")
            .arg(&request.repo_id)
            .arg("--revision")
            .arg(&request.revision)
            .arg("--resolve-only")
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
            let mut failure = None;
            for line in BufReader::new(stdout).lines() {
                let line = line.map_err(UpstreamError::Io)?;
                let value: serde_json::Value = serde_json::from_str(&line)
                    .map_err(|_| UpstreamError::InvalidOutput(InvalidOutputReason::NonJsonLine))?;
                match value.get("type").and_then(|value| value.as_str()) {
                    // A resolve-only call owns no staging, so there is nothing
                    // to record from a resolved event and nothing to report
                    // from a progress event; the result event is authoritative.
                    Some("progress" | "resolved") => continue,
                    // A resolve-only invocation fails for the same reasons an
                    // acquisition does, so it reports them the same way.
                    Some("failure") => {
                        let reported = parse_failure_event(value)?;
                        failure = failure.or(Some(reported));
                    }
                    Some("result") => {
                        result =
                            Some(serde_json::from_value::<HelperOutput>(value).map_err(|_| {
                                UpstreamError::InvalidOutput(InvalidOutputReason::MalformedResult)
                            })?);
                    }
                    None => continue,
                    _ => {
                        return Err(UpstreamError::InvalidOutput(
                            InvalidOutputReason::UnsupportedEventType,
                        ))
                    }
                }
            }
            Ok((result, failure))
        })();
        let (result, failure) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                terminate_and_reap(&mut child);
                return Err(error);
            }
        };
        let status = child.wait().map_err(UpstreamError::Io)?;
        if let Some(failure) = failure {
            return Err(UpstreamError::HelperFailure(failure));
        }
        if !status.success() {
            return Err(helper_exit_failure(status.code()));
        }
        let response: HelperOutput = result.ok_or(UpstreamError::InvalidOutput(
            InvalidOutputReason::MissingResult,
        ))?;
        if !is_hf_commit(&response.commit) {
            return Err(UpstreamError::InvalidOutput(
                InvalidOutputReason::MalformedResultCommit,
            ));
        }
        Ok(response.sanitized())
    }
}

fn terminate_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The failure event as the helper writes it (Issue 0084).
#[derive(Debug, Deserialize)]
struct HelperFailureEvent {
    #[serde(default)]
    version: u32,
    class: String,
    #[serde(default)]
    exception: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Reads one reported failure, or rejects the helper's contract.
///
/// An unreadable event, an unimplemented class and an unsupported version are
/// all the same operational answer — the helper must be fixed — so they share
/// one reason rather than multiplying classes nothing branches on.
fn parse_failure_event(value: serde_json::Value) -> Result<HelperFailure, UpstreamError> {
    let malformed = || UpstreamError::InvalidOutput(InvalidOutputReason::MalformedFailure);
    let event: HelperFailureEvent = serde_json::from_value(value).map_err(|_| malformed())?;
    if event.version > FAILURE_EVENT_VERSION {
        return Err(malformed());
    }
    let class = HelperFailureClass::parse(&event.class).ok_or_else(malformed)?;
    Ok(HelperFailure::reported(
        class,
        event.exception.as_deref(),
        event.message.as_deref(),
    ))
}

/// What a non-zero helper exit means when no failure event explained it.
///
/// The three codes the helper contract defined before the failure event existed
/// still name their class. Anything else is a helper that failed without saying
/// why — including one killed outright — and that is reported as the helper's
/// own contract failure rather than as an upstream failure nobody classified.
fn helper_exit_failure(code: Option<i32>) -> UpstreamError {
    match code {
        Some(10) => UpstreamError::Unavailable,
        Some(11) => UpstreamError::NotFound,
        Some(12) => UpstreamError::Unauthorized,
        Some(13) => {
            UpstreamError::HelperFailure(HelperFailure::from_class(HelperFailureClass::RateLimited))
        }
        _ => UpstreamError::InvalidOutput(InvalidOutputReason::MissingFailureEvent),
    }
}

#[derive(Debug, Deserialize)]
struct HelperOutput {
    #[serde(rename = "type")]
    _kind: Option<String>,
    commit: String,
    files: Vec<String>,
    /// The commit's whole upstream file list. A helper that does not report one
    /// leaves this empty, and ModelKeep records nothing rather than recording an
    /// empty repository.
    #[serde(default)]
    repository_files: Vec<UpstreamFile>,
}

impl HelperOutput {
    /// This output with every reported file validated as untrusted input.
    fn sanitized(self) -> Self {
        Self {
            repository_files: self
                .repository_files
                .into_iter()
                .filter_map(UpstreamFile::sanitized)
                .collect(),
            ..self
        }
    }
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

    fn run_resolve_only(
        result_line: &str,
        arguments: &std::path::Path,
    ) -> Option<RevisionInventory> {
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("helper.sh");
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\nfor argument in \"$@\"; do echo \"$argument\" >> '{}'; done\n{result_line}\n",
                arguments.display()
            ),
        )
        .unwrap();
        OfficialHfFetcher {
            python: "sh".into(),
            helper,
        }
        .inventory(&InventoryRequest {
            repo_type: RepositoryType::Model,
            repo_id: "public/model".into(),
            revision: "main".into(),
            files: vec!["q4/".into()],
            exclude: vec!["q4/b.gguf".into()],
        })
        .unwrap()
    }

    #[test]
    fn resolve_only_helper_invocation_returns_the_selection_inventory() {
        let commit = "a".repeat(40);
        let directory = tempfile::tempdir().unwrap();
        let arguments = directory.path().join("arguments");
        let script = format!(
            "echo '{{\"type\":\"resolved\",\"version\":1,\"commit\":\"{commit}\"}}'\necho '{{\"type\":\"result\",\"commit\":\"{commit}\",\"files\":[\"q4/a.gguf\"],\"sizes\":{{\"q4/a.gguf\":3}}}}'"
        );

        let inventory = run_resolve_only(&script, &arguments).unwrap();

        assert_eq!(inventory.commit, commit);
        assert_eq!(inventory.files, vec!["q4/a.gguf"]);
        let recorded = fs::read_to_string(arguments).unwrap();
        let recorded = recorded.lines().collect::<Vec<_>>();
        assert!(recorded.contains(&"--resolve-only"), "{recorded:?}");
        assert!(recorded.windows(2).any(|pair| pair == ["--file", "q4/"]));
        assert!(recorded
            .windows(2)
            .any(|pair| pair == ["--exclude", "q4/b.gguf"]));
        // A resolve-only call owns no staging directory.
        assert!(!recorded.contains(&"--output"), "{recorded:?}");
    }

    #[test]
    fn resolve_only_accepts_a_selection_that_matches_nothing_upstream() {
        let commit = "a".repeat(40);
        let directory = tempfile::tempdir().unwrap();
        let arguments = directory.path().join("arguments");
        let script = format!("echo '{{\"type\":\"result\",\"commit\":\"{commit}\",\"files\":[]}}'");

        let inventory = run_resolve_only(&script, &arguments).unwrap();

        // Unlike an acquisition, this is an answer rather than an empty snapshot.
        assert_eq!(inventory.commit, commit);
        assert!(inventory.files.is_empty());
    }

    #[test]
    fn a_fetcher_without_inventory_support_reports_no_inventory() {
        struct PlainFetcher;

        impl UpstreamFetcher for PlainFetcher {
            fn fetch(&self, _request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
                Err(UpstreamError::Unavailable)
            }
        }

        assert_eq!(
            PlainFetcher
                .inventory(&InventoryRequest {
                    repo_type: RepositoryType::Model,
                    repo_id: "public/model".into(),
                    revision: "main".into(),
                    files: Vec::new(),
                    exclude: Vec::new(),
                })
                .unwrap(),
            None
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
        assert_eq!(InvalidOutputReason::ALL.len(), 15);
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
            // Issue 0084: a helper that fails without reporting why, which is
            // what every unclassified exit used to be reported as.
            ("exit 1".into(), None, InvalidOutputReason::MissingFailureEvent),
            (
                "kill -9 $$".into(),
                None,
                InvalidOutputReason::MissingFailureEvent,
            ),
            (
                "echo '{\"type\":\"failure\",\"class\":\"invented\"}'".into(),
                None,
                InvalidOutputReason::MalformedFailure,
            ),
            (
                "echo '{\"type\":\"failure\",\"message\":\"no class\"}'".into(),
                None,
                InvalidOutputReason::MalformedFailure,
            ),
            (
                "echo '{\"type\":\"failure\",\"version\":2,\"class\":\"unavailable\"}'".into(),
                None,
                InvalidOutputReason::MalformedFailure,
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

    /// Issue 0084. Every class the helper can report survives the process
    /// boundary with the reason the helper sanitized, including the
    /// `client_failure` that a permission failure inside the transfer is — the
    /// shape of the Issue 0086 incident, which the parent used to reduce to
    /// `UpstreamError::Failed`.
    #[test]
    fn helper_reported_failure_classes_are_kept_with_their_reason() {
        for (class, exit_code) in [
            (HelperFailureClass::Unavailable, 10),
            (HelperFailureClass::NotFound, 11),
            (HelperFailureClass::Unauthorized, 12),
            (HelperFailureClass::RateLimited, 13),
            (HelperFailureClass::ClientFailure, 14),
        ] {
            let script = format!(
                "echo '{{\"type\":\"failure\",\"version\":1,\"class\":\"{}\",\"exception\":\"PermissionError\",\"message\":\"PermissionError: [Errno 13] Permission denied\"}}'\nexit {exit_code}",
                class.as_str()
            );
            let error = run_helper(&script, None, |_| {});
            let UpstreamError::HelperFailure(failure) = &error else {
                panic!("expected a reported failure for {class:?}, got {error:?}");
            };
            assert_eq!(failure.class(), class);
            assert_eq!(failure.exception(), Some("PermissionError"));
            assert_eq!(
                failure.message(),
                Some("PermissionError: [Errno 13] Permission denied")
            );
            assert!(
                error.to_string().contains("Permission denied"),
                "reason was lost: {error}"
            );
            assert_eq!(error.safe_reason(), error.to_string());
        }
    }

    #[test]
    fn reported_failure_is_authoritative_over_a_successful_result() {
        let commit = "a".repeat(40);
        let script = format!(
            "echo '{{\"type\":\"result\",\"commit\":\"{commit}\",\"files\":[\"config.json\"]}}'\necho '{{\"type\":\"failure\",\"version\":1,\"class\":\"client_failure\",\"exception\":\"RuntimeError\",\"message\":\"RuntimeError: transfer failed\"}}'\nexit 0"
        );
        let error = run_helper(&script, None, |_| {});
        let UpstreamError::HelperFailure(failure) = &error else {
            panic!("a reported failure must fail closed, got {error:?}");
        };
        assert_eq!(failure.class(), HelperFailureClass::ClientFailure);
    }

    #[test]
    fn the_first_reported_failure_is_the_one_kept() {
        let script = "echo '{\"type\":\"failure\",\"class\":\"unauthorized\",\"message\":\"first\"}'\necho '{\"type\":\"failure\",\"class\":\"client_failure\",\"message\":\"second\"}'\nexit 12";
        let error = run_helper(script, None, |_| {});
        let UpstreamError::HelperFailure(failure) = &error else {
            panic!("expected a reported failure, got {error:?}");
        };
        assert_eq!(failure.class(), HelperFailureClass::Unauthorized);
        assert_eq!(failure.message(), Some("first"));
    }

    #[test]
    fn rate_limiting_reported_only_by_exit_code_keeps_its_class() {
        let error = run_helper("exit 13", None, |_| {});
        let UpstreamError::HelperFailure(failure) = &error else {
            panic!("expected a reported failure, got {error:?}");
        };
        assert_eq!(failure.class(), HelperFailureClass::RateLimited);
        assert_eq!(failure.message(), None);
        assert_eq!(error.safe_reason(), "upstream rate limited the acquisition");
    }

    #[test]
    fn resolve_only_failure_is_reported_with_its_class() {
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("helper.sh");
        fs::write(
            &helper,
            "#!/bin/sh\necho '{\"type\":\"failure\",\"version\":1,\"class\":\"rate_limited\",\"exception\":\"HfHubHTTPError\",\"message\":\"HfHubHTTPError: 429 Too Many Requests\"}'\nexit 13\n",
        )
        .unwrap();
        let error = OfficialHfFetcher {
            python: "sh".into(),
            helper,
        }
        .inventory(&InventoryRequest {
            repo_type: RepositoryType::Model,
            repo_id: "public/model".into(),
            revision: "main".into(),
            files: Vec::new(),
            exclude: Vec::new(),
        })
        .unwrap_err();
        let UpstreamError::HelperFailure(failure) = &error else {
            panic!("expected a reported failure, got {error:?}");
        };
        assert_eq!(failure.class(), HelperFailureClass::RateLimited);
        assert!(error.to_string().contains("429"));
    }

    /// The helper sanitizes credentials, because only it knows the exception and
    /// its context. What the parent guarantees is that a report cannot be
    /// unbounded and cannot forge a log record.
    #[test]
    fn reported_failure_detail_is_bounded_as_untrusted_input() {
        let failure = HelperFailure::reported(
            HelperFailureClass::ClientFailure,
            Some("Runtime\tError"),
            Some(&format!(
                "first line\n{{\"event\":\"archive_published\"}} {}",
                "x".repeat(4096)
            )),
        );
        assert_eq!(failure.exception(), Some("Runtime Error"));
        let message = failure.message().unwrap();
        assert_eq!(message.chars().count(), REPORTED_DETAIL_LIMIT);
        assert!(!message.contains('\n'));
        assert!(message.starts_with("first line {\"event\":\"archive_published\"} x"));
        assert_eq!(
            HelperFailure::reported(HelperFailureClass::ClientFailure, Some("  "), Some("\n\t"))
                .message(),
            None
        );
        assert_eq!(
            HelperFailure::reported(HelperFailureClass::ClientFailure, None, None).safe_reason(),
            "upstream client failure"
        );
    }

    #[test]
    fn helper_failure_classes_are_stable_and_credential_safe() {
        assert_eq!(HelperFailureClass::ALL.len(), 5);
        let mut names = std::collections::BTreeSet::new();
        for class in HelperFailureClass::ALL {
            assert!(names.insert(class.as_str()), "duplicate class {class:?}");
            assert_eq!(HelperFailureClass::parse(class.as_str()), Some(class));
            let reason = HelperFailure::from_class(class).safe_reason();
            assert_eq!(reason, class.safe_reason());
            assert!(!reason.is_empty());
            assert!(!reason.contains("://"));
            assert!(!reason.contains('\n'));
        }
        // The bucket this replaced is deliberately not a reportable class.
        assert_eq!(HelperFailureClass::parse("failed"), None);
        assert_eq!(HelperFailureClass::parse(""), None);
    }

    #[test]
    fn io_failures_are_reported_by_kind_without_their_path() {
        let error = UpstreamError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "/archive/.modelkeep-fetch/private-path",
        ));
        assert_eq!(error.safe_reason(), "upstream I/O error: PermissionDenied");
        assert!(!error.safe_reason().contains("private-path"));
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
