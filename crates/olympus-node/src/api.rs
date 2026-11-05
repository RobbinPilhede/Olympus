use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{self, HeaderMap, HeaderName};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::UdpSocket;

use crate::hf::check_repo;
use crate::node::{Discovery, Node, StartError};

/// The datagram clients broadcast to find nodes.
pub const DISCOVER_DATAGRAM: &[u8] = b"OLYMPUS_DISCOVER";

const HOP_BY_HOP: [HeaderName; 7] = [
    header::CONNECTION,
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

type NodeState = State<Arc<Node>>;

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<StartError> for ApiError {
    fn from(e: StartError) -> Self {
        match e {
            StartError::NotFound(m) => Self(StatusCode::NOT_FOUND, m),
            StartError::Busy(m) => Self(StatusCode::CONFLICT, m),
            StartError::BadRequest(m) => Self(StatusCode::BAD_REQUEST, m),
            StartError::Upstream(m) => Self(StatusCode::BAD_GATEWAY, m),
        }
    }
}

fn upstream(e: &anyhow::Error) -> ApiError {
    ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}"))
}

/// The HTTP surface; every route requires the bearer token.
pub fn router(node: Arc<Node>) -> Router {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/hf/search", get(hf_search))
        .route("/api/hf/files", get(hf_files))
        .route("/api/pf/search", get(pf_search))
        .route("/api/pf/files", get(pf_files))
        .route("/api/pull", post(pull))
        .route("/api/pull/cancel", post(pull_cancel))
        .route("/api/local", get(local))
        .route("/api/local/{id}", delete(local_delete))
        .route("/api/load", post(load))
        .route("/api/unload", post(unload))
        .route("/v1/models", get(proxy))
        .route("/v1/chat/completions", post(proxy))
        .layer(middleware::from_fn_with_state(node.clone(), require_token))
        .with_state(node)
}

async fn require_token(State(node): NodeState, request: Request, next: Next) -> Response {
    let expected = format!("Bearer {}", node.token);
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .is_some_and(|value| value.as_bytes() == expected.as_bytes());
    if !authorized {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "Missing or wrong token. Send Authorization: Bearer <token>.".to_string(),
        )
        .into_response();
    }
    next.run(request).await
}

async fn status(State(node): NodeState) -> impl IntoResponse {
    Json(node.status())
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

async fn hf_search(State(node): NodeState, Query(query): Query<SearchQuery>) -> Result<Response, ApiError> {
    let hits = node.hf.search(&query.q).await.map_err(|e| upstream(&e))?;
    Ok(Json(hits).into_response())
}

#[derive(Deserialize)]
struct FilesQuery {
    repo: String,
}

async fn hf_files(State(node): NodeState, Query(query): Query<FilesQuery>) -> Result<Response, ApiError> {
    check_repo(&query.repo).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;
    let listing = node.quant_files(&query.repo).await.map_err(|e| upstream(&e))?;
    Ok(Json(listing).into_response())
}

async fn pf_search(State(node): NodeState, Query(query): Query<SearchQuery>) -> Result<Response, ApiError> {
    let hits = node.pirate.search(&query.q).await.map_err(|e| upstream(&e))?;
    Ok(Json(hits).into_response())
}

#[derive(Deserialize)]
struct TorrentQuery {
    magnet: String,
}

async fn pf_files(State(node): NodeState, Query(query): Query<TorrentQuery>) -> Result<Response, ApiError> {
    if !crate::pirate::is_magnet(&query.magnet) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "That isn't a magnet link.".to_string()));
    }
    let listing = node.torrent_quants(&query.magnet).await.map_err(|e| upstream(&e))?;
    Ok(Json(listing).into_response())
}

#[derive(Deserialize)]
struct PullBody {
    repo: String,
    quant: String,
    magnet: Option<String>,
}

async fn pull(State(node): NodeState, Json(body): Json<PullBody>) -> Result<Response, ApiError> {
    node.start_pull(body.repo, body.quant, body.magnet).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response())
}

async fn pull_cancel(State(node): NodeState) -> impl IntoResponse {
    node.cancel_pull();
    ok()
}

async fn local(State(node): NodeState) -> impl IntoResponse {
    Json(node.store.list())
}

async fn local_delete(State(node): NodeState, Path(id): Path<String>) -> Result<Response, ApiError> {
    match node.delete_local(&id).await {
        Ok(true) => Ok(ok().into_response()),
        Ok(false) => Err(ApiError(StatusCode::NOT_FOUND, format!("No local model has the id {id}."))),
        Err(e) => Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
struct LoadBody {
    id: String,
    ctx: Option<String>,
    parallel: Option<u32>,
}

async fn load(State(node): NodeState, Json(body): Json<LoadBody>) -> Result<Response, ApiError> {
    node.start_load(&body.id, body.ctx, body.parallel)?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response())
}

async fn unload(State(node): NodeState) -> impl IntoResponse {
    node.unload().await;
    ok()
}

fn ok() -> Json<Value> {
    Json(json!({ "ok": true }))
}

fn openai_error(status: StatusCode, message: &str) -> Response {
    let body = json!({ "error": { "message": message, "type": "model_not_ready" } });
    (status, Json(body)).into_response()
}

async fn proxy(State(node): NodeState, request: Request) -> Response {
    let not_ready = "No model is ready on this node. Load one with POST /api/load and try again.";
    let Some(port) = node.ready_port() else {
        return openai_error(StatusCode::SERVICE_UNAVAILABLE, not_ready);
    };
    let (parts, body) = request.into_parts();
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let url = format!("http://127.0.0.1:{port}{path}");

    let mut headers = parts.headers;
    headers.remove(header::AUTHORIZATION);
    headers.remove(header::HOST);
    strip_hop_by_hop(&mut headers);

    let upstream_request = node
        .proxy
        .request(parts.method, url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()));
    let upstream_response = match upstream_request.send().await {
        Ok(response) => response,
        Err(_) => return openai_error(StatusCode::SERVICE_UNAVAILABLE, not_ready),
    };

    let status = upstream_response.status();
    let mut headers = upstream_response.headers().clone();
    strip_hop_by_hop(&mut headers);
    // Dropping this body stream when the client leaves closes the upstream connection too.
    let mut response = Response::new(Body::from_stream(upstream_response.bytes_stream()));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in &HOP_BY_HOP {
        headers.remove(name);
    }
    headers.remove("keep-alive");
}

/// The reply to one datagram, if it is exactly the discovery probe.
pub fn discovery_reply(datagram: &[u8], discovery: &Discovery) -> Option<Vec<u8>> {
    if datagram != DISCOVER_DATAGRAM {
        return None;
    }
    serde_json::to_vec(discovery).ok()
}

/// Answers discovery datagrams until the process ends.
pub async fn run_discovery(socket: UdpSocket, node: Arc<Node>) {
    let mut buf = [0u8; 64];
    loop {
        // Windows reports an earlier reply's ICMP port-unreachable as a recv error; keep serving.
        let Ok((len, from)) = socket.recv_from(&mut buf).await else {
            continue;
        };
        if let Some(reply) = discovery_reply(&buf[..len], &node.discovery()) {
            socket.send_to(&reply, from).await.ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discovery(state: &'static str) -> Discovery {
        Discovery {
            name: "pc".to_string(),
            port: 47475,
            version: "0.1.0".to_string(),
            gpu: None,
            vram_mb: 0,
            ram_mb: 1024,
            model: None,
            state,
        }
    }

    #[test]
    fn discovery_reply_has_exactly_the_contract_fields() {
        let reply = discovery_reply(b"OLYMPUS_DISCOVER", &discovery("idle")).unwrap();
        let value: Value = serde_json::from_slice(&reply).unwrap();

        assert_eq!(
            value,
            json!({
                "name": "pc", "port": 47475, "version": "0.1.0", "gpu": null,
                "vram_mb": 0, "ram_mb": 1024, "model": null, "state": "idle"
            })
        );
    }

    #[test]
    fn other_datagrams_get_no_reply() {
        for bad in [&b"olympus_discover"[..], b"OLYMPUS_DISCOVER\n", b"", b"OLYMPUS"] {
            assert!(discovery_reply(bad, &discovery("idle")).is_none());
        }
    }

    #[tokio::test]
    async fn discovery_answers_over_a_real_socket_after_ignoring_noise() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"noise", addr).await.unwrap();
        client.send_to(b"OLYMPUS_DISCOVER", addr).await.unwrap();

        let mut buf = [0u8; 64];
        let (first, from) = server.recv_from(&mut buf).await.unwrap();
        assert!(discovery_reply(&buf[..first], &discovery("idle")).is_none());
        let (second, _) = server.recv_from(&mut buf).await.unwrap();
        let reply = discovery_reply(&buf[..second], &discovery("ready")).unwrap();
        server.send_to(&reply, from).await.unwrap();

        let mut out = [0u8; 512];
        let (n, _) = client.recv_from(&mut out).await.unwrap();
        let value: Value = serde_json::from_slice(&out[..n]).unwrap();
        assert_eq!(value["state"], "ready");
    }
}
