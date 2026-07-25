use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use uuid::Uuid;

const DEFAULT_BIND: &str = "127.0.0.1:7114";
const DEFAULT_STATE_DIR: &str = "state/diagnostics";
const MAX_UPLOAD_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_STORED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CONFIGURED_STORED_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_LIST_LIMIT: usize = 200;

#[derive(Clone)]
struct AppState {
    state_dir: Arc<PathBuf>,
    admin_token: Option<String>,
    max_stored_bytes: u64,
    write_lock: Arc<Mutex<()>>,
}

#[derive(Clone)]
struct Config {
    bind: String,
    state_dir: PathBuf,
    admin_token: Option<String>,
    max_stored_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticsReportV2 {
    schema_version: u32,
    consent: bool,
    client_version: Option<String>,
    platform: Option<PlatformSummary>,
    log: Option<String>,
    #[serde(default)]
    artifacts: Vec<TextArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextArtifact {
    name: String,
    content: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PlatformSummary {
    os_family: Option<OsFamily>,
    arch: Option<Architecture>,
    memory_mb_bucket: Option<MemoryBucket>,
    steam_deck: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum OsFamily {
    Windows,
    Linux,
    Macos,
    SteamOs,
    Other,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
enum Architecture {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
    #[serde(rename = "other")]
    Other,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
enum MemoryBucket {
    #[serde(rename = "lt-4096")]
    Lt4096,
    #[serde(rename = "4096-8191")]
    Mb4096To8191,
    #[serde(rename = "8192-16383")]
    Mb8192To16383,
    #[serde(rename = "16384-32767")]
    Mb16384To32767,
    #[serde(rename = "ge-32768")]
    Ge32768,
    #[serde(rename = "unknown")]
    Unknown,
}

#[derive(Debug, Serialize, Deserialize)]
struct RunMetadata {
    schema_version: u32,
    run_id: String,
    received_at_unix: u64,
    received_at_unix_ms: u128,
    source_format: String,
    client_version: Option<String>,
    platform: Option<PlatformSummary>,
    original_bytes: usize,
    stored_bytes: usize,
    redaction_count: usize,
    hostname_collected: bool,
    persistent_machine_id_collected: bool,
}

#[derive(Debug, Serialize)]
struct UploadRunResponse {
    schema_version: u32,
    run_id: String,
    stored_bytes: usize,
    redaction_count: usize,
}

#[derive(Debug, Serialize)]
struct ListRunsResponse {
    schema_version: u32,
    runs: Vec<RunSummary>,
}

#[derive(Debug, Serialize)]
struct RunSummary {
    run_id: String,
    received_at_unix: Option<u64>,
    source_format: Option<String>,
    stored_bytes: Option<usize>,
    redaction_count: Option<usize>,
}

#[derive(Debug, Serialize)]
struct RunDetail {
    schema_version: u32,
    metadata: RunMetadata,
    vapor_log: String,
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    limit: Option<usize>,
}

struct RedactionResult {
    text: String,
    replacements: usize,
}

struct PreparedReport {
    log_text: String,
    client_version: Option<String>,
    platform: Option<PlatformSummary>,
}

struct StoredRun {
    run_id: String,
    stored_bytes: usize,
    redaction_count: usize,
}

enum StoreError {
    QuotaExceeded(String),
    Io(std::io::Error),
    Serialize(serde_json::Error),
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialize(error)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = read_config()?;
    fs::create_dir_all(config.state_dir.join("runs")).await?;

    let bind = config.bind.clone();
    let state = AppState {
        state_dir: Arc::new(config.state_dir),
        admin_token: config.admin_token,
        max_stored_bytes: config.max_stored_bytes,
        write_lock: Arc::new(Mutex::new(())),
    };
    let app = build_router(state);

    let listener = TcpListener::bind(&bind).await?;
    eprintln!("vapor-diagnostics-server listening on {bind}");
    axum::serve(listener, app).await?;

    Ok(())
}

fn read_config() -> Result<Config, String> {
    Ok(Config {
        bind: env::var("VAPOR_DIAGNOSTICS_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string()),
        state_dir: PathBuf::from(
            env::var("VAPOR_DIAGNOSTICS_STATE").unwrap_or_else(|_| DEFAULT_STATE_DIR.into()),
        ),
        admin_token: env::var("VAPOR_DIAGNOSTICS_ADMIN_TOKEN")
            .ok()
            .filter(|value| !value.is_empty()),
        max_stored_bytes: parse_max_stored_bytes(
            env::var("VAPOR_DIAGNOSTICS_MAX_STORED_BYTES").ok(),
        )?,
    })
}

fn parse_max_stored_bytes(value: Option<String>) -> Result<u64, String> {
    let Some(value) = value else {
        return Ok(DEFAULT_MAX_STORED_BYTES);
    };
    let parsed = value
        .parse::<u64>()
        .map_err(|_| "VAPOR_DIAGNOSTICS_MAX_STORED_BYTES must be numeric".to_string())?;
    if parsed == 0 || parsed > MAX_CONFIGURED_STORED_BYTES {
        return Err(format!(
            "VAPOR_DIAGNOSTICS_MAX_STORED_BYTES must be between 1 and {MAX_CONFIGURED_STORED_BYTES}"
        ));
    }
    Ok(parsed)
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/status", get(status))
        .route("/v1/runs", post(upload_run).get(list_runs))
        .route("/v1/runs/{run_id}", get(download_run))
        .route("/v1/export", get(export_runs))
        .route("/v2/reports", post(upload_report_v2).get(list_reports_v2))
        .route("/v2/reports/{run_id}", get(download_report_v2))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn status(State(state): State<AppState>) -> Response {
    let run_count = match read_run_ids(state.state_dir.join("runs")).await {
        Ok(run_ids) => run_ids.len(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("diagnostics: failed to list runs: {error}\n"),
            )
                .into_response();
        }
    };
    let stored_bytes = directory_size(&state.state_dir.join("runs")).unwrap_or_default();
    let body = format!(
        "service = \"vapor-diagnostics-server\"\nstate = \"{}\"\nruns = {}\nmax_upload_bytes = {}\nmax_stored_bytes = {}\nstored_bytes = {}\nschema_versions = [1, 2]\nupload_auth_model = \"explicit-opt-in-unauthenticated\"\nread_auth_model = \"admin-token-scaffold\"\nhostname_collected = false\npersistent_machine_id_collected = false\n",
        state.state_dir.display(),
        run_count,
        MAX_UPLOAD_BYTES,
        state.max_stored_bytes,
        stored_bytes
    );
    (StatusCode::OK, body).into_response()
}

async fn upload_run(State(state): State<AppState>, body: Bytes) -> Response {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty diagnostics upload\n").into_response();
    }

    let raw = String::from_utf8_lossy(&body);
    match store_run(&state, "legacy-text", None, None, raw.as_ref(), body.len()).await {
        Ok(stored) => (
            StatusCode::CREATED,
            format!("diagnostics: uploaded run {}\n", stored.run_id),
        )
            .into_response(),
        Err(error) => store_error_response(error),
    }
}

async fn upload_report_v2(State(state): State<AppState>, body: Bytes) -> Response {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty diagnostics upload\n").into_response();
    }

    let report = match serde_json::from_slice::<DiagnosticsReportV2>(&body) {
        Ok(report) => report,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid diagnostics v2 JSON: {error}\n"),
            )
                .into_response();
        }
    };
    let prepared = match report.into_prepared_report() {
        Ok(prepared) => prepared,
        Err(error) => return (StatusCode::BAD_REQUEST, format!("{error}\n")).into_response(),
    };

    match store_run(
        &state,
        "diagnostics-report-v2",
        prepared.client_version,
        prepared.platform,
        &prepared.log_text,
        body.len(),
    )
    .await
    {
        Ok(stored) => (
            StatusCode::CREATED,
            Json(UploadRunResponse {
                schema_version: 2,
                run_id: stored.run_id,
                stored_bytes: stored.stored_bytes,
                redaction_count: stored.redaction_count,
            }),
        )
            .into_response(),
        Err(error) => store_error_response(error),
    }
}

async fn list_runs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.admin_token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid admin token\n").into_response();
    }

    let mut run_ids = match read_run_ids(state.state_dir.join("runs")).await {
        Ok(run_ids) => run_ids,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("diagnostics: failed to list runs: {error}\n"),
            )
                .into_response()
        }
    };
    run_ids.sort();
    (StatusCode::OK, format!("{}\n", run_ids.join("\n"))).into_response()
}

async fn list_reports_v2(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    if !authorized(&headers, &state.admin_token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid admin token\n").into_response();
    }

    let limit = query.limit.unwrap_or(50).min(MAX_LIST_LIMIT);
    match read_run_summaries(state.state_dir.join("runs"), limit).await {
        Ok(runs) => (
            StatusCode::OK,
            Json(ListRunsResponse {
                schema_version: 2,
                runs,
            }),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("diagnostics: failed to list reports: {error}\n"),
        )
            .into_response(),
    }
}

async fn download_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> Response {
    if !authorized(&headers, &state.admin_token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid admin token\n").into_response();
    }
    if !valid_run_id(&run_id) {
        return (StatusCode::BAD_REQUEST, "invalid diagnostics run id\n").into_response();
    }

    let run_dir = state.state_dir.join("runs").join(&run_id);
    let metadata = fs::read_to_string(run_dir.join("metadata.toml"))
        .await
        .unwrap_or_default();
    let log = fs::read_to_string(run_dir.join("vapor.log"))
        .await
        .unwrap_or_default();
    if metadata.is_empty() && log.is_empty() {
        return (StatusCode::NOT_FOUND, "diagnostics run not found\n").into_response();
    }

    let body = format!("# {run_id}\n\n--- metadata.toml ---\n{metadata}\n--- vapor.log ---\n{log}");
    (StatusCode::OK, body).into_response()
}

async fn download_report_v2(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> Response {
    if !authorized(&headers, &state.admin_token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid admin token\n").into_response();
    }
    if !valid_run_id(&run_id) {
        return (StatusCode::BAD_REQUEST, "invalid diagnostics run id\n").into_response();
    }

    let run_dir = state.state_dir.join("runs").join(&run_id);
    let metadata = match read_metadata_json(&run_dir).await {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return (StatusCode::NOT_FOUND, "diagnostics run not found\n").into_response(),
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("diagnostics: failed to read metadata: {error}\n"),
            )
                .into_response()
        }
    };
    let vapor_log = fs::read_to_string(run_dir.join("vapor.log"))
        .await
        .unwrap_or_default();
    (
        StatusCode::OK,
        Json(RunDetail {
            schema_version: 2,
            metadata,
            vapor_log,
        }),
    )
        .into_response()
}

async fn export_runs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.admin_token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid admin token\n").into_response();
    }

    let mut body = String::from("# Vapor diagnostics export scaffold\n\n");
    let mut run_ids = match read_run_ids(state.state_dir.join("runs")).await {
        Ok(run_ids) => run_ids,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("diagnostics: failed to list runs: {error}\n"),
            )
                .into_response()
        }
    };
    run_ids.sort();
    for run_id in run_ids {
        let run_dir = state.state_dir.join("runs").join(&run_id);
        let metadata = fs::read_to_string(run_dir.join("metadata.toml"))
            .await
            .unwrap_or_default();
        body.push_str(&format!("## {run_id}\n{metadata}\n"));
    }

    (StatusCode::OK, body).into_response()
}

impl DiagnosticsReportV2 {
    fn into_prepared_report(self) -> Result<PreparedReport, String> {
        if self.schema_version != 2 {
            return Err("diagnostics v2 upload requires schema_version = 2".to_string());
        }
        if !self.consent {
            return Err("diagnostics v2 upload requires consent = true".to_string());
        }

        let mut sections = BTreeMap::new();
        if let Some(log) = self.log {
            if !log.trim().is_empty() {
                sections.insert("vapor.log".to_string(), log);
            }
        }
        for artifact in self.artifacts {
            if !allowed_artifact_name(&artifact.name) {
                return Err(format!(
                    "diagnostics v2 artifact '{}' is not allowlisted",
                    artifact.name
                ));
            }
            if artifact.content.trim().is_empty() {
                continue;
            }
            sections.insert(artifact.name, artifact.content);
        }
        if sections.is_empty() {
            return Err("diagnostics v2 upload must include log text or artifacts".to_string());
        }

        let mut text = String::new();
        if let Some(client_version) = &self.client_version {
            text.push_str(&format!(
                "client_version = \"{}\"\n",
                safe_inline(client_version)
            ));
        }
        if let Some(platform) = &self.platform {
            text.push_str(&format!("platform = {}\n", platform_summary_text(platform)));
        }
        for (name, content) in sections {
            text.push_str(&format!("\n--- {name} ---\n{content}\n"));
        }
        Ok(PreparedReport {
            log_text: text,
            client_version: self.client_version,
            platform: self.platform,
        })
    }
}

fn allowed_artifact_name(name: &str) -> bool {
    matches!(
        name,
        "vapor.log" | "launcher.log" | "steps.txt" | "errors.txt"
    )
}

fn platform_summary_text(platform: &PlatformSummary) -> String {
    serde_json::to_string(platform).unwrap_or_else(|_| "{}".to_string())
}

fn safe_inline(input: &str) -> String {
    input
        .chars()
        .filter(|ch| !matches!(ch, '\n' | '\r' | '\0'))
        .take(120)
        .collect()
}

async fn store_run(
    state: &AppState,
    source_format: &str,
    client_version: Option<String>,
    platform: Option<PlatformSummary>,
    raw_text: &str,
    original_bytes: usize,
) -> Result<StoredRun, StoreError> {
    let _guard = state.write_lock.lock().await;
    let runs_dir = state.state_dir.join("runs");
    fs::create_dir_all(&runs_dir).await?;

    let redacted = redact_text(raw_text);
    let stored_bytes = redacted.text.len();
    let current_size = directory_size(&runs_dir)?;
    if current_size.saturating_add(stored_bytes as u64) > state.max_stored_bytes {
        return Err(StoreError::QuotaExceeded(format!(
            "diagnostics storage quota exceeded: current={} attempted={} max={}",
            current_size, stored_bytes, state.max_stored_bytes
        )));
    }

    let received_at_unix_ms = unix_now_millis();
    let received_at_unix = (received_at_unix_ms / 1000) as u64;
    let run_id = unique_run_id(&runs_dir, received_at_unix_ms)?;
    let staging_dir = runs_dir.join(format!(".upload-{run_id}"));
    let final_dir = runs_dir.join(&run_id);

    fs::create_dir(&staging_dir).await?;
    let metadata = RunMetadata {
        schema_version: 2,
        run_id: run_id.clone(),
        received_at_unix,
        received_at_unix_ms,
        source_format: source_format.to_string(),
        client_version,
        platform,
        original_bytes,
        stored_bytes,
        redaction_count: redacted.replacements,
        hostname_collected: false,
        persistent_machine_id_collected: false,
    };
    let metadata_json = serde_json::to_string_pretty(&metadata)?;
    fs::write(staging_dir.join("metadata.json"), metadata_json).await?;
    fs::write(staging_dir.join("metadata.toml"), metadata_toml(&metadata)).await?;
    fs::write(staging_dir.join("vapor.log"), redacted.text.as_bytes()).await?;
    fs::rename(&staging_dir, &final_dir).await?;
    fs::write(state.state_dir.join("latest.txt"), format!("{run_id}\n")).await?;

    Ok(StoredRun {
        run_id,
        stored_bytes,
        redaction_count: redacted.replacements,
    })
}

fn unique_run_id(runs_dir: &FsPath, received_at_unix_ms: u128) -> std::io::Result<String> {
    for _ in 0..10 {
        let run_id = format!("diag-{received_at_unix_ms}-{}", Uuid::new_v4());
        if !runs_dir.join(&run_id).exists() {
            return Ok(run_id);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "failed to allocate unique diagnostics run id",
    ))
}

fn metadata_toml(metadata: &RunMetadata) -> String {
    format!(
        "schema_version = {}\nrun_id = \"{}\"\nreceived_at_unix = {}\nreceived_at_unix_ms = {}\nsource_format = \"{}\"\nclient_version = {}\noriginal_bytes = {}\nstored_bytes = {}\nredaction_count = {}\nhostname_collected = false\npersistent_machine_id_collected = false\n",
        metadata.schema_version,
        metadata.run_id,
        metadata.received_at_unix,
        metadata.received_at_unix_ms,
        metadata.source_format,
        metadata
            .client_version
            .as_ref()
            .map(|value| format!("\"{}\"", safe_inline(value)))
            .unwrap_or_else(|| "\"\"".to_string()),
        metadata.original_bytes,
        metadata.stored_bytes,
        metadata.redaction_count,
    )
}

fn store_error_response(error: StoreError) -> Response {
    match error {
        StoreError::QuotaExceeded(message) => {
            (StatusCode::PAYLOAD_TOO_LARGE, format!("{message}\n")).into_response()
        }
        StoreError::Io(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("diagnostics: storage error: {error}\n"),
        )
            .into_response(),
        StoreError::Serialize(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("diagnostics: metadata error: {error}\n"),
        )
            .into_response(),
    }
}

async fn read_run_ids(runs_dir: PathBuf) -> std::io::Result<Vec<String>> {
    if !runs_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = fs::read_dir(runs_dir).await?;
    let mut run_ids = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            let run_id = entry.file_name().to_string_lossy().to_string();
            if valid_run_id(&run_id) {
                run_ids.push(run_id);
            }
        }
    }

    Ok(run_ids)
}

async fn read_run_summaries(runs_dir: PathBuf, limit: usize) -> std::io::Result<Vec<RunSummary>> {
    let mut run_ids = read_run_ids(runs_dir.clone()).await?;
    run_ids.sort_by(|left, right| right.cmp(left));
    run_ids.truncate(limit);

    let mut summaries = Vec::with_capacity(run_ids.len());
    for run_id in run_ids {
        let run_dir = runs_dir.join(&run_id);
        match read_metadata_json(&run_dir).await? {
            Some(metadata) => summaries.push(RunSummary {
                run_id,
                received_at_unix: Some(metadata.received_at_unix),
                source_format: Some(metadata.source_format),
                stored_bytes: Some(metadata.stored_bytes),
                redaction_count: Some(metadata.redaction_count),
            }),
            None => summaries.push(RunSummary {
                run_id,
                received_at_unix: None,
                source_format: None,
                stored_bytes: None,
                redaction_count: None,
            }),
        }
    }
    Ok(summaries)
}

async fn read_metadata_json(run_dir: &FsPath) -> std::io::Result<Option<RunMetadata>> {
    let path = run_dir.join("metadata.json");
    let text = match fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = serde_json::from_str(&text).map_err(std::io::Error::other)?;
    Ok(Some(metadata))
}

fn directory_size(path: &FsPath) -> std::io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut total = 0_u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total = total.saturating_add(directory_size(&entry.path())?);
        } else if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn authorized(headers: &HeaderMap, expected: &Option<String>) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {expected}"))
}

fn valid_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn redact_text(input: &str) -> RedactionResult {
    let mut text = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut replacements = 0_usize;

    let patterns = [
        r#"(?i)\b((?:password|passwd|token|secret|credential|ticket|auth)\b\s*=\s*)("[^"]*"|'[^']*'|[^\s&]+)"#,
        r#"(?im)^(\s*(authorization|cookie)\s*:\s*).+$"#,
        r#"(?i)\b((?:password|passwd|token|secret|credential|ticket|auth)\b\s*:\s*)("[^"]*"|'[^']*'|[^\s&]+)"#,
        r#"(?i)([?&](?:token|secret|password|ticket|auth)=)[^&\s]+"#,
        r#"\bgh[pousr]_[A-Za-z0-9_]{16,}\b"#,
    ];
    for pattern in patterns {
        let regex = Regex::new(pattern).expect("redaction regex compiles");
        text = regex
            .replace_all(&text, |captures: &regex::Captures<'_>| {
                replacements += 1;
                if let Some(prefix) = captures.get(1) {
                    format!("{}<redacted>", prefix.as_str())
                } else {
                    "<redacted>".to_string()
                }
            })
            .to_string();
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }

    RedactionResult { text, replacements }
}

fn unix_now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_assignment_values_with_spacing() {
        let result = redact_text("password = \"visible-secret\"\ntoken=abc123\n");
        assert!(result.text.contains("<redacted>"));
        assert!(!result.text.contains("visible-secret"));
        assert!(!result.text.contains("abc123"));
        assert!(result.replacements >= 2);
    }

    #[test]
    fn redacts_headers_and_github_tokens() {
        let result = redact_text(
            "Authorization: Bearer secret-token\nCookie: session=secret\nghp_abcdefghijklmnopqrstuvwxyz\n",
        );
        assert!(!result.text.contains("secret-token"));
        assert!(!result.text.contains("session=secret"));
        assert!(!result.text.contains("ghp_abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn rejects_non_allowlisted_artifact_names() {
        let report = DiagnosticsReportV2 {
            schema_version: 2,
            consent: true,
            client_version: None,
            platform: None,
            log: None,
            artifacts: vec![TextArtifact {
                name: "hostname.txt".to_string(),
                content: "host = \"nope\"".to_string(),
            }],
        };
        assert!(report.into_prepared_report().is_err());
    }

    #[test]
    fn rejects_invalid_quota_values() {
        assert!(parse_max_stored_bytes(Some("0".to_string())).is_err());
        assert!(parse_max_stored_bytes(Some("not-a-number".to_string())).is_err());
        assert_eq!(
            parse_max_stored_bytes(None).unwrap(),
            DEFAULT_MAX_STORED_BYTES
        );
    }
}
