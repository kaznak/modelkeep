use std::{
    collections::BTreeMap,
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

use crate::upstream::FetchProgress;
use crate::{
    pullthrough::PullThrough, validate_repository_id, validate_revision_ref, Archive, ArchiveError,
    RepositorySummary,
};

const ADMIN_CAPABILITY: &str = "io.modelkeep/cap/admin";
static JOB_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct Config {
    pub address: SocketAddr,
    bearer_token: Option<String>,
    trust_tailscale_headers: bool,
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
    logical_archive_bytes: u64,
    principal: PrincipalView,
    auth_methods: Vec<&'static str>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Job {
    id: String,
    kind: JobKind,
    state: JobState,
    phase: String,
    repo_id: Option<String>,
    revision: Option<String>,
    resolved_commit: Option<String>,
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
    repo_id: Option<String>,
    revision: Option<String>,
    resolved_commit: Option<String>,
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
            repo_id: job.repo_id,
            revision: job.revision,
            resolved_commit: job.resolved_commit,
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

#[derive(Debug, Deserialize)]
struct JobRequest {
    kind: JobKind,
    repo_id: Option<String>,
    revision: Option<String>,
}

#[derive(Clone)]
struct JobManager {
    inner: Arc<JobManagerInner>,
}

struct JobManagerInner {
    directory: PathBuf,
    jobs: Mutex<BTreeMap<String, Job>>,
}

impl JobManager {
    fn open(archive: &Archive) -> Result<Self, ArchiveError> {
        let directory = archive.root.join("state").join("jobs");
        fs::create_dir_all(&directory)?;
        let manager = Self {
            inner: Arc::new(JobManagerInner {
                directory,
                jobs: Mutex::new(BTreeMap::new()),
            }),
        };
        for entry in fs::read_dir(&manager.inner.directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            let mut job: Job = serde_json::from_slice(&fs::read(entry.path())?)
                .map_err(|error| ArchiveError::IntegrityMismatch(error.to_string()))?;
            if matches!(job.state, JobState::Queued | JobState::Running) {
                job.state = JobState::Failed;
                job.phase = "interrupted".into();
                job.error_class = Some("interrupted".into());
                job.message = Some("job interrupted by process restart".into());
                let now = unix_timestamp();
                job.finished_at = Some(now);
                job.updated_at = now;
                manager.persist(&job)?;
            }
            manager
                .inner
                .jobs
                .lock()
                .unwrap()
                .insert(job.id.clone(), job);
        }
        Ok(manager)
    }

    fn list(&self) -> Vec<Job> {
        let mut jobs = self
            .inner
            .jobs
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        jobs
    }

    fn get(&self, id: &str) -> Option<Job> {
        self.inner.jobs.lock().unwrap().get(id).cloned()
    }

    fn submit(
        &self,
        request: JobRequest,
        idempotency_key: Option<&str>,
        archive: Arc<Archive>,
        pullthrough: Option<Arc<PullThrough>>,
        principal: PrincipalView,
    ) -> Result<(Job, bool), &'static str> {
        validate_job_request(&request)?;
        let idempotency_hash = idempotency_key.map(hash_idempotency_key).transpose()?;
        let idempotency_request_hash = idempotency_hash
            .as_ref()
            .map(|_| hash_idempotency_request(&request, &principal));
        let mut jobs = self.inner.jobs.lock().unwrap();
        if let Some(hash) = &idempotency_hash {
            if let Some(existing) = jobs
                .values()
                .find(|job| job.idempotency_hash.as_ref() == Some(hash))
                .cloned()
            {
                return if existing.idempotency_request_hash == idempotency_request_hash {
                    Ok((existing, false))
                } else {
                    Err("idempotency_conflict")
                };
            }
        }
        let now = unix_timestamp();
        let mut job = None;
        for _ in 0..16 {
            let candidate = Job {
                id: new_job_id(now),
                kind: request.kind,
                state: JobState::Queued,
                phase: "queued".into(),
                repo_id: request.repo_id.clone(),
                revision: request.revision.clone(),
                resolved_commit: None,
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
        tokio::task::spawn_blocking(move || manager.run(&job_id, archive, pullthrough));
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
        let result: Result<Option<String>, (&'static str, String)> = match job.kind {
            JobKind::Prefetch => pullthrough
                .as_ref()
                .ok_or_else(|| ("upstream_disabled", "pull-through is disabled".into()))
                .and_then(|pullthrough| {
                    pullthrough
                        .ensure_with_progress(
                            job.repo_id.as_deref().unwrap(),
                            job.revision.as_deref().unwrap(),
                            &[],
                            &progress,
                        )
                        .map(Some)
                        .map_err(classify_pullthrough_error)
                }),
            JobKind::Refresh => pullthrough
                .as_ref()
                .ok_or_else(|| ("upstream_disabled", "pull-through is disabled".into()))
                .and_then(|pullthrough| {
                    pullthrough
                        .refresh_with_progress(
                            job.repo_id.as_deref().unwrap(),
                            job.revision.as_deref().unwrap(),
                            false,
                            &progress,
                        )
                        .map(|result| Some(result.proposed))
                        .map_err(classify_pullthrough_error)
                }),
            JobKind::Verify => archive
                .verify_revision(
                    job.repo_id.as_deref().unwrap(),
                    job.revision.as_deref().unwrap(),
                )
                .map(|_| job.revision.clone())
                .map_err(classify_archive_error),
            JobKind::Audit => archive
                .audit()
                .map_err(classify_archive_error)
                .and_then(|report| {
                    if report.failures.is_empty() {
                        Ok(None)
                    } else {
                        Err((
                            "integrity",
                            format!("{} revisions failed audit", report.failures.len()),
                        ))
                    }
                }),
        };
        match result {
            Ok(commit) => {
                self.update(id, |job| {
                    job.state = JobState::Completed;
                    job.phase = "completed".into();
                    job.resolved_commit = commit;
                    job.finished_at = Some(unix_timestamp());
                });
            }
            Err((class, message)) => {
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
        let mut jobs = self.inner.jobs.lock().unwrap();
        let job = jobs.get_mut(id).ok_or("not_found")?;
        if job.state != JobState::Queued {
            return Err("not_cancellable");
        }
        job.state = JobState::Cancelled;
        job.phase = "cancelled".into();
        let now = unix_timestamp();
        job.finished_at = Some(now);
        job.updated_at = now;
        let snapshot = job.clone();
        drop(jobs);
        self.persist(&snapshot).map_err(|_| "storage")?;
        Ok(snapshot)
    }

    fn record_progress(&self, id: &str, event: FetchProgress) {
        let snapshot = self.update(id, |job| {
            job.phase = event.phase.clone();
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
        let mut jobs = self.inner.jobs.lock().unwrap();
        let job = jobs.get_mut(id)?;
        update(job);
        job.updated_at = unix_timestamp();
        let snapshot = job.clone();
        drop(jobs);
        if let Err(error) = self.persist(&snapshot) {
            tracing::error!(event = "admin_job_persist_failed", job_id = %id, error = %error, "failed to persist management job");
        }
        Some(snapshot)
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
        let result = self.persist(job);
        let remove_result = fs::remove_file(&reservation);
        File::open(&self.inner.directory)?.sync_all()?;
        result?;
        remove_result?;
        Ok(true)
    }
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
    let all_jobs = state.jobs.list();
    let start = query.cursor.as_ref().map_or(0, |cursor| {
        all_jobs
            .iter()
            .position(|job| &job.id == cursor)
            .map_or(all_jobs.len(), |index| index + 1)
    });
    let mut jobs = all_jobs
        .into_iter()
        .skip(start)
        .take(limit + 1)
        .collect::<Vec<_>>();
    let next_cursor = (jobs.len() > limit).then(|| jobs[limit - 1].id.clone());
    jobs.truncate(limit);
    Json(JobPage {
        items: jobs.into_iter().map(JobView::from).collect(),
        next_cursor,
    })
    .into_response()
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
        Some(job) => Json(JobView::from(job)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorBody { error: "not_found" }),
        )
            .into_response(),
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
    match state.archive.list_repositories() {
        Ok(repositories) => Json(StatusBody {
            version: env!("CARGO_PKG_VERSION"),
            // Startup and `/readyz` perform the active write probe. Management UI
            // polling only reports its last result so it cannot create continuous
            // fsync traffic on the archive volume.
            ready: state.archive.last_readiness().unwrap_or(false),
            pullthrough_enabled,
            repository_count: repositories.len(),
            logical_archive_bytes: repositories
                .iter()
                .map(|repository| repository.logical_bytes)
                .sum(),
            principal,
            auth_methods: state.config.auth_methods(),
        })
        .into_response(),
        Err(error) => archive_error(error),
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
    match state.archive.list_repositories() {
        Ok(repositories) => {
            let mut filtered = repositories
                .into_iter()
                .filter(|repository| {
                    query
                        .cursor
                        .as_ref()
                        .is_none_or(|cursor| repository.repo_id > *cursor)
                })
                .take(limit + 1)
                .collect::<Vec<_>>();
            let next_cursor = (filtered.len() > limit).then(|| filtered[limit - 1].repo_id.clone());
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
    Path((namespace, repository)): Path<(String, String)>,
) -> Response {
    if !authorized(&state.config, &headers) {
        return unauthorized(&state.config);
    }
    match state
        .archive
        .repository_inventory(&format!("{namespace}/{repository}"))
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

fn validate_job_request(request: &JobRequest) -> Result<(), &'static str> {
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
    Ok(())
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

fn hash_idempotency_request(request: &JobRequest, principal: &PrincipalView) -> String {
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
        PullThroughError::UpstreamUnavailable | PullThroughError::UpstreamFailed => "upstream",
        PullThroughError::UpstreamNotFound => "not_found",
        PullThroughError::UpstreamUnauthorized => "authorization",
        PullThroughError::Integrity | PullThroughError::UpstreamInvalidOutput => "integrity",
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
    use crate::upstream::{FetchRequest, FetchedRevision, UpstreamError, UpstreamFetcher};
    use axum::{body::to_bytes, body::Body, http::Request};
    use tower::ServiceExt;

    struct FixtureFetcher;

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
            repo_id: None,
            revision: None,
            resolved_commit: None,
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
            .jobs
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
        manager.record_progress("progress-test", FetchProgress::phase("validating_revision"));

        let job = manager.get("progress-test").unwrap();
        assert_eq!(job.phase, "validating_revision");
        assert_eq!(job.progress_bytes, Some(75));
        assert_eq!(job.total_bytes, Some(100));
        assert_eq!(job.progress_files, Some(2));
        assert_eq!(job.total_files, Some(7));
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
            assert!(!directory.path().join("tmp").exists());
        }
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
    }

    #[tokio::test]
    async fn idempotency_key_is_scoped_to_request_and_principal() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Arc::new(Archive::new(directory.path()).unwrap());
        let manager = JobManager::open(&archive).unwrap();
        let first = manager.submit(
            JobRequest {
                kind: JobKind::Audit,
                repo_id: None,
                revision: None,
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
                repo_id: None,
                revision: None,
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
        assert!(reopened.get("legacy").is_some());
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
            repo_id: None,
            revision: None,
            resolved_commit: None,
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
        let interrupted = reopened.get("restart-test").unwrap();
        assert_eq!(interrupted.state, JobState::Failed);
        assert_eq!(interrupted.error_class.as_deref(), Some("interrupted"));
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
}
