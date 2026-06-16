//! Headless HTTP transport for the hashit logical-filesystem API.
//!
//! Thin axum handlers over [`crate::api`]. Read-only, bound to localhost by
//! default, optionally guarded by a bearer token. A separate web app (any
//! framework) is meant to be built on top of this — hashit itself ships no UI.
//!
//! Each request opens the index on a blocking thread (SQLite WAL allows
//! concurrent readers), so handlers never hold a connection across `.await`.

use std::net::{IpAddr, SocketAddr};
use std::path::Path as FsPath;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{delete, get, post},
    Router,
};
use serde::Deserialize;
use tokio_util::io::ReaderStream;
use tower_http::cors::{Any, CorsLayer};

use crate::api;
use crate::store::{QueryFilter, Store};

/// Shared server state.
#[derive(Clone)]
struct AppState {
    token: Arc<Option<String>>,
    allow_write: bool,
}

/// Run the API server, blocking until shutdown. Builds its own tokio runtime so
/// the rest of hashit can stay synchronous.
pub fn run(host: &str, port: u16, token: Option<String>, allow_write: bool) -> Result<()> {
    let ip: IpAddr = host.parse().with_context_host(host)?;
    let addr = SocketAddr::new(ip, port);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(serve(addr, token, allow_write))
}

/// Tiny helper so `host.parse()` yields a useful error message.
trait HostCtx<T> {
    fn with_context_host(self, host: &str) -> Result<T>;
}
impl<T, E: std::fmt::Display> HostCtx<T> for std::result::Result<T, E> {
    fn with_context_host(self, host: &str) -> Result<T> {
        self.map_err(|e| anyhow::anyhow!("invalid --host {host}: {e}"))
    }
}

async fn serve(addr: SocketAddr, token: Option<String>, allow_write: bool) -> Result<()> {
    let state = AppState {
        token: Arc::new(token),
        allow_write,
    };
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    let app = Router::new()
        .route("/v1/drives", get(drives))
        .route("/v1/ls", get(ls))
        .route("/v1/stat", get(stat))
        .route("/v1/query", get(query))
        .route("/v1/content/:hash", get(content))
        .route("/v1/content/:hash/meta", get(detail))
        .route("/v1/thumb/:hash", get(thumb))
        // Mutations (gated by --allow-write inside each handler).
        .route("/v1/content/:hash/tags", post(add_tags))
        .route("/v1/content/:hash/tags/:tag", delete(remove_tag))
        .route("/v1/content/:hash/favorite", post(add_favorite).delete(remove_favorite))
        .route("/v1/links", post(link))
        .route("/v1/links/:hash", delete(unlink))
        .route("/v1/content/:hash/dedup", post(dedup))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(cors)
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    eprintln!("hashit api listening on http://{bound}");
    match state.token.as_ref() {
        Some(t) => eprintln!("auth: send `Authorization: Bearer {t}` or `?token={t}`"),
        None => eprintln!("auth: disabled (--no-token)"),
    }
    eprintln!(
        "writes: {}",
        if allow_write {
            "enabled (--allow-write)"
        } else {
            "disabled (read-only)"
        }
    );
    axum::serve(listener, app).await?;
    Ok(())
}

// -- auth ------------------------------------------------------------------

/// Reject requests lacking the bearer token (header or `?token=`), unless no
/// token is configured.
async fn auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.token.as_ref() else {
        return next.run(req).await;
    };
    let from_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    let from_query = req.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("token=").map(str::to_string))
    });
    let ok = from_header == Some(expected.as_str()) || from_query.as_deref() == Some(expected);
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

// -- error handling --------------------------------------------------------

/// Wraps an error as a 500 response.
struct AppError(anyhow::Error);
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", self.0)).into_response()
    }
}
impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError(e)
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// Run a blocking store operation off the async runtime, opening the index
/// fresh for the request.
async fn db<T, F>(f: F) -> Result<T, AppError>
where
    F: FnOnce(&Store) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let store = Store::open_default()?;
        f(&store)
    })
    .await
    .map_err(|e| AppError(anyhow::anyhow!("task join error: {e}")))?
    .map_err(AppError)
}

/// Like [`db`], but with a mutable store (for write operations).
async fn db_mut<T, F>(f: F) -> Result<T, AppError>
where
    F: FnOnce(&mut Store) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut store = Store::open_default()?;
        f(&mut store)
    })
    .await
    .map_err(|e| AppError(anyhow::anyhow!("task join error: {e}")))?
    .map_err(AppError)
}

/// `Some(403)` unless the server was started with `--allow-write`.
fn write_blocked(state: &AppState) -> Option<Response> {
    if state.allow_write {
        None
    } else {
        Some(
            (
                StatusCode::FORBIDDEN,
                "writes are disabled; start the server with --allow-write",
            )
                .into_response(),
        )
    }
}

// -- handlers --------------------------------------------------------------

async fn drives() -> Result<Response, AppError> {
    let rows = db(api::drives).await?;
    Ok(Json(rows).into_response())
}

#[derive(Deserialize)]
struct LsParams {
    drive: String,
    #[serde(default)]
    path: String,
}

async fn ls(Query(p): Query<LsParams>) -> Result<Response, AppError> {
    let (drive, path) = (p.drive, p.path);
    let rows = db(move |s| api::list_dir(s, &drive, &path)).await?;
    Ok(Json(rows).into_response())
}

async fn stat(Query(p): Query<LsParams>) -> Result<Response, AppError> {
    let (drive, path) = (p.drive, p.path);
    match db(move |s| api::stat(s, &drive, &path)).await? {
        Some(entry) => Ok(Json(entry).into_response()),
        None => Ok(not_found()),
    }
}

#[derive(Deserialize)]
struct QueryParams {
    #[serde(rename = "type")]
    file_type: Option<String>,
    ext: Option<String>,
    hash: Option<String>,
    drive: Option<String>,
    #[serde(default)]
    offline: bool,
    key: Option<String>,
    value: Option<String>,
    tag: Option<String>,
    #[serde(default)]
    favorite: bool,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn query(Query(p): Query<QueryParams>) -> Result<Response, AppError> {
    let filter = QueryFilter {
        file_type: p.file_type,
        ext: p.ext,
        hash_prefix: p.hash,
        drive_id: p.drive,
        offline_only: p.offline,
        key: p.key,
        value: p.value,
        tag: p.tag,
        favorite: p.favorite,
        limit: p.limit.unwrap_or(100).clamp(1, 1000),
        offset: p.offset.unwrap_or(0).max(0),
    };
    let rows = db(move |s| api::query(s, &filter)).await?;
    Ok(Json(rows).into_response())
}

async fn detail(Path(hash): Path<String>) -> Result<Response, AppError> {
    match db(move |s| api::detail(s, &hash)).await? {
        Some(d) => Ok(Json(d).into_response()),
        None => Ok(not_found()),
    }
}

async fn content(Path(hash): Path<String>) -> Result<Response, AppError> {
    let resolved = db(move |s| api::content_source(s, &hash)).await?;
    let Some((_, _, path)) = resolved else {
        return Ok(not_found());
    };
    serve_file(&path, content_type_for(&path)).await
}

async fn thumb(Path(hash): Path<String>) -> Result<Response, AppError> {
    let path = db(move |s| api::thumb(s, &hash)).await?;
    let Some(path) = path else {
        return Ok(not_found());
    };
    serve_file(&path, "image/jpeg").await
}

// -- mutation handlers (require --allow-write) ------------------------------

#[derive(Deserialize)]
struct TagsBody {
    tags: Vec<String>,
}

async fn add_tags(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    Json(body): Json<TagsBody>,
) -> Result<Response, AppError> {
    if let Some(r) = write_blocked(&state) {
        return Ok(r);
    }
    let tags = db_mut(move |s| api::add_tags(s, &hash, &body.tags)).await?;
    Ok(Json(tags).into_response())
}

async fn remove_tag(
    State(state): State<AppState>,
    Path((hash, tag)): Path<(String, String)>,
) -> Result<Response, AppError> {
    if let Some(r) = write_blocked(&state) {
        return Ok(r);
    }
    let tags = db_mut(move |s| api::remove_tag(s, &hash, &tag)).await?;
    Ok(Json(tags).into_response())
}

async fn add_favorite(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Response, AppError> {
    if let Some(r) = write_blocked(&state) {
        return Ok(r);
    }
    db_mut(move |s| api::set_favorite(s, &hash, true)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn remove_favorite(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Response, AppError> {
    if let Some(r) = write_blocked(&state) {
        return Ok(r);
    }
    db_mut(move |s| api::set_favorite(s, &hash, false)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct LinkBody {
    hashes: Vec<String>,
}

async fn link(
    State(state): State<AppState>,
    Json(body): Json<LinkBody>,
) -> Result<Response, AppError> {
    if let Some(r) = write_blocked(&state) {
        return Ok(r);
    }
    let group = db_mut(move |s| api::link(s, &body.hashes)).await?;
    Ok(Json(serde_json::json!({ "group": group })).into_response())
}

async fn unlink(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Response, AppError> {
    if let Some(r) = write_blocked(&state) {
        return Ok(r);
    }
    let unlinked = db_mut(move |s| api::unlink(s, &hash)).await?;
    Ok(Json(serde_json::json!({ "unlinked": unlinked })).into_response())
}

#[derive(Deserialize)]
struct DedupBody {
    keep_drive: String,
    keep_path: String,
    #[serde(default)]
    confirm: bool,
}

async fn dedup(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    Json(body): Json<DedupBody>,
) -> Result<Response, AppError> {
    if let Some(r) = write_blocked(&state) {
        return Ok(r);
    }
    if !body.confirm {
        return Ok((
            StatusCode::BAD_REQUEST,
            "dedup deletes files; resend with {\"confirm\": true}",
        )
            .into_response());
    }
    let outcome =
        db_mut(move |s| api::dedup_keep(s, &hash, &body.keep_drive, &body.keep_path)).await?;
    Ok(Json(outcome).into_response())
}

/// Stream a file from disk with the given content type (no buffering it all
/// into memory).
async fn serve_file(path: &FsPath, content_type: &str) -> Result<Response, AppError> {
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return Ok(not_found()),
    };
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(body)
        .map_err(|e| AppError(anyhow::anyhow!(e)))
}

/// Best-effort content type from a file extension.
fn content_type_for(path: &FsPath) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("heic") => "image/heic",
        Some("tif" | "tiff") => "image/tiff",
        Some("pdf") => "application/pdf",
        Some("mp4" | "mov") => "video/mp4",
        Some("txt") => "text/plain; charset=utf-8",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}
