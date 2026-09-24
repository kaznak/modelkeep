use std::io::SeekFrom;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    signal, task,
};
use tokio_util::io::ReaderStream;

use crate::pullthrough::{PullThrough, PullThroughError};
use crate::upstream::FetchProgress;
use crate::{
    is_hf_commit, parse_range, Archive, ArchiveError, ByteRange, RangeError, RepositoryType,
};

/// Default bound on a cold-miss `resolve` response (Issue 0069).
///
/// Both supported clients abandon a `resolve` metadata request after ten
/// seconds of silence: measured against `huggingface_hub` 0.36.0 and 1.27.0, a
/// `HEAD` held for eleven seconds raises `ReadTimeoutError (read timeout=10)`
/// in the client rather than delivering any status ModelKeep chose. Eight
/// seconds keeps ModelKeep's documented answer inside that window with margin
/// for a reverse proxy, while still letting a small file finish and stream
/// normally.
pub const DEFAULT_COLD_MISS_DEADLINE: Duration = Duration::from_secs(8);

const COLD_MISS_PENDING_BODY: &str = "{\"error\":\"acquisition in progress\"}";

const RESOLVE_DEADLINE_VARIABLE: &str = "MODELKEEP_COLD_MISS_DEADLINE_SECONDS";
const METADATA_DEADLINE_VARIABLE: &str = "MODELKEEP_METADATA_COLD_MISS_DEADLINE_SECONDS";

/// How long a cold miss waits for acquisition before answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColdMissPolicy {
    /// The `resolve` bound. `None` restores an unbounded wait, which holds the
    /// connection open with no headers for the whole transfer.
    pub deadline: Option<Duration>,
    /// The repository metadata bound, `None` by default.
    ///
    /// A metadata cold miss waits for the acquisition to finish unless an
    /// operator opts in, because neither supported client retries a bounded
    /// metadata answer: see [`DEFAULT_METADATA_COLD_MISS_DEADLINE`].
    pub metadata_deadline: Option<Duration>,
}

/// Bound on a cold-miss repository metadata response: none by default.
///
/// The `resolve` bound exists because both supported clients abandon a
/// `resolve` request after ten seconds and retry a `503` on their own. Neither
/// property holds on `/api/.../revision/...` or `/api/.../tree/...`, measured
/// against `huggingface_hub` 0.36.0 and 1.27.0 on 2026-09-24:
///
/// - the clients apply no read timeout to a metadata request. 0.36.0 waited
///   30 s and 1.27.0 waited 12 s per request and then completed the download,
///   so waiting is slow but correct rather than broken;
/// - neither client retries a metadata `503` — nor `429`, `425`, `500` or
///   `504`. Each ends the call on the first response.
///
/// Bounding metadata by default would therefore turn every first mirror of a
/// repository whose acquisition outlasts the deadline into a failed
/// `hf download`, which is ModelKeep's central use. The bound stays available
/// for an operator who prefers a fast, explicit answer over a long wait.
pub const DEFAULT_METADATA_COLD_MISS_DEADLINE: Option<Duration> = None;

impl Default for ColdMissPolicy {
    fn default() -> Self {
        Self {
            deadline: Some(DEFAULT_COLD_MISS_DEADLINE),
            metadata_deadline: DEFAULT_METADATA_COLD_MISS_DEADLINE,
        }
    }
}

impl ColdMissPolicy {
    pub fn from_env() -> Result<Self, String> {
        Self::from_values(
            std::env::var(RESOLVE_DEADLINE_VARIABLE),
            std::env::var(METADATA_DEADLINE_VARIABLE),
        )
    }

    fn from_values(
        resolve: Result<String, std::env::VarError>,
        metadata: Result<String, std::env::VarError>,
    ) -> Result<Self, String> {
        Ok(Self {
            deadline: deadline_from_value(
                resolve,
                RESOLVE_DEADLINE_VARIABLE,
                Some(DEFAULT_COLD_MISS_DEADLINE),
            )?,
            metadata_deadline: deadline_from_value(
                metadata,
                METADATA_DEADLINE_VARIABLE,
                DEFAULT_METADATA_COLD_MISS_DEADLINE,
            )?,
        })
    }

    #[cfg(test)]
    fn from_value(value: Result<String, std::env::VarError>) -> Result<Self, String> {
        Self::from_values(value, Err(std::env::VarError::NotPresent))
    }

    fn retry_after_seconds(&self) -> u64 {
        retry_after_seconds(self.deadline)
    }

    fn metadata_retry_after_seconds(&self) -> u64 {
        retry_after_seconds(self.metadata_deadline)
    }
}

/// Reads one whole-second deadline setting. `0` means an unbounded wait.
fn deadline_from_value(
    value: Result<String, std::env::VarError>,
    variable: &str,
    default: Option<Duration>,
) -> Result<Option<Duration>, String> {
    match value {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("invalid {variable}: {error}")),
        Ok(value) => {
            let seconds = value
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("invalid {variable}: expected whole seconds"))?;
            Ok((seconds > 0).then(|| Duration::from_secs(seconds)))
        }
    }
}

fn retry_after_seconds(deadline: Option<Duration>) -> u64 {
    deadline.map_or(1, |deadline| deadline.as_secs().max(1))
}

#[derive(Clone)]
pub struct HttpState {
    archive: Arc<Archive>,
    pullthrough: Option<Arc<PullThrough>>,
    cold_miss: ColdMissPolicy,
}
pub async fn serve(archive: Archive, address: std::net::SocketAddr) -> std::io::Result<()> {
    serve_router(router(archive), address).await
}

pub async fn serve_with_pullthrough(
    archive: Archive,
    pullthrough: Arc<PullThrough>,
    address: std::net::SocketAddr,
) -> std::io::Result<()> {
    serve_with_pullthrough_and_policy(archive, pullthrough, ColdMissPolicy::default(), address)
        .await
}

pub async fn serve_with_pullthrough_and_policy(
    archive: Archive,
    pullthrough: Arc<PullThrough>,
    cold_miss: ColdMissPolicy,
    address: std::net::SocketAddr,
) -> std::io::Result<()> {
    serve_router(
        router_with_pullthrough_and_policy(archive, pullthrough, cold_miss),
        address,
    )
    .await
}

async fn serve_router(router: Router, address: std::net::SocketAddr) -> std::io::Result<()> {
    let listener = match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(
                event = "server_bind_failed",
                listen_address = %address,
                error = %error,
                "failed to bind HTTP listener"
            );
            return Err(error);
        }
    };
    tracing::info!(
        event = "server_ready",
        listen_address = %address,
        "modelkeep is ready to serve requests"
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!(event = "shutdown_completed", "modelkeep shutdown completed");
    Ok(())
}

pub(crate) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    signal::ctrl_c()
        .await
        .expect("failed to install shutdown signal handler");
    tracing::info!(event = "shutdown_started", "modelkeep shutdown started");
}

pub fn router(archive: Archive) -> Router {
    router_with_state(HttpState {
        archive: Arc::new(archive),
        pullthrough: None,
        cold_miss: ColdMissPolicy::default(),
    })
}

pub fn router_with_pullthrough(archive: Archive, pullthrough: Arc<PullThrough>) -> Router {
    router_with_pullthrough_and_policy(archive, pullthrough, ColdMissPolicy::default())
}

pub fn router_with_pullthrough_and_policy(
    archive: Archive,
    pullthrough: Arc<PullThrough>,
    cold_miss: ColdMissPolicy,
) -> Router {
    router_with_state(HttpState {
        archive: Arc::new(archive),
        pullthrough: Some(pullthrough),
        cold_miss,
    })
}

fn router_with_state(state: HttpState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route(
            "/api/models/{namespace}/{repo}/revision/{revision}",
            get(model_info),
        )
        .route(
            "/api/models/{namespace}/{repo}/tree/{revision}",
            get(model_tree),
        )
        .route(
            "/api/datasets/{namespace}/{repo}/revision/{revision}",
            get(dataset_info),
        )
        .route(
            "/api/datasets/{namespace}/{repo}/tree/{revision}",
            get(dataset_tree),
        )
        .route(
            "/{namespace}/{repo}/resolve/{revision}/{*path}",
            get(get_file).head(head_file),
        )
        .route(
            "/datasets/{namespace}/{repo}/resolve/{revision}/{*path}",
            get(get_dataset_file).head(head_dataset_file),
        )
        .with_state(state)
}

async fn healthz() -> StatusCode {
    tracing::debug!(
        event = "health_probe_succeeded",
        endpoint = "/healthz",
        "health probe succeeded"
    );
    StatusCode::OK
}

async fn readyz(State(state): State<HttpState>) -> StatusCode {
    match state.archive.check_readiness() {
        Ok(()) => {
            tracing::debug!(
                event = "readiness_probe_succeeded",
                endpoint = "/readyz",
                "readiness probe succeeded"
            );
            StatusCode::OK
        }
        Err(error) => {
            tracing::warn!(event = "readiness_probe_failed", endpoint = "/readyz", error = %error, "archive readiness check failed");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

async fn model_info(
    State(state): State<HttpState>,
    Path((namespace, repo, revision)): Path<(String, String, String)>,
) -> Result<Response, StatusCode> {
    repository_info(state, namespace, repo, revision, RepositoryType::Model).await
}

async fn dataset_info(
    State(state): State<HttpState>,
    Path((namespace, repo, revision)): Path<(String, String, String)>,
) -> Result<Response, StatusCode> {
    repository_info(state, namespace, repo, revision, RepositoryType::Dataset).await
}

async fn repository_info(
    state: HttpState,
    namespace: String,
    repo: String,
    revision: String,
    repo_type: RepositoryType,
) -> Result<Response, StatusCode> {
    let repo_id = format!("{namespace}/{repo}");
    tracing::info!(
        event = "archive_request",
        repo_type = %repo_type,
        request_kind = "model_info",
        repo_id = %repo_id,
        requested_revision = %revision,
        "archive request received"
    );
    let commit = match if is_hf_commit(&revision) {
        state
            .archive
            .revision_path_for_type(repo_type, &repo_id, &revision)
            .and_then(|path| {
                if path.is_dir() {
                    Ok(revision.clone())
                } else {
                    Err(ArchiveError::Io(std::io::Error::from(
                        std::io::ErrorKind::NotFound,
                    )))
                }
            })
    } else {
        state
            .archive
            .resolve_ref_for_type(repo_type, &repo_id, &revision)
    } {
        Ok(commit) => {
            tracing::info!(event = "archive_hit", request_kind = "model_info", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %revision, commit = %commit, "archive request served locally");
            commit
        }
        Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(event = "archive_miss", request_kind = "model_info", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %revision, "archive request requires upstream acquisition");
            let Some(pullthrough) = state.pullthrough.clone() else {
                return Err(StatusCode::NOT_FOUND);
            };
            // Unbounded by default: a metadata cold miss waits for the
            // acquisition, because no supported client retries a bounded
            // answer on this route.
            let acquired = await_cold_miss_acquisition(
                state.cold_miss.metadata_deadline,
                pullthrough,
                repo_type,
                &repo_id,
                &revision,
                "model_info",
                None,
            )
            .await?;
            let Some(commit) = acquired else {
                return cold_miss_pending_response(
                    false,
                    state.cold_miss.metadata_retry_after_seconds(),
                );
            };
            commit
        }
        Err(error) => return Err(status_for_archive_error(error)),
    };
    let manifest = validated_manifest(&state.archive, repo_type, &repo_id, &commit)?;
    let siblings = manifest["files"]
        .as_array()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
        .iter()
        .filter_map(|file| file["path"].as_str())
        .filter(|path| !crate::is_internal_archive_path(path))
        .map(|path| serde_json::json!({ "rfilename": path }))
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!({
        "id": repo_id, "sha": commit, "private": false, "downloads": 0,
        "likes": 0, "tags": [], "siblings": siblings,
    }))
    .into_response())
}

async fn model_tree(
    State(state): State<HttpState>,
    Path((namespace, repo, revision)): Path<(String, String, String)>,
) -> Result<Response, StatusCode> {
    repository_tree(state, namespace, repo, revision, RepositoryType::Model).await
}

async fn dataset_tree(
    State(state): State<HttpState>,
    Path((namespace, repo, revision)): Path<(String, String, String)>,
) -> Result<Response, StatusCode> {
    repository_tree(state, namespace, repo, revision, RepositoryType::Dataset).await
}

async fn repository_tree(
    state: HttpState,
    namespace: String,
    repo: String,
    revision: String,
    repo_type: RepositoryType,
) -> Result<Response, StatusCode> {
    let repo_id = format!("{namespace}/{repo}");
    tracing::info!(
        event = "archive_request",
        repo_type = %repo_type,
        request_kind = "model_tree",
        repo_id = %repo_id,
        requested_revision = %revision,
        "archive request received"
    );
    let commit = match if is_hf_commit(&revision) {
        state
            .archive
            .revision_path_for_type(repo_type, &repo_id, &revision)
            .and_then(|path| {
                if path.is_dir() {
                    Ok(revision.clone())
                } else {
                    Err(ArchiveError::Io(std::io::Error::from(
                        std::io::ErrorKind::NotFound,
                    )))
                }
            })
    } else {
        state
            .archive
            .resolve_ref_for_type(repo_type, &repo_id, &revision)
    } {
        Ok(commit) => {
            tracing::info!(event = "archive_hit", request_kind = "model_tree", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %revision, commit = %commit, "archive request served locally");
            commit
        }
        Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(event = "archive_miss", request_kind = "model_tree", repo_type = %repo_type, repo_id = %repo_id, requested_revision = %revision, "archive request requires upstream acquisition");
            let Some(pullthrough) = state.pullthrough.clone() else {
                return Err(StatusCode::NOT_FOUND);
            };
            // Unbounded by default: a metadata cold miss waits for the
            // acquisition, because no supported client retries a bounded
            // answer on this route.
            let acquired = await_cold_miss_acquisition(
                state.cold_miss.metadata_deadline,
                pullthrough,
                repo_type,
                &repo_id,
                &revision,
                "model_tree",
                None,
            )
            .await?;
            let Some(commit) = acquired else {
                return cold_miss_pending_response(
                    false,
                    state.cold_miss.metadata_retry_after_seconds(),
                );
            };
            commit
        }
        Err(error) => return Err(status_for_archive_error(error)),
    };
    let manifest = validated_manifest(&state.archive, repo_type, &repo_id, &commit)?;
    let files = manifest["files"]
        .as_array()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
        .iter()
        .filter(|file| {
            file["path"]
                .as_str()
                .is_some_and(|path| !crate::is_internal_archive_path(path))
        })
        .map(|file| {
            serde_json::json!({
                "type": "file",
                "path": file["path"],
                "size": file["size"],
                "oid": file["sha256"],
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(files).into_response())
}

fn validated_manifest(
    archive: &Archive,
    repo_type: RepositoryType,
    repo_id: &str,
    commit: &str,
) -> Result<serde_json::Value, StatusCode> {
    match archive
        .is_complete_revision_for_type(repo_type, repo_id, commit)
        .map_err(status_for_archive_error)?
    {
        true => {}
        false => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
    serde_json::from_str(
        &archive
            .manifest_for_type(repo_type, repo_id, commit)
            .map_err(status_for_archive_error)?,
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn get_file(
    State(state): State<HttpState>,
    Path((namespace, repo, revision, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    file_response(
        state,
        (namespace, repo, revision, path),
        headers,
        false,
        RepositoryType::Model,
    )
    .await
}

async fn head_file(
    State(state): State<HttpState>,
    Path((namespace, repo, revision, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    file_response(
        state,
        (namespace, repo, revision, path),
        headers,
        true,
        RepositoryType::Model,
    )
    .await
}

async fn get_dataset_file(
    State(state): State<HttpState>,
    Path((namespace, repo, revision, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    file_response(
        state,
        (namespace, repo, revision, path),
        headers,
        false,
        RepositoryType::Dataset,
    )
    .await
}

async fn head_dataset_file(
    State(state): State<HttpState>,
    Path((namespace, repo, revision, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    file_response(
        state,
        (namespace, repo, revision, path),
        headers,
        true,
        RepositoryType::Dataset,
    )
    .await
}

async fn file_response(
    state: HttpState,
    target: (String, String, String, String),
    headers: HeaderMap,
    head_only: bool,
    repo_type: RepositoryType,
) -> Result<Response, StatusCode> {
    let (namespace, repo, revision, path) = target;
    let repo_id = format!("{namespace}/{repo}");
    tracing::info!(event = "archive_request", request_kind = if head_only { "head_file" } else { "get_file" }, repo_type = %repo_type, repo_id = %repo_id, requested_revision = %revision, path = %path, "archive request received");
    if crate::is_internal_archive_path(&path) {
        return Err(StatusCode::NOT_FOUND);
    }
    let resolved_result = if is_hf_commit(&revision) {
        state
            .archive
            .resolve_file_for_type(repo_type, &repo_id, &revision, &path)
            .map(|file| (file, revision.clone()))
    } else {
        state
            .archive
            .resolve_ref_for_type(repo_type, &repo_id, &revision)
            .and_then(|commit| {
                state
                    .archive
                    .resolve_file_for_type(repo_type, &repo_id, &commit, &path)
                    .map(|file| (file, commit))
            })
    };
    let (resolved, resolved_commit) = match resolved_result {
        Ok((resolved, commit)) => {
            tracing::info!(event = "archive_hit", request_kind = if head_only { "head_file" } else { "get_file" }, repo_type = %repo_type, repo_id = %repo_id, requested_revision = %revision, commit = %commit, path = %path, "archive request served locally");
            (resolved, commit)
        }
        Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(event = "archive_miss", request_kind = if head_only { "head_file" } else { "get_file" }, repo_type = %repo_type, repo_id = %repo_id, requested_revision = %revision, path = %path, "archive request requires upstream acquisition");
            let Some(pullthrough) = state.pullthrough.clone() else {
                return Err(StatusCode::NOT_FOUND);
            };
            let request_kind = if head_only { "head_file" } else { "get_file" };
            let commit = await_cold_miss_acquisition(
                state.cold_miss.deadline,
                pullthrough,
                repo_type,
                &repo_id,
                &revision,
                request_kind,
                Some(&path),
            )
            .await?;
            let Some(commit) = commit else {
                return cold_miss_pending_response(
                    head_only,
                    state.cold_miss.retry_after_seconds(),
                );
            };
            let resolved = state
                .archive
                .resolve_file_for_type(repo_type, &repo_id, &commit, &path)
                .map_err(status_for_archive_error)?;
            (resolved, commit)
        }
        Err(error) => return Err(status_for_archive_error(error)),
    };
    let size = resolved.size;
    let range = match headers.get(header::RANGE) {
        Some(value) => {
            let value = value.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;
            Some(
                parse_range(value, size)
                    .map_err(status_for_range_error)?
                    .ok_or(StatusCode::RANGE_NOT_SATISFIABLE)?,
            )
        }
        None => None,
    };
    let (status, start, end) = match range {
        Some(ByteRange { start, end }) => (StatusCode::PARTIAL_CONTENT, start, end),
        None => (StatusCode::OK, 0, size.saturating_sub(1)),
    };
    let content_length = if size == 0 { 0 } else { end - start + 1 };
    let mut response = Response::builder()
        .status(status)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, content_length)
        .header(header::ETAG, format!("\"{resolved_commit}-{size}\""))
        .header("x-repo-commit", &resolved_commit);
    if let Some(ByteRange { start, end }) = range {
        response = response.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(format!("\"{resolved_commit}-{size}\"").as_str())
    {
        return response
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
    }
    let body = if head_only {
        Body::empty()
    } else {
        let mut file = tokio::fs::File::open(resolved.path)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        file.seek(SeekFrom::Start(start))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Body::from_stream(ReaderStream::new(file.take(content_length)))
    };
    response
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Waits for the acquisition a missing request needs, bounded by `wait`.
///
/// Every route that can miss shares this one wait, so the routes cannot drift
/// apart in anything but the bound their caller passes: the same flight, the
/// same liveness events, and the same pending answer. The acquisition runs on
/// its own thread inside the flight, so giving up here neither cancels the
/// transfer nor holds the Tokio runtime open past the deadline, and a later
/// request joins the same flight instead of starting a second download.
///
/// `wait: None` waits for the acquisition to finish, which is what the
/// metadata routes do by default ([`DEFAULT_METADATA_COLD_MISS_DEADLINE`]).
///
/// `path` is the one file a `resolve` needs. A metadata route has no path and
/// passes `None`, which acquires the revision's snapshot (ADR-0008).
///
/// `Ok(None)` means the deadline elapsed with the acquisition still running;
/// the caller answers with [`cold_miss_pending_response`].
async fn await_cold_miss_acquisition(
    wait: Option<Duration>,
    pullthrough: Arc<PullThrough>,
    repo_type: RepositoryType,
    repo_id: &str,
    revision: &str,
    request_kind: &'static str,
    path: Option<&str>,
) -> Result<Option<String>, StatusCode> {
    let files: Vec<String> = path.map(str::to_string).into_iter().collect();
    let deadline = wait.map(|bound| Instant::now() + bound);
    let acquired_bytes = Arc::new(AtomicU64::new(0));
    let observed_bytes = Arc::clone(&acquired_bytes);
    // A metadata acquisition has no single path; the empty field keeps one
    // event shape for every request kind.
    let event_path = path.unwrap_or_default().to_string();
    let progress_path = event_path.clone();
    let progress_repo_id = repo_id.to_string();
    let progress_revision = revision.to_string();
    let owned_repo_id = repo_id.to_string();
    let owned_revision = revision.to_string();
    let commit = task::spawn_blocking(move || {
        let progress = move |event: FetchProgress| {
            let Some(acquired) = advanced_bytes(&event, &observed_bytes) else {
                return;
            };
            tracing::info!(event = "acquisition_progress", request_kind, repo_type = %repo_type, repo_id = %progress_repo_id, requested_revision = %progress_revision, path = %progress_path, phase = %event.phase, acquired_bytes = acquired, total_bytes = event.total, "cold-miss acquisition is transferring bytes");
        };
        pullthrough.ensure_bounded_for_type(
            repo_type,
            &owned_repo_id,
            &owned_revision,
            &files,
            deadline,
            &progress,
        )
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(status_for_pullthrough_error)?;
    if commit.is_none() {
        let deadline_seconds = wait.map_or(0, |deadline| deadline.as_secs());
        tracing::warn!(event = "acquisition_deadline_exceeded", request_kind, repo_type = %repo_type, repo_id = %repo_id, requested_revision = %revision, path = %event_path, deadline_seconds, acquired_bytes = acquired_bytes.load(Ordering::Relaxed), "cold-miss acquisition exceeded the response deadline and continues in the background");
    }
    Ok(commit)
}

/// The byte count to report when an acquisition event shows real movement.
///
/// A repeated or lower counter says the acquisition is alive, not that it is
/// advancing; reporting it as progress is the defect Issue 0068 names, so only
/// a strictly higher byte count than anything seen before is progress.
fn advanced_bytes(event: &FetchProgress, observed: &AtomicU64) -> Option<u64> {
    if event.unit.as_deref() != Some("bytes") {
        return None;
    }
    let completed = event.completed?;
    (completed > observed.fetch_max(completed, Ordering::Relaxed)).then_some(completed)
}

/// The documented cold-miss answer once the deadline passes.
///
/// `503` with `Retry-After` is the only candidate both supported clients retry
/// on their own: measured against `huggingface_hub` 0.36.0 and 1.27.0, both
/// repeat the `resolve` request after `503`, only 1.27.0 retries `429`, and
/// neither retries `425`. Each retry joins the acquisition that is still
/// running, so no retry starts a second download.
fn cold_miss_pending_response(
    head_only: bool,
    retry_after_seconds: u64,
) -> Result<Response, StatusCode> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::RETRY_AFTER, retry_after_seconds.to_string())
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, COLD_MISS_PENDING_BODY.len())
        .header(header::CACHE_CONTROL, "no-store")
        .body(if head_only {
            Body::empty()
        } else {
            Body::from(COLD_MISS_PENDING_BODY)
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn status_for_archive_error(error: ArchiveError) -> StatusCode {
    match error {
        ArchiveError::InvalidPath(_) => StatusCode::BAD_REQUEST,
        ArchiveError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            StatusCode::NOT_FOUND
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The client-facing status for a pull-through failure class.
///
/// A cancelled acquisition (Issue 0076) is answered `502`, the existing
/// "acquisition failed" class, rather than any new status or state of its own.
/// It is deliberately not `503`: `503` means "acquiring, retry", and both
/// supported clients retry it by themselves, which would immediately restart the
/// transfer an operator just stopped. `502` is honest — the acquisition this
/// request was waiting for did not complete — and it is never `404`, because
/// nothing was learned about whether upstream holds the file, and never a
/// success, because nothing was published.
fn status_for_pullthrough_error(error: PullThroughError) -> StatusCode {
    match error {
        PullThroughError::UpstreamNotFound => StatusCode::NOT_FOUND,
        PullThroughError::UpstreamUnauthorized => StatusCode::UNAUTHORIZED,
        PullThroughError::UpstreamUnavailable
        | PullThroughError::UpstreamFailed
        | PullThroughError::Cancelled => StatusCode::BAD_GATEWAY,
        PullThroughError::Storage => StatusCode::INSUFFICIENT_STORAGE,
        PullThroughError::UnsafePath => StatusCode::BAD_REQUEST,
        PullThroughError::UpstreamInvalidOutput(_)
        | PullThroughError::Integrity
        | PullThroughError::Conflict => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn status_for_range_error(error: RangeError) -> StatusCode {
    match error {
        RangeError::Invalid => StatusCode::BAD_REQUEST,
        RangeError::Unsatisfiable => StatusCode::RANGE_NOT_SATISFIABLE,
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use axum::body::to_bytes;
    use axum::http::Method;
    use std::io::Write;
    use std::sync::Mutex;
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

    fn capture_logs(filter: &str) -> (LogWriter, tracing::subscriber::DefaultGuard) {
        let writer = LogWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_env_filter(EnvFilter::new(filter))
            .with_writer(writer.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (writer, guard)
    }

    fn test_router() -> (Router, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        archive
            .publish_revision(crate::PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                files: vec![crate::ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"0123456789".to_vec(),
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
        archive
            .publish_revision_for_type(
                RepositoryType::Dataset,
                crate::PublishRequest {
                    repo_id: "org/model".into(),
                    requested_revision: "main".into(),
                    commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                    files: vec![crate::ArchiveFile {
                        path: "data/example.jsonl".into(),
                        bytes: b"dataset-payload".to_vec(),
                    }],
                },
            )
            .unwrap();
        archive
            .update_ref_for_type(
                RepositoryType::Dataset,
                "org/model",
                "main",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .unwrap();
        (router(archive), directory)
    }

    #[tokio::test]
    async fn dataset_routes_serve_the_dataset_namespace_without_model_collision() {
        let (app, _directory) = test_router();

        let info = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/datasets/org/model/revision/main")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(info.status(), StatusCode::OK);
        let info: serde_json::Value =
            serde_json::from_slice(&to_bytes(info.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(info["id"], "org/model");
        assert_eq!(info["sha"], "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        let tree = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/datasets/org/model/tree/main")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tree.status(), StatusCode::OK);
        let tree: serde_json::Value =
            serde_json::from_slice(&to_bytes(tree.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(tree[0]["path"], "data/example.jsonl");

        let file = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/datasets/org/model/resolve/main/data/example.jsonl")
                    .header(header::RANGE, "bytes=0-6")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(file.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            to_bytes(file.into_body(), usize::MAX).await.unwrap(),
            "dataset"
        );
    }

    #[tokio::test]
    async fn metadata_routes_reject_unvalidated_manifests() {
        async fn assert_rejected(app: Router, paths: &[&str]) {
            for path in paths {
                let response = app
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(*path)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            }
        }

        let (app, directory) = test_router();
        let dataset_manifest = directory
            .path()
            .join("datasets/org/model/revisions")
            .join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .join(".modelkeep-manifest.json");
        let valid_dataset = std::fs::read_to_string(&dataset_manifest).unwrap();

        std::fs::write(
            &dataset_manifest,
            valid_dataset.replace("\"repo_type\":\"dataset\"", "\"repo_type\":\"model\""),
        )
        .unwrap();
        assert_rejected(
            app.clone(),
            &[
                "/api/datasets/org/model/revision/main",
                "/api/datasets/org/model/tree/main",
            ],
        )
        .await;

        std::fs::write(
            &dataset_manifest,
            valid_dataset.replace("\"repo_type\":\"dataset\",", ""),
        )
        .unwrap();
        assert_rejected(
            app.clone(),
            &[
                "/api/datasets/org/model/revision/main",
                "/api/datasets/org/model/tree/main",
            ],
        )
        .await;

        let model_manifest = directory
            .path()
            .join("models/org/model/revisions")
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .join(".modelkeep-manifest.json");
        let incomplete = std::fs::read_to_string(&model_manifest)
            .unwrap()
            .replace("\"complete\":true", "\"complete\":false");
        std::fs::write(model_manifest, incomplete).unwrap();
        assert_rejected(
            app,
            &[
                "/api/models/org/model/revision/main",
                "/api/models/org/model/tree/main",
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let (app, _directory) = test_router();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn successful_probe_events_are_available_at_debug() {
        let (writer, _guard) = capture_logs("debug");
        let (app, _directory) = test_router();
        for endpoint in ["/healthz", "/readyz"] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(endpoint)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let output = writer.output();
        assert!(output.contains("health_probe_succeeded"));
        assert!(output.contains("readiness_probe_succeeded"));
    }

    #[tokio::test]
    async fn successful_probe_events_are_quiet_at_info() {
        let (writer, _guard) = capture_logs("info");
        let (app, _directory) = test_router();
        for endpoint in ["/healthz", "/readyz"] {
            app.clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(endpoint)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        }
        let output = writer.output();
        assert!(!output.contains("health_probe_succeeded"));
        assert!(!output.contains("readiness_probe_succeeded"));
    }

    #[tokio::test]
    async fn readiness_endpoint_reports_archive_state() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let app = router(archive.clone());
        assert_eq!(archive.last_readiness(), None);
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(archive.last_readiness(), Some(true));
        std::fs::remove_dir_all(directory.path().join("tmp")).unwrap();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(archive.last_readiness(), Some(false));

        std::fs::create_dir(directory.path().join("tmp")).unwrap();
        assert!(archive.check_readiness().is_ok());
        assert_eq!(archive.last_readiness(), Some(true));
    }

    #[tokio::test]
    async fn serves_model_info_for_revision() {
        let (app, _directory) = test_router();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/models/org/model/revision/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["sha"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(value["siblings"][0]["rfilename"], "config.json");
    }

    #[tokio::test]
    async fn serves_repository_tree_from_manifest() {
        let (app, _directory) = test_router();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/models/org/model/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?recursive=true&expand=false")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value[0]["type"], "file");
        assert_eq!(value[0]["path"], "config.json");
        assert_eq!(value[0]["size"], 10);
    }

    #[tokio::test]
    async fn hides_legacy_internal_staging_entries_from_clients() {
        let (app, directory) = test_router();
        let manifest_path = directory
            .path()
            .join("models/org/model/revisions")
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .join(".modelkeep-manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["files"].as_array_mut().unwrap().extend([
            serde_json::json!({
                "path": ".modelkeep-staging-lease",
                "size": 1,
                "sha256": "0"
            }),
            serde_json::json!({
                "path": ".cache/huggingface/download.json",
                "size": 1,
                "sha256": "0"
            }),
        ]);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let info = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/models/org/model/revision/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(info.status(), StatusCode::OK);
        let info: serde_json::Value =
            serde_json::from_slice(&to_bytes(info.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(info["siblings"].as_array().unwrap().len(), 1);
        assert_eq!(info["siblings"][0]["rfilename"], "config.json");

        let tree = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/models/org/model/tree/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?recursive=true&expand=false")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tree.status(), StatusCode::OK);
        let tree: serde_json::Value =
            serde_json::from_slice(&to_bytes(tree.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(tree.as_array().unwrap().len(), 1);
        assert_eq!(tree[0]["path"], "config.json");

        let internal = app
            .oneshot(
                axum::http::Request::builder()
                    .method(Method::HEAD)
                    .uri("/org/model/resolve/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/.modelkeep-staging-lease")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(internal.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn serves_full_and_partial_files() {
        let (app, _directory) = test_router();
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/config.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "0123456789"
        );

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/config.json")
                    .header(header::RANGE, "bytes=2-5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "2345"
        );
    }

    #[tokio::test]
    async fn request_and_hit_events_are_correlated_without_credentials() {
        let (writer, _guard) = capture_logs("info");
        let (app, _directory) = test_router();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/main/config.json?token=signed-query-secret")
                    .header(header::AUTHORIZATION, "Bearer bearer-header-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let output = writer.output();
        assert!(output.contains("archive_request"));
        assert!(output.contains("archive_hit"));
        assert!(output.contains("org/model"));
        assert!(output.contains("main"));
        assert!(output.contains("config.json"));
        assert!(output.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!output.contains("signed-query-secret"));
        assert!(!output.contains("bearer-header-secret"));
        assert!(!output.to_ascii_lowercase().contains("authorization"));
    }

    #[tokio::test]
    async fn miss_event_is_correlated_without_upstream() {
        let (writer, _guard) = capture_logs("info");
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let response = router(archive)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/missing/resolve/main/config.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let output = writer.output();
        assert!(output.contains("archive_request"));
        assert!(output.contains("archive_miss"));
        assert!(output.contains("org/missing"));
        assert!(output.contains("config.json"));
    }

    #[tokio::test]
    async fn returns_not_modified_for_matching_etag() {
        let (app, _directory) = test_router();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/config.json")
                    .header(
                        header::IF_NONE_MATCH,
                        "\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-10\"",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            response.headers()[header::ETAG],
            "\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-10\""
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn mutable_ref_response_uses_the_resolved_commit_identity() {
        let (app, _directory) = test_router();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/main/config.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::ETAG],
            "\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-10\""
        );
        assert_eq!(
            response.headers()["x-repo-commit"],
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[tokio::test]
    async fn head_returns_metadata_without_body() {
        let (app, _directory) = test_router();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("HEAD")
                    .uri("/org/model/resolve/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/config.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .len(),
            0
        );
    }
    struct HttpFakeFetcher;

    impl crate::upstream::UpstreamFetcher for HttpFakeFetcher {
        fn fetch(
            &self,
            request: &crate::upstream::FetchRequest,
        ) -> Result<crate::upstream::FetchedRevision, crate::upstream::UpstreamError> {
            // The helper honours the selection it is given: an unrestricted
            // request takes the repository, a selected one takes only what the
            // caller asked for.
            let upstream: [(&str, &[u8]); 2] = [
                ("config.json", b"cold-http"),
                ("tokenizer.json", b"tokenizer-http"),
            ];
            let mut files = Vec::new();
            for (path, bytes) in upstream {
                if !request.files.is_empty() && !request.files.iter().any(|file| file == path) {
                    continue;
                }
                std::fs::write(request.staging.join(path), bytes).unwrap();
                files.push(path.to_string());
            }
            Ok(crate::upstream::FetchedRevision {
                commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                files,
                staging: request.staging.clone(),
            })
        }
    }

    struct ErrorFetcher(UpstreamErrorKind);

    #[derive(Clone, Copy)]
    enum UpstreamErrorKind {
        NotFound,
        Unauthorized,
        Unavailable,
    }

    impl crate::upstream::UpstreamFetcher for ErrorFetcher {
        fn fetch(
            &self,
            _request: &crate::upstream::FetchRequest,
        ) -> Result<crate::upstream::FetchedRevision, crate::upstream::UpstreamError> {
            Err(match self.0 {
                UpstreamErrorKind::NotFound => crate::upstream::UpstreamError::NotFound,
                UpstreamErrorKind::Unauthorized => crate::upstream::UpstreamError::Unauthorized,
                UpstreamErrorKind::Unavailable => crate::upstream::UpstreamError::Unavailable,
            })
        }
    }

    #[tokio::test]
    async fn cold_miss_fetches_then_serves_mutable_revision() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), Arc::new(HttpFakeFetcher)));
        let app = router_with_pullthrough(archive, pullthrough);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/main/config.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "cold-http"
        );
    }

    #[tokio::test]
    async fn resolve_for_an_unheld_path_extends_the_published_revision() {
        let commit = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        archive
            .publish_revision(crate::PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: commit.into(),
                files: vec![crate::ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"already-archived".to_vec(),
                }],
            })
            .unwrap();
        archive.update_ref("org/model", "main", commit).unwrap();
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), Arc::new(HttpFakeFetcher)));
        let app = router_with_pullthrough(archive.clone(), pullthrough);

        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/main/tokenizer.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "tokenizer-http"
        );

        // The revision grew; it was neither republished nor rewritten.
        assert_eq!(archive.list_revisions("org/model").unwrap(), vec![commit]);
        assert_eq!(archive.verify_revision("org/model", commit).unwrap(), 2);
        let served = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/main/config.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(served.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(served.into_body(), usize::MAX).await.unwrap(),
            "already-archived"
        );
    }

    #[tokio::test]
    async fn cold_tree_request_fetches_immutable_revision() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), Arc::new(HttpFakeFetcher)));
        let app = router_with_pullthrough(archive.clone(), pullthrough);
        let commit = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!(
                        "/api/models/org/model/tree/{commit}?recursive=true&expand=false"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 2);
        assert_eq!(value[0]["path"], "config.json");
        assert!(archive.is_complete_revision("org/model", commit).unwrap());
    }

    #[tokio::test]
    async fn archived_revision_is_served_while_upstream_is_unavailable() {
        let commit = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        archive
            .publish_revision(crate::PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: commit.into(),
                files: vec![crate::ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"already-archived".to_vec(),
                }],
            })
            .unwrap();
        archive.update_ref("org/model", "main", commit).unwrap();
        let pullthrough = Arc::new(PullThrough::new(
            archive.clone(),
            Arc::new(ErrorFetcher(UpstreamErrorKind::Unavailable)),
        ));
        let app = router_with_pullthrough(archive, pullthrough);

        // Core invariant 8: metadata and file serving for archived content must
        // not depend on upstream being reachable.
        for uri in [
            "/api/models/org/model/revision/main",
            "/api/models/org/model/tree/main",
        ] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
        }
        let file = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/main/config.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(file.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(file.into_body(), usize::MAX).await.unwrap(),
            "already-archived"
        );
    }

    #[tokio::test]
    async fn preserves_upstream_failure_classes_in_http_status() {
        for (kind, expected) in [
            (UpstreamErrorKind::NotFound, StatusCode::NOT_FOUND),
            (UpstreamErrorKind::Unauthorized, StatusCode::UNAUTHORIZED),
            (UpstreamErrorKind::Unavailable, StatusCode::BAD_GATEWAY),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let archive = Archive::new(directory.path()).unwrap();
            let pullthrough = Arc::new(PullThrough::new(
                archive.clone(),
                Arc::new(ErrorFetcher(kind)),
            ));
            let response = router_with_pullthrough(archive, pullthrough)
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/api/models/org/model/revision/main")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }

    #[test]
    fn preserves_archive_failure_classes_in_http_status() {
        assert_eq!(
            status_for_pullthrough_error(PullThroughError::Integrity),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            status_for_pullthrough_error(PullThroughError::Storage),
            StatusCode::INSUFFICIENT_STORAGE
        );
        assert_eq!(
            status_for_pullthrough_error(PullThroughError::UnsafePath),
            StatusCode::BAD_REQUEST
        );
    }

    const COLD_MISS_COMMIT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct Gate {
        open: Mutex<bool>,
        changed: std::sync::Condvar,
    }

    impl Gate {
        fn new() -> Self {
            Self {
                open: Mutex::new(false),
                changed: std::sync::Condvar::new(),
            }
        }

        fn wait(&self) {
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.changed.wait(open).unwrap();
            }
        }

        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }

        /// Waits, but gives up when the acquisition is cancelled. `false` means
        /// the transfer was stopped rather than released.
        fn wait_cancellable(&self, cancel: &crate::upstream::Cancellation) -> bool {
            let mut open = self.open.lock().unwrap();
            while !*open {
                if cancel.is_cancelled() {
                    return false;
                }
                let (next, _timeout) = self
                    .changed
                    .wait_timeout(open, Duration::from_millis(10))
                    .unwrap();
                open = next;
            }
            true
        }
    }

    /// An upstream that reports byte movement and then blocks until released,
    /// standing in for a transfer that cannot finish inside a response.
    struct GatedFetcher {
        gate: Arc<Gate>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::upstream::UpstreamFetcher for GatedFetcher {
        fn fetch(
            &self,
            request: &crate::upstream::FetchRequest,
        ) -> Result<crate::upstream::FetchedRevision, crate::upstream::UpstreamError> {
            self.fetch_with_progress(request, &|_| {})
        }

        fn fetch_with_progress(
            &self,
            request: &crate::upstream::FetchRequest,
            progress: &(dyn Fn(FetchProgress) + Send + Sync),
        ) -> Result<crate::upstream::FetchedRevision, crate::upstream::UpstreamError> {
            self.run(request, progress, None)
        }

        fn fetch_cancellable(
            &self,
            request: &crate::upstream::FetchRequest,
            progress: &(dyn Fn(FetchProgress) + Send + Sync),
            cancel: &crate::upstream::Cancellation,
        ) -> Result<crate::upstream::FetchedRevision, crate::upstream::UpstreamError> {
            self.run(request, progress, Some(cancel))
        }
    }

    impl GatedFetcher {
        fn run(
            &self,
            request: &crate::upstream::FetchRequest,
            progress: &(dyn Fn(FetchProgress) + Send + Sync),
            cancel: Option<&crate::upstream::Cancellation>,
        ) -> Result<crate::upstream::FetchedRevision, crate::upstream::UpstreamError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let bytes = |completed: u64| FetchProgress {
                version: 1,
                phase: "downloading_files".into(),
                unit: Some("bytes".into()),
                completed: Some(completed),
                total: Some(9),
            };
            progress(bytes(0));
            progress(bytes(4));
            // A counter that does not move is liveness, never progress.
            progress(bytes(4));
            match cancel {
                Some(cancel) if !self.gate.wait_cancellable(cancel) => {
                    return Err(crate::upstream::UpstreamError::Cancelled)
                }
                Some(_) => {}
                None => self.gate.wait(),
            }
            std::fs::write(request.staging.join("config.json"), b"cold-http").unwrap();
            Ok(crate::upstream::FetchedRevision {
                commit: COLD_MISS_COMMIT.into(),
                files: vec!["config.json".into()],
                staging: request.staging.clone(),
            })
        }
    }

    struct ColdMissFixture {
        directory: tempfile::TempDir,
        archive: Archive,
        pullthrough: Arc<PullThrough>,
        gate: Arc<Gate>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ColdMissFixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let archive = Archive::new(directory.path()).unwrap();
            let gate = Arc::new(Gate::new());
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let pullthrough = Arc::new(PullThrough::new(
                archive.clone(),
                Arc::new(GatedFetcher {
                    gate: Arc::clone(&gate),
                    calls: Arc::clone(&calls),
                }),
            ));
            Self {
                directory,
                archive,
                pullthrough,
                gate,
                calls,
            }
        }

        /// A router with the shipped policy: `resolve` bounded, metadata not.
        fn router(&self, deadline: Duration) -> Router {
            self.router_with(ColdMissPolicy {
                deadline: Some(deadline),
                ..ColdMissPolicy::default()
            })
        }

        /// A router for an operator who opted the metadata routes into the
        /// bound as well.
        fn metadata_router(&self, deadline: Duration) -> Router {
            self.router_with(ColdMissPolicy {
                deadline: Some(deadline),
                metadata_deadline: Some(deadline),
            })
        }

        fn router_with(&self, policy: ColdMissPolicy) -> Router {
            router_with_pullthrough_and_policy(
                self.archive.clone(),
                Arc::clone(&self.pullthrough),
                policy,
            )
        }

        fn acquisitions(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// How many acquisitions ran, once the flight has reached upstream.
        ///
        /// The flight starts on its own thread, so a loaded machine can answer
        /// the bounded request before that thread gets there; waiting for it
        /// keeps "exactly one acquisition" an assertion about the flight rather
        /// than about scheduling.
        fn started_acquisitions(&self) -> usize {
            for _ in 0..3000 {
                if self.acquisitions() > 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            self.acquisitions()
        }

        fn staging_directories(&self) -> Vec<String> {
            std::fs::read_dir(self.directory.path().join("tmp"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".fetch-active-"))
                .collect()
        }

        /// Lets the acquisition finish and waits until it has published.
        fn settle(&self) {
            self.gate.release();
            for _ in 0..3000 {
                if self
                    .archive
                    .is_complete_revision("org/model", COLD_MISS_COMMIT)
                    .unwrap_or(false)
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("the acquisition did not publish after being released");
        }
    }

    fn cold_miss_request(method: Method, uri: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn a_cold_miss_resolve_answers_within_the_deadline_and_keeps_acquiring() {
        let (logs, _guard) = capture_logs("info");
        let fixture = ColdMissFixture::new();
        let app = fixture.router(Duration::from_millis(200));
        let started = Instant::now();
        let response = app
            .oneshot(cold_miss_request(
                Method::GET,
                "/org/model/resolve/main/config.json",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the cold miss was not bounded: {:?}",
            started.elapsed()
        );
        assert_eq!(fixture.started_acquisitions(), 1);
        let output = logs.output();
        assert!(output.contains("archive_miss"), "{output}");
        assert!(output.contains("acquisition_deadline_exceeded"), "{output}");

        // The acquisition was never cancelled, so it still publishes.
        fixture.settle();
        assert_eq!(fixture.acquisitions(), 1);
    }

    #[tokio::test]
    async fn a_retry_joins_the_running_acquisition_and_is_served_once_it_finishes() {
        let fixture = ColdMissFixture::new();
        let impatient = fixture.router(Duration::from_millis(100));
        let patient = fixture.router(Duration::from_secs(30));

        let bounded = impatient
            .oneshot(cold_miss_request(
                Method::GET,
                "/org/model/resolve/main/config.json",
            ))
            .await
            .unwrap();
        assert_eq!(bounded.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(fixture.started_acquisitions(), 1);

        let gate = Arc::clone(&fixture.gate);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            gate.release();
        });
        let served = patient
            .oneshot(cold_miss_request(
                Method::GET,
                "/org/model/resolve/main/config.json",
            ))
            .await
            .unwrap();
        assert_eq!(served.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(served.into_body(), usize::MAX).await.unwrap(),
            "cold-http"
        );
        // The retry joined the flight rather than starting a second download.
        assert_eq!(fixture.acquisitions(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_acquisition_answers_a_waiting_client_with_an_upstream_failure() {
        // Issue 0076: a client that is waiting when an operator stops the
        // acquisition is answered from the existing failure vocabulary — `502`,
        // "the acquisition did not complete" — rather than hanging, a false
        // `404`, or a success that delivers nothing. It is deliberately not
        // `503`: both supported clients retry `503` on their own, which would
        // restart the transfer that was just stopped.
        let fixture = ColdMissFixture::new();
        let app = fixture.router_with(ColdMissPolicy {
            deadline: None,
            metadata_deadline: None,
        });
        let pullthrough = Arc::clone(&fixture.pullthrough);
        let cancelling = tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let view = pullthrough.in_flight_acquisitions();
                if let Some(item) = view.items.iter().find(|item| item.state == "transferring") {
                    return pullthrough.cancel_acquisition(&item.id);
                }
                assert!(Instant::now() < deadline, "no acquisition started");
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/main/config.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            cancelling.await.unwrap(),
            Some(crate::upstream::CancelOutcome::Cancelled)
        );
        // Nothing partial was published or served.
        assert!(fixture
            .archive
            .list_revisions("org/model")
            .unwrap()
            .is_empty());
        fixture.gate.release();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cold_misses_are_all_bounded_and_share_one_acquisition() {
        let fixture = ColdMissFixture::new();
        let app = fixture.router(Duration::from_millis(200));
        let started = Instant::now();
        let mut requests = Vec::new();
        for _ in 0..4 {
            let app = app.clone();
            requests.push(tokio::spawn(async move {
                app.oneshot(cold_miss_request(
                    Method::GET,
                    "/org/model/resolve/main/config.json",
                ))
                .await
                .unwrap()
                .status()
            }));
        }
        for request in requests {
            assert_eq!(
                request.await.unwrap(),
                StatusCode::SERVICE_UNAVAILABLE,
                "every concurrent cold miss must be bounded"
            );
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "concurrent cold misses were not bounded: {:?}",
            started.elapsed()
        );
        assert_eq!(fixture.started_acquisitions(), 1);
        fixture.settle();
        assert_eq!(fixture.acquisitions(), 1);
    }

    #[tokio::test]
    async fn head_obeys_the_same_cold_miss_deadline_as_get() {
        let fixture = ColdMissFixture::new();
        let app = fixture.router(Duration::from_millis(200));
        let response = app
            .oneshot(cold_miss_request(
                Method::HEAD,
                "/org/model/resolve/main/config.json",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .len(),
            0
        );
        assert_eq!(fixture.started_acquisitions(), 1);
    }

    #[tokio::test]
    async fn a_cold_miss_that_reaches_its_deadline_publishes_nothing_and_keeps_staging() {
        let fixture = ColdMissFixture::new();
        let app = fixture.router(Duration::from_millis(200));
        let response = app
            .clone()
            .oneshot(cold_miss_request(
                Method::GET,
                "/org/model/resolve/main/config.json",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        // Nothing partial became observable as a completed object.
        assert!(fixture
            .archive
            .list_revisions("org/model")
            .unwrap()
            .is_empty());
        assert!(fixture
            .archive
            .resolve_file("org/model", COLD_MISS_COMMIT, "config.json")
            .is_err());
        assert!(!fixture
            .archive
            .is_complete_revision("org/model", COLD_MISS_COMMIT)
            .unwrap_or(false));
        // The transfer keeps its identified staging (ADR-0017) rather than
        // being orphaned by the caller giving up.
        assert_eq!(fixture.started_acquisitions(), 1);
        assert_eq!(
            fixture.staging_directories().len(),
            1,
            "the interrupted request must leave exactly one resumable staging directory"
        );

        fixture.settle();
        let served = app
            .oneshot(cold_miss_request(
                Method::GET,
                "/org/model/resolve/main/config.json",
            ))
            .await
            .unwrap();
        assert_eq!(served.status(), StatusCode::OK);
        assert!(fixture.staging_directories().is_empty());
    }

    const METADATA_ROUTES: [&str; 2] = [
        "/api/models/org/model/revision/main",
        "/api/models/org/model/tree/main",
    ];

    async fn json_body(response: Response) -> serde_json::Value {
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    /// The shipped default: a metadata cold miss waits for the acquisition.
    ///
    /// Bounding it would break the first mirror of any repository whose
    /// acquisition outlasts the deadline, because neither supported client
    /// retries a bounded metadata answer
    /// ([`a_configured_metadata_deadline_is_an_explicit_operator_trade`]).
    #[tokio::test]
    async fn a_cold_metadata_request_waits_for_the_acquisition_by_default() {
        assert_eq!(ColdMissPolicy::default().metadata_deadline, None);
        let (logs, guard) = capture_logs("info");
        let fixture = ColdMissFixture::new();
        // `resolve` keeps its bound; the metadata routes are not bounded by it.
        let app = fixture.router(Duration::from_millis(100));
        let held = Duration::from_millis(700);
        let gate = Arc::clone(&fixture.gate);
        std::thread::spawn(move || {
            std::thread::sleep(held);
            gate.release();
        });

        let started = Instant::now();
        let info = app
            .clone()
            .oneshot(cold_miss_request(
                Method::GET,
                "/api/models/org/model/revision/main",
            ))
            .await
            .unwrap();
        assert_eq!(
            info.status(),
            StatusCode::OK,
            "a metadata cold miss must not be cut off by the resolve deadline"
        );
        assert!(
            started.elapsed() >= held,
            "the request answered before the acquisition finished: {:?}",
            started.elapsed()
        );
        let info = json_body(info).await;
        assert_eq!(info["sha"], COLD_MISS_COMMIT);
        assert_eq!(info["siblings"][0]["rfilename"], "config.json");

        let tree = app
            .oneshot(cold_miss_request(
                Method::GET,
                "/api/models/org/model/tree/main",
            ))
            .await
            .unwrap();
        assert_eq!(tree.status(), StatusCode::OK);
        assert_eq!(json_body(tree).await[0]["path"], "config.json");

        let output = logs.output();
        drop(guard);
        assert!(output.contains("archive_miss"), "{output}");
        assert!(
            !output.contains("acquisition_deadline_exceeded"),
            "the default metadata wait must not report a deadline: {output}"
        );
        assert_eq!(fixture.acquisitions(), 1);
    }

    #[tokio::test]
    async fn a_configured_metadata_deadline_answers_within_it_and_keeps_acquiring() {
        for route in METADATA_ROUTES {
            let (logs, guard) = capture_logs("info");
            let fixture = ColdMissFixture::new();
            let app = fixture.metadata_router(Duration::from_millis(200));
            let started = Instant::now();
            let response = app
                .oneshot(cold_miss_request(Method::GET, route))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{route} must answer with the documented cold-miss status"
            );
            assert_eq!(response.headers()[header::RETRY_AFTER], "1");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{route} was not bounded: {:?}",
                started.elapsed()
            );
            assert_eq!(fixture.started_acquisitions(), 1);
            let output = logs.output();
            drop(guard);
            assert!(output.contains("archive_miss"), "{output}");
            assert!(output.contains("acquisition_deadline_exceeded"), "{output}");

            // The acquisition was never cancelled, so it still publishes.
            fixture.settle();
            assert_eq!(fixture.acquisitions(), 1);
        }
    }

    #[tokio::test]
    async fn a_metadata_retry_joins_the_running_acquisition_and_is_answered_once_it_finishes() {
        let fixture = ColdMissFixture::new();
        let impatient = fixture.metadata_router(Duration::from_millis(100));
        let patient = fixture.metadata_router(Duration::from_secs(30));

        let bounded = impatient
            .oneshot(cold_miss_request(
                Method::GET,
                "/api/models/org/model/revision/main",
            ))
            .await
            .unwrap();
        assert_eq!(bounded.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(fixture.started_acquisitions(), 1);

        let gate = Arc::clone(&fixture.gate);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            gate.release();
        });
        let info = patient
            .clone()
            .oneshot(cold_miss_request(
                Method::GET,
                "/api/models/org/model/revision/main",
            ))
            .await
            .unwrap();
        assert_eq!(info.status(), StatusCode::OK);
        let info = json_body(info).await;
        assert_eq!(info["sha"], COLD_MISS_COMMIT);
        assert_eq!(info["siblings"][0]["rfilename"], "config.json");

        let tree = patient
            .oneshot(cold_miss_request(
                Method::GET,
                "/api/models/org/model/tree/main",
            ))
            .await
            .unwrap();
        assert_eq!(tree.status(), StatusCode::OK);
        let tree = json_body(tree).await;
        assert_eq!(tree[0]["path"], "config.json");
        assert_eq!(tree[0]["size"], "cold-http".len());

        // Every later request joined or read the archive; none started a
        // second download.
        assert_eq!(fixture.acquisitions(), 1);
    }

    #[tokio::test]
    async fn an_archived_revision_answers_metadata_without_entering_the_acquisition_path() {
        let fixture = ColdMissFixture::new();
        fixture
            .archive
            .publish_revision(crate::PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: COLD_MISS_COMMIT.into(),
                files: vec![crate::ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"already-archived".to_vec(),
                }],
            })
            .unwrap();
        fixture
            .archive
            .update_ref("org/model", "main", COLD_MISS_COMMIT)
            .unwrap();
        // The gate is never released: a warm metadata answer that entered the
        // miss path would block instead of answering.
        let app = fixture.metadata_router(Duration::from_secs(30));
        for route in METADATA_ROUTES {
            let started = Instant::now();
            let response = app
                .clone()
                .oneshot(cold_miss_request(Method::GET, route))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{route}");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{route} must not wait on acquisition: {:?}",
                started.elapsed()
            );
        }
        assert_eq!(fixture.acquisitions(), 0);
    }

    #[tokio::test]
    async fn a_metadata_request_that_reaches_its_deadline_publishes_nothing() {
        let fixture = ColdMissFixture::new();
        let app = fixture.metadata_router(Duration::from_millis(200));
        let response = app
            .clone()
            .oneshot(cold_miss_request(
                Method::GET,
                "/api/models/org/model/revision/main",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        // The pending answer is not metadata: it never reports an empty or
        // partial file list as the revision's contents.
        assert_eq!(
            json_body(response).await["error"],
            "acquisition in progress"
        );

        assert!(fixture
            .archive
            .list_revisions("org/model")
            .unwrap()
            .is_empty());
        assert!(!fixture
            .archive
            .is_complete_revision("org/model", COLD_MISS_COMMIT)
            .unwrap_or(false));
        assert!(fixture
            .archive
            .manifest("org/model", COLD_MISS_COMMIT)
            .is_err());
        assert_eq!(fixture.started_acquisitions(), 1);
        assert_eq!(
            fixture.staging_directories().len(),
            1,
            "the interrupted request must leave exactly one resumable staging directory"
        );

        fixture.settle();
        let served = app
            .oneshot(cold_miss_request(
                Method::GET,
                "/api/models/org/model/revision/main",
            ))
            .await
            .unwrap();
        assert_eq!(served.status(), StatusCode::OK);
        assert_eq!(json_body(served).await["sha"], COLD_MISS_COMMIT);
        assert!(fixture.staging_directories().is_empty());
    }

    /// Fixes the measured reaction of the supported clients to a bounded
    /// metadata answer (`huggingface_hub` 0.36.0 and 1.27.0, 2026-09-24),
    /// which is *not* the reaction they have to a bounded `resolve`, and which
    /// is why the metadata bound is opt-in:
    ///
    /// - `/api/models/.../revision/...` and `/api/models/.../tree/...`: neither
    ///   client retries any candidate status. `503`, `429`, `425`, `500` and
    ///   `504` each end the call on the first response, as
    ///   `LocalEntryNotFoundError` (from `snapshot_download`) or
    ///   `HfHubHTTPError` (from `list_repo_tree`). On `resolve` both clients
    ///   retry `503` on their own; on metadata neither does.
    /// - holding the metadata request open instead: neither client applies its
    ///   10-second `resolve` read timeout here. 0.36.0 waited 30 s and 1.27.0
    ///   waited 12 s per metadata request, and both then completed the
    ///   download. Waiting is slow, not broken.
    /// - 1.27.0 requests `revision` and then `tree` during a download; 0.36.0
    ///   requests `revision` only.
    ///
    /// So a bound on these routes is a trade an operator makes deliberately:
    /// it buys a prompt, classified answer at the cost of failing every cold
    /// download whose acquisition outlasts the deadline. Waiting remains the
    /// default. When an operator does configure it, the answer is the one the
    /// `resolve` route already documents — `503` with `Retry-After`, never
    /// confusable with `404` absent, with the acquisition still running and a
    /// repeated request joining it.
    #[test]
    fn a_configured_metadata_deadline_is_an_explicit_operator_trade() {
        // Shipped: `resolve` bounded, metadata waiting.
        let shipped = ColdMissPolicy::default();
        assert_eq!(shipped.deadline, Some(DEFAULT_COLD_MISS_DEADLINE));
        assert_eq!(shipped.metadata_deadline, None);

        // Configuring the metadata bound does not disturb the `resolve` bound.
        let configured =
            ColdMissPolicy::from_values(Err(std::env::VarError::NotPresent), Ok("30".into()))
                .unwrap();
        assert_eq!(configured.deadline, Some(DEFAULT_COLD_MISS_DEADLINE));
        assert_eq!(configured.metadata_deadline, Some(Duration::from_secs(30)));
        assert_eq!(configured.metadata_retry_after_seconds(), 30);

        // And the answer it produces is the documented one.
        let pending = cold_miss_pending_response(false, 30).unwrap();
        assert_eq!(pending.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(pending.headers()[header::RETRY_AFTER], "30");
    }

    #[test]
    fn the_metadata_cold_miss_deadline_is_a_separate_setting_that_defaults_to_waiting() {
        let unset = Err(std::env::VarError::NotPresent);
        assert_eq!(
            ColdMissPolicy::from_values(unset.clone(), unset.clone()).unwrap(),
            ColdMissPolicy::default()
        );
        // Zero is the default and says so explicitly.
        assert_eq!(
            ColdMissPolicy::from_values(unset.clone(), Ok("0".into()))
                .unwrap()
                .metadata_deadline,
            None
        );
        // The two settings are independent in both directions.
        let resolve_only = ColdMissPolicy::from_values(Ok("0".into()), unset.clone()).unwrap();
        assert_eq!(resolve_only.deadline, None);
        assert_eq!(resolve_only.metadata_deadline, None);
        let both = ColdMissPolicy::from_values(Ok("5".into()), Ok("60".into())).unwrap();
        assert_eq!(both.deadline, Some(Duration::from_secs(5)));
        assert_eq!(both.metadata_deadline, Some(Duration::from_secs(60)));
        // A malformed metadata setting is a configuration failure, never a
        // silently applied bound.
        assert!(ColdMissPolicy::from_values(unset.clone(), Ok("soon".into())).is_err());
        assert!(ColdMissPolicy::from_values(unset, Ok("-1".into())).is_err());
    }

    /// Fixes the measured reaction of the supported clients to the candidate
    /// statuses (`huggingface_hub` 0.36.0 and 1.27.0, 2026-09-24):
    ///
    /// - `503`: both clients repeat the `resolve` request on their own. 0.36.0
    ///   backs off 1s, 2s, 4s, 8s, 8s; 1.27.0 follows `Retry-After` instead of
    ///   its own backoff.
    /// - `429`: only 1.27.0 retries; 0.36.0 fails on the first response.
    /// - `425`: neither client retries.
    /// - holding the request open instead: both clients abandon the `resolve`
    ///   `HEAD` after 10 s with `ReadTimeoutError (read timeout=10)`, which is
    ///   the `status=000` the issue reports.
    ///
    /// So the answer is `503` with `Retry-After`, produced inside the client's
    /// 10-second window.
    #[test]
    fn the_cold_miss_deadline_answers_with_the_status_supported_clients_retry() {
        assert!(
            DEFAULT_COLD_MISS_DEADLINE < Duration::from_secs(10),
            "the default deadline must answer inside the clients' 10-second resolve timeout"
        );
        assert_eq!(
            ColdMissPolicy::default().retry_after_seconds(),
            DEFAULT_COLD_MISS_DEADLINE.as_secs()
        );
        for head_only in [false, true] {
            let response = cold_miss_pending_response(head_only, 8).unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers()[header::RETRY_AFTER], "8");
        }
    }

    #[tokio::test]
    async fn an_archived_file_is_served_without_entering_the_acquisition_path() {
        let fixture = ColdMissFixture::new();
        fixture
            .archive
            .publish_revision(crate::PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: COLD_MISS_COMMIT.into(),
                files: vec![crate::ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"already-archived".to_vec(),
                }],
            })
            .unwrap();
        fixture
            .archive
            .update_ref("org/model", "main", COLD_MISS_COMMIT)
            .unwrap();
        // The gate is never released: a warm read that entered the miss path
        // would block instead of answering.
        let app = fixture.router(Duration::from_secs(30));
        let started = Instant::now();
        let response = app
            .oneshot(cold_miss_request(
                Method::GET,
                "/org/model/resolve/main/config.json",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            "already-archived"
        );
        assert_eq!(fixture.acquisitions(), 0);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a warm read must not wait on acquisition: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn only_byte_movement_counts_as_acquisition_progress() {
        let observed = AtomicU64::new(0);
        let bytes = |completed: Option<u64>| FetchProgress {
            version: 1,
            phase: "downloading_files".into(),
            unit: Some("bytes".into()),
            completed,
            total: Some(16),
        };
        assert_eq!(advanced_bytes(&bytes(Some(0)), &observed), None);
        assert_eq!(advanced_bytes(&bytes(Some(8)), &observed), Some(8));
        assert_eq!(advanced_bytes(&bytes(Some(8)), &observed), None);
        assert_eq!(advanced_bytes(&bytes(Some(3)), &observed), None);
        assert_eq!(advanced_bytes(&bytes(None), &observed), None);
        assert_eq!(advanced_bytes(&bytes(Some(9)), &observed), Some(9));
        assert_eq!(
            advanced_bytes(&FetchProgress::phase("acquiring_snapshot"), &observed),
            None
        );
    }

    #[test]
    fn the_cold_miss_deadline_is_configured_in_whole_seconds() {
        assert_eq!(
            ColdMissPolicy::from_value(Err(std::env::VarError::NotPresent)).unwrap(),
            ColdMissPolicy::default()
        );
        assert_eq!(
            ColdMissPolicy::from_value(Ok("30".into()))
                .unwrap()
                .deadline,
            Some(Duration::from_secs(30))
        );
        // Zero restores the unbounded wait for an operator who wants it.
        assert_eq!(
            ColdMissPolicy::from_value(Ok("0".into())).unwrap().deadline,
            None
        );
        assert!(ColdMissPolicy::from_value(Ok("soon".into())).is_err());
        assert!(ColdMissPolicy::from_value(Ok("-1".into())).is_err());
    }
}
