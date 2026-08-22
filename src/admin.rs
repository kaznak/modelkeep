use std::{env, net::SocketAddr, sync::Arc};

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{pullthrough::PullThrough, Archive, ArchiveError, RepositorySummary};

const ADMIN_CAPABILITY: &str = "io.modelkeep/cap/admin";

#[derive(Clone)]
pub struct Config {
    pub address: SocketAddr,
    bearer_token: Option<String>,
    trust_tailscale_headers: bool,
}

impl Config {
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

pub fn router(archive: Archive, config: Config, pullthrough_enabled: bool) -> Router {
    let state = AdminState {
        archive: Arc::new(archive),
        config,
    };
    Router::new()
        .route(
            "/api/admin/v1/status",
            get(move |State(state), headers| status(state, headers, pullthrough_enabled)),
        )
        .route("/api/admin/v1/repositories", get(repositories))
        .route(
            "/api/admin/v1/repositories/{namespace}/{repository}",
            get(repository),
        )
        .with_state(state)
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
    axum::serve(listener, router(archive, config, pullthrough.is_some()))
        .with_graceful_shutdown(crate::http::shutdown_signal())
        .await
}

async fn status(state: AdminState, headers: HeaderMap, pullthrough_enabled: bool) -> Response {
    if !authorized(&state.config, &headers) {
        return unauthorized();
    }
    match state.archive.list_repositories() {
        Ok(repositories) => Json(StatusBody {
            version: env!("CARGO_PKG_VERSION"),
            ready: state.archive.check_readiness().is_ok(),
            pullthrough_enabled,
            repository_count: repositories.len(),
            logical_archive_bytes: repositories
                .iter()
                .map(|repository| repository.logical_bytes)
                .sum(),
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
        return unauthorized();
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
        return unauthorized();
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
    bearer_authorized || tailscale_authorized
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

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(ErrorBody {
            error: "unauthorized",
        }),
    )
        .into_response()
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
    use axum::{body::to_bytes, body::Body, http::Request};
    use tower::ServiceExt;

    fn request(path: &str, token: Option<&str>) -> Request<Body> {
        let mut request = Request::builder().uri(path);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        request.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn management_inventory_requires_authorization_and_paginates() {
        let directory = tempfile::tempdir().unwrap();
        let archive = Archive::new(directory.path()).unwrap();
        let app = router(
            archive,
            Config::token("127.0.0.1:0".parse().unwrap(), "secret"),
            false,
        );

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
    }
}
