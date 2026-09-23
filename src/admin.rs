use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::SocketAddr,
    path::PathBuf,
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use sha2::{Digest, Sha256};

use crate::upstream::{FetchProgress, FileSelection};
use crate::{
    pullthrough::{AcquisitionOutcome, PullThrough, PullThroughError},
    validate_repository_id, validate_revision_ref, Archive, ArchiveError, ArchiveResult,
    RepositorySummary, RepositoryType, SelfCheckState,
};

const ADMIN_CAPABILITY: &str = "io.modelkeep/cap/admin";
static JOB_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct Config {
    pub address: SocketAddr,
    bearer_token: Option<String>,
    trust_tailscale_headers: bool,
}

fn validate_job_id(id: &str) -> ArchiveResult<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(ArchiveError::InvalidPath(id.into()));
    }
    Ok(())
}

fn validate_digest(value: &str) -> ArchiveResult<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ArchiveError::IntegrityMismatch(
            "invalid job index digest".into(),
        ));
    }
    Ok(())
}

fn hex_encode(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hex_decode(value: &str) -> Option<String> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
}

fn history_key(job: &Job) -> String {
    format!("{:020}-{}", job.created_at, hex_encode(&job.id))
}

fn job_id_from_history_key(key: &str) -> Option<String> {
    let (timestamp, encoded) = key.split_once('-')?;
    if timestamp.len() != 20 || !timestamp.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let id = hex_decode(encoded)?;
    validate_job_id(&id).ok()?;
    Some(id)
}

fn write_new_json(path: &std::path::Path, value: &impl Serialize) -> ArchiveResult<()> {
    let temporary = path.with_extension(format!("{}.tmp", new_job_id(unix_timestamp())));
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    match fs::hard_link(&temporary, path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
    }
    fs::remove_file(temporary)?;
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

impl Config {
    fn auth_methods(&self) -> Vec<&'static str> {
        let mut methods = Vec::new();
        if self.trust_tailscale_headers {
            methods.push("tailscale");
        }
        if self.bearer_token.is_some() {
            methods.push("bearer");
        }
        methods
    }

    pub fn from_env() -> Result<Option<Self>, String> {
        let Some(address) = env::var("MODELKEEP_ADMIN_ADDRESS").ok() else {
            return Ok(None);
        };
        let address = address
            .parse::<SocketAddr>()
            .map_err(|error| format!("invalid MODELKEEP_ADMIN_ADDRESS: {error}"))?;
        let bearer_token = env::var("MODELKEEP_ADMIN_TOKEN")
            .ok()
            .filter(|value| !value.is_empty());
        let trust_tailscale_headers = env::var("MODELKEEP_TRUST_TAILSCALE_HEADERS")
            .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
        if bearer_token.is_none() && !trust_tailscale_headers {
            return Err(
                "management listener requires MODELKEEP_ADMIN_TOKEN or trusted Tailscale headers"
                    .into(),
            );
        }
        Ok(Some(Self {
            address,
            bearer_token,
            trust_tailscale_headers,
        }))
    }

    #[cfg(test)]
    fn token(address: SocketAddr, token: &str) -> Self {
        Self {
            address,
            bearer_token: Some(token.into()),
            trust_tailscale_headers: false,
        }
    }
}

#[derive(Clone)]
struct AdminState {
    archive: Arc<Archive>,
    config: Config,
    pullthrough: Option<Arc<PullThrough>>,
    jobs: JobManager,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
}

#[derive(Debug, Serialize)]
struct StatusBody {
    version: &'static str,
    ready: bool,
    pullthrough_enabled: bool,
    repository_count: usize,
    model_repository_count: usize,
    dataset_repository_count: usize,
    logical_archive_bytes: u64,
    archive_filesystem_path: String,
    archive_filesystem_total_bytes: u64,
    archive_filesystem_available_bytes: u64,
    archive_filesystem_available_percent: u8,
    archive_filesystem_low_space: bool,
    self_check: SelfCheckSummary,
    principal: PrincipalView,
    auth_methods: Vec<&'static str>,
}

/// Bounded summary of the startup archive self-check.
///
/// `status` distinguishes "never checked" from "checked and clean", which is
/// the point of reporting a zero-findings result at all. The findings
/// themselves stay in the structured event stream; only their counts by class
/// appear here so the route stays a fixed-size status answer.
#[derive(Debug, Serialize)]
struct SelfCheckSummary {
    status: &'static str,
    completed_at: u64,
    duration_ms: u64,
    repositories_checked: usize,
    revisions_checked: usize,
    files_checked: usize,
    refs_checked: usize,
    staging_directories: usize,
    orphaned_staging_directories: usize,
    oldest_orphaned_staging_age_seconds: u64,
    filtered_internal_paths: usize,
    finding_count: usize,
    findings_by_kind: BTreeMap<&'static str, usize>,
}

impl SelfCheckSummary {
    fn idle(status: &'static str) -> Self {
        Self {
            status,
            completed_at: 0,
            duration_ms: 0,
            repositories_checked: 0,
            revisions_checked: 0,
            files_checked: 0,
            refs_checked: 0,
            staging_directories: 0,
            orphaned_staging_directories: 0,
            oldest_orphaned_staging_age_seconds: 0,
            filtered_internal_paths: 0,
            finding_count: 0,
            findings_by_kind: BTreeMap::new(),
        }
    }

    fn from_state(state: SelfCheckState) -> Self {
        match state {
            SelfCheckState::NeverRun => Self::idle("never_run"),
            SelfCheckState::Running => Self::idle("running"),
            SelfCheckState::Completed(report) => Self {
                status: report.status(),
                completed_at: report.completed_at,
                duration_ms: report.duration_ms,
                repositories_checked: report.repositories_checked,
                revisions_checked: report.revisions_checked,
                files_checked: report.files_checked,
                refs_checked: report.refs_checked,
                staging_directories: report.staging_directories,
                orphaned_staging_directories: report.orphaned_staging_directories,
                oldest_orphaned_staging_age_seconds: report.oldest_orphaned_staging_age_seconds,
                filtered_internal_paths: report.filtered_internal_paths,
                finding_count: report.findings.len(),
                findings_by_kind: report.findings_by_kind(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilesystemCapacity {
    total_bytes: u64,
    available_bytes: u64,
    available_percent: u8,
    low_space: bool,
}

fn filesystem_capacity(path: &std::path::Path) -> std::io::Result<FilesystemCapacity> {
    let status = rustix::fs::statvfs(path)?;
    Ok(filesystem_capacity_from_blocks(
        status.f_blocks,
        status.f_bavail,
        status.f_frsize,
    ))
}

fn filesystem_capacity_from_blocks(
    total_blocks: u64,
    available_blocks: u64,
    fragment_size: u64,
) -> FilesystemCapacity {
    let total_bytes = total_blocks.saturating_mul(fragment_size);
    let available_bytes = available_blocks.saturating_mul(fragment_size);
    let available_percent = if total_bytes == 0 {
        0
    } else {
        ((u128::from(available_bytes) * 100) / u128::from(total_bytes)).min(100) as u8
    };
    let low_space =
        total_bytes == 0 || u128::from(available_bytes) * 100 <= u128::from(total_bytes) * 10;
    FilesystemCapacity {
        total_bytes,
        available_bytes,
        available_percent,
        low_space,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PrincipalView {
    auth_method: String,
    login: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PageQuery {
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Debug, Serialize)]
struct RepositoryPage {
    items: Vec<RepositorySummary>,
    next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JobKind {
    Prefetch,
    Refresh,
    Verify,
    Audit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JobState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// What a terminal acquisition job did to the archive.
///
/// Issue 0071: a job that transferred nothing because the selection was
/// already archived and a job that reached a terminal state having moved zero
/// bytes are otherwise indistinguishable to an operator reading the API.
/// This mirrors [`AcquisitionOutcome`] into the durable job record, which
/// ADR-0018 already treats as the authority for job history; nothing else
/// stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JobOutcome {
    /// Every path the selection covers was already archived.
    AlreadyArchived,
    /// A revision was published for the first time.
    Published,
    /// An already published revision gained the paths it did not hold.
    Extended,
}

/// What running one job produced: the resolved commit when there is one, the
/// acquisition outcome when the job acquired, or a safe failure class/message.
type JobRunResult = Result<(Option<String>, Option<JobOutcome>), (&'static str, String)>;

impl From<AcquisitionOutcome> for JobOutcome {
    fn from(outcome: AcquisitionOutcome) -> Self {
        match outcome {
            AcquisitionOutcome::AlreadyArchived => Self::AlreadyArchived,
            AcquisitionOutcome::Published => Self::Published,
            AcquisitionOutcome::Extended => Self::Extended,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Job {
    id: String,
    kind: JobKind,
    state: JobState,
    phase: String,
    #[serde(default)]
    repo_type: RepositoryType,
    repo_id: Option<String>,
    revision: Option<String>,
    /// Normalized acquisition selection (ADR-0020 decision 5). Empty means the
    /// whole repository, which is what every record written before selections
    /// existed means, so an absent field needs no migration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<String>,
    /// Terminal acquisition outcome; absent for a job that acquires nothing and
    /// for any record written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcome: Option<JobOutcome>,
    resolved_commit: Option<String>,
    #[serde(default)]
    resumed: bool,
    progress_bytes: Option<u64>,
    total_bytes: Option<u64>,
    #[serde(default)]
    progress_files: Option<u64>,
    #[serde(default)]
    total_files: Option<u64>,
    #[serde(default)]
    last_progress_at: Option<u64>,
    #[serde(default)]
    started_at: Option<u64>,
    #[serde(default)]
    finished_at: Option<u64>,
    #[serde(default)]
    principal: Option<PrincipalView>,
    error_class: Option<String>,
    message: Option<String>,
    idempotency_hash: Option<String>,
    #[serde(default)]
    idempotency_request_hash: Option<String>,
    created_at: u64,
    updated_at: u64,
}

#[derive(Debug, Serialize)]
struct JobView {
    id: String,
    kind: JobKind,
    state: JobState,
    phase: String,
    repo_type: RepositoryType,
    repo_id: Option<String>,
    revision: Option<String>,
    include: Vec<String>,
    exclude: Vec<String>,
    outcome: Option<JobOutcome>,
    resolved_commit: Option<String>,
    resumed: bool,
    progress_bytes: Option<u64>,
    total_bytes: Option<u64>,
    progress_files: Option<u64>,
    total_files: Option<u64>,
    last_progress_at: Option<u64>,
    started_at: Option<u64>,
    finished_at: Option<u64>,
    principal: Option<PrincipalView>,
    error_class: Option<String>,
    message: Option<String>,
    created_at: u64,
    updated_at: u64,
}

impl From<Job> for JobView {
    fn from(job: Job) -> Self {
        Self {
            id: job.id,
            kind: job.kind,
            state: job.state,
            phase: job.phase,
            repo_type: job.repo_type,
            repo_id: job.repo_id,
            revision: job.revision,
            include: job.include,
            exclude: job.exclude,
            outcome: job.outcome,
            resolved_commit: job.resolved_commit,
            resumed: job.resumed,
            progress_bytes: job.progress_bytes,
            total_bytes: job.total_bytes,
            progress_files: job.progress_files,
            total_files: job.total_files,
            last_progress_at: job.last_progress_at,
            started_at: job.started_at,
            finished_at: job.finished_at,
            principal: job.principal,
            error_class: job.error_class,
            message: job.message,
            created_at: job.created_at,
            updated_at: job.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
struct JobPage {
    items: Vec<JobView>,
    next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct JobRequest {
    kind: JobKind,
    #[serde(default)]
    repo_type: RepositoryType,
    repo_id: Option<String>,
    revision: Option<String>,
    /// Acquisition selection patterns. Accepted for `prefetch` only; they are
    /// untrusted input and are validated by [`FileSelection`].
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
}

#[derive(Clone)]
struct JobManager {
    inner: Arc<JobManagerInner>,
}

struct JobManagerInner {
    directory: PathBuf,
    index_directory: PathBuf,
    active_directory: PathBuf,
    idempotency_directory: PathBuf,
    active_jobs: Mutex<BTreeMap<String, Job>>,
    #[cfg(test)]
    read_count: AtomicU64,
}

#[derive(Debug, Serialize, Deserialize)]
struct IdempotencyEntry {
    job_id: String,
    request_hash: String,
}

impl JobManager {
    fn open(archive: &Archive) -> Result<Self, ArchiveError> {
        let directory = archive.root.join("state").join("jobs");
        fs::create_dir_all(&directory)?;
        let index_directory = directory.join("by-created");
        let active_directory = directory.join("active");
        let idempotency_directory = directory.join("idempotency");
        fs::create_dir_all(&index_directory)?;
        fs::create_dir_all(&active_directory)?;
        fs::create_dir_all(&idempotency_directory)?;
        let manager = Self {
            inner: Arc::new(JobManagerInner {
                directory,
                index_directory,
                active_directory,
                idempotency_directory,
                active_jobs: Mutex::new(BTreeMap::new()),
                #[cfg(test)]
                read_count: AtomicU64::new(0),
            }),
        };
        manager.migrate_indexes_once()?;
        for entry in fs::read_dir(&manager.inner.active_directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if validate_job_id(&id).is_err() {
                tracing::warn!(event = "admin_active_job_skipped", job_id = %id, "skipped invalid active job marker");
                continue;
            }
            let mut job = match manager.read_job(&id) {
                Ok(job) => job,
                Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::remove_file(entry.path())?;
                    continue;
                }
                Err(error) => {
                    tracing::warn!(event = "admin_active_job_skipped", job_id = %id, error = %error, "skipped unreadable active management job");
                    continue;
                }
            };
            if matches!(job.state, JobState::Queued | JobState::Running) {
                job.state = JobState::Failed;
                job.phase = "interrupted".into();
                job.error_class = Some("interrupted".into());
                job.message = Some("job interrupted by process restart".into());
                let now = unix_timestamp();
                job.finished_at = Some(now);
                job.updated_at = now;
                manager.persist(&job)?;
            } else {
                manager.set_active_marker(&job)?;
            }
        }
        Ok(manager)
    }

    fn list_page(&self, limit: usize, cursor: Option<&str>) -> ArchiveResult<JobPage> {
        let cursor_key = match cursor {
            Some(id) => match self.read_job(id) {
                Ok(job) => Some(history_key(&job)),
                Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(JobPage {
                        items: Vec::new(),
                        next_cursor: None,
                    });
                }
                Err(error) => return Err(error),
            },
            None => None,
        };
        let capacity = limit + 1;
        let mut selected = BinaryHeap::<Reverse<String>>::new();
        for entry in fs::read_dir(&self.inner.index_directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let key = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = job_id_from_history_key(&key) else {
                continue;
            };
            if cursor_key.as_ref().is_some_and(|cursor| key >= *cursor)
                || !self.job_path(&id)?.is_file()
            {
                continue;
            }
            if selected.len() < capacity {
                selected.push(Reverse(key));
            } else if selected.peek().is_some_and(|smallest| key > smallest.0) {
                selected.pop();
                selected.push(Reverse(key));
            }
        }
        let mut keys = selected
            .into_iter()
            .map(|value| value.0)
            .collect::<Vec<_>>();
        keys.sort_unstable_by(|left, right| right.cmp(left));
        let mut jobs = keys
            .iter()
            .map(|key| {
                let id = job_id_from_history_key(key).ok_or_else(|| {
                    ArchiveError::IntegrityMismatch("invalid job history index".into())
                })?;
                let job = self.read_job(&id)?;
                if history_key(&job) != *key {
                    return Err(ArchiveError::IntegrityMismatch(
                        "job history index mismatch".into(),
                    ));
                }
                Ok(job)
            })
            .collect::<ArchiveResult<Vec<_>>>()?;
        let next_cursor = (jobs.len() > limit).then(|| jobs[limit - 1].id.clone());
        jobs.truncate(limit);
        Ok(JobPage {
            items: jobs.into_iter().map(JobView::from).collect(),
            next_cursor,
        })
    }

    fn get(&self, id: &str) -> ArchiveResult<Option<Job>> {
        validate_job_id(id)?;
        if let Some(job) = self.inner.active_jobs.lock().unwrap().get(id).cloned() {
            return Ok(Some(job));
        }
        match self.read_job(id) {
            Ok(job) => Ok(Some(job)),
            Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn submit(
        &self,
        request: JobRequest,
        idempotency_key: Option<&str>,
        archive: Arc<Archive>,
        pullthrough: Option<Arc<PullThrough>>,
        principal: PrincipalView,
    ) -> Result<(Job, bool), &'static str> {
        let selection = validate_job_request(&request)?;
        let idempotency_hash = idempotency_key.map(hash_idempotency_key).transpose()?;
        let idempotency_request_hash = idempotency_hash
            .as_ref()
            .map(|_| hash_idempotency_request(&request, &selection, &principal));
        let mut jobs = self.inner.active_jobs.lock().unwrap();
        if let Some(hash) = &idempotency_hash {
            match self.read_idempotency(hash) {
                Ok(Some(entry)) => match self.read_job(&entry.job_id) {
                    Ok(existing) => {
                        // A record written before selections existed describes a
                        // whole-repository request, so it can only match one.
                        let legacy_model_match = request.repo_type == RepositoryType::Model
                            && selection.is_unrestricted()
                            && entry.request_hash
                                == hash_legacy_idempotency_request(&request, &principal);
                        return if Some(entry.request_hash) == idempotency_request_hash
                            || legacy_model_match
                        {
                            Ok((existing, false))
                        } else {
                            Err("idempotency_conflict")
                        };
                    }
                    Err(ArchiveError::Io(error))
                        if error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        self.remove_idempotency(hash).map_err(|_| "storage")?;
                    }
                    Err(_) => return Err("storage"),
                },
                Ok(None) => {}
                Err(_) => return Err("storage"),
            }
        }
        if let Some(existing) = jobs
            .values()
            .find(|job| is_equivalent_active_job(job, &request, &selection))
            .cloned()
        {
            return Ok((existing, false));
        }
        let now = unix_timestamp();
        let mut job = None;
        for _ in 0..16 {
            let candidate = Job {
                id: new_job_id(now),
                kind: request.kind,
                state: JobState::Queued,
                phase: "queued".into(),
                repo_type: request.repo_type,
                repo_id: request.repo_id.clone(),
                revision: request.revision.clone(),
                include: selection.include().to_vec(),
                exclude: selection.exclude().to_vec(),
                outcome: None,
                resolved_commit: None,
                resumed: false,
                progress_bytes: None,
                total_bytes: None,
                progress_files: None,
                total_files: None,
                last_progress_at: None,
                started_at: None,
                finished_at: None,
                principal: Some(principal.clone()),
                error_class: None,
                message: None,
                idempotency_hash: idempotency_hash.clone(),
                idempotency_request_hash: idempotency_request_hash.clone(),
                created_at: now,
                updated_at: now,
            };
            if jobs.contains_key(&candidate.id) {
                continue;
            }
            if self.persist_new(&candidate).map_err(|_| "storage")? {
                job = Some(candidate);
                break;
            }
        }
        let job = job.ok_or("storage")?;
        jobs.insert(job.id.clone(), job.clone());
        drop(jobs);
        let manager = self.clone();
        let job_id = job.id.clone();
        // Management work is deliberately detached from Tokio's blocking pool.
        // Dropping a Tokio runtime waits indefinitely for active spawn_blocking tasks,
        // which prevented the container's PID 1 from exiting while a prefetch helper
        // was running. A detached OS thread is terminated with the container process;
        // startup recovery then records the job as interrupted and preserves eligible
        // staging for a later retry.
        std::thread::spawn(move || manager.run(&job_id, archive, pullthrough));
        Ok((job, true))
    }

    fn run(&self, id: &str, archive: Arc<Archive>, pullthrough: Option<Arc<PullThrough>>) {
        let Some(job) = self.update(id, |job| {
            if job.state == JobState::Cancelled {
                return;
            }
            job.state = JobState::Running;
            job.started_at = Some(unix_timestamp());
            job.phase = match job.kind {
                JobKind::Prefetch | JobKind::Refresh => "acquiring_snapshot",
                JobKind::Verify => "verifying_revision",
                JobKind::Audit => "auditing_archive",
            }
            .into();
        }) else {
            return;
        };
        if job.state == JobState::Cancelled {
            return;
        }
        let progress_manager = self.clone();
        let progress_job_id = id.to_string();
        let progress = move |event: FetchProgress| {
            progress_manager.record_progress(&progress_job_id, event);
        };
        let result: JobRunResult = match job.kind {
            JobKind::Prefetch => pullthrough
                .as_ref()
                .ok_or_else(|| ("upstream_disabled", "pull-through is disabled".into()))
                .and_then(|pullthrough| {
                    // The stored patterns were normalized at submission; a
                    // record that no longer validates is an unsafe path, never
                    // an unrestricted acquisition.
                    let selection = FileSelection::new(&job.include, &job.exclude)
                        .map_err(|_| classify_pullthrough_error(PullThroughError::UnsafePath))?;
                    pullthrough
                        .ensure_selected_with_progress_for_type(
                            job.repo_type,
                            job.repo_id.as_deref().unwrap(),
                            job.revision.as_deref().unwrap(),
                            &selection,
                            &progress,
                        )
                        .map(|result| (Some(result.commit), Some(result.outcome.into())))
                        .map_err(classify_pullthrough_error)
                }),
            JobKind::Refresh => pullthrough
                .as_ref()
                .ok_or_else(|| ("upstream_disabled", "pull-through is disabled".into()))
                .and_then(|pullthrough| {
                    pullthrough
                        .refresh_with_progress_for_type(
                            job.repo_type,
                            job.repo_id.as_deref().unwrap(),
                            job.revision.as_deref().unwrap(),
                            false,
                            &progress,
                        )
                        .map(|result| (Some(result.proposed), None))
                        .map_err(classify_pullthrough_error)
                }),
            JobKind::Verify => archive
                .verify_revision_for_type(
                    job.repo_type,
                    job.repo_id.as_deref().unwrap(),
                    job.revision.as_deref().unwrap(),
                )
                .map(|_| (job.revision.clone(), None))
                .map_err(classify_archive_error),
            JobKind::Audit => archive
                .audit()
                .map_err(classify_archive_error)
                .and_then(|report| {
                    if report.failures.is_empty() {
                        Ok((None, None))
                    } else {
                        Err((
                            "integrity",
                            format!("{} revisions failed audit", report.failures.len()),
                        ))
                    }
                }),
        };
        match result {
            Ok((commit, outcome)) => {
                self.update(id, |job| {
                    job.state = JobState::Completed;
                    job.phase = "completed".into();
                    job.resolved_commit = commit;
                    job.outcome = outcome;
                    job.finished_at = Some(unix_timestamp());
                });
            }
            Err((class, message)) => {
                tracing::warn!(
                    event = "admin_job_failed",
                    job_id = %id,
                    job_kind = ?job.kind,
                    repo_type = %job.repo_type,
                    repo_id = job.repo_id.as_deref().unwrap_or(""),
                    revision = job.revision.as_deref().unwrap_or(""),
                    error_class = class,
                    safe_reason = %message,
                    "management job failed"
                );
                self.update(id, |job| {
                    job.state = JobState::Failed;
                    job.phase = "failed".into();
                    job.error_class = Some(class.into());
                    job.message = Some(message);
                    job.finished_at = Some(unix_timestamp());
                });
            }
        }
    }

    fn cancel(&self, id: &str) -> Result<Job, &'static str> {
        let mut jobs = self.inner.active_jobs.lock().unwrap();
        let job = jobs.get_mut(id).ok_or("not_found")?;
        if job.state != JobState::Queued {
            return Err("not_cancellable");
        }
        let previous = job.clone();
        job.state = JobState::Cancelled;
        job.phase = "cancelled".into();
        let now = unix_timestamp();
        job.finished_at = Some(now);
        job.updated_at = now;
        let snapshot = job.clone();
        drop(jobs);
        if self.persist(&snapshot).is_err() {
            self.inner
                .active_jobs
                .lock()
                .unwrap()
                .insert(id.into(), previous);
            return Err("storage");
        }
        self.inner.active_jobs.lock().unwrap().remove(id);
        Ok(snapshot)
    }

    fn record_progress(&self, id: &str, event: FetchProgress) {
        let snapshot = self.update(id, |job| {
            job.phase = event.phase.clone();
            if event.phase == "resuming_snapshot" {
                job.resumed = true;
            }
            match (event.unit.as_deref(), event.completed) {
                (Some("bytes"), Some(completed)) => {
                    if job.total_bytes.is_none() {
                        job.total_bytes = event.total;
                    }
                    let bounded = job
                        .total_bytes
                        .map_or(completed, |total| completed.min(total));
                    job.progress_bytes = Some(bounded.max(job.progress_bytes.unwrap_or(0)));
                }
                (Some("files"), Some(completed)) => {
                    if job.total_files.is_none() {
                        job.total_files = event.total;
                    }
                    let bounded = job
                        .total_files
                        .map_or(completed, |total| completed.min(total));
                    job.progress_files = Some(bounded.max(job.progress_files.unwrap_or(0)));
                }
                _ => {}
            }
            job.last_progress_at = Some(unix_timestamp());
        });
        if let Some(job) = snapshot {
            tracing::info!(
                event = "admin_job_progress",
                job_id = %id,
                repo_type = %job.repo_type,
                repo_id = job.repo_id.as_deref().unwrap_or(""),
                progress_bytes = job.progress_bytes,
                total_bytes = job.total_bytes,
                progress_files = job.progress_files,
                total_files = job.total_files,
                "management job progress"
            );
        }
    }

    fn update(&self, id: &str, update: impl FnOnce(&mut Job)) -> Option<Job> {
        let mut jobs = self.inner.active_jobs.lock().unwrap();
        let job = jobs.get_mut(id)?;
        let previous = job.clone();
        update(job);
        job.updated_at = unix_timestamp();
        let snapshot = job.clone();
        drop(jobs);
        let persisted = match self.persist(&snapshot) {
            Ok(()) => true,
            Err(error) => {
                if let Ok(mut jobs) = self.inner.active_jobs.lock() {
                    jobs.insert(id.to_string(), previous.clone());
                }
                tracing::error!(event = "admin_job_persist_failed", job_id = %id, error = %error, "failed to persist management job");
                false
            }
        };
        if persisted && !matches!(snapshot.state, JobState::Queued | JobState::Running) {
            self.inner.active_jobs.lock().unwrap().remove(id);
        }
        Some(if persisted { snapshot } else { previous })
    }

    fn migrate_indexes_once(&self) -> ArchiveResult<()> {
        let sentinel = self.inner.directory.join(".index-v1");
        if sentinel.is_file() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.inner.directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            let job: Job = match serde_json::from_slice(&fs::read(entry.path())?) {
                Ok(job) => job,
                Err(error) => {
                    tracing::warn!(
                        event = "admin_job_index_skipped",
                        path = %entry.path().display(),
                        error = %error,
                        "skipped malformed management job during index migration"
                    );
                    continue;
                }
            };
            let file_id = entry
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string();
            if validate_job_id(&job.id).is_err() || job.id != file_id {
                tracing::warn!(
                    event = "admin_job_index_skipped",
                    path = %entry.path().display(),
                    "skipped management job with invalid identity during index migration"
                );
                continue;
            }
            self.ensure_history_index(&job)?;
            self.ensure_idempotency_index(&job)?;
            self.set_active_marker(&job)?;
        }
        let temporary = self
            .inner
            .directory
            .join(format!(".index-v1-{}.tmp", new_job_id(unix_timestamp())));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(b"1\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &sentinel)?;
        File::open(&self.inner.directory)?.sync_all()?;
        Ok(())
    }

    fn job_path(&self, id: &str) -> ArchiveResult<PathBuf> {
        validate_job_id(id)?;
        Ok(self.inner.directory.join(format!("{id}.json")))
    }

    fn read_job(&self, id: &str) -> ArchiveResult<Job> {
        #[cfg(test)]
        self.inner.read_count.fetch_add(1, Ordering::Relaxed);
        let path = self.job_path(id)?;
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() {
            return Err(ArchiveError::IntegrityMismatch(
                "job record is not a regular file".into(),
            ));
        }
        let job: Job = serde_json::from_slice(&fs::read(path)?)
            .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
        if job.id != id {
            return Err(ArchiveError::IntegrityMismatch(
                "job record identity mismatch".into(),
            ));
        }
        Ok(job)
    }

    fn ensure_history_index(&self, job: &Job) -> ArchiveResult<()> {
        let path = self.inner.index_directory.join(history_key(job));
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => file.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        File::open(&self.inner.index_directory)?.sync_all()?;
        Ok(())
    }

    fn ensure_idempotency_index(&self, job: &Job) -> ArchiveResult<()> {
        let (Some(hash), Some(request_hash)) = (
            job.idempotency_hash.as_deref(),
            job.idempotency_request_hash.as_deref(),
        ) else {
            return Ok(());
        };
        let path = self
            .inner
            .idempotency_directory
            .join(format!("{hash}.json"));
        validate_digest(hash)?;
        if path.is_file() {
            let existing: IdempotencyEntry = serde_json::from_slice(&fs::read(&path)?)
                .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
            if existing.job_id != job.id && !self.job_path(&existing.job_id)?.is_file() {
                fs::remove_file(&path)?;
            } else if existing.job_id != job.id || existing.request_hash != request_hash {
                return Err(ArchiveError::IntegrityMismatch(
                    "idempotency index conflict".into(),
                ));
            } else {
                return Ok(());
            }
        }
        let entry = IdempotencyEntry {
            job_id: job.id.clone(),
            request_hash: request_hash.into(),
        };
        write_new_json(&path, &entry)
    }

    fn read_idempotency(&self, hash: &str) -> ArchiveResult<Option<IdempotencyEntry>> {
        validate_digest(hash)?;
        let path = self
            .inner
            .idempotency_directory
            .join(format!("{hash}.json"));
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn remove_idempotency(&self, hash: &str) -> ArchiveResult<()> {
        validate_digest(hash)?;
        match fs::remove_file(
            self.inner
                .idempotency_directory
                .join(format!("{hash}.json")),
        ) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn set_active_marker(&self, job: &Job) -> ArchiveResult<()> {
        let path = self.inner.active_directory.join(&job.id);
        if matches!(job.state, JobState::Queued | JobState::Running) {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(file) => file.sync_all()?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        } else {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        File::open(&self.inner.active_directory)?.sync_all()?;
        Ok(())
    }

    fn persist(&self, job: &Job) -> Result<(), ArchiveError> {
        let temporary = self.inner.directory.join(format!(".{}.tmp", job.id));
        let final_path = self.inner.directory.join(format!("{}.json", job.id));
        let bytes = serde_json::to_vec(job)
            .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, final_path)?;
        File::open(&self.inner.directory)?.sync_all()?;
        if let Err(error) = self.ensure_history_index(job) {
            tracing::error!(event = "admin_job_index_update_failed", job_id = %job.id, index = "by_created", error = %error, "job authority persisted but index update failed");
        }
        if let Err(error) = self.ensure_idempotency_index(job) {
            tracing::error!(event = "admin_job_index_update_failed", job_id = %job.id, index = "idempotency", error = %error, "job authority persisted but index update failed");
        }
        if let Err(error) = self.set_active_marker(job) {
            tracing::error!(event = "admin_job_index_update_failed", job_id = %job.id, index = "active", error = %error, "job authority persisted but index update failed");
        }
        Ok(())
    }

    fn persist_new(&self, job: &Job) -> Result<bool, ArchiveError> {
        let reservation = self.inner.directory.join(format!(".{}.reserve", job.id));
        let final_path = self.inner.directory.join(format!("{}.json", job.id));
        let reservation_file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&reservation)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        reservation_file.sync_all()?;
        if final_path.exists() {
            let _ = fs::remove_file(&reservation);
            File::open(&self.inner.directory)?.sync_all()?;
            return Ok(false);
        }
        self.ensure_history_index(job)?;
        self.ensure_idempotency_index(job)?;
        self.set_active_marker(job)?;
        let result = self.persist(job);
        let remove_result = fs::remove_file(&reservation);
        File::open(&self.inner.directory)?.sync_all()?;
        result?;
        remove_result?;
        Ok(true)
    }
}

fn is_equivalent_active_job(job: &Job, request: &JobRequest, selection: &FileSelection) -> bool {
    matches!(job.state, JobState::Queued | JobState::Running)
        && job.kind == request.kind
        && job.repo_type == request.repo_type
        && job.repo_id == request.repo_id
        && job.revision == request.revision
        && job.include == selection.include()
        && job.exclude == selection.exclude()
}

pub fn router(
    archive: Archive,
    config: Config,
    pullthrough: Option<Arc<PullThrough>>,
) -> Result<Router, ArchiveError> {
    let jobs = JobManager::open(&archive)?;
    let state = AdminState {
        archive: Arc::new(archive),
        config,
        pullthrough,
        jobs,
    };
    let pullthrough_enabled = state.pullthrough.is_some();
    Ok(Router::new()
        .route("/", get(crate::admin_ui::root))
        .route("/admin/", get(crate::admin_ui::index))
        .route("/admin/app.js", get(crate::admin_ui::script))
        .route("/admin/style.css", get(crate::admin_ui::style))
        .route(
            "/api/admin/v1/status",
            get(move |State(state), headers| status(state, headers, pullthrough_enabled)),
        )
        .route("/api/admin/v1/repositories", get(repositories))
        .route(
            "/api/admin/v1/repositories/{namespace}/{repository}",
            get(model_repository),
        )
        .route(
            "/api/admin/v1/repositories/{repo_type}/{namespace}/{repository}",
            get(repository),
        )
        .route("/api/admin/v1/jobs", get(list_jobs).post(create_job))
        .route("/api/admin/v1/jobs/{id}", get(job).delete(cancel_job))
        .with_state(state))
}

pub async fn serve(
    archive: Archive,
    pullthrough: Option<Arc<PullThrough>>,
    config: Config,
) -> std::io::Result<()> {
    let address = config.address;
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| {
            tracing::error!(
                event = "admin_server_bind_failed",
                listen_address = %address,
                error = %error,
                "failed to bind management listener"
            );
            error
        })?;
    tracing::info!(
        event = "admin_server_ready",
        listen_address = %address,
        "management API is ready"
    );
    let router = router(archive, config, pullthrough)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    axum::serve(listener, router)
        .with_graceful_shutdown(crate::http::shutdown_signal())
        .await
}

async fn list_jobs(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    if !authorized(&state.config, &headers) {
        return unauthorized(&state.config);
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    match state.jobs.list_page(limit, query.cursor.as_deref()) {
        Ok(page) => Json(page).into_response(),
        Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: "invalid_cursor",
            }),
        )
            .into_response(),
        Err(error) => archive_error(error),
    }
}

async fn job(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !authorized(&state.config, &headers) {
        return unauthorized(&state.config);
    }
    match state.jobs.get(&id) {
        Ok(Some(job)) => Json(JobView::from(job)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody { error: "not_found" }),
        )
            .into_response(),
        Err(ArchiveError::InvalidPath(_)) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: "invalid_request",
            }),
        )
            .into_response(),
        Err(error) => archive_error(error),
    }
}

async fn create_job(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(request): Json<JobRequest>,
) -> Response {
    let Some(principal) = authenticate(&state.config, &headers) else {
        return unauthorized(&state.config);
    };
    if !csrf_authorized(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorBody {
                error: "csrf_required",
            }),
        )
            .into_response();
    }
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok());
    match state.jobs.submit(
        request,
        idempotency_key,
        state.archive.clone(),
        state.pullthrough.clone(),
        principal,
    ) {
        Ok((job, created)) => (
            if created {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            },
            Json(JobView::from(job)),
        )
            .into_response(),
        Err("invalid_request") | Err("invalid_idempotency_key") => (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: "invalid_request",
            }),
        )
            .into_response(),
        Err("idempotency_conflict") => (
            StatusCode::CONFLICT,
            Json(ErrorBody {
                error: "idempotency_conflict",
            }),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                error: "job_storage_error",
            }),
        )
            .into_response(),
    }
}

async fn cancel_job(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !authorized(&state.config, &headers) {
        return unauthorized(&state.config);
    }
    if !csrf_authorized(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorBody {
                error: "csrf_required",
            }),
        )
            .into_response();
    }
    match state.jobs.cancel(&id) {
        Ok(job) => Json(JobView::from(job)).into_response(),
        Err("not_found") => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody { error: "not_found" }),
        )
            .into_response(),
        Err("not_cancellable") => (
            StatusCode::CONFLICT,
            Json(ErrorBody {
                error: "not_cancellable",
            }),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                error: "job_storage_error",
            }),
        )
            .into_response(),
    }
}

async fn status(state: AdminState, headers: HeaderMap, pullthrough_enabled: bool) -> Response {
    let Some(principal) = authenticate(&state.config, &headers) else {
        return unauthorized(&state.config);
    };
    match (
        state.archive.list_repositories(),
        filesystem_capacity(&state.archive.root),
    ) {
        (Ok(repositories), Ok(capacity)) => Json(StatusBody {
            version: env!("CARGO_PKG_VERSION"),
            // Startup and `/readyz` perform the active write probe. Management UI
            // polling only reports its last result so it cannot create continuous
            // fsync traffic on the archive volume.
            ready: state.archive.last_readiness().unwrap_or(false),
            pullthrough_enabled,
            repository_count: repositories.len(),
            model_repository_count: repositories
                .iter()
                .filter(|repository| repository.repo_type == RepositoryType::Model)
                .count(),
            dataset_repository_count: repositories
                .iter()
                .filter(|repository| repository.repo_type == RepositoryType::Dataset)
                .count(),
            logical_archive_bytes: repositories
                .iter()
                .map(|repository| repository.logical_bytes)
                .sum(),
            archive_filesystem_path: state.archive.root.display().to_string(),
            archive_filesystem_total_bytes: capacity.total_bytes,
            archive_filesystem_available_bytes: capacity.available_bytes,
            archive_filesystem_available_percent: capacity.available_percent,
            archive_filesystem_low_space: capacity.low_space,
            // The self-check runs once at startup, beside serving. Reporting
            // its stored result keeps this route a read of cached state, and
            // never starts an archive walk from a status poll.
            self_check: SelfCheckSummary::from_state(state.archive.self_check_state()),
            principal,
            auth_methods: state.config.auth_methods(),
        })
        .into_response(),
        (Err(error), _) => archive_error(error),
        (_, Err(error)) => archive_error(error.into()),
    }
}

async fn repositories(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    if !authorized(&state.config, &headers) {
        return unauthorized(&state.config);
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let cursor = match query.cursor.as_deref().map(parse_repository_cursor) {
        Some(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorBody {
                    error: "invalid_cursor",
                }),
            )
                .into_response();
        }
        Some(Some(cursor)) => Some(cursor),
        None => None,
    };
    match state.archive.list_repositories() {
        Ok(repositories) => {
            let mut filtered = repositories
                .into_iter()
                .filter(|repository| {
                    cursor.as_ref().is_none_or(|cursor| {
                        (repository.repo_type, repository.repo_id.as_str())
                            > (cursor.0, cursor.1.as_str())
                    })
                })
                .take(limit + 1)
                .collect::<Vec<_>>();
            let next_cursor =
                (filtered.len() > limit).then(|| repository_cursor(&filtered[limit - 1]));
            filtered.truncate(limit);
            Json(RepositoryPage {
                items: filtered,
                next_cursor,
            })
            .into_response()
        }
        Err(error) => archive_error(error),
    }
}

async fn repository(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((repo_type, namespace, repository)): Path<(RepositoryType, String, String)>,
) -> Response {
    if !authorized(&state.config, &headers) {
        return unauthorized(&state.config);
    }
    match state
        .archive
        .repository_inventory_for_type(repo_type, &format!("{namespace}/{repository}"))
    {
        Ok(inventory) => Json(inventory).into_response(),
        Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody { error: "not_found" }),
        )
            .into_response(),
        Err(error) => archive_error(error),
    }
}

async fn model_repository(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((namespace, repository)): Path<(String, String)>,
) -> Response {
    if !authorized(&state.config, &headers) {
        return unauthorized(&state.config);
    }
    match state
        .archive
        .repository_inventory_for_type(RepositoryType::Model, &format!("{namespace}/{repository}"))
    {
        Ok(inventory) => Json(inventory).into_response(),
        Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody { error: "not_found" }),
        )
            .into_response(),
        Err(error) => archive_error(error),
    }
}

fn repository_cursor(repository: &RepositorySummary) -> String {
    format!("{}:{}", repository.repo_type, repository.repo_id)
}

fn parse_repository_cursor(cursor: &str) -> Option<(RepositoryType, String)> {
    let (repo_type, repo_id) = cursor.split_once(':')?;
    let repo_type = match repo_type {
        "model" => RepositoryType::Model,
        "dataset" => RepositoryType::Dataset,
        _ => return None,
    };
    validate_repository_id(repo_id).ok()?;
    Some((repo_type, repo_id.to_string()))
}

fn authorized(config: &Config, headers: &HeaderMap) -> bool {
    authenticate(config, headers).is_some()
}

fn authenticate(config: &Config, headers: &HeaderMap) -> Option<PrincipalView> {
    let bearer_authorized = config.bearer_token.as_ref().is_some_and(|expected| {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|provided| constant_time_eq(expected.as_bytes(), provided.as_bytes()))
    });
    let tailscale_authorized = config.trust_tailscale_headers
        && headers
            .get("tailscale-app-capabilities")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
            .and_then(|value| value.get(ADMIN_CAPABILITY).cloned())
            .and_then(|value| value.as_array().cloned())
            .is_some_and(|capabilities| !capabilities.is_empty());
    if tailscale_authorized {
        return Some(PrincipalView {
            auth_method: "tailscale".into(),
            login: trusted_identity_header(headers, "tailscale-user-login"),
            name: trusted_identity_header(headers, "tailscale-user-name"),
        });
    }
    bearer_authorized.then(|| PrincipalView {
        auth_method: "bearer".into(),
        login: None,
        name: None,
    })
}

fn trusted_identity_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .map(str::to_owned)
}

fn csrf_authorized(headers: &HeaderMap) -> bool {
    headers
        .get("x-modelkeep-csrf")
        .and_then(|value| value.to_str().ok())
        == Some("1")
}

/// Validates a submission and returns its normalized acquisition selection.
///
/// Patterns are untrusted input; validation and normalization are delegated to
/// [`FileSelection`] (ADR-0020 decision 1) rather than matched here. Only
/// `prefetch` acquires, so a selection on any other kind is rejected the same
/// way `audit` rejects target fields.
fn validate_job_request(request: &JobRequest) -> Result<FileSelection, &'static str> {
    match request.kind {
        JobKind::Audit => {
            if request.repo_id.is_some() || request.revision.is_some() {
                return Err("invalid_request");
            }
        }
        JobKind::Prefetch | JobKind::Refresh | JobKind::Verify => {
            let repo_id = request.repo_id.as_deref().ok_or("invalid_request")?;
            let revision = request.revision.as_deref().ok_or("invalid_request")?;
            if validate_repository_id(repo_id).is_err() || validate_revision_ref(revision).is_err()
            {
                return Err("invalid_request");
            }
        }
    }
    if request.kind != JobKind::Prefetch && (request.include.is_some() || request.exclude.is_some())
    {
        return Err("invalid_request");
    }
    FileSelection::new(
        request.include.as_deref().unwrap_or_default(),
        request.exclude.as_deref().unwrap_or_default(),
    )
    .map_err(|_| "invalid_request")
}

fn hash_idempotency_key(value: &str) -> Result<String, &'static str> {
    if value.is_empty() || value.len() > 128 || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("invalid_idempotency_key");
    }
    Ok(Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The exact-request component of an idempotency entry.
///
/// The normalized selection is part of acquisition identity (ADR-0020
/// decision 5), so two submissions differing only in their patterns are
/// different requests. An unrestricted selection contributes nothing, which
/// keeps the hash of every request that carries no selection identical to the
/// one recorded before selections existed.
fn hash_idempotency_request(
    request: &JobRequest,
    selection: &FileSelection,
    principal: &PrincipalView,
) -> String {
    let mut value = format!(
        "{:?}\0{}\0{}\0{}\0{}\0{}",
        request.kind,
        request.repo_type,
        request.repo_id.as_deref().unwrap_or(""),
        request.revision.as_deref().unwrap_or(""),
        principal.auth_method,
        principal.login.as_deref().unwrap_or("")
    );
    if !selection.is_unrestricted() {
        // Patterns hold no control characters, so a newline join cannot make
        // two different selections share one hash.
        value.push('\0');
        value.push_str(&selection.identity().join("\n"));
    }
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hash_legacy_idempotency_request(request: &JobRequest, principal: &PrincipalView) -> String {
    let value = format!(
        "{:?}\0{}\0{}\0{}\0{}",
        request.kind,
        request.repo_id.as_deref().unwrap_or(""),
        request.revision.as_deref().unwrap_or(""),
        principal.auth_method,
        principal.login.as_deref().unwrap_or("")
    );
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn new_job_id(now: u64) -> String {
    let mut random = [0u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .is_ok()
    {
        return format!(
            "{now}-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
    }
    format!(
        "{now}-fallback-{}-{}",
        process::id(),
        JOB_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn classify_pullthrough_error(
    error: crate::pullthrough::PullThroughError,
) -> (&'static str, String) {
    use crate::pullthrough::PullThroughError;
    let class = match error {
        PullThroughError::UpstreamUnavailable
        | PullThroughError::UpstreamInvalidOutput(_)
        | PullThroughError::UpstreamFailed => "upstream",
        PullThroughError::UpstreamNotFound => "not_found",
        PullThroughError::UpstreamUnauthorized => "authorization",
        PullThroughError::Integrity => "integrity",
        PullThroughError::Storage => "storage",
        PullThroughError::UnsafePath => "unsafe_path",
        PullThroughError::Conflict => "conflict",
    };
    (class, error.to_string())
}

fn classify_archive_error(error: ArchiveError) -> (&'static str, String) {
    let class = match &error {
        ArchiveError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => "not_found",
        ArchiveError::Io(_) => "storage",
        ArchiveError::InvalidPath(_) => "unsafe_path",
        ArchiveError::IntegrityMismatch(_) => "integrity",
        ArchiveError::AlreadyPublished(_) => "conflict",
        ArchiveError::ReferencedRevision(_) => "referenced",
    };
    (class, error.to_string())
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn constant_time_eq(expected: &[u8], provided: &[u8]) -> bool {
    let mut difference = expected.len() ^ provided.len();
    let length = expected.len().max(provided.len());
    for index in 0..length {
        difference |= usize::from(
            expected.get(index).copied().unwrap_or(0) ^ provided.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn unauthorized(config: &Config) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(ErrorBody {
            error: "unauthorized",
        }),
    )
        .into_response();
    if config.bearer_token.is_some() {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
    }
    response.headers_mut().insert(
        "x-modelkeep-auth-methods",
        config.auth_methods().join(",").parse().unwrap(),
    );
    response
}

fn archive_error(error: ArchiveError) -> Response {
    tracing::warn!(event = "admin_archive_error", error = %error, "management archive query failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody {
            error: "archive_error",
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pullthrough::PullThroughError;
    use crate::upstream::{
        FetchRequest, FetchedRevision, InvalidOutputReason, InventoryRequest, OfficialHfFetcher,
        RevisionInventory, UpstreamError, UpstreamFetcher,
    };
    use crate::{ArchiveFile, PublishRequest};
    use axum::{body::to_bytes, body::Body, http::Request};
    use std::io::Write;
    use std::sync::OnceLock;
    use tower::ServiceExt;
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

    fn capture_global_logs() -> LogWriter {
        static WRITER: OnceLock<LogWriter> = OnceLock::new();
        let writer = WRITER
            .get_or_init(|| {
                let writer = LogWriter::default();
                let subscriber = tracing_subscriber::fmt()
                    .json()
                    .with_env_filter(EnvFilter::new("info"))
                    .with_writer(writer.clone())
                    .finish();
                tracing::subscriber::set_global_default(subscriber).unwrap();
                writer
            })
            .clone();
        writer.0.lock().unwrap().clear();
        writer
    }

    struct FixtureFetcher;

    struct BlockingFetcher {
        calls: std::sync::atomic::AtomicUsize,
        released: (Mutex<bool>, std::sync::Condvar),
    }

    impl BlockingFetcher {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                released: (Mutex::new(false), std::sync::Condvar::new()),
            }
        }

        fn release(&self) {
            *self.released.0.lock().unwrap() = true;
            self.released.1.notify_all();
        }
    }

    impl UpstreamFetcher for BlockingFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut released = self.released.0.lock().unwrap();
            while !*released {
                released = self.released.1.wait(released).unwrap();
            }
            std::fs::create_dir_all(&request.staging).map_err(UpstreamError::Io)?;
            std::fs::write(request.staging.join("config.json"), b"model")
                .map_err(UpstreamError::Io)?;
            Ok(FetchedRevision {
                commit: "d".repeat(40),
                files: vec!["config.json".into()],
                staging: request.staging.clone(),
            })
        }
    }

    #[test]
    fn invalid_helper_output_is_an_upstream_failure() {
        let (class, message) = classify_pullthrough_error(PullThroughError::UpstreamInvalidOutput(
            InvalidOutputReason::EmptySnapshot,
        ));
        assert_eq!(class, "upstream");
        assert_eq!(
            message,
            "upstream invalid output: helper returned an empty snapshot"
        );
    }

    #[test]
    fn invalid_helper_output_is_safe_in_management_state_and_failure_event() {
        let writer = capture_global_logs();
        let directory = tempfile::tempdir().unwrap();
        let archive = Arc::new(Archive::new(directory.path()).unwrap());
        let manager = JobManager::open(&archive).unwrap();
        let mut job = stored_job("invalid-output-job", 1);
        job.kind = JobKind::Prefetch;
        job.state = JobState::Queued;
        job.phase = "queued".into();
        job.repo_id = Some("public/model".into());
        job.revision = Some("main".into());
        job.started_at = None;
        job.finished_at = None;
        assert!(manager.persist_new(&job).unwrap());
        manager
            .inner
            .active_jobs
            .lock()
            .unwrap()
            .insert(job.id.clone(), job.clone());
        let helper = directory.path().join("unsafe-helper.sh");
        std::fs::write(
            &helper,
            concat!(
                "#!/bin/sh\n",
                "echo '{\"type\":\"unsupported\",\"payload\":\"https://signed.example?token=stdout-secret\"}'\n",
                "echo 'Bearer stderr-secret' >&2\n",
            ),
        )
        .unwrap();
        let pullthrough = Arc::new(PullThrough::new(
            (*archive).clone(),
            Arc::new(OfficialHfFetcher {
                python: "sh".into(),
                helper,
            }),
        ));

        manager.run(&job.id, archive, Some(pullthrough));

        let failed = manager.get(&job.id).unwrap().unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.error_class.as_deref(), Some("upstream"));
        assert_eq!(
            failed.message.as_deref(),
            Some("upstream invalid output: helper emitted an unsupported event type")
        );
        let state = serde_json::to_string(&failed).unwrap();
        let output = writer.output();
        for expected in [
            "admin_job_failed",
            "invalid-output-job",
            "public/model",
            "main",
            "upstream",
            "safe_reason",
            "helper emitted an unsupported event type",
        ] {
            assert!(output.contains(expected), "missing {expected}: {output}");
        }
        for secret in [
            "signed.example",
            "stdout-secret",
            "Bearer stderr-secret",
            "stderr-secret",
        ] {
            assert!(!state.contains(secret));
            assert!(!output.contains(secret));
        }
    }

    impl UpstreamFetcher for FixtureFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            std::fs::create_dir_all(&request.staging).map_err(UpstreamError::Io)?;
            std::fs::write(request.staging.join("config.json"), b"model")
                .map_err(UpstreamError::Io)?;
            Ok(FetchedRevision {
                commit: "c".repeat(40),
                files: vec!["config.json".into()],
                staging: request.staging.clone(),
            })
        }
    }

    fn request(path: &str, token: Option<&str>) -> Request<Body> {
        let mut request = Request::builder().uri(path);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        request.body(Body::empty()).unwrap()
    }

    fn job_request(csrf: bool, idempotency_key: &str) -> Request<Body> {
        job_request_with_body(csrf, idempotency_key, r#"{"kind":"audit"}"#)
    }

    fn job_request_with_body(
        csrf: bool,
        idempotency_key: &str,
        body: &'static str,
    ) -> Request<Body> {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/admin/v1/jobs")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", idempotency_key);
        if csrf {
            request = request.header("x-modelkeep-csrf", "1");
        }
        request.body(Body::from(body)).unwrap()
    }

    fn stored_job(id: &str, created_at: u64) -> Job {
        Job {
            id: id.into(),
            kind: JobKind::Audit,
            state: JobState::Completed,
            phase: "completed".into(),
            repo_type: RepositoryType::Model,
            repo_id: None,
            revision: None,
            include: Vec::new(),
            exclude: Vec::new(),
            outcome: None,
            resolved_commit: None,
            resumed: false,
            progress_bytes: None,
            total_bytes: None,
            progress_files: None,
            total_files: None,
            last_progress_at: None,
            started_at: Some(created_at),
            finished_at: Some(created_at),
            principal: None,
            error_class: None,
            message: None,
            idempotency_hash: None,
            idempotency_request_hash: None,
            created_at,
            updated_at: created_at,
        }
    }

    #[test]
    fn legacy_jobs_and_requests_default_to_model_repositories() {
        let mut stored = serde_json::to_value(stored_job("legacy-model-job", 1)).unwrap();
        stored.as_object_mut().unwrap().remove("repo_type");
        let job: Job = serde_json::from_value(stored).unwrap();
        assert_eq!(job.repo_type, RepositoryType::Model);

        let request: JobRequest =
            serde_json::from_str(r#"{"kind":"prefetch","repo_id":"org/repo","revision":"main"}"#)
                .unwrap();
        assert_eq!(request.repo_type, RepositoryType::Model);
    }

    #[test]
    fn repository_cursor_includes_type_and_rejects_invalid_values() {
        let model = RepositorySummary {
            repo_type: RepositoryType::Model,
            repo_id: "org/shared".into(),
            revision_count: 1,
            ref_count: 1,
            logical_bytes: 1,
        };
        let dataset = RepositorySummary {
            repo_type: RepositoryType::Dataset,
            ..model.clone()
        };
        assert_eq!(repository_cursor(&model), "model:org/shared");
        assert_eq!(repository_cursor(&dataset), "dataset:org/shared");
        assert_eq!(
            parse_repository_cursor("dataset:org/shared"),
            Some((RepositoryType::Dataset, "org/shared".into()))
        );
        assert_eq!(parse_repository_cursor("space:org/shared"), None);
        assert_eq!(parse_repository_cursor("model:../escape"), None);
    }

    #[test]
    fn repository_type_is_part_of_idempotency_identity() {
        let principal = PrincipalView {
            auth_method: "bearer".into(),
            login: None,
            name: None,
        };
        let model = JobRequest {
            kind: JobKind::Prefetch,
            repo_type: RepositoryType::Model,
            repo_id: Some("org/shared".into()),
            revision: Some("main".into()),
            include: None,
            exclude: None,
        };
        let dataset = JobRequest {
            repo_type: RepositoryType::Dataset,
            ..model.clone()
        };
        let selection = FileSelection::all();
        assert_ne!(
            hash_idempotency_request(&model, &selection, &principal),
            hash_idempotency_request(&dataset, &selection, &principal)
        );
    }

    #[test]
    fn active_job_deduplication_is_scoped_by_repository_type() {
        let mut active = stored_job("active-shared", 1);
        active.kind = JobKind::Prefetch;
        active.state = JobState::Running;
        active.repo_id = Some("org/shared".into());
        active.revision = Some("main".into());
        let model = JobRequest {
            kind: JobKind::Prefetch,
            repo_type: RepositoryType::Model,
            repo_id: Some("org/shared".into()),
            revision: Some("main".into()),
            include: None,
            exclude: None,
        };
        let dataset = JobRequest {
            repo_type: RepositoryType::Dataset,
            ..model.clone()
        };
        let selection = FileSelection::all();
        assert!(is_equivalent_active_job(&active, &model, &selection));
        assert!(!is_equivalent_active_job(&active, &dataset, &selection));
        active.repo_type = RepositoryType::Dataset;
        assert!(is_equivalent_active_job(&active, &dataset, &selection));
    }

    #[test]
    fn revision_progress_keeps_initial_totals_and_accepts_phase_only_events() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let manager = JobManager::open(&archive).unwrap();
        let mut job = stored_job("progress-test", 1);
        job.state = JobState::Running;
        job.phase = "downloading".into();
        job.progress_bytes = None;
        job.total_bytes = None;
        job.progress_files = None;
        job.total_files = None;
        job.finished_at = None;
        manager
            .inner
            .active_jobs
            .lock()
            .unwrap()
            .insert(job.id.clone(), job);

        manager.record_progress(
            "progress-test",
            FetchProgress {
                version: 1,
                phase: "downloading".into(),
                unit: Some("bytes".into()),
                completed: Some(40),
                total: Some(100),
            },
        );
        manager.record_progress(
            "progress-test",
            FetchProgress {
                version: 1,
                phase: "downloading".into(),
                unit: Some("bytes".into()),
                completed: Some(75),
                total: Some(200),
            },
        );
        manager.record_progress(
            "progress-test",
            FetchProgress {
                version: 1,
                phase: "downloading".into(),
                unit: Some("files".into()),
                completed: Some(2),
                total: Some(7),
            },
        );
        manager.record_progress("progress-test", FetchProgress::phase("resuming_snapshot"));
        manager.record_progress("progress-test", FetchProgress::phase("validating_revision"));

        let job = manager.get("progress-test").unwrap().unwrap();
        assert_eq!(job.phase, "validating_revision");
        assert_eq!(job.progress_bytes, Some(75));
        assert_eq!(job.total_bytes, Some(100));
        assert_eq!(job.progress_files, Some(2));
        assert_eq!(job.total_files, Some(7));
        assert!(job.resumed);
        assert!(job.last_progress_at.is_some());
    }

    fn prefetch_request() -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/admin/v1/jobs")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-modelkeep-csrf", "1")
            .header("idempotency-key", "prefetch-model")
            .body(Body::from(
                r#"{"kind":"prefetch","repo_id":"org/model","revision":"main"}"#,
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn management_inventory_requires_authorization_and_paginates() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let app = router(
            archive,
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            None,
        )
        .unwrap();

        let denied = app
            .clone()
            .oneshot(request("/api/admin/v1/repositories", None))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let allowed = app
            .oneshot(request(
                "/api/admin/v1/repositories?limit=1",
                Some("secret"),
            ))
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        assert!(allowed
            .headers()
            .get("access-control-allow-origin")
            .is_none());
        let body = to_bytes(allowed.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["items"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn management_inventory_distinguishes_model_and_dataset_namespaces() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        for (repo_type, commit, bytes) in [
            (RepositoryType::Model, "a".repeat(40), b"model".as_slice()),
            (
                RepositoryType::Dataset,
                "b".repeat(40),
                b"dataset".as_slice(),
            ),
        ] {
            archive
                .publish_revision_for_type(
                    repo_type,
                    PublishRequest {
                        repo_id: "org/shared".into(),
                        requested_revision: "main".into(),
                        commit,
                        files: vec![ArchiveFile {
                            path: "data.bin".into(),
                            bytes: bytes.to_vec(),
                        }],
                    },
                )
                .unwrap();
        }
        let app = router(
            archive,
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            None,
        )
        .unwrap();

        let response = app
            .clone()
            .oneshot(request(
                "/api/admin/v1/repositories?limit=10",
                Some("secret"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(value["items"].as_array().unwrap().len(), 2);
        assert_eq!(value["items"][0]["repo_type"], "model");
        assert_eq!(value["items"][1]["repo_type"], "dataset");

        let detail = app
            .clone()
            .oneshot(request(
                "/api/admin/v1/repositories/dataset/org/shared",
                Some("secret"),
            ))
            .await
            .unwrap();
        assert_eq!(detail.status(), StatusCode::OK);
        let detail: serde_json::Value =
            serde_json::from_slice(&to_bytes(detail.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(detail["repo_type"], "dataset");
        assert_eq!(detail["revisions"][0]["logical_bytes"], 7);

        let legacy_model_detail = app
            .clone()
            .oneshot(request(
                "/api/admin/v1/repositories/org/shared",
                Some("secret"),
            ))
            .await
            .unwrap();
        assert_eq!(legacy_model_detail.status(), StatusCode::OK);
        let legacy_model_detail: serde_json::Value = serde_json::from_slice(
            &to_bytes(legacy_model_detail.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(legacy_model_detail["repo_type"], "model");

        let status = app
            .oneshot(request("/api/admin/v1/status", Some("secret")))
            .await
            .unwrap();
        let status: serde_json::Value =
            serde_json::from_slice(&to_bytes(status.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(status["repository_count"], 2);
        assert_eq!(status["model_repository_count"], 1);
        assert_eq!(status["dataset_repository_count"], 1);
    }

    #[tokio::test]
    async fn management_status_reports_cached_readiness_without_probing_storage() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        archive.check_readiness().unwrap();
        let app = router(
            archive,
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            None,
        )
        .unwrap();

        std::fs::remove_dir_all(directory.path().join("tmp")).unwrap();
        for _ in 0..3 {
            let response = app
                .clone()
                .oneshot(request("/api/admin/v1/status", Some("secret")))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value["ready"], true);
            assert_eq!(
                value["archive_filesystem_path"],
                directory.path().display().to_string()
            );
            assert!(value["archive_filesystem_total_bytes"].as_u64().unwrap() > 0);
            assert!(
                value["archive_filesystem_available_bytes"]
                    .as_u64()
                    .unwrap()
                    <= value["archive_filesystem_total_bytes"].as_u64().unwrap()
            );
            assert!(
                value["archive_filesystem_available_percent"]
                    .as_u64()
                    .unwrap()
                    <= 100
            );
            assert!(value["archive_filesystem_low_space"].is_boolean());
            assert!(!directory.path().join("tmp").exists());
        }
    }

    #[tokio::test]
    async fn management_status_distinguishes_an_unchecked_archive_from_a_clean_one() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        archive
            .publish_revision(crate::PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: "a".repeat(40),
                files: vec![crate::ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"{}".to_vec(),
                }],
            })
            .unwrap();
        let app = router(
            archive.clone(),
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            None,
        )
        .unwrap();
        let read_status = || {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(request("/api/admin/v1/status", Some("secret")))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                serde_json::from_slice::<serde_json::Value>(
                    &to_bytes(response.into_body(), usize::MAX).await.unwrap(),
                )
                .unwrap()
            }
        };

        let before = read_status().await;
        assert_eq!(before["self_check"]["status"], "never_run");

        archive.self_check();
        let clean = read_status().await;
        assert_eq!(clean["self_check"]["status"], "clean");
        assert_eq!(clean["self_check"]["finding_count"], 0);
        assert_eq!(clean["self_check"]["revisions_checked"], 1);
        assert_eq!(clean["self_check"]["files_checked"], 1);
        assert!(clean["self_check"]["completed_at"].as_u64().unwrap() > 0);
        assert!(clean["self_check"]["duration_ms"].is_number());
        assert_eq!(
            clean["self_check"]["findings_by_kind"],
            serde_json::json!({})
        );

        std::fs::remove_file(
            directory
                .path()
                .join("models/org/model/revisions")
                .join("a".repeat(40))
                .join("config.json"),
        )
        .unwrap();
        archive.self_check();
        let damaged = read_status().await;
        assert_eq!(damaged["self_check"]["status"], "findings");
        assert_eq!(damaged["self_check"]["finding_count"], 1);
        assert_eq!(
            damaged["self_check"]["findings_by_kind"],
            serde_json::json!({ "missing_file": 1 })
        );
    }

    #[test]
    fn filesystem_capacity_warns_at_ten_percent_available() {
        let warning = filesystem_capacity_from_blocks(100, 10, 4096);
        assert_eq!(warning.total_bytes, 409_600);
        assert_eq!(warning.available_bytes, 40_960);
        assert_eq!(warning.available_percent, 10);
        assert!(warning.low_space);

        let healthy = filesystem_capacity_from_blocks(100, 11, 4096);
        assert_eq!(healthy.available_percent, 11);
        assert!(!healthy.low_space);

        let just_above_threshold = filesystem_capacity_from_blocks(10_000, 1_001, 4096);
        assert_eq!(just_above_threshold.available_percent, 10);
        assert!(!just_above_threshold.low_space);
    }

    #[tokio::test]
    async fn job_submission_requires_csrf_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let app = router(
            archive,
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            None,
        )
        .unwrap();

        let denied = app
            .clone()
            .oneshot(job_request(false, "audit-once"))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        let first = app
            .clone()
            .oneshot(job_request(true, "audit-once"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        let first: serde_json::Value =
            serde_json::from_slice(&to_bytes(first.into_body(), usize::MAX).await.unwrap())
                .unwrap();

        let repeated = app
            .clone()
            .oneshot(job_request(true, "audit-once"))
            .await
            .unwrap();
        assert_eq!(repeated.status(), StatusCode::OK);
        let repeated: serde_json::Value =
            serde_json::from_slice(&to_bytes(repeated.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(first["id"], repeated["id"]);
        assert!(first.get("idempotency_hash").is_none());
        assert!(first["progress_bytes"].is_null());
        assert!(first["total_bytes"].is_null());

        let conflict = app
            .oneshot(job_request_with_body(
                true,
                "audit-once",
                r#"{"kind":"prefetch","repo_id":"org/model","revision":"main"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn initial_job_persistence_never_overwrites_an_existing_id() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let manager = JobManager::open(&archive).unwrap();
        let original = stored_job("collision", 1);
        assert!(manager.persist_new(&original).unwrap());

        let mut replacement = stored_job("collision", 2);
        replacement.message = Some("must not replace".into());
        assert!(!manager.persist_new(&replacement).unwrap());
        let persisted: Job = serde_json::from_slice(
            &fs::read(manager.inner.directory.join("collision.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted.created_at, 1);
        assert_eq!(persisted.message, None);
    }

    #[test]
    fn equivalent_active_job_is_reused_across_keys_and_principals() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Arc::new(Archive::new(directory.path()).unwrap());
        let manager = JobManager::open(&archive).unwrap();
        let mut existing = stored_job("active-prefetch", 1);
        existing.kind = JobKind::Prefetch;
        existing.state = JobState::Running;
        existing.repo_id = Some("org/model".into());
        existing.revision = Some("main".into());
        existing.principal = Some(PrincipalView {
            auth_method: "tailscale".into(),
            login: Some("first@example.com".into()),
            name: None,
        });
        manager
            .inner
            .active_jobs
            .lock()
            .unwrap()
            .insert(existing.id.clone(), existing.clone());

        let (reused, created) = manager
            .submit(
                JobRequest {
                    kind: JobKind::Prefetch,
                    repo_type: RepositoryType::Model,
                    repo_id: Some("org/model".into()),
                    revision: Some("main".into()),
                    include: None,
                    exclude: None,
                },
                Some("different-key"),
                archive,
                None,
                PrincipalView {
                    auth_method: "tailscale".into(),
                    login: Some("second@example.com".into()),
                    name: None,
                },
            )
            .unwrap();

        assert!(!created);
        assert_eq!(reused.id, existing.id);
        assert_eq!(reused.principal, existing.principal);
        assert_eq!(manager.inner.active_jobs.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn terminal_or_different_jobs_do_not_block_submission() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Arc::new(Archive::new(directory.path()).unwrap());
        let manager = JobManager::open(&archive).unwrap();
        let mut terminal = stored_job("completed-prefetch", 1);
        terminal.kind = JobKind::Prefetch;
        terminal.state = JobState::Completed;
        terminal.repo_id = Some("org/model".into());
        terminal.revision = Some("main".into());
        let mut failed = terminal.clone();
        failed.id = "failed-prefetch".into();
        failed.state = JobState::Failed;
        let mut cancelled = terminal.clone();
        cancelled.id = "cancelled-prefetch".into();
        cancelled.state = JobState::Cancelled;
        let mut different = terminal.clone();
        different.id = "different-refresh".into();
        different.kind = JobKind::Refresh;
        different.state = JobState::Running;
        for job in [terminal, failed, cancelled] {
            manager.persist(&job).unwrap();
        }
        manager.persist(&different).unwrap();
        manager
            .inner
            .active_jobs
            .lock()
            .unwrap()
            .insert(different.id.clone(), different);

        let (submitted, created) = manager
            .submit(
                JobRequest {
                    kind: JobKind::Prefetch,
                    repo_type: RepositoryType::Model,
                    repo_id: Some("org/model".into()),
                    revision: Some("main".into()),
                    include: None,
                    exclude: None,
                },
                Some("retry-prefetch"),
                archive,
                None,
                PrincipalView {
                    auth_method: "bearer".into(),
                    login: None,
                    name: None,
                },
            )
            .unwrap();

        assert!(created);
        assert_ne!(submitted.id, "completed-prefetch");
        assert_ne!(submitted.id, "different-refresh");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_equivalent_submissions_create_one_active_job() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Arc::new(Archive::new(directory.path()).unwrap());
        let manager = JobManager::open(&archive).unwrap();
        let fetcher = Arc::new(BlockingFetcher::new());
        let pullthrough = Arc::new(PullThrough::new(archive.as_ref().clone(), fetcher.clone()));
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let mut submissions = Vec::new();

        for index in 0..8 {
            let manager = manager.clone();
            let archive = archive.clone();
            let pullthrough = pullthrough.clone();
            let barrier = barrier.clone();
            submissions.push(tokio::spawn(async move {
                barrier.wait().await;
                manager
                    .submit(
                        JobRequest {
                            kind: JobKind::Prefetch,
                            repo_type: RepositoryType::Model,
                            repo_id: Some("org/concurrent".into()),
                            revision: Some("main".into()),
                            include: None,
                            exclude: None,
                        },
                        Some(&format!("concurrent-{index}")),
                        archive,
                        Some(pullthrough),
                        PrincipalView {
                            auth_method: "bearer".into(),
                            login: None,
                            name: None,
                        },
                    )
                    .unwrap()
            }));
        }

        let mut results = Vec::new();
        for submission in submissions {
            results.push(submission.await.unwrap());
        }
        let job_id = results[0].0.id.clone();
        assert!(results.iter().all(|(job, _)| job.id == job_id));
        assert_eq!(results.iter().filter(|(_, created)| *created).count(), 1);
        assert_eq!(manager.inner.active_jobs.lock().unwrap().len(), 1);

        fetcher.release();
        for _ in 0..100 {
            if manager
                .get(&job_id)
                .is_ok_and(|job| job.is_some_and(|job| job.state == JobState::Completed))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(fetcher.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            manager.get(&job_id).unwrap().unwrap().state,
            JobState::Completed
        );
    }

    #[tokio::test]
    async fn job_pagination_is_stable_for_same_timestamp_ids() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let manager = JobManager::open(&archive).unwrap();
        for id in ["job-a", "job-c", "job-b"] {
            assert!(manager.persist_new(&stored_job(id, 10)).unwrap());
        }
        drop(manager);
        let app = router(
            archive,
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            None,
        )
        .unwrap();

        let first = app
            .clone()
            .oneshot(request("/api/admin/v1/jobs?limit=2", Some("secret")))
            .await
            .unwrap();
        let first: serde_json::Value =
            serde_json::from_slice(&to_bytes(first.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(first["items"][0]["id"], "job-c");
        assert_eq!(first["items"][1]["id"], "job-b");
        assert_eq!(first["next_cursor"], "job-b");

        let second = app
            .clone()
            .oneshot(request(
                "/api/admin/v1/jobs?limit=2&cursor=job-b",
                Some("secret"),
            ))
            .await
            .unwrap();
        let second: serde_json::Value =
            serde_json::from_slice(&to_bytes(second.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(second["items"].as_array().unwrap().len(), 1);
        assert_eq!(second["items"][0]["id"], "job-a");

        let unknown = app
            .oneshot(request(
                "/api/admin/v1/jobs?limit=2&cursor=missing-job",
                Some("secret"),
            ))
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::OK);
        let unknown: serde_json::Value =
            serde_json::from_slice(&to_bytes(unknown.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(unknown["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn startup_keeps_terminal_history_on_disk_and_page_decoding_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let manager = JobManager::open(&archive).unwrap();
        for index in 0..10_000u64 {
            let job = stored_job(&format!("history-{index:05}"), index);
            fs::write(
                manager.inner.directory.join(format!("{}.json", job.id)),
                serde_json::to_vec(&job).unwrap(),
            )
            .unwrap();
            fs::write(manager.inner.index_directory.join(history_key(&job)), b"").unwrap();
        }
        drop(manager);

        let reopened = JobManager::open(&archive).unwrap();
        assert_eq!(reopened.inner.read_count.load(Ordering::Relaxed), 0);
        assert!(reopened.inner.active_jobs.lock().unwrap().is_empty());
        let page = reopened.list_page(25, None).unwrap();
        assert_eq!(page.items.len(), 25);
        assert!(page.next_cursor.is_some());
        assert_eq!(reopened.inner.read_count.load(Ordering::Relaxed), 26);
        assert_eq!(page.items[0].id, "history-09999");
    }

    #[test]
    fn terminal_job_direct_lookup_reads_one_validated_record() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let manager = JobManager::open(&archive).unwrap();
        manager.persist_new(&stored_job("lookup-job", 1)).unwrap();
        manager.inner.read_count.store(0, Ordering::Relaxed);
        assert_eq!(manager.get("lookup-job").unwrap().unwrap().id, "lookup-job");
        assert_eq!(manager.inner.read_count.load(Ordering::Relaxed), 1);
        assert!(matches!(
            manager.get("../escape"),
            Err(ArchiveError::InvalidPath(_))
        ));

        let mut mismatched = stored_job("other-job", 1);
        mismatched.id = "other-job".into();
        fs::write(
            manager.inner.directory.join("lookup-job.json"),
            serde_json::to_vec(&mismatched).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            manager.get("lookup-job"),
            Err(ArchiveError::IntegrityMismatch(_))
        ));
    }

    #[tokio::test]
    async fn idempotency_key_is_scoped_to_request_and_principal() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Arc::new(Archive::new(directory.path()).unwrap());
        let manager = JobManager::open(&archive).unwrap();
        let first = manager.submit(
            JobRequest {
                kind: JobKind::Audit,
                repo_type: RepositoryType::Model,
                repo_id: None,
                revision: None,
                include: None,
                exclude: None,
            },
            Some("shared-key"),
            archive.clone(),
            None,
            PrincipalView {
                auth_method: "tailscale".into(),
                login: Some("first@example.com".into()),
                name: None,
            },
        );
        assert!(first.is_ok());

        let different_principal = manager.submit(
            JobRequest {
                kind: JobKind::Audit,
                repo_type: RepositoryType::Model,
                repo_id: None,
                revision: None,
                include: None,
                exclude: None,
            },
            Some("shared-key"),
            archive,
            None,
            PrincipalView {
                auth_method: "tailscale".into(),
                login: Some("second@example.com".into()),
                name: None,
            },
        );
        assert!(matches!(different_principal, Err("idempotency_conflict")));
    }

    #[tokio::test]
    async fn idempotency_index_survives_restart_without_loading_terminal_job() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Arc::new(Archive::new(directory.path()).unwrap());
        let manager = JobManager::open(&archive).unwrap();
        let request = JobRequest {
            kind: JobKind::Audit,
            repo_type: RepositoryType::Model,
            repo_id: None,
            revision: None,
            include: None,
            exclude: None,
        };
        let principal = PrincipalView {
            auth_method: "bearer".into(),
            login: None,
            name: None,
        };
        let key_hash = hash_idempotency_key("restart-key").unwrap();
        let mut existing = stored_job("idempotent-history", 1);
        existing.idempotency_hash = Some(key_hash);
        existing.idempotency_request_hash =
            Some(hash_legacy_idempotency_request(&request, &principal));
        manager.persist_new(&existing).unwrap();
        drop(manager);

        let reopened = JobManager::open(&archive).unwrap();
        assert!(reopened.inner.active_jobs.lock().unwrap().is_empty());
        let (job, created) = reopened
            .submit(
                request.clone(),
                Some("restart-key"),
                archive.clone(),
                None,
                principal.clone(),
            )
            .unwrap();
        assert!(!created);
        assert_eq!(job.id, "idempotent-history");

        let dataset_request = JobRequest {
            repo_type: RepositoryType::Dataset,
            ..request
        };
        assert!(matches!(
            reopened.submit(
                dataset_request,
                Some("restart-key"),
                archive,
                None,
                principal
            ),
            Err("idempotency_conflict")
        ));
    }

    #[test]
    fn legacy_job_records_without_request_hash_remain_readable() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let manager = JobManager::open(&archive).unwrap();
        let mut value = serde_json::to_value(stored_job("legacy", 1)).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("idempotency_request_hash");
        fs::write(
            manager.inner.directory.join("legacy.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        drop(manager);

        let reopened = JobManager::open(&archive).unwrap();
        assert!(reopened.get("legacy").unwrap().is_some());
    }

    #[tokio::test]
    async fn prefetch_job_publishes_complete_revision() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), Arc::new(FixtureFetcher)));
        let app = router(
            archive.clone(),
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            Some(pullthrough),
        )
        .unwrap();
        let response = app.clone().oneshot(prefetch_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let submitted: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let id = submitted["id"].as_str().unwrap();

        let mut completed = None;
        for _ in 0..100 {
            let response = app
                .clone()
                .oneshot(request(&format!("/api/admin/v1/jobs/{id}"), Some("secret")))
                .await
                .unwrap();
            let value: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            if value["state"] == "completed" {
                completed = Some(value);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let completed = completed.expect("prefetch job did not complete");
        assert_eq!(completed["resolved_commit"], "c".repeat(40));
        assert!(completed["started_at"].as_u64().is_some());
        assert!(completed["finished_at"].as_u64().is_some());
        assert!(completed["finished_at"].as_u64() >= completed["started_at"].as_u64());
        assert!(archive
            .is_complete_revision("org/model", &"c".repeat(40))
            .unwrap());
    }

    #[test]
    fn active_jobs_become_interrupted_failures_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let manager = JobManager::open(&archive).unwrap();
        let job = Job {
            id: "restart-test".into(),
            kind: JobKind::Audit,
            state: JobState::Running,
            phase: "auditing_archive".into(),
            repo_type: RepositoryType::Model,
            repo_id: None,
            revision: None,
            include: Vec::new(),
            exclude: Vec::new(),
            outcome: None,
            resolved_commit: None,
            resumed: false,
            progress_bytes: None,
            total_bytes: None,
            progress_files: None,
            total_files: None,
            last_progress_at: None,
            started_at: Some(1),
            finished_at: None,
            principal: None,
            error_class: None,
            message: None,
            idempotency_hash: None,
            idempotency_request_hash: None,
            created_at: 1,
            updated_at: 1,
        };
        manager.persist(&job).unwrap();
        drop(manager);

        let reopened = JobManager::open(&archive).unwrap();
        let interrupted = reopened.get("restart-test").unwrap().unwrap();
        assert_eq!(interrupted.state, JobState::Failed);
        assert_eq!(interrupted.error_class.as_deref(), Some("interrupted"));
    }

    #[test]
    fn legacy_job_directory_is_indexed_once_and_active_job_is_recovered() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let jobs = directory.path().join("state/jobs");
        fs::create_dir_all(&jobs).unwrap();
        let terminal = stored_job("legacy-terminal", 1);
        fs::write(
            jobs.join("legacy-terminal.json"),
            serde_json::to_vec(&terminal).unwrap(),
        )
        .unwrap();
        let mut active = stored_job("legacy-active", 2);
        active.state = JobState::Running;
        active.phase = "auditing_archive".into();
        active.finished_at = None;
        fs::write(
            jobs.join("legacy-active.json"),
            serde_json::to_vec(&active).unwrap(),
        )
        .unwrap();
        fs::write(jobs.join("malformed.json"), b"not-json").unwrap();

        let manager = JobManager::open(&archive).unwrap();
        assert!(jobs.join(".index-v1").is_file());
        assert_eq!(manager.list_page(10, None).unwrap().items.len(), 2);
        let recovered = manager.get("legacy-active").unwrap().unwrap();
        assert_eq!(recovered.state, JobState::Failed);
        assert_eq!(recovered.error_class.as_deref(), Some("interrupted"));
        assert!(manager.inner.active_jobs.lock().unwrap().is_empty());
        assert!(matches!(
            manager.get("malformed"),
            Err(ArchiveError::IntegrityMismatch(_))
        ));
    }

    #[test]
    fn malformed_active_record_does_not_block_other_restart_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let manager = JobManager::open(&archive).unwrap();
        let mut healthy = stored_job("healthy-active", 1);
        healthy.state = JobState::Running;
        healthy.finished_at = None;
        manager.persist(&healthy).unwrap();
        fs::write(
            manager.inner.directory.join("broken-active.json"),
            b"broken",
        )
        .unwrap();
        fs::write(manager.inner.active_directory.join("broken-active"), b"").unwrap();
        drop(manager);

        let reopened = JobManager::open(&archive).unwrap();
        assert_eq!(
            reopened.get("healthy-active").unwrap().unwrap().state,
            JobState::Failed
        );
        assert!(reopened
            .inner
            .active_directory
            .join("broken-active")
            .is_file());
        assert!(matches!(
            reopened.get("broken-active"),
            Err(ArchiveError::IntegrityMismatch(_))
        ));
    }

    #[test]
    fn capability_headers_require_explicit_trust() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "tailscale-app-capabilities",
            r#"{"io.modelkeep/cap/admin":[{}]}"#.parse().unwrap(),
        );
        let mut config = Config::token("127.0.0.1:0".parse().unwrap(), "secret");
        assert!(!authorized(&config, &headers));
        config.trust_tailscale_headers = true;
        assert!(authorized(&config, &headers));
        headers.insert(
            "tailscale-user-login",
            "operator@example.com".parse().unwrap(),
        );
        headers.insert("tailscale-user-name", "Example Operator".parse().unwrap());
        let principal = authenticate(&config, &headers).unwrap();
        assert_eq!(principal.auth_method, "tailscale");
        assert_eq!(principal.login.as_deref(), Some("operator@example.com"));
        assert_eq!(principal.name.as_deref(), Some("Example Operator"));
    }

    /// Fetcher that records every acquisition request and can enumerate
    /// upstream, so a selection can be observed end to end and a published
    /// revision can be reconciled against upstream's file list.
    #[derive(Default)]
    struct SelectionFetcher {
        requests: Mutex<Vec<FetchRequest>>,
    }

    impl SelectionFetcher {
        const FILES: [&'static str; 2] = ["config.json", "weights/model.bin"];

        fn matches(path: &str, patterns: &[String]) -> bool {
            patterns
                .iter()
                .any(|pattern| match pattern.strip_suffix('*') {
                    Some(prefix) => path.starts_with(prefix),
                    None => pattern.as_str() == path,
                })
        }

        fn matched(include: &[String], exclude: &[String]) -> Vec<String> {
            Self::FILES
                .into_iter()
                .filter(|path| include.is_empty() || Self::matches(path, include))
                .filter(|path| !Self::matches(path, exclude))
                .map(str::to_string)
                .collect()
        }

        fn selections(&self) -> Vec<(Vec<String>, Vec<String>)> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|request| (request.files.clone(), request.exclude.clone()))
                .collect()
        }
    }

    impl UpstreamFetcher for SelectionFetcher {
        fn fetch(&self, request: &FetchRequest) -> Result<FetchedRevision, UpstreamError> {
            self.requests.lock().unwrap().push(request.clone());
            let files = Self::matched(&request.files, &request.exclude);
            std::fs::create_dir_all(&request.staging).map_err(UpstreamError::Io)?;
            for path in &files {
                let target = request.staging.join(path);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(UpstreamError::Io)?;
                }
                std::fs::write(target, path.as_bytes()).map_err(UpstreamError::Io)?;
            }
            Ok(FetchedRevision {
                commit: "e".repeat(40),
                files,
                staging: request.staging.clone(),
            })
        }

        fn inventory(
            &self,
            request: &InventoryRequest,
        ) -> Result<Option<RevisionInventory>, UpstreamError> {
            Ok(Some(RevisionInventory {
                commit: "e".repeat(40),
                files: Self::matched(&request.files, &request.exclude),
            }))
        }
    }

    fn selected_prefetch(include: Option<Vec<&str>>, exclude: Option<Vec<&str>>) -> JobRequest {
        JobRequest {
            kind: JobKind::Prefetch,
            repo_type: RepositoryType::Model,
            repo_id: Some("org/model".into()),
            revision: Some("main".into()),
            include: include.map(|patterns| patterns.into_iter().map(String::from).collect()),
            exclude: exclude.map(|patterns| patterns.into_iter().map(String::from).collect()),
        }
    }

    fn test_principal() -> PrincipalView {
        PrincipalView {
            auth_method: "bearer".into(),
            login: None,
            name: None,
        }
    }

    async fn submit_body(app: &Router, key: &'static str, body: &'static str) -> serde_json::Value {
        let response = app
            .clone()
            .oneshot(job_request_with_body(true, key, body))
            .await
            .unwrap();
        let status = response.status();
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(status, StatusCode::ACCEPTED, "{value}");
        value
    }

    async fn terminal_job(app: &Router, id: &str) -> serde_json::Value {
        for _ in 0..200 {
            let response = app
                .clone()
                .oneshot(request(&format!("/api/admin/v1/jobs/{id}"), Some("secret")))
                .await
                .unwrap();
            let value: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            if value["state"] == "completed" || value["state"] == "failed" {
                return value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("job {id} did not reach a terminal state");
    }

    #[tokio::test]
    async fn prefetch_selection_reaches_the_acquisition_helper() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let fetcher = Arc::new(SelectionFetcher::default());
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher.clone()));
        let app = router(
            archive.clone(),
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            Some(pullthrough),
        )
        .unwrap();

        let submitted = submit_body(
            &app,
            "selected-prefetch",
            r#"{"kind":"prefetch","repo_id":"org/model","revision":"main","include":["weights/*","config.json"],"exclude":["weights/model.bin"]}"#,
        )
        .await;
        let completed = terminal_job(&app, submitted["id"].as_str().unwrap()).await;

        assert_eq!(completed["state"], "completed");
        assert_eq!(completed["outcome"], "published");
        assert_eq!(
            completed["include"],
            serde_json::json!(["config.json", "weights/*"])
        );
        assert_eq!(
            completed["exclude"],
            serde_json::json!(["weights/model.bin"])
        );
        assert_eq!(
            fetcher.selections(),
            vec![(
                vec!["config.json".to_string(), "weights/*".to_string()],
                vec!["weights/model.bin".to_string()]
            )]
        );
        // Only the selected file was archived; the excluded one was not.
        let manifest = archive
            .manifest_for_type(RepositoryType::Model, "org/model", &"e".repeat(40))
            .unwrap();
        assert!(manifest.contains("config.json"));
        assert!(!manifest.contains("weights/model.bin"));
    }

    #[tokio::test]
    async fn unsafe_selection_patterns_are_rejected_as_file_selection_rejects_them() {
        for pattern in [
            "../secrets.bin",
            "/etc/passwd",
            "weights/../../escape",
            "!weights/model.bin",
            ".modelkeep-state/lease",
            "weights\\model.bin",
            "",
        ] {
            let patterns = vec![pattern.to_string()];
            assert!(
                FileSelection::new(&patterns, &[]).is_err(),
                "expected {pattern:?} to be unsafe"
            );
            assert_eq!(
                validate_job_request(&selected_prefetch(Some(vec![pattern]), None)).err(),
                Some("invalid_request"),
                "include {pattern:?}"
            );
            assert_eq!(
                validate_job_request(&selected_prefetch(None, Some(vec![pattern]))).err(),
                Some("invalid_request"),
                "exclude {pattern:?}"
            );
        }
        for pattern in ["config.json", "weights/*", "weights/"] {
            assert!(FileSelection::new(&[pattern.to_string()], &[]).is_ok());
            assert!(validate_job_request(&selected_prefetch(Some(vec![pattern]), None)).is_ok());
        }

        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let app = router(
            archive,
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            Some(Arc::new(PullThrough::new(
                Archive::new(directory.path()).unwrap(),
                Arc::new(SelectionFetcher::default()),
            ))),
        )
        .unwrap();
        let response = app
            .oneshot(job_request_with_body(
                true,
                "traversal-selection",
                r#"{"kind":"prefetch","repo_id":"org/model","revision":"main","include":["../etc/passwd"]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"], "invalid_request");
    }

    #[tokio::test]
    async fn selection_is_rejected_for_kinds_that_do_not_acquire() {
        for kind in [JobKind::Refresh, JobKind::Verify, JobKind::Audit] {
            let targeted = kind != JobKind::Audit;
            let base = JobRequest {
                kind,
                repo_type: RepositoryType::Model,
                repo_id: targeted.then(|| "org/model".to_string()),
                revision: targeted.then(|| "main".to_string()),
                include: None,
                exclude: None,
            };
            assert!(validate_job_request(&base).is_ok());
            assert_eq!(
                validate_job_request(&JobRequest {
                    include: Some(vec!["config.json".into()]),
                    ..base.clone()
                })
                .err(),
                Some("invalid_request"),
                "{kind:?} include"
            );
            assert_eq!(
                validate_job_request(&JobRequest {
                    exclude: Some(vec!["config.json".into()]),
                    ..base
                })
                .err(),
                Some("invalid_request"),
                "{kind:?} exclude"
            );
        }

        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let app = router(
            archive,
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            None,
        )
        .unwrap();
        let response = app
            .oneshot(job_request_with_body(
                true,
                "refresh-selection",
                r#"{"kind":"refresh","repo_id":"org/model","revision":"main","include":["config.json"]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn identical_selections_deduplicate_and_different_selections_are_separate_jobs() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Arc::new(Archive::new(directory.path()).unwrap());
        let manager = JobManager::open(&archive).unwrap();

        let (first, created) = manager
            .submit(
                selected_prefetch(Some(vec!["weights/*"]), None),
                Some("selection-key"),
                archive.clone(),
                None,
                test_principal(),
            )
            .unwrap();
        assert!(created);

        let (repeated, created) = manager
            .submit(
                selected_prefetch(Some(vec!["weights/*"]), None),
                Some("selection-key"),
                archive.clone(),
                None,
                test_principal(),
            )
            .unwrap();
        assert!(!created);
        assert_eq!(repeated.id, first.id);

        let (other, created) = manager
            .submit(
                selected_prefetch(Some(vec!["config.json"]), None),
                Some("other-selection-key"),
                archive.clone(),
                None,
                test_principal(),
            )
            .unwrap();
        assert!(created);
        assert_ne!(other.id, first.id);
        assert_eq!(other.include, vec!["config.json".to_string()]);

        // Reusing one key for a different selection is a conflict, not a reuse.
        assert!(matches!(
            manager.submit(
                selected_prefetch(Some(vec!["config.json"]), None),
                Some("selection-key"),
                archive,
                None,
                test_principal(),
            ),
            Err("idempotency_conflict")
        ));
    }

    #[test]
    fn selection_normalization_decides_idempotency_identity() {
        let principal = test_principal();
        let ordered = selected_prefetch(Some(vec!["a.bin", "b.bin"]), Some(vec!["c.bin"]));
        let shuffled = selected_prefetch(
            Some(vec!["b.bin", "a.bin", "b.bin"]),
            Some(vec!["c.bin", "c.bin"]),
        );
        let swapped = selected_prefetch(Some(vec!["c.bin"]), Some(vec!["a.bin", "b.bin"]));
        let hash = |request: &JobRequest| {
            let selection = validate_job_request(request).unwrap();
            hash_idempotency_request(request, &selection, &principal)
        };
        assert_eq!(hash(&ordered), hash(&shuffled));
        assert_ne!(hash(&ordered), hash(&swapped));
        assert_ne!(hash(&ordered), hash(&selected_prefetch(None, None)));

        let directory = tempfile::tempdir().unwrap();
        let archive = Arc::new(Archive::new(directory.path()).unwrap());
        let manager = JobManager::open(&archive).unwrap();
        let (first, created) = manager
            .submit(
                ordered,
                Some("normalized-key"),
                archive.clone(),
                None,
                principal.clone(),
            )
            .unwrap();
        assert!(created);
        let (repeated, created) = manager
            .submit(shuffled, Some("normalized-key"), archive, None, principal)
            .unwrap();
        assert!(!created);
        assert_eq!(repeated.id, first.id);
    }

    #[tokio::test]
    async fn prefetch_records_publication_then_an_already_archived_no_op() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), Arc::new(FixtureFetcher)));
        let app = router(
            archive,
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            Some(pullthrough),
        )
        .unwrap();

        let first = submit_body(
            &app,
            "outcome-first",
            r#"{"kind":"prefetch","repo_id":"org/model","revision":"main"}"#,
        )
        .await;
        let first = terminal_job(&app, first["id"].as_str().unwrap()).await;
        assert_eq!(first["state"], "completed");
        assert_eq!(first["outcome"], "published");

        let second = submit_body(
            &app,
            "outcome-second",
            r#"{"kind":"prefetch","repo_id":"org/model","revision":"main"}"#,
        )
        .await;
        let second = terminal_job(&app, second["id"].as_str().unwrap()).await;
        assert_eq!(second["state"], "completed");
        assert_eq!(second["outcome"], "already_archived");
        assert_ne!(second["outcome"], first["outcome"]);
    }

    #[tokio::test]
    async fn prefetch_that_extends_a_revision_records_the_extension() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let fetcher = Arc::new(SelectionFetcher::default());
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher.clone()));
        let app = router(
            archive.clone(),
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            Some(pullthrough),
        )
        .unwrap();

        let selected = submit_body(
            &app,
            "extend-selected",
            r#"{"kind":"prefetch","repo_id":"org/model","revision":"main","include":["config.json"]}"#,
        )
        .await;
        let selected = terminal_job(&app, selected["id"].as_str().unwrap()).await;
        assert_eq!(selected["outcome"], "published");

        let whole = submit_body(
            &app,
            "extend-whole",
            r#"{"kind":"prefetch","repo_id":"org/model","revision":"main"}"#,
        )
        .await;
        let whole = terminal_job(&app, whole["id"].as_str().unwrap()).await;
        assert_eq!(whole["state"], "completed");
        assert_eq!(whole["outcome"], "extended");
        // The extension acquired only the path the revision lacked.
        assert_eq!(
            fetcher.selections().last().unwrap().0,
            vec!["weights/model.bin".to_string()]
        );
        let manifest = archive
            .manifest_for_type(RepositoryType::Model, "org/model", &"e".repeat(40))
            .unwrap();
        assert!(manifest.contains("config.json"));
        assert!(manifest.contains("weights/model.bin"));
    }

    #[test]
    fn job_records_without_selection_or_outcome_remain_readable() {
        let mut job = stored_job("legacy-outcome", 1);
        job.kind = JobKind::Prefetch;
        job.include = vec!["config.json".into()];
        job.outcome = Some(JobOutcome::Extended);
        let mut value = serde_json::to_value(&job).unwrap();
        assert_eq!(value["outcome"], "extended");
        assert_eq!(value["include"], serde_json::json!(["config.json"]));
        for field in ["include", "exclude", "outcome"] {
            value.as_object_mut().unwrap().remove(field);
        }

        let legacy: Job = serde_json::from_value(value.clone()).unwrap();
        assert!(legacy.include.is_empty());
        assert!(legacy.exclude.is_empty());
        assert_eq!(legacy.outcome, None);

        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let manager = JobManager::open(&archive).unwrap();
        fs::write(
            manager.inner.directory.join("legacy-outcome.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        drop(manager);

        let reopened = JobManager::open(&archive).unwrap();
        let recovered = reopened.get("legacy-outcome").unwrap().unwrap();
        assert_eq!(recovered.outcome, None);
        assert!(recovered.include.is_empty());
    }
}
