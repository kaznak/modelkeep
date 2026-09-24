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

use std::collections::BTreeMap;

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

/// One file in a repository metadata answer.
///
/// The fields split by who they belong to (Issue 0079). `size` and `oid` are the
/// Hub's, carried with the Hub's meaning: `oid` is the git object id upstream
/// recorded for that path and nothing else. `digest` is ModelKeep's own, the
/// sha256 of the bytes it holds and serves for that path, which is the value the
/// `resolve` route advertises as `ETag`.
///
/// Every one of them is `None` when nothing states it: `oid` when the revision
/// has no recorded upstream file list, `digest` when the archive does not hold
/// the file. ModelKeep reports the absence rather than substituting the other
/// value for it, which is what made the two fields mean each other's names
/// before.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataFile {
    path: String,
    size: Option<u64>,
    oid: Option<String>,
    digest: Option<String>,
}

/// ModelKeep's own per-file property, beside the Hub's fields.
///
/// A nested object rather than a bare key, so a later addition needs no new
/// top-level name and no consumer has to learn a second place to look. `sha256`
/// is the digest ModelKeep serves as the file's `ETag` and validates its own
/// bytes against, and it is `null` for a path the archive does not hold, where
/// ModelKeep has no bytes to stand behind.
fn modelkeep_property(file: &MetadataFile) -> serde_json::Value {
    serde_json::json!({ "sha256": file.digest })
}

/// What the metadata routes answer for one revision.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepositoryMetadata {
    commit: String,
    files: Vec<MetadataFile>,
}

impl RepositoryMetadata {
    /// Upstream's answer for a revision the archive does not hold (Issue 0074).
    ///
    /// The response is assembled from upstream's file list rather than relayed:
    /// nothing upstream said about where its payload lives reaches the client,
    /// so no answer can point a client around the mirror (core invariant 10).
    /// `oid` is upstream's git object id for the commit, which is what the Hub
    /// itself reports there, and never a digest ModelKeep claims to have
    /// verified. `digest` is `None` throughout: the archive holds none of these
    /// files yet, so there is no value ModelKeep serves and stands behind.
    fn from_upstream(metadata: crate::upstream::UpstreamRepositoryFiles) -> Self {
        let mut files = metadata
            .files
            .into_iter()
            .map(|file| MetadataFile {
                path: file.path,
                size: file.size,
                oid: file.blob_id,
                digest: None,
            })
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Self {
            commit: metadata.commit,
            files,
        }
    }
}

async fn repository_info(
    state: HttpState,
    namespace: String,
    repo: String,
    revision: String,
    repo_type: RepositoryType,
) -> Result<Response, StatusCode> {
    let repo_id = format!("{namespace}/{repo}");
    let Some(answer) =
        repository_metadata(&state, repo_type, &repo_id, &revision, "model_info").await?
    else {
        return cold_miss_pending_response(false, state.cold_miss.metadata_retry_after_seconds());
    };
    // The Hub's `files_metadata` shape — `rfilename`, `size`, `blobId` — with
    // the Hub's meanings, plus ModelKeep's own property beside them (Issue
    // 0079). The wire name is `blobId`: both pinned clients read
    // `sibling.get("blobId")` here and expose it as `RepoSibling.blob_id`, and a
    // sibling spelled `blob_id` leaves the client's own attribute `None`. The
    // `tree` route spells the same value `oid`, which is the Hub's name there.
    // No `lfs` object: see `modelkeep-api.md` for the deviation and the
    // measurement behind it.
    let siblings = answer
        .files
        .iter()
        .map(|file| {
            serde_json::json!({
                "rfilename": file.path,
                "size": file.size,
                "blobId": file.oid,
                "modelkeep": modelkeep_property(file),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!({
        "id": repo_id, "sha": answer.commit, "private": false, "downloads": 0,
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
    let Some(answer) =
        repository_metadata(&state, repo_type, &repo_id, &revision, "model_tree").await?
    else {
        return cold_miss_pending_response(false, state.cold_miss.metadata_retry_after_seconds());
    };
    let files = answer
        .files
        .iter()
        .map(|file| {
            serde_json::json!({
                "type": "file",
                "path": file.path,
                "size": file.size,
                "oid": file.oid,
                "modelkeep": modelkeep_property(file),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(files).into_response())
}

/// Answers a repository metadata request, from the archive or from upstream.
///
/// Both metadata routes share this, so they cannot drift apart in which
/// revision they report or where the answer came from. `Ok(None)` means a
/// configured metadata deadline elapsed with an acquisition still running, and
/// the caller answers [`cold_miss_pending_response`].
///
/// An archived revision is answered from the archive and never contacts upstream
/// (core invariant 8). A revision the archive does not hold is answered from
/// upstream's file list **without acquiring it** (Issue 0074), which is what
/// lets the client's own file filter narrow the first acquisition: the per-file
/// requests that follow acquire only what the client asks for. Nothing is
/// written to the archive for such an answer. Only a fetcher that cannot report
/// upstream's per-file metadata falls back to acquiring the revision and
/// answering from the archive, because reporting a repository as empty because
/// it could not be enumerated would be worse than waiting.
async fn repository_metadata(
    state: &HttpState,
    repo_type: RepositoryType,
    repo_id: &str,
    revision: &str,
    request_kind: &'static str,
) -> Result<Option<RepositoryMetadata>, StatusCode> {
    tracing::info!(
        event = "archive_request",
        repo_type = %repo_type,
        request_kind,
        repo_id = %repo_id,
        requested_revision = %revision,
        "archive request received"
    );
    let commit = match archived_commit(&state.archive, repo_type, repo_id, revision) {
        Ok(commit) => {
            tracing::info!(event = "archive_hit", request_kind, repo_type = %repo_type, repo_id = %repo_id, requested_revision = %revision, commit = %commit, "archive request served locally");
            commit
        }
        Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(event = "archive_miss", request_kind, repo_type = %repo_type, repo_id = %repo_id, requested_revision = %revision, "archive request requires upstream acquisition");
            let Some(pullthrough) = state.pullthrough.clone() else {
                return Err(StatusCode::NOT_FOUND);
            };
            if let Some(metadata) =
                upstream_metadata(Arc::clone(&pullthrough), repo_type, repo_id, revision).await?
            {
                return Ok(Some(RepositoryMetadata::from_upstream(metadata)));
            }
            // Unbounded by default: a metadata cold miss waits for the
            // acquisition, because no supported client retries a bounded
            // answer on this route.
            let acquired = await_cold_miss_acquisition(
                state.cold_miss.metadata_deadline,
                pullthrough,
                repo_type,
                repo_id,
                revision,
                request_kind,
                None,
            )
            .await?;
            let Some(commit) = acquired else {
                return Ok(None);
            };
            commit
        }
        Err(error) => return Err(status_for_archive_error(error)),
    };
    archived_metadata(&state.archive, repo_type, repo_id, &commit).map(Some)
}

/// The commit an archived metadata request resolves to, or a not-found error.
fn archived_commit(
    archive: &Archive,
    repo_type: RepositoryType,
    repo_id: &str,
    revision: &str,
) -> Result<String, ArchiveError> {
    if is_hf_commit(revision) {
        let path = archive.revision_path_for_type(repo_type, repo_id, revision)?;
        if path.is_dir() {
            Ok(revision.to_string())
        } else {
            Err(ArchiveError::Io(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            )))
        }
    } else {
        archive.resolve_ref_for_type(repo_type, repo_id, revision)
    }
}

/// Upstream's file list for a revision the archive does not hold.
///
/// The helper invocation is blocking and owns no staging, so it runs on the
/// blocking pool and is never gated behind a transfer (ADR-0021 decision 3).
async fn upstream_metadata(
    pullthrough: Arc<PullThrough>,
    repo_type: RepositoryType,
    repo_id: &str,
    revision: &str,
) -> Result<Option<crate::upstream::UpstreamRepositoryFiles>, StatusCode> {
    let owned_repo_id = repo_id.to_string();
    let owned_revision = revision.to_string();
    task::spawn_blocking(move || {
        pullthrough.upstream_metadata_for_type(repo_type, &owned_repo_id, &owned_revision)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(status_for_pullthrough_error)
}

/// What the archive reports for a revision it holds.
///
/// The answer is the manifest's file set together with the commit's recorded
/// upstream file list where the revision has one, so a partially archived
/// revision reports the whole repository instead of presenting its subset as
/// all there is — which for a sharded model means handing back a model with
/// shards missing and calling it a successful download. A revision without a
/// record reports exactly the archived set, as before.
///
/// `oid` is the git object id upstream recorded for that path, and nothing else:
/// a revision with no record, or a path the record does not name, reports no
/// `oid` rather than a digest standing in for one (Issue 0079). ModelKeep's own
/// digest is reported in its own property instead, for every path the archive
/// holds, and is the value the `resolve` route returns as `ETag`. ModelKeep never
/// reports a digest for bytes it does not hold, and never presents an upstream
/// value as one it verified. `size` is what ModelKeep will serve when it holds
/// the file.
fn archived_metadata(
    archive: &Archive,
    repo_type: RepositoryType,
    repo_id: &str,
    commit: &str,
) -> Result<RepositoryMetadata, StatusCode> {
    let manifest = validated_manifest(archive, repo_type, repo_id, commit)?;
    // An unreadable record is an integrity failure, never a miss and never an
    // empty repository.
    let recorded = archive
        .upstream_files_for_type(repo_type, repo_id, commit)
        .map_err(status_for_archive_error)?
        .unwrap_or_default()
        .into_iter()
        .map(|file| (file.path.clone(), file))
        .collect::<BTreeMap<_, _>>();
    let mut files = BTreeMap::new();
    for file in manifest["files"]
        .as_array()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
    {
        let path = file["path"]
            .as_str()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        if crate::is_internal_archive_path(path) {
            continue;
        }
        files.insert(
            path.to_string(),
            MetadataFile {
                path: path.to_string(),
                size: file["size"].as_u64(),
                oid: recorded
                    .get(path)
                    .and_then(|recorded| recorded.blob_id.clone()),
                digest: file["sha256"].as_str().map(str::to_string),
            },
        );
    }
    for (path, recorded) in &recorded {
        files.entry(path.clone()).or_insert_with(|| MetadataFile {
            path: path.clone(),
            size: recorded.size,
            oid: recorded.blob_id.clone(),
            digest: None,
        });
    }
    Ok(RepositoryMetadata {
        commit: commit.to_string(),
        files: files.into_values().collect(),
    })
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
    // The validator is the file's recorded content digest, not a value derived
    // from its identity plus its length (Issue 0078). `huggingface_hub` names
    // each blob in its cache after this value, so two files of equal size and
    // different bytes sharing one validator makes the client store one file's
    // bytes under both paths. A digest cannot collide unless the bytes are the
    // same, in which case sharing the blob is correct and is what the Hub does
    // with an LFS `oid`. Both pinned clients accept the digest in `ETag` alone
    // and need no `x-linked-etag`; see
    // `docs/observations/hugging-face-content-validator-2026-09-24.md`.
    let etag = format!("\"{}\"", resolved.sha256);
    let mut response = Response::builder()
        .status(status)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, content_length)
        .header(header::ETAG, &etag)
        .header("x-repo-commit", &resolved_commit);
    if let Some(ByteRange { start, end }) = range {
        response = response.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    // Compared against the same content-derived value, so a `304` states that
    // the client holds these bytes rather than merely a file of this length in
    // this revision.
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(etag.as_str())
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

    /// The validator a response must carry for `bytes`: its content digest,
    /// quoted as a strong ETag.
    fn content_validator(bytes: &[u8]) -> String {
        use sha2::Digest;
        format!("\"{:x}\"", sha2::Sha256::digest(bytes))
    }

    async fn validator_of(app: &Router, uri: &str) -> String {
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
        assert_eq!(response.status(), StatusCode::OK);
        response.headers()[header::ETAG].to_str().unwrap().into()
    }

    /// A router over one revision holding `files`, so a test can choose the
    /// lengths and the bytes independently of each other.
    fn router_over(files: &[(&str, &[u8])]) -> (Router, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        archive
            .publish_revision(crate::PublishRequest {
                repo_id: "org/shards".into(),
                requested_revision: "main".into(),
                commit: "dddddddddddddddddddddddddddddddddddddddd".into(),
                files: files
                    .iter()
                    .map(|(path, bytes)| crate::ArchiveFile {
                        path: (*path).into(),
                        bytes: bytes.to_vec(),
                    })
                    .collect(),
            })
            .unwrap();
        (router(archive), directory)
    }

    #[tokio::test]
    async fn returns_not_modified_for_matching_etag() {
        let (app, _directory) = test_router();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/config.json")
                    .header(header::IF_NONE_MATCH, content_validator(b"0123456789"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            response.headers()[header::ETAG],
            content_validator(b"0123456789")
        );
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .len(),
            0
        );
    }

    /// Issue 0078. Two files of one length and different bytes must not share a
    /// validator, because `huggingface_hub` names a cached blob after it and
    /// would then hold one file's bytes under both paths.
    #[tokio::test]
    async fn equal_length_files_are_served_with_distinct_validators() {
        let first = b"MODELKEEP-COLLISION-SHARD-1";
        let second = b"MODELKEEP-COLLISION-SHARD-2";
        assert_eq!(first.len(), second.len());
        let (app, _directory) = router_over(&[
            ("model-00001-of-00002.safetensors", first),
            ("model-00002-of-00002.safetensors", second),
        ]);
        let base = "/org/shards/resolve/dddddddddddddddddddddddddddddddddddddddd";
        let first_validator =
            validator_of(&app, &format!("{base}/model-00001-of-00002.safetensors")).await;
        let second_validator =
            validator_of(&app, &format!("{base}/model-00002-of-00002.safetensors")).await;
        assert_ne!(first_validator, second_validator);
        // Distinctness alone would also be satisfied by a value derived from
        // the path, which would break deduplication; the validator has to *be*
        // the content digest.
        assert_eq!(first_validator, content_validator(first));
        assert_eq!(second_validator, content_validator(second));
    }

    /// Issue 0078, the other half of the contract. Byte-identical files share a
    /// validator on purpose: a client that stores one blob for both is correct,
    /// and is doing what the Hub does with an LFS `oid`. Do not "fix" this into
    /// per-path validators to make collisions impossible by construction.
    #[tokio::test]
    async fn byte_identical_files_share_one_validator() {
        let payload = b"MODELKEEP-IDENTICAL-PAYLOAD";
        let (app, _directory) = router_over(&[
            ("duplicate-one.json", payload),
            ("duplicate-two.json", payload),
        ]);
        let base = "/org/shards/resolve/dddddddddddddddddddddddddddddddddddddddd";
        let first = validator_of(&app, &format!("{base}/duplicate-one.json")).await;
        let second = validator_of(&app, &format!("{base}/duplicate-two.json")).await;
        assert_eq!(first, second);
        assert_eq!(first, content_validator(payload));
    }

    /// Issue 0078. Every manifest this implementation can read records a digest
    /// for every file, because the field is required to deserialize at all. A
    /// manifest without one therefore fails closed: the file is not served, and
    /// in particular is not served with a validator synthesised from something
    /// other than its content.
    #[tokio::test]
    async fn a_manifest_without_a_recorded_digest_is_not_served() {
        let (app, directory) = test_router();
        let manifest_path = directory
            .path()
            .join("models/org/model/revisions")
            .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .join(".modelkeep-manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["files"][0]
            .as_object_mut()
            .unwrap()
            .remove("sha256");
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/config.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response.headers().get(header::ETAG).is_none());
    }

    /// Issue 0078. A `304` must mean "you hold these bytes", so another file's
    /// validator cannot satisfy the condition even at the same length in the
    /// same revision.
    #[tokio::test]
    async fn if_none_match_from_another_file_is_not_a_match() {
        let first = b"MODELKEEP-COLLISION-SHARD-1";
        let second = b"MODELKEEP-COLLISION-SHARD-2";
        let (app, _directory) = router_over(&[
            ("model-00001-of-00002.safetensors", first),
            ("model-00002-of-00002.safetensors", second),
        ]);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(concat!(
                        "/org/shards/resolve/",
                        "dddddddddddddddddddddddddddddddddddddddd",
                        "/model-00002-of-00002.safetensors"
                    ))
                    .header(header::IF_NONE_MATCH, content_validator(first))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::ETAG], content_validator(second));
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            second.as_slice()
        );
    }

    /// Issue 0078. A client validates and ranges over the same identity, so a
    /// `HEAD` and a partial response must advertise the whole file's validator.
    #[tokio::test]
    async fn head_and_range_carry_the_full_response_validator() {
        let payload = b"MODELKEEP-COLLISION-SHARD-1";
        let (app, _directory) = router_over(&[("model-00001-of-00002.safetensors", payload)]);
        let uri = concat!(
            "/org/shards/resolve/",
            "dddddddddddddddddddddddddddddddddddddddd",
            "/model-00001-of-00002.safetensors"
        );
        let whole = validator_of(&app, uri).await;
        assert_eq!(whole, content_validator(payload));

        let head = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("HEAD")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()[header::ETAG], whole);

        let partial = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .header(header::RANGE, "bytes=0-4")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(partial.headers()[header::ETAG], whole);
        assert_eq!(
            to_bytes(partial.into_body(), usize::MAX).await.unwrap(),
            b"MODEL".as_slice()
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
        // The resolved commit stays the revision identity in `x-repo-commit`;
        // the validator is the content digest and so does not carry it
        // (Issue 0078).
        assert_eq!(
            response.headers()[header::ETAG],
            content_validator(b"0123456789")
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

    /// A fetcher that reports upstream's per-file metadata, like the production
    /// helper, and records everything it was asked to do.
    struct MetadataFetcher {
        commit: String,
        upstream: Vec<(String, Vec<u8>)>,
        transfers: Arc<Mutex<Vec<Vec<String>>>>,
        metadata_calls: Arc<std::sync::atomic::AtomicUsize>,
        metadata_error: Option<UpstreamErrorKind>,
    }

    impl MetadataFetcher {
        fn new(commit: &str, upstream: &[(&str, &[u8])]) -> Self {
            Self {
                commit: commit.into(),
                upstream: upstream
                    .iter()
                    .map(|(path, bytes)| ((*path).to_string(), bytes.to_vec()))
                    .collect(),
                transfers: Arc::new(Mutex::new(Vec::new())),
                metadata_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                metadata_error: None,
            }
        }

        fn failing(commit: &str, kind: UpstreamErrorKind) -> Self {
            let mut fetcher = Self::new(commit, &[]);
            fetcher.metadata_error = Some(kind);
            fetcher
        }

        /// Upstream's own values: a byte length and a git object id that is
        /// deliberately not any digest of the content, which is what a real
        /// non-LFS `blob_id` is.
        fn reported(&self) -> Vec<crate::UpstreamFile> {
            self.upstream
                .iter()
                .enumerate()
                .map(|(index, (path, bytes))| crate::UpstreamFile {
                    path: path.clone(),
                    size: Some(bytes.len() as u64),
                    blob_id: Some(format!("{:040x}", 0xb10b0000u64 + index as u64)),
                    lfs_sha256: None,
                })
                .collect()
        }

        fn error(&self) -> crate::upstream::UpstreamError {
            match self.metadata_error {
                Some(UpstreamErrorKind::NotFound) => crate::upstream::UpstreamError::NotFound,
                Some(UpstreamErrorKind::Unauthorized) => {
                    crate::upstream::UpstreamError::Unauthorized
                }
                _ => crate::upstream::UpstreamError::Unavailable,
            }
        }
    }

    impl crate::upstream::UpstreamFetcher for MetadataFetcher {
        fn fetch(
            &self,
            request: &crate::upstream::FetchRequest,
        ) -> Result<crate::upstream::FetchedRevision, crate::upstream::UpstreamError> {
            self.transfers.lock().unwrap().push(request.files.clone());
            if self.metadata_error.is_some() {
                return Err(self.error());
            }
            let mut files = Vec::new();
            for (path, bytes) in &self.upstream {
                if !request.files.is_empty() && !request.files.iter().any(|file| file == path) {
                    continue;
                }
                std::fs::write(request.staging.join(path), bytes).unwrap();
                files.push(path.clone());
            }
            crate::write_staged_upstream_files(
                &request.staging,
                request.repo_type,
                &request.repo_id,
                &self.commit,
                &self.reported(),
            )
            .unwrap();
            Ok(crate::upstream::FetchedRevision {
                commit: self.commit.clone(),
                files,
                staging: request.staging.clone(),
            })
        }

        fn repository_files(
            &self,
            _request: &crate::upstream::InventoryRequest,
        ) -> Result<Option<crate::upstream::UpstreamRepositoryFiles>, crate::upstream::UpstreamError>
        {
            self.metadata_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.metadata_error.is_some() {
                return Err(self.error());
            }
            Ok(Some(crate::upstream::UpstreamRepositoryFiles {
                commit: self.commit.clone(),
                files: self.reported(),
            }))
        }
    }

    async fn json_body(response: Response) -> serde_json::Value {
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    async fn get(app: &Router, uri: &str) -> Response {
        app.clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    const METADATA_COMMIT: &str = "cccccccccccccccccccccccccccccccccccccccc";

    /// Issue 0074: the whole point. A metadata request for a revision the archive
    /// has never seen is answered from upstream's file list, and starts no
    /// acquisition, so the client's own filter decides what is transferred next.
    #[tokio::test]
    async fn metadata_for_an_unarchived_revision_answers_without_acquiring() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let fetcher = Arc::new(MetadataFetcher::new(
            METADATA_COMMIT,
            &[
                ("config.json", b"cold-http"),
                ("model-00001-of-00002.safetensors", b"shard-one"),
            ],
        ));
        let transfers = Arc::clone(&fetcher.transfers);
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher));
        let app = router_with_pullthrough(archive.clone(), pullthrough);

        let info = get(&app, "/api/models/org/model/revision/main").await;
        assert_eq!(info.status(), StatusCode::OK);
        let info = json_body(info).await;
        assert_eq!(info["sha"], METADATA_COMMIT);
        assert_eq!(
            info["siblings"]
                .as_array()
                .unwrap()
                .iter()
                .map(|sibling| sibling["rfilename"].as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            ["config.json", "model-00001-of-00002.safetensors"]
        );

        let tree = json_body(get(&app, "/api/models/org/model/tree/main").await).await;
        assert_eq!(tree[0]["path"], "config.json");
        assert_eq!(tree[0]["size"], 9);
        // Upstream's own git object id, which is what the Hub reports here.
        assert_eq!(tree[0]["oid"], format!("{:040x}", 0xb10b0000u64));

        // No acquisition was started, and nothing was written to the archive:
        // a metadata answer from upstream never becomes archived state.
        assert!(transfers.lock().unwrap().is_empty());
        assert!(!archive
            .revision_path("org/model", METADATA_COMMIT)
            .unwrap()
            .exists());
        assert!(archive.resolve_ref("org/model", "main").is_err());
    }

    /// A mutable ref resolved on the metadata route is still learned.
    ///
    /// A supported client resolves `main` through metadata and then requests every
    /// file by commit, so nothing after the metadata request names the ref. The
    /// archive has to learn the name anyway, or a revision mirrored by an ordinary
    /// `hf download` would be downloadable only by commit while upstream is
    /// unavailable (core invariant 8).
    #[tokio::test]
    async fn a_ref_resolved_on_the_metadata_route_is_learned_once_a_revision_exists() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let fetcher = Arc::new(MetadataFetcher::new(
            METADATA_COMMIT,
            &[("config.json", b"cold-http")],
        ));
        let calls = Arc::clone(&fetcher.metadata_calls);
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher));
        let app = router_with_pullthrough(archive.clone(), pullthrough);

        assert_eq!(
            get(&app, "/api/models/org/model/revision/main")
                .await
                .status(),
            StatusCode::OK
        );
        // Answering metadata wrote nothing, so the ref cannot exist yet: it would
        // name a revision the archive does not have.
        assert!(archive.resolve_ref("org/model", "main").is_err());

        // The client now asks for the file by commit, as both supported clients do.
        let file = get(
            &app,
            "/org/model/resolve/cccccccccccccccccccccccccccccccccccccccc/config.json",
        )
        .await;
        assert_eq!(file.status(), StatusCode::OK);

        assert_eq!(
            archive.resolve_ref("org/model", "main").unwrap(),
            METADATA_COMMIT
        );
        // And the name now answers from the archive, without upstream.
        let observed = calls.load(std::sync::atomic::Ordering::SeqCst);
        let info = json_body(get(&app, "/api/models/org/model/revision/main").await).await;
        assert_eq!(info["sha"], METADATA_COMMIT);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), observed);
    }

    /// Issue 0074 acceptance: an unarchived revision whose upstream cannot be
    /// reached fails in a way an operator can classify, and publishes nothing.
    #[tokio::test]
    async fn an_unarchived_metadata_request_reports_an_unreachable_upstream() {
        for (kind, expected) in [
            (UpstreamErrorKind::Unavailable, StatusCode::BAD_GATEWAY),
            (UpstreamErrorKind::NotFound, StatusCode::NOT_FOUND),
            (UpstreamErrorKind::Unauthorized, StatusCode::UNAUTHORIZED),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let archive = Archive::new(directory.path()).unwrap();
            let fetcher = Arc::new(MetadataFetcher::failing(METADATA_COMMIT, kind));
            let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher));
            let app = router_with_pullthrough(archive.clone(), pullthrough);

            for uri in [
                "/api/models/org/model/revision/main",
                "/api/models/org/model/tree/main",
            ] {
                let response = get(&app, uri).await;
                assert_eq!(response.status(), expected, "{uri}");
                // No fabricated metadata: the body is not a file list.
                let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                let body = String::from_utf8_lossy(&body).into_owned();
                assert!(!body.contains("rfilename"), "{uri}: {body}");
                assert!(!body.contains("config.json"), "{uri}: {body}");
            }
            assert!(!archive
                .revision_path("org/model", METADATA_COMMIT)
                .unwrap()
                .exists());
        }
    }

    /// Core invariant 8, with a recorded file list in place: an archived revision
    /// answers metadata from the archive and never consults upstream.
    #[tokio::test]
    async fn an_archived_revision_answers_metadata_without_contacting_upstream() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        archive
            .publish_revision(crate::PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: METADATA_COMMIT.into(),
                files: vec![crate::ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"archived".to_vec(),
                }],
            })
            .unwrap();
        archive
            .update_ref("org/model", "main", METADATA_COMMIT)
            .unwrap();
        archive
            .record_upstream_files_for_type(
                RepositoryType::Model,
                "org/model",
                METADATA_COMMIT,
                &[crate::UpstreamFile {
                    path: "config.json".into(),
                    size: Some(8),
                    blob_id: Some("d".repeat(40)),
                    lfs_sha256: None,
                }],
            )
            .unwrap();
        let fetcher = Arc::new(MetadataFetcher::failing(
            METADATA_COMMIT,
            UpstreamErrorKind::Unavailable,
        ));
        let calls = Arc::clone(&fetcher.metadata_calls);
        let transfers = Arc::clone(&fetcher.transfers);
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher));
        let app = router_with_pullthrough(archive, pullthrough);

        for uri in [
            "/api/models/org/model/revision/main",
            "/api/models/org/model/tree/main",
            "/api/models/org/model/revision/cccccccccccccccccccccccccccccccccccccccc",
        ] {
            assert_eq!(get(&app, uri).await.status(), StatusCode::OK, "{uri}");
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(transfers.lock().unwrap().is_empty());
    }

    /// Issue 0074: a partially archived revision stops presenting its subset as
    /// the whole repository, which for a sharded model means handing back a model
    /// with shards missing and calling it a successful download. The path it does
    /// not hold is reported, and requesting it extends the same revision.
    #[tokio::test]
    async fn a_partially_archived_revision_reports_the_files_it_does_not_hold() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let fetcher = Arc::new(MetadataFetcher::new(
            METADATA_COMMIT,
            &[
                ("config.json", b"cold-http"),
                ("model-00001-of-00002.safetensors", b"shard-one"),
            ],
        ));
        let reported = fetcher.reported();
        let transfers = Arc::clone(&fetcher.transfers);
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher));
        let app = router_with_pullthrough(archive.clone(), pullthrough);

        // One file is requested, so one file is archived (Issue 0070).
        let file = get(&app, "/org/model/resolve/main/config.json").await;
        assert_eq!(file.status(), StatusCode::OK);
        assert_eq!(transfers.lock().unwrap().clone(), vec![vec!["config.json"]]);

        let tree = json_body(get(&app, "/api/models/org/model/tree/main").await).await;
        let entries = tree.as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1]["path"], "model-00001-of-00002.safetensors");
        // The size and object id upstream recorded, for a file the archive does
        // not hold. No digest is claimed for bytes ModelKeep does not have.
        assert_eq!(entries[1]["size"], 9);
        assert_eq!(entries[1]["oid"], reported[1].blob_id.clone().unwrap());

        // Requesting the reported path extends the same revision.
        let shard = get(
            &app,
            "/org/model/resolve/main/model-00001-of-00002.safetensors",
        )
        .await;
        assert_eq!(shard.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(shard.into_body(), usize::MAX).await.unwrap(),
            "shard-one"
        );
        assert_eq!(
            archive.list_revisions("org/model").unwrap(),
            vec![METADATA_COMMIT.to_string()]
        );
        assert_eq!(
            transfers.lock().unwrap().clone(),
            vec![
                vec!["config.json"],
                vec!["model-00001-of-00002.safetensors"]
            ]
        );
    }

    /// A revision published before the upstream file list was recorded, or
    /// imported from a client cache, keeps answering exactly the archived set.
    /// Nothing migrates it, and nothing about it fails.
    #[tokio::test]
    async fn a_revision_without_a_recorded_file_list_reports_the_archived_set() {
        let (app, directory) = test_router();
        let archive = Archive::open_read_only(directory.path()).unwrap();
        let commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(archive
            .upstream_files_for_type(RepositoryType::Model, "org/model", commit)
            .unwrap()
            .is_none());

        let info = json_body(get(&app, "/api/models/org/model/revision/main").await).await;
        assert_eq!(
            info["siblings"].as_array().unwrap().len(),
            1,
            "only the archived set"
        );
        let tree = json_body(get(&app, "/api/models/org/model/tree/main").await).await;
        assert_eq!(tree.as_array().unwrap().len(), 1);
        assert_eq!(tree[0]["path"], "config.json");
        assert_eq!(tree[0]["size"], 10);
        // Issue 0079: the git object id was never recorded for this revision, so
        // it is reported as absent. Nothing stands in for it — not ModelKeep's
        // digest, which is reported in ModelKeep's own property, and not any
        // other value.
        assert_eq!(tree[0]["oid"], serde_json::Value::Null);
        assert_eq!(info["siblings"][0]["blobId"], serde_json::Value::Null);
        assert!(tree[0].get("lfs").is_none(), "{tree}");
        let digest = tree[0]["modelkeep"]["sha256"].as_str().unwrap().to_string();
        assert_eq!(info["siblings"][0]["modelkeep"]["sha256"], digest);
        let file = get(&app, "/org/model/resolve/main/config.json").await;
        assert_eq!(file.headers()[header::ETAG], format!("\"{digest}\""));
    }

    /// The documented meaning of a tree `oid`, pinned against `resolve`.
    ///
    /// `oid` is upstream's git object id for the commit — a fact about the commit,
    /// not a digest ModelKeep verified — and is unrelated to the `ETag`.
    /// ModelKeep's own digest is reported beside it, in ModelKeep's own property,
    /// and equals the `ETag` exactly. Where no object id was recorded, `oid` is
    /// absent and the digest is still there, so the two never stand in for each
    /// other (Issue 0079).
    #[tokio::test]
    async fn the_tree_object_id_is_upstreams_and_the_digest_is_modelkeeps_own() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let fetcher = Arc::new(MetadataFetcher::new(
            METADATA_COMMIT,
            &[("config.json", b"cold-http")],
        ));
        let blob_id = fetcher.reported()[0].blob_id.clone().unwrap();
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher));
        let app = router_with_pullthrough(archive.clone(), pullthrough);

        assert_eq!(
            get(&app, "/org/model/resolve/main/config.json")
                .await
                .status(),
            StatusCode::OK
        );
        let tree = json_body(get(&app, "/api/models/org/model/tree/main").await).await;
        assert_eq!(tree[0]["oid"], blob_id);
        let file = get(&app, "/org/model/resolve/main/config.json").await;
        let etag = file.headers()[header::ETAG].to_str().unwrap().to_string();
        assert_eq!(etag, format!("\"{}\"", crate::sha256(b"cold-http")));
        assert_ne!(etag.trim_matches('"'), blob_id);
        // ModelKeep's own digest is the `ETag`, in ModelKeep's own property, and
        // is never what `oid` holds.
        assert_eq!(tree[0]["modelkeep"]["sha256"], etag.trim_matches('"'));

        // The same revision without its record still answers, and still carries
        // the digest. Only the value it never recorded goes absent.
        std::fs::remove_file(
            archive
                .revision_path("org/model", METADATA_COMMIT)
                .unwrap()
                .join(crate::UPSTREAM_FILES_FILE),
        )
        .unwrap();
        let tree = json_body(get(&app, "/api/models/org/model/tree/main").await).await;
        assert_eq!(tree[0]["oid"], serde_json::Value::Null);
        assert_eq!(tree[0]["modelkeep"]["sha256"], etag.trim_matches('"'));
    }

    /// Issue 0079, the LFS decision, pinned so a relayed digest cannot reappear.
    ///
    /// ModelKeep reports no `lfs` object even where upstream recorded an LFS
    /// sha256 for the path, so the validator a supported client uses stays the
    /// `ETag` ModelKeep computed from the bytes it holds. The recorded upstream
    /// LFS digest is deliberately a value ModelKeep never serves, and this test
    /// fails if it ever reaches a response body — which is exactly what emitting
    /// `lfs` would do, and what would move the client's validator off the value
    /// ModelKeep verifies. See
    /// `docs/observations/hugging-face-lfs-reporting-2026-09-24.md`.
    #[tokio::test]
    async fn an_lfs_managed_file_reports_no_lfs_object_and_one_verifiable_digest() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let commit = METADATA_COMMIT;
        archive
            .publish_revision(crate::PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: commit.into(),
                files: vec![crate::ArchiveFile {
                    path: "model.safetensors".into(),
                    bytes: b"lfs-managed-payload".to_vec(),
                }],
            })
            .unwrap();
        archive.update_ref("org/model", "main", commit).unwrap();
        // A digest that is not the digest of the archived bytes, so relaying it
        // is observable rather than accidentally correct.
        let upstream_lfs_digest = crate::sha256(b"a digest modelkeep never serves");
        assert_ne!(upstream_lfs_digest, crate::sha256(b"lfs-managed-payload"));
        archive
            .record_upstream_files_for_type(
                RepositoryType::Model,
                "org/model",
                commit,
                &[crate::UpstreamFile {
                    path: "model.safetensors".into(),
                    size: Some(19),
                    blob_id: Some("e".repeat(40)),
                    lfs_sha256: Some(upstream_lfs_digest.clone()),
                }],
            )
            .unwrap();
        let app = router(archive);

        let file = get(&app, "/org/model/resolve/main/model.safetensors").await;
        let etag = file.headers()[header::ETAG].to_str().unwrap().to_string();
        let served = etag.trim_matches('"').to_string();
        assert_eq!(served, crate::sha256(b"lfs-managed-payload"));

        for uri in [
            "/api/models/org/model/revision/main",
            "/api/models/org/model/tree/main",
        ] {
            let body = to_bytes(get(&app, uri).await.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = String::from_utf8_lossy(&body).into_owned();
            assert!(!body.contains("\"lfs\""), "{uri}: {body}");
            assert!(!body.contains("xetHash"), "{uri}: {body}");
            assert!(!body.contains("pointerSize"), "{uri}: {body}");
            // The validator a client can read from a listing is ModelKeep's own
            // digest, and upstream's LFS digest appears nowhere.
            assert!(!body.contains(&upstream_lfs_digest), "{uri}: {body}");
            assert!(body.contains(&served), "{uri}: {body}");
        }

        let tree = json_body(get(&app, "/api/models/org/model/tree/main").await).await;
        assert_eq!(tree[0]["oid"], "e".repeat(40));
        assert_eq!(tree[0]["modelkeep"]["sha256"], served);
        let info = json_body(get(&app, "/api/models/org/model/revision/main").await).await;
        assert_eq!(info["siblings"][0]["blobId"], "e".repeat(40));
        assert_eq!(info["siblings"][0]["modelkeep"]["sha256"], served);
    }

    /// Issue 0079: ModelKeep's property is on every file the archive holds, in
    /// both routes, and its digest is the one `resolve` advertises. For a file the
    /// archive does not hold the property is present and states nothing, because
    /// there are no bytes ModelKeep can stand behind.
    #[tokio::test]
    async fn every_archived_file_carries_modelkeeps_digest_in_both_routes() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let fetcher = Arc::new(MetadataFetcher::new(
            METADATA_COMMIT,
            &[
                ("config.json", b"cold-http"),
                ("model-00001-of-00002.safetensors", b"shard-one"),
            ],
        ));
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher));
        let app = router_with_pullthrough(archive.clone(), pullthrough);

        // One file requested, so the revision holds one of the two it reports.
        assert_eq!(
            get(&app, "/org/model/resolve/main/config.json")
                .await
                .status(),
            StatusCode::OK
        );

        let tree = json_body(get(&app, "/api/models/org/model/tree/main").await).await;
        let info = json_body(get(&app, "/api/models/org/model/revision/main").await).await;
        let entries = tree.as_array().unwrap();
        let siblings = info["siblings"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(siblings.len(), 2);
        for (entry, sibling) in entries.iter().zip(siblings) {
            let path = entry["path"].as_str().unwrap();
            assert_eq!(sibling["rfilename"], path);
            // The property is a nested object in both routes, so a later
            // addition needs no new name.
            assert!(entry["modelkeep"].is_object(), "{entry}");
            assert!(sibling["modelkeep"].is_object(), "{sibling}");
            assert_eq!(entry["modelkeep"], sibling["modelkeep"]);
            let held = get(
                &app,
                &format!("/org/model/resolve/cccccccccccccccccccccccccccccccccccccccc/{path}"),
            )
            .await;
            match entry["modelkeep"]["sha256"].as_str() {
                Some(digest) => {
                    assert_eq!(path, "config.json");
                    assert_eq!(held.headers()[header::ETAG], format!("\"{digest}\""));
                }
                // The archive does not hold this one: no digest is claimed, and
                // no other value is substituted for it.
                None => {
                    assert_eq!(path, "model-00001-of-00002.safetensors");
                    assert_eq!(entry["modelkeep"]["sha256"], serde_json::Value::Null);
                }
            }
        }
    }

    /// Issue 0079: the `revision` route's siblings carry the Hub's per-file fields
    /// with the Hub's meanings, and ModelKeep's property beside them. They used to
    /// carry `rfilename` alone, which made the digest reachable only through the
    /// `tree` route's `oid`.
    #[tokio::test]
    async fn revision_siblings_carry_the_hub_per_file_fields_and_modelkeeps_own() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let fetcher = Arc::new(MetadataFetcher::new(
            METADATA_COMMIT,
            &[("config.json", b"cold-http")],
        ));
        let blob_id = fetcher.reported()[0].blob_id.clone().unwrap();
        let pullthrough = Arc::new(PullThrough::new(archive.clone(), fetcher));
        let app = router_with_pullthrough(archive.clone(), pullthrough);
        assert_eq!(
            get(&app, "/org/model/resolve/main/config.json")
                .await
                .status(),
            StatusCode::OK
        );

        let info = json_body(get(&app, "/api/models/org/model/revision/main").await).await;
        let sibling = &info["siblings"][0];
        assert_eq!(sibling["rfilename"], "config.json");
        assert_eq!(sibling["size"], 9);
        assert_eq!(sibling["blobId"], blob_id);
        assert!(sibling.get("lfs").is_none(), "{sibling}");
        let file = get(&app, "/org/model/resolve/main/config.json").await;
        let etag = file.headers()[header::ETAG].to_str().unwrap().to_string();
        assert_eq!(sibling["modelkeep"]["sha256"], etag.trim_matches('"'));
        // The two routes report one file list, so they cannot drift apart in what
        // they say about a file either.
        let tree = json_body(get(&app, "/api/models/org/model/tree/main").await).await;
        assert_eq!(tree[0]["size"], sibling["size"]);
        assert_eq!(tree[0]["oid"], sibling["blobId"]);
        assert_eq!(tree[0]["modelkeep"], sibling["modelkeep"]);
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
