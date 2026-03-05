//! HTTP server for serving bootstrap files and the built-in web UI.

use std::path::PathBuf;

/// Web UI files, embedded at compile time.
const INDEX_HTML: &str = include_str!("../web/index.html");
const TOR_FAST_BOOTSTRAP_JS: &str = include_str!("../web/torFastBootstrap.js");

use anyhow::Result;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

#[derive(Clone)]
struct AppState {
    output_dir: PathBuf,
}

/// Start the HTTP server. Runs forever; call via `tokio::spawn`.
pub async fn run(output_dir: PathBuf, port: u16) -> Result<()> {
    let state = AppState { output_dir };
    let app = Router::new()
        .route("/", get(handle_index))
        .route("/torFastBootstrap.js", get(handle_js))
        .route("/metadata.json", get(handle_metadata))
        .route("/bootstrap.zip", get(handle_bootstrap_zip))
        .route("/bootstrap.zip.br", get(handle_bootstrap_zip_br))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("HTTP server listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Which pre-compressed variant to serve, if any.
enum Encoding {
    Brotli,
    Gzip,
    Identity,
}

/// Pick the best encoding the client accepts. Prefer brotli > gzip > identity.
fn best_encoding(headers: &HeaderMap) -> Encoding {
    let accept = headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(",");

    if accept.split(',').any(|p| p.trim().starts_with("br")) {
        Encoding::Brotli
    } else if accept.split(',').any(|p| {
        let t = p.trim();
        t.starts_with("gzip") || t.starts_with("x-gzip")
    }) {
        Encoding::Gzip
    } else {
        Encoding::Identity
    }
}

/// Serve a file with content-negotiation over pre-compressed variants.
/// Tries `.br` (brotli) or `.gz` (gzip) on disk, falls back to identity.
async fn serve_file(
    dir: &PathBuf,
    filename: &str,
    content_type: &str,
    headers: &HeaderMap,
) -> Response {
    match best_encoding(headers) {
        Encoding::Brotli => {
            if let Ok(data) = tokio::fs::read(dir.join(format!("{}.br", filename))).await {
                return (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, content_type),
                        (header::CONTENT_ENCODING, "br"),
                    ],
                    data,
                )
                    .into_response();
            }
        }
        Encoding::Gzip => {
            if let Ok(data) = tokio::fs::read(dir.join(format!("{}.gz", filename))).await {
                return (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, content_type),
                        (header::CONTENT_ENCODING, "gzip"),
                    ],
                    data,
                )
                    .into_response();
            }
        }
        Encoding::Identity => {}
    }

    match tokio::fs::read(dir.join(filename)).await {
        Ok(data) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type)],
            data,
        )
            .into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// GET / — web UI.
async fn handle_index() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
        .into_response()
}

/// GET /torFastBootstrap.js
async fn handle_js() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        TOR_FAST_BOOTSTRAP_JS,
    )
        .into_response()
}

/// GET /metadata.json — identity, gzip, or brotli.
async fn handle_metadata(State(state): State<AppState>, headers: HeaderMap) -> Response {
    serve_file(&state.output_dir, "metadata.json", "application/json", &headers).await
}

/// GET /bootstrap.zip — identity, gzip, or brotli.
async fn handle_bootstrap_zip(State(state): State<AppState>, headers: HeaderMap) -> Response {
    serve_file(
        &state.output_dir,
        "bootstrap.zip",
        "application/zip",
        &headers,
    )
    .await
}

/// GET /bootstrap.zip.br — always serves the brotli-compressed bytes.
/// If the client accepts brotli, respond with `Content-Type: application/zip`
/// and `Content-Encoding: br` so the browser decompresses transparently.
/// Otherwise, serve raw bytes as `application/octet-stream` for manual decoding.
///
/// Both paths include `X-Decompressed-Content-Length` with the uncompressed zip
/// size, so clients can show accurate download progress even when the browser
/// handles decompression transparently (where `Content-Length` reflects the
/// compressed size but the stream delivers decompressed bytes).
async fn handle_bootstrap_zip_br(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let data = match tokio::fs::read(state.output_dir.join("bootstrap.zip.br")).await {
        Ok(d) => d,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    // Get decompressed size from the uncompressed zip on disk.
    let decompressed_len = tokio::fs::metadata(state.output_dir.join("bootstrap.zip"))
        .await
        .map(|m| m.len().to_string())
        .unwrap_or_default();
    if matches!(best_encoding(&headers), Encoding::Brotli) {
        let mut res = (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/zip"),
                (header::CONTENT_ENCODING, "br"),
            ],
            data,
        )
            .into_response();
        if !decompressed_len.is_empty() {
            res.headers_mut().insert(
                "x-decompressed-content-length",
                decompressed_len.parse().unwrap(),
            );
        }
        res
    } else {
        let mut res = (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/octet-stream")],
            data,
        )
            .into_response();
        if !decompressed_len.is_empty() {
            res.headers_mut().insert(
                "x-decompressed-content-length",
                decompressed_len.parse().unwrap(),
            );
        }
        res
    }
}
