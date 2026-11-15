use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Bytes};
use axum::extract::{FromRequest, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use shrunk::{Agg, Db, DeleteResult, Error, Point, Predicate, Query, QueryStats};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

mod retention;
use retention::RetentionPolicy;

const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_BATCH_POINTS: usize = 10_000;
const MAX_QUERY_POINTS: usize = 100_000;
const MAX_SERIES: usize = 10_000;
const CHECKPOINT_WAL_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Store>>,
    token: Arc<[u8]>,
    requests: Arc<Semaphore>,
    operations: Arc<Semaphore>,
}

struct Store {
    db: Db,
    wal_path: PathBuf,
}

impl AppState {
    async fn run<F>(&self, operation: F) -> Result<Value, ApiError>
    where
        F: FnOnce(&mut Store) -> Result<Value, ApiError> + Send + 'static,
    {
        // Keep the permit inside the blocking operation: a disconnected client
        // cannot leave unlimited queued work behind.
        let permit = self
            .operations
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::busy())?;
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut store = store.lock().map_err(|_| ApiError::busy())?;
            operation(&mut store)
        })
        .await
        .map_err(|_| ApiError::internal("database worker failed"))?
    }
}

struct ApiError(StatusCode, &'static str, String);

impl ApiError {
    fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, "invalid_request", message.into())
    }

    fn busy() -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "database busy; retry later".into(),
        )
    }

    fn internal(message: &str) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            message.into(),
        )
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let (status, code) = match &error {
            Error::UnknownSeries(_) => (StatusCode::NOT_FOUND, "unknown_series"),
            Error::OutOfOrder { .. } | Error::Duplicate { .. } => {
                (StatusCode::CONFLICT, "timestamp_conflict")
            }
            Error::InvalidInput(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
            Error::Limit(_) => (StatusCode::PAYLOAD_TOO_LARGE, "limit_exceeded"),
            Error::Locked(_) => return Self::busy(),
            _ => {
                eprintln!("database operation failed: {error}");
                return Self::internal("database operation failed; check server logs");
            }
        };
        Self(status, code, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.0,
            Json(json!({"error": {"code": self.1, "message": self.2}})),
        )
            .into_response();
        if self.0 == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
        }
        response
    }
}

// Read bodies with a deadline as well as a size bound, including chunked bodies.
struct Body<T>(T);

fn json_content_type(request: &Request) -> bool {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        == Some("application/json")
}

fn require_json_content_type(request: &Request) -> Result<(), ApiError> {
    if json_content_type(request) {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type",
            "use Content-Type: application/json".into(),
        ))
    }
}

async fn read_request_bytes(request: Request) -> Result<Bytes, ApiError> {
    tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(request.into_body(), MAX_BODY_BYTES),
    )
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::REQUEST_TIMEOUT,
            "body_timeout",
            "request body timed out".into(),
        )
    })?
    .map_err(|_| {
        ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_limit",
            format!("request body must not exceed {MAX_BODY_BYTES} bytes"),
        )
    })
}

fn parse_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes).map_err(|e| ApiError::bad(format!("invalid JSON: {e}")))
}

impl<T: DeserializeOwned + Send> FromRequest<AppState> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, _: &AppState) -> Result<Self, Self::Rejection> {
        require_json_content_type(&request)?;
        let bytes = read_request_bytes(request).await?;
        parse_json(&bytes).map(Self)
    }
}

async fn authorize(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !bool::from(supplied.as_bytes().ct_eq(&state.token)) {
        return ApiError(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a valid bearer token is required".into(),
        )
        .into_response();
    }
    let Ok(_permit) = state.requests.try_acquire() else {
        return ApiError::busy().into_response();
    };
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/v1/write", post(write))
        .route("/v1/query", post(query))
        .route("/v1/aggregate", post(aggregate))
        .route("/v1/delete", post(delete_range))
        .route("/v1/delete-series", post(delete_series))
        .route("/v1/retain", post(retain))
        .route("/v1/downsample", post(downsample))
        .route("/v1/rollup", post(rollup))
        .route("/v1/series", get(series))
        .route("/v1/checkpoint", post(checkpoint))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .route("/healthz", get(|| async { Json(json!({"status": "ok"})) }))
        .fallback(|| async {
            ApiError(
                StatusCode::NOT_FOUND,
                "not_found",
                "unknown endpoint".into(),
            )
        })
        .with_state(state)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputPoint {
    ts: i64,
    value: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteRequest {
    series: String,
    points: Vec<InputPoint>,
}

fn check_series(series: &str) -> Result<(), ApiError> {
    if series.is_empty() || series.len() > 255 || series.chars().any(char::is_control) {
        return Err(ApiError::bad(
            "series must contain 1..255 UTF-8 bytes without control characters",
        ));
    }
    Ok(())
}

async fn write(
    State(state): State<AppState>,
    Body(input): Body<WriteRequest>,
) -> Result<Json<Value>, ApiError> {
    check_series(&input.series)?;
    if input.points.is_empty() || input.points.len() > MAX_BATCH_POINTS {
        return Err(ApiError::bad(format!(
            "points must contain 1..{MAX_BATCH_POINTS} samples"
        )));
    }
    if input.points.iter().any(|p| !p.value.is_finite()) {
        return Err(ApiError::bad("values must be finite JSON numbers"));
    }
    state
        .run(move |store| {
            let names = store.db.series_names();
            if names.len() >= MAX_SERIES && !names.contains(&input.series.as_str()) {
                return Err(ApiError::bad(format!(
                    "database supports at most {MAX_SERIES} series"
                )));
            }
            // Checkpoint before a write, so a failed checkpoint never turns an
            // already durable append into an error response.
            if fs::metadata(&store.wal_path)?.len() >= CHECKPOINT_WAL_BYTES {
                store.db.checkpoint()?;
            }
            let points: Vec<_> = input
                .points
                .into_iter()
                .map(|p| Point::new(p.ts, p.value))
                .collect();
            store.db.append(&input.series, &points)?;
            Ok(json!({"written": points.len()}))
        })
        .await
        .map(Json)
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error).into()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter {
    op: String,
    value: f64,
}

impl Filter {
    fn predicate(&self) -> Result<Predicate, ApiError> {
        if !self.value.is_finite() {
            return Err(ApiError::bad("filter value must be finite"));
        }
        match self.op.as_str() {
            "gt" => Ok(Predicate::Gt(self.value)),
            "ge" => Ok(Predicate::Ge(self.value)),
            "lt" => Ok(Predicate::Lt(self.value)),
            "le" => Ok(Predicate::Le(self.value)),
            "eq" => Ok(Predicate::Eq(self.value)),
            "ne" => Ok(Predicate::Ne(self.value)),
            _ => Err(ApiError::bad("filter op must be gt, ge, lt, le, eq, or ne")),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryRequest {
    series: String,
    from: Option<i64>,
    to: Option<i64>,
    limit: Option<usize>,
    filter: Option<Filter>,
}

fn check_range(series: &str, from: Option<i64>, to: Option<i64>) -> Result<(i64, i64), ApiError> {
    check_series(series)?;
    let from = from.unwrap_or(i64::MIN);
    let to = to.unwrap_or(i64::MAX);
    if from > to {
        return Err(ApiError::bad("from must be less than or equal to to"));
    }
    Ok((from, to))
}

fn stats(stats: QueryStats) -> Value {
    json!({
        "blocks_total": stats.blocks_total,
        "blocks_visited": stats.blocks_visited,
        "blocks_decoded": stats.blocks_decoded,
        "blocks_skipped_time": stats.blocks_skipped_time,
        "blocks_skipped_value": stats.blocks_skipped_value,
        "blocks_skipped_limit": stats.blocks_skipped_limit,
        "fast_blocks": stats.fast_blocks,
        "points_decoded": stats.points_decoded,
        "pending_points": stats.pending_points,
        "bytes_decoded": stats.bytes_decoded,
    })
}

async fn query(
    State(state): State<AppState>,
    Body(input): Body<QueryRequest>,
) -> Result<Json<Value>, ApiError> {
    let (from, to) = check_range(&input.series, input.from, input.to)?;
    let limit = input.limit.unwrap_or(1000);
    if limit == 0 || limit > MAX_QUERY_POINTS {
        return Err(ApiError::bad(format!(
            "limit must be in 1..{MAX_QUERY_POINTS}"
        )));
    }
    let predicate = input.filter.as_ref().map(Filter::predicate).transpose()?;
    state
        .run(move |store| {
            // One extra point lets clients distinguish exactly full results from
            // truncation without discarding a sample at a page boundary.
            let mut q = Query::new(&input.series, from, to).limit(limit + 1);
            q.predicate = predicate;
            let mut result = store.db.query(&q)?;
            let truncated = result.points.len() > limit;
            result.points.truncate(limit);
            let points: Vec<_> = result
                .points
                .iter()
                .map(|p| json!({"ts": p.ts, "value": p.value}))
                .collect();
            Ok(json!({"points": points, "truncated": truncated, "stats": stats(result.stats)}))
        })
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AggregateRequest {
    series: String,
    from: Option<i64>,
    to: Option<i64>,
    agg: String,
    filter: Option<Filter>,
}

async fn aggregate(
    State(state): State<AppState>,
    Body(input): Body<AggregateRequest>,
) -> Result<Json<Value>, ApiError> {
    let (from, to) = check_range(&input.series, input.from, input.to)?;
    let agg = Agg::parse(&input.agg)
        .ok_or_else(|| ApiError::bad("agg must be count, sum, min, max, or avg"))?;
    let predicate = input.filter.as_ref().map(Filter::predicate).transpose()?;
    state
        .run(move |store| {
            let result = store
                .db
                .aggregate(&input.series, from, to, agg, predicate)?;
            Ok(json!({"value": result.value, "stats": stats(result.stats)}))
        })
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteRequest {
    series: String,
    from: Option<i64>,
    to: Option<i64>,
}

fn deletion_result(result: DeleteResult) -> Value {
    json!({
        "points_deleted": result.points_deleted,
        "blocks_dropped": result.blocks_dropped,
        "blocks_rewritten": result.blocks_rewritten,
    })
}

async fn delete_range(
    State(state): State<AppState>,
    Body(input): Body<DeleteRequest>,
) -> Result<Json<Value>, ApiError> {
    if input.from.is_none() && input.to.is_none() {
        return Err(ApiError::bad(
            "range deletion requires from or to; use delete-series to remove a whole series",
        ));
    }
    let (from, to) = check_range(&input.series, input.from, input.to)?;
    state
        .run(move |store| {
            Ok(deletion_result(store.db.delete_range(
                &input.series,
                from,
                to,
            )?))
        })
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeriesRequest {
    series: String,
}

async fn delete_series(
    State(state): State<AppState>,
    Body(input): Body<SeriesRequest>,
) -> Result<Json<Value>, ApiError> {
    check_series(&input.series)?;
    state
        .run(move |store| {
            let deleted = store.db.delete_series(&input.series)?;
            Ok(json!({"series_deleted": true, "points_deleted": deleted}))
        })
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainRequest {
    cutoff: Option<i64>,
    now: Option<i64>,
    window_ms: Option<i64>,
}

impl RetainRequest {
    fn cutoff(&self) -> Result<i64, ApiError> {
        match (self.cutoff, self.now, self.window_ms) {
            (Some(cutoff), None, None) => Ok(cutoff),
            (None, Some(now), Some(window)) if window > 0 => Ok(now.saturating_sub(window)),
            _ => Err(ApiError::bad(
                "use cutoff or both now and a positive window_ms",
            )),
        }
    }
}

async fn retain(
    State(state): State<AppState>,
    Body(input): Body<RetainRequest>,
) -> Result<Json<Value>, ApiError> {
    let cutoff = input.cutoff()?;
    state
        .run(move |store| {
            let mut result = deletion_result(store.db.retain_since(cutoff)?);
            result["cutoff"] = json!(cutoff);
            Ok(result)
        })
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DownsampleRequest {
    series: String,
    from: Option<i64>,
    to: Option<i64>,
    interval_ms: i64,
    agg: String,
    limit: Option<usize>,
}

fn check_bucket_options(interval: i64, agg: &str, limit: usize) -> Result<Agg, ApiError> {
    if interval <= 0 {
        return Err(ApiError::bad("interval_ms must be positive"));
    }
    if limit == 0 || limit > MAX_QUERY_POINTS {
        return Err(ApiError::bad(format!(
            "bucket limit must be in 1..={MAX_QUERY_POINTS}"
        )));
    }
    Agg::parse(agg).ok_or_else(|| ApiError::bad("agg must be count, sum, min, max, or avg"))
}

fn next_bucket_start(ts: i64, interval: i64) -> Option<i64> {
    let width = i128::from(interval);
    i64::try_from((i128::from(ts).div_euclid(width) + 1) * width).ok()
}

async fn downsample(
    State(state): State<AppState>,
    Body(input): Body<DownsampleRequest>,
) -> Result<Json<Value>, ApiError> {
    let (from, to) = check_range(&input.series, input.from, input.to)?;
    let limit = input.limit.unwrap_or(1000);
    let agg = check_bucket_options(input.interval_ms, &input.agg, limit)?;
    state
        .run(move |store| {
            let mut result = store.db.downsample_with_limit(
                &input.series,
                from,
                to,
                input.interval_ms,
                agg,
                limit + 1,
            )?;
            let truncated = result.points.len() > limit;
            result.points.truncate(limit);
            let next_from = if truncated {
                result
                    .points
                    .last()
                    .and_then(|p| next_bucket_start(p.ts, input.interval_ms))
            } else {
                None
            };
            let points: Vec<_> = result
                .points
                .iter()
                .map(|p| json!({"ts": p.ts, "value": p.value}))
                .collect();
            Ok(json!({"points": points, "truncated": truncated,
                "next_from": next_from, "stats": stats(result.stats)}))
        })
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RollupRequest {
    series: String,
    target: String,
    from: Option<i64>,
    to: Option<i64>,
    interval_ms: i64,
    agg: String,
    max_buckets: Option<usize>,
}

async fn rollup(
    State(state): State<AppState>,
    Body(input): Body<RollupRequest>,
) -> Result<Json<Value>, ApiError> {
    let (from, to) = check_range(&input.series, input.from, input.to)?;
    check_series(&input.target)?;
    let max_buckets = input.max_buckets.unwrap_or(MAX_QUERY_POINTS);
    let agg = check_bucket_options(input.interval_ms, &input.agg, max_buckets)?;
    state
        .run(move |store| {
            let names = store.db.series_names();
            if input.series == input.target || names.contains(&input.target.as_str()) {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "series_exists",
                    "rollup target must be a new series distinct from the source".into(),
                ));
            }
            if names.len() >= MAX_SERIES {
                return Err(ApiError::bad(format!(
                    "database supports at most {MAX_SERIES} series"
                )));
            }
            let count = store.db.rollup_with_limit(
                &input.series,
                &input.target,
                from..=to,
                input.interval_ms,
                agg,
                max_buckets,
            )?;
            Ok(json!({"series": input.target, "written": count}))
        })
        .await
        .map(Json)
}

async fn series(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    state.run(|store| {
        let series: Vec<_> = store.db.summaries().into_iter().map(|s| json!({
            "name": s.name, "points": s.points, "blocks": s.blocks,
            "stored_bytes": s.stored_bytes, "raw_bytes": s.raw_bytes,
        })).collect();
        Ok(json!({"series": series, "points": store.db.point_count(), "format_version": store.db.format_version()}))
    }).await.map(Json)
}

async fn checkpoint(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    // An omitted body is allowed. Any supplied body must follow the same
    // content type, size, and deadline rules as the other POST endpoints.
    let has_content_type = request.headers().contains_key(header::CONTENT_TYPE);
    if has_content_type {
        require_json_content_type(&request)?;
    }
    let bytes = read_request_bytes(request).await?;
    if !bytes.is_empty() {
        if !has_content_type {
            return Err(ApiError(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "content_type",
                "use Content-Type: application/json".into(),
            ));
        }
        let value: Value = parse_json(&bytes)?;
        if !matches!(value, Value::Object(ref fields) if fields.is_empty()) {
            return Err(ApiError::bad(
                "checkpoint body must be an empty JSON object",
            ));
        }
    }
    state
        .run(|store| {
            store.db.checkpoint()?;
            Ok(json!({"checkpointed": true}))
        })
        .await
        .map(Json)
}

fn token() -> Result<Vec<u8>, String> {
    let token =
        match (
            env::var("SHRUNK_TOKEN").ok(),
            env::var_os("SHRUNK_TOKEN_FILE"),
        ) {
            (Some(_), Some(_)) => {
                return Err("set only one of SHRUNK_TOKEN and SHRUNK_TOKEN_FILE".into())
            }
            (Some(token), None) => token,
            (None, Some(path)) => fs::read_to_string(path)
                .map_err(|e| format!("cannot read token file: {e}"))?
                .trim_end_matches(['\r', '\n'])
                .to_owned(),
            (None, None) => return Err(
                "set SHRUNK_TOKEN or SHRUNK_TOKEN_FILE to a secret token of at least 32 characters"
                    .into(),
            ),
        };
    if !(32..=4096).contains(&token.len()) || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("token must be 32..4096 printable ASCII characters without whitespace".into());
    }
    Ok(token.into_bytes())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .expect("install Ctrl-C handler");
    eprintln!("shutdown requested; draining requests");
}

#[tokio::main(worker_threads = 2)]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("shrunk-server: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let listen: SocketAddr = env::var("SHRUNK_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8080".into())
        .parse()?;
    match env::args().nth(1).as_deref() {
        Some("healthcheck") => return healthcheck(listen),
        Some(_) => {
            return Err(
                "usage: shrunk-server [healthcheck]; configure with SHRUNK_* environment variables"
                    .into(),
            )
        }
        None => {}
    }
    let token = token()?;
    let data_dir = PathBuf::from(env::var_os("SHRUNK_DATA_DIR").unwrap_or_else(|| "data".into()));
    fs::create_dir_all(&data_dir)?;
    let path = data_dir.join("shrunk.db");
    let policy = RetentionPolicy::from_env()?;
    // Reserve the address before opening the database: startup retention can
    // publish a new snapshot, and a failed bind must not change stored data.
    // Requests are only accepted after the open and initial sweep succeed.
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let mut db = Db::open_rw(&path)?;
    if let Some(policy) = policy {
        RetentionPolicy::report(policy.sweep(&mut db).map_err(|error| error.2)?);
    }
    let state = AppState {
        store: Arc::new(Mutex::new(Store {
            db,
            wal_path: data_dir.join("shrunk.db.wal"),
        })),
        token: token.into(),
        requests: Arc::new(Semaphore::new(16)),
        operations: Arc::new(Semaphore::new(8)),
    };
    eprintln!("shrunk-server listening on {}", listener.local_addr()?);
    let retention = policy.map(|policy| policy.start(state.clone()));
    let result = axum::serve(listener, app(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await;
    if let Some((stop, task)) = retention {
        let _ = stop.send(true);
        task.await?;
    }
    result?;
    state
        .run(|store| {
            store.db.checkpoint()?;
            Ok(json!(null))
        })
        .await
        .map_err(|e| e.2)?;
    Ok(())
}

fn healthcheck(mut address: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut status = [0; 12];
    stream.read_exact(&mut status)?;
    if &status != b"HTTP/1.1 200" {
        return Err("health check did not receive HTTP 200".into());
    }
    Ok(())
}
