use std::io::SeekFrom;
use std::sync::Arc;

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
use tracing::info;

use crate::pullthrough::{PullThrough, PullThroughError};
use crate::{is_hf_commit, parse_range, Archive, ArchiveError, ByteRange, RangeError};

#[derive(Clone)]
pub struct HttpState {
    archive: Arc<Archive>,
    pullthrough: Option<Arc<PullThrough>>,
}
pub async fn serve(archive: Archive, address: std::net::SocketAddr) -> std::io::Result<()> {
    serve_router(router(archive), address).await
}

pub async fn serve_with_pullthrough(
    archive: Archive,
    pullthrough: Arc<PullThrough>,
    address: std::net::SocketAddr,
) -> std::io::Result<()> {
    serve_router(router_with_pullthrough(archive, pullthrough), address).await
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
    })
}

pub fn router_with_pullthrough(archive: Archive, pullthrough: Arc<PullThrough>) -> Router {
    router_with_state(HttpState {
        archive: Arc::new(archive),
        pullthrough: Some(pullthrough),
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
            "/{namespace}/{repo}/resolve/{revision}/{*path}",
            get(get_file).head(head_file),
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
) -> Result<impl IntoResponse, StatusCode> {
    let repo_id = format!("{namespace}/{repo}");
    let commit = match if is_hf_commit(&revision) {
        state
            .archive
            .revision_path(&repo_id, &revision)
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
        state.archive.resolve_ref(&repo_id, &revision)
    } {
        Ok(commit) => commit,
        Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            let Some(pullthrough) = state.pullthrough.clone() else {
                return Err(StatusCode::NOT_FOUND);
            };
            let requested = revision.clone();
            let repo = repo_id.clone();
            task::spawn_blocking(move || pullthrough.ensure(&repo, &requested, &[]))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .map_err(status_for_pullthrough_error)?
        }
        Err(error) => return Err(status_for_archive_error(error)),
    };
    let manifest: serde_json::Value = serde_json::from_str(
        &state
            .archive
            .manifest(&repo_id, &commit)
            .map_err(status_for_archive_error)?,
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let siblings = manifest["files"]
        .as_array()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
        .iter()
        .filter_map(|file| file["path"].as_str())
        .map(|path| serde_json::json!({ "rfilename": path }))
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!({
        "id": repo_id, "sha": commit, "private": false, "downloads": 0,
        "likes": 0, "tags": [], "siblings": siblings,
    })))
}

async fn model_tree(
    State(state): State<HttpState>,
    Path((namespace, repo, revision)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let repo_id = format!("{namespace}/{repo}");
    let commit = if is_hf_commit(&revision) {
        revision
    } else {
        state
            .archive
            .resolve_ref(&repo_id, &revision)
            .map_err(status_for_archive_error)?
    };
    let manifest: serde_json::Value = serde_json::from_str(
        &state
            .archive
            .manifest(&repo_id, &commit)
            .map_err(status_for_archive_error)?,
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let files = manifest["files"]
        .as_array()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
        .iter()
        .map(|file| {
            serde_json::json!({
                "type": "file",
                "path": file["path"],
                "size": file["size"],
                "oid": file["sha256"],
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(files))
}

async fn get_file(
    State(state): State<HttpState>,
    Path((namespace, repo, revision, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    file_response(state, namespace, repo, revision, path, headers, false).await
}

async fn head_file(
    State(state): State<HttpState>,
    Path((namespace, repo, revision, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    file_response(state, namespace, repo, revision, path, headers, true).await
}

async fn file_response(
    state: HttpState,
    namespace: String,
    repo: String,
    revision: String,
    path: String,
    headers: HeaderMap,
    head_only: bool,
) -> Result<Response, StatusCode> {
    let repo_id = format!("{namespace}/{repo}");
    info!(repo_id = %repo_id, requested_revision = %revision, path = %path, "archive request");
    let resolved_result = if is_hf_commit(&revision) {
        state
            .archive
            .resolve_file(&repo_id, &revision, &path)
            .map(|file| (file, revision.clone()))
    } else {
        state
            .archive
            .resolve_ref(&repo_id, &revision)
            .and_then(|commit| {
                state
                    .archive
                    .resolve_file(&repo_id, &commit, &path)
                    .map(|file| (file, commit))
            })
    };
    let (resolved, resolved_commit) = match resolved_result {
        Ok(resolved) => resolved,
        Err(ArchiveError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            info!(repo_id = %repo_id, requested_revision = %revision, path = %path, "archive miss");
            let Some(pullthrough) = state.pullthrough.clone() else {
                return Err(StatusCode::NOT_FOUND);
            };
            let requested = revision.clone();
            let requested_file = path.clone();
            let repo = repo_id.clone();
            let commit = task::spawn_blocking(move || {
                pullthrough.ensure(&repo, &requested, &[requested_file])
            })
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .map_err(status_for_pullthrough_error)?;
            let resolved = state
                .archive
                .resolve_file(&repo_id, &commit, &path)
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

fn status_for_archive_error(error: ArchiveError) -> StatusCode {
    match error {
        ArchiveError::InvalidPath(_) => StatusCode::BAD_REQUEST,
        ArchiveError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            StatusCode::NOT_FOUND
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn status_for_pullthrough_error(error: PullThroughError) -> StatusCode {
    tracing::warn!(error_class = ?error, "pull-through request failed");
    match error {
        PullThroughError::UpstreamNotFound => StatusCode::NOT_FOUND,
        PullThroughError::UpstreamUnauthorized => StatusCode::UNAUTHORIZED,
        PullThroughError::UpstreamUnavailable | PullThroughError::UpstreamFailed => {
            StatusCode::BAD_GATEWAY
        }
        PullThroughError::Storage => StatusCode::INSUFFICIENT_STORAGE,
        PullThroughError::UnsafePath => StatusCode::BAD_REQUEST,
        PullThroughError::UpstreamInvalidOutput
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
        (router(archive), directory)
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
            std::fs::write(request.staging.join("config.json"), b"cold-http").unwrap();
            assert!(request.files.is_empty());
            std::fs::write(request.staging.join("tokenizer.json"), b"tokenizer-http").unwrap();
            Ok(crate::upstream::FetchedRevision {
                commit: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                files: vec!["config.json".into(), "tokenizer.json".into()],
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
}
