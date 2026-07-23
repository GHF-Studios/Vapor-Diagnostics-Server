use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::net::TcpListener;

const DEFAULT_BIND: &str = "127.0.0.1:7114";
const DEFAULT_STATE_DIR: &str = "state/diagnostics";
const MAX_UPLOAD_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    state_dir: Arc<PathBuf>,
    admin_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind = env::var("VAPOR_DIAGNOSTICS_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let state_dir = PathBuf::from(
        env::var("VAPOR_DIAGNOSTICS_STATE").unwrap_or_else(|_| DEFAULT_STATE_DIR.into()),
    );
    fs::create_dir_all(state_dir.join("runs")).await?;

    let state = AppState {
        state_dir: Arc::new(state_dir),
        admin_token: env::var("VAPOR_DIAGNOSTICS_ADMIN_TOKEN")
            .ok()
            .filter(|value| !value.is_empty()),
    };
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/runs", post(upload_run).get(list_runs))
        .route("/v1/runs/{run_id}", get(download_run))
        .route("/v1/export", get(export_runs))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state);

    let listener = TcpListener::bind(&bind).await?;
    eprintln!("vapor-diagnostics-server listening on {bind}");
    axum::serve(listener, app).await?;

    Ok(())
}

async fn healthz() -> &'static str {
    "ok\n"
}

async fn upload_run(State(state): State<AppState>, body: Bytes) -> impl IntoResponse {
    if body.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "empty diagnostics upload\n".to_string(),
        );
    }

    let run_id = format!("diag-{}", unix_now_millis());
    let run_dir = state.state_dir.join("runs").join(&run_id);

    if let Err(error) = fs::create_dir_all(&run_dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("diagnostics: failed to create run directory: {error}\n"),
        );
    }

    let raw = String::from_utf8_lossy(&body);
    let redacted = redact_text(&raw);
    if let Err(error) = fs::write(run_dir.join("vapor.log"), redacted.as_bytes()).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("diagnostics: failed to write log: {error}\n"),
        );
    }

    let metadata = format!(
        "schema_version = 1\nrun_id = \"{}\"\nreceived_at_unix = {}\noriginal_bytes = {}\nstored_bytes = {}\nhostname_collected = false\npersistent_machine_id_collected = false\n",
        run_id,
        unix_now(),
        body.len(),
        redacted.len()
    );
    if let Err(error) = fs::write(run_dir.join("metadata.toml"), metadata).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("diagnostics: failed to write metadata: {error}\n"),
        );
    }
    if let Err(error) = fs::write(state.state_dir.join("latest.txt"), format!("{run_id}\n")).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("diagnostics: failed to write latest pointer: {error}\n"),
        );
    }

    (
        StatusCode::CREATED,
        format!("diagnostics: uploaded run {run_id}\n"),
    )
}

async fn list_runs(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !authorized(&headers, &state.admin_token) {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin token\n".to_string(),
        );
    }

    let mut run_ids = match read_run_ids(state.state_dir.join("runs")).await {
        Ok(run_ids) => run_ids,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("diagnostics: failed to list runs: {error}\n"),
            )
        }
    };
    run_ids.sort();
    (StatusCode::OK, format!("{}\n", run_ids.join("\n")))
}

async fn download_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> impl IntoResponse {
    if !authorized(&headers, &state.admin_token) {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin token\n".to_string(),
        );
    }
    if !valid_run_id(&run_id) {
        return (
            StatusCode::BAD_REQUEST,
            "invalid diagnostics run id\n".to_string(),
        );
    }

    let run_dir = state.state_dir.join("runs").join(&run_id);
    let metadata = fs::read_to_string(run_dir.join("metadata.toml"))
        .await
        .unwrap_or_default();
    let log = fs::read_to_string(run_dir.join("vapor.log"))
        .await
        .unwrap_or_default();
    if metadata.is_empty() && log.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            "diagnostics run not found\n".to_string(),
        );
    }

    let body = format!("# {run_id}\n\n--- metadata.toml ---\n{metadata}\n--- vapor.log ---\n{log}");
    (StatusCode::OK, body)
}

async fn export_runs(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !authorized(&headers, &state.admin_token) {
        return (
            StatusCode::UNAUTHORIZED,
            "missing or invalid admin token\n".to_string(),
        );
    }

    let mut body = String::from("# Vapor diagnostics export scaffold\n\n");
    let mut run_ids = match read_run_ids(state.state_dir.join("runs")).await {
        Ok(run_ids) => run_ids,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("diagnostics: failed to list runs: {error}\n"),
            )
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

    (StatusCode::OK, body)
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

fn redact_text(input: &str) -> String {
    input
        .lines()
        .map(|line| {
            line.split_whitespace()
                .map(redact_token)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_token(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    let sensitive = [
        "password",
        "passwd",
        "token",
        "secret",
        "credential",
        "cookie",
        "authorization",
        "auth",
        "ticket",
    ];

    if !sensitive.iter().any(|needle| lower.contains(needle)) {
        return token.to_string();
    }

    if let Some((name, _)) = token.split_once('=') {
        format!("{name}=<redacted>")
    } else if let Some((name, _)) = token.split_once(':') {
        format!("{name}:<redacted>")
    } else {
        "<redacted>".to_string()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
