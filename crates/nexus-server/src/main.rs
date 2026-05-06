use axum::{
    Router,
    extract::Query,
    http::{HeaderMap, HeaderName, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use std::net::SocketAddr;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nexus_server=info,tower_http=info".into()),
        )
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8090);

    let web_dir = std::env::var("WEB_DIR").unwrap_or_else(|_| "web".into());

    let client = Client::builder()
        .build()
        .expect("failed to create HTTP client");

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .expose_headers(Any);

    let app = Router::new()
        .route("/health", get(health))
        .route("/proxy/fetch", get(proxy_fetch))
        .route("/api/anthropic/*rest", axum::routing::any(proxy_anthropic))
        .route("/api/openai/*rest", axum::routing::any(proxy_openai))
        .fallback_service(ServeDir::new(&web_dir).append_index_html_on_directories(true))
        .layer(cors)
        .with_state(client);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("Nexus server listening on {addr}");
    tracing::info!("Serving static files from: {web_dir}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn health() -> &'static str {
    "ok"
}

// --- Proxy fetch (CORS bypass for web_fetch tool) ---

#[derive(Deserialize)]
struct FetchParams {
    url: String,
}

async fn proxy_fetch(
    axum::extract::State(client): axum::extract::State<Client>,
    Query(params): Query<FetchParams>,
) -> Response {
    match client.get(&params.url).send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            match resp.text().await {
                Ok(body) => {
                    // Truncate to 512KB
                    let truncated = if body.len() > 524_288 {
                        format!(
                            "{}\n\n[Truncated: {} bytes total]",
                            &body[..524_288],
                            body.len()
                        )
                    } else {
                        body
                    };
                    (status, truncated).into_response()
                }
                Err(e) => {
                    (StatusCode::BAD_GATEWAY, format!("Failed to read response: {e}")).into_response()
                }
            }
        }
        Err(e) => {
            (StatusCode::BAD_GATEWAY, format!("Fetch failed: {e}")).into_response()
        }
    }
}

// --- LLM API proxies ---

async fn proxy_anthropic(
    axum::extract::State(client): axum::extract::State<Client>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_to(
        &client,
        "https://api.anthropic.com",
        "/api/anthropic",
        &uri,
        &headers,
        body,
    )
    .await
}

async fn proxy_openai(
    axum::extract::State(client): axum::extract::State<Client>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_to(
        &client,
        "https://api.openai.com",
        "/api/openai",
        &uri,
        &headers,
        body,
    )
    .await
}

async fn proxy_to(
    client: &Client,
    upstream_base: &str,
    strip_prefix: &str,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().strip_prefix(strip_prefix).unwrap_or(uri.path());
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let upstream_url = format!("{upstream_base}{path}{query}");

    // Determine method from headers hint or default to POST for API calls
    let method = if body.is_empty() {
        reqwest::Method::GET
    } else {
        reqwest::Method::POST
    };

    let mut req = client.request(method, &upstream_url);

    // Forward relevant headers
    let forward_headers = [
        "content-type",
        "authorization",
        "x-api-key",
        "anthropic-version",
        "anthropic-beta",
        "accept",
    ];

    for name in forward_headers {
        if let Some(val) = headers.get(name) {
            if let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) {
                req = req.header(header_name, val.as_bytes());
            }
        }
    }

    if !body.is_empty() {
        req = req.body(body);
    }

    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY);

            let is_stream = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .map(|ct| ct.contains("text/event-stream"))
                .unwrap_or(false);

            // Build response headers
            let mut resp_headers = HeaderMap::new();
            for name in ["content-type", "x-request-id", "request-id"] {
                if let Some(val) = resp.headers().get(name) {
                    if let Ok(hn) = HeaderName::from_bytes(name.as_bytes()) {
                        resp_headers.insert(hn, val.clone());
                    }
                }
            }

            if is_stream {
                // Stream SSE responses through
                let stream = resp.bytes_stream().map(|chunk| {
                    chunk.map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    })
                });
                let body = axum::body::Body::from_stream(stream);
                (status, resp_headers, body).into_response()
            } else {
                // Buffer non-streaming responses
                match resp.bytes().await {
                    Ok(bytes) => (status, resp_headers, bytes).into_response(),
                    Err(e) => {
                        (StatusCode::BAD_GATEWAY, format!("Read error: {e}")).into_response()
                    }
                }
            }
        }
        Err(e) => {
            tracing::error!("Proxy error to {upstream_url}: {e}");
            (StatusCode::BAD_GATEWAY, format!("Proxy error: {e}")).into_response()
        }
    }
}
