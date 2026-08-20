use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use tokio::task;

use crate::{parse_range, Archive, ArchiveError, ByteRange, RangeError};

#[derive(Clone)]
pub struct HttpState {
    archive: Arc<Archive>,
}

pub async fn serve(archive: Archive, address: std::net::SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, router(archive)).await
}

pub fn router(archive: Archive) -> Router {
    let state = HttpState {
        archive: Arc::new(archive),
    };
    Router::new()
        .route(
            "/{namespace}/{repo}/resolve/{revision}/{*path}",
            get(get_file).head(head_file),
        )
        .with_state(state)
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
    let resolved_result = state.archive.resolve_file(&repo_id, &revision, &path);
    let resolved = resolved_result.map_err(status_for_archive_error)?;
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
        .header(header::ETAG, format!("\"{revision}-{size}\""));
    if let Some(ByteRange { start, end }) = range {
        response = response.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
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
}
