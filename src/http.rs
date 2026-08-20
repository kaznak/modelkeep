use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use tokio::task;
use tracing::info;

use crate::pullthrough::PullThrough;
use crate::{parse_range, Archive, ArchiveError, ByteRange, RangeError};

#[derive(Clone)]
pub struct HttpState {
    archive: Arc<Archive>,
    pullthrough: Option<Arc<PullThrough>>,
}
pub async fn serve(archive: Archive, address: std::net::SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, router(archive)).await
}

pub async fn serve_with_pullthrough(
    archive: Archive,
    pullthrough: Arc<PullThrough>,
    address: std::net::SocketAddr,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, router_with_pullthrough(archive, pullthrough)).await
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
        .route(
            "/api/models/{namespace}/{repo}/revision/{revision}",
            get(model_info),
        )
        .route(
            "/{namespace}/{repo}/resolve/{revision}/{*path}",
            get(get_file).head(head_file),
        )
        .with_state(state)
}

async fn model_info(
    State(state): State<HttpState>,
    Path((namespace, repo, revision)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let repo_id = format!("{namespace}/{repo}");
    let commit = match if is_commit(&revision) {
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
                .map_err(|_| StatusCode::BAD_GATEWAY)?
                .map_err(|_| StatusCode::BAD_GATEWAY)?
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
    let resolved_result = if is_commit(&revision) {
        state.archive.resolve_file(&repo_id, &revision, &path)
    } else {
        state
            .archive
            .resolve_ref(&repo_id, &revision)
            .and_then(|commit| state.archive.resolve_file(&repo_id, &commit, &path))
    };
    let resolved = match resolved_result {
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
            .map_err(|_| StatusCode::BAD_GATEWAY)?
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
            state
                .archive
                .resolve_file(&repo_id, &commit, &path)
                .map_err(status_for_archive_error)?
        }
        Err(error) => return Err(status_for_archive_error(error)),
    };
    let size = resolved.size;
    let etag_revision = if is_commit(&revision) {
        revision.clone()
    } else {
        state
            .archive
            .resolve_ref(&repo_id, &revision)
            .unwrap_or(revision.clone())
    };
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
        .header(header::ETAG, format!("\"{etag_revision}-{size}\""))
        .header("x-repo-commit", &etag_revision);
    if let Some(ByteRange { start, end }) = range {
        response = response.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(format!("\"{etag_revision}-{size}\"").as_str())
    {
        return response
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
    }
    let body = if head_only {
        Body::empty()
    } else {
        let file = resolved.path;
        let bytes = task::spawn_blocking(move || read_range(&file, start, content_length))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Body::from(bytes)
    };
    response
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn read_range(path: &std::path::Path, start: u64, length: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; usize::try_from(length).map_err(|_| std::io::ErrorKind::InvalidInput)?];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
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

fn status_for_range_error(error: RangeError) -> StatusCode {
    match error {
        RangeError::Invalid => StatusCode::BAD_REQUEST,
        RangeError::Unsatisfiable => StatusCode::RANGE_NOT_SATISFIABLE,
    }
}

fn is_commit(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {

    use super::*;
    use axum::body::to_bytes;
    use tower::ServiceExt;

    fn test_router() -> (Router, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        archive
            .publish_revision(crate::PublishRequest {
                repo_id: "org/model".into(),
                requested_revision: "main".into(),
                commit: "aaaaaaaa".into(),
                files: vec![crate::ArchiveFile {
                    path: "config.json".into(),
                    bytes: b"0123456789".to_vec(),
                }],
            })
            .unwrap();
        (router(archive), directory)
    }

    #[tokio::test]
    async fn serves_model_info_for_revision() {
        let (app, _directory) = test_router();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/models/org/model/revision/aaaaaaaa")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["sha"], "aaaaaaaa");
        assert_eq!(value["siblings"][0]["rfilename"], "config.json");
    }

    #[tokio::test]
    async fn serves_full_and_partial_files() {
        let (app, _directory) = test_router();
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/org/model/resolve/aaaaaaaa/config.json")
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
                    .uri("/org/model/resolve/aaaaaaaa/config.json")
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
                    .uri("/org/model/resolve/aaaaaaaa/config.json")
                    .header(header::IF_NONE_MATCH, "\"aaaaaaaa-10\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::ETAG], "\"aaaaaaaa-10\"");
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn head_returns_metadata_without_body() {
        let (app, _directory) = test_router();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("HEAD")
                    .uri("/org/model/resolve/aaaaaaaa/config.json")
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
            Ok(crate::upstream::FetchedRevision {
                commit: "bbbbbbbb".into(),
                files: vec!["config.json".into()],
                staging: request.staging.clone(),
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
}
