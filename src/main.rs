use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_BIND: &str = "127.0.0.1:7114";
const DEFAULT_STATE_DIR: &str = "state/diagnostics";
const MAX_UPLOAD_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn main() -> io::Result<()> {
    let bind = env::var("VAPOR_DIAGNOSTICS_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let state_dir = PathBuf::from(
        env::var("VAPOR_DIAGNOSTICS_STATE").unwrap_or_else(|_| DEFAULT_STATE_DIR.to_string()),
    );
    fs::create_dir_all(state_dir.join("runs"))?;

    let listener = TcpListener::bind(&bind)?;
    eprintln!("vapor-diagnostics-server listening on {bind}");

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(error) = handle_connection(&mut stream, &state_dir) {
                    eprintln!("request failed: {error}");
                }
            }
            Err(error) => eprintln!("connection failed: {error}"),
        }
    }

    Ok(())
}

fn handle_connection(stream: &mut TcpStream, state_dir: &Path) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let Some(request) = read_request(stream, MAX_UPLOAD_BYTES)? else {
        return Ok(());
    };

    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/healthz") => respond_text(stream, "200 OK", "ok\n"),
        ("POST", "/v1/runs") => upload_run(stream, state_dir, &request),
        ("GET", "/v1/runs") => list_runs(stream, state_dir, &request),
        ("GET", path) if path.starts_with("/v1/runs/") => {
            let run_id = path.trim_start_matches("/v1/runs/");
            download_run(stream, state_dir, &request, run_id)
        }
        ("GET", "/v1/export") => export_runs(stream, state_dir, &request),
        _ => respond_text(stream, "404 Not Found", "not found\n"),
    }
}

fn upload_run(stream: &mut TcpStream, state_dir: &Path, request: &Request) -> io::Result<()> {
    if request.body.is_empty() {
        return respond_text(stream, "400 Bad Request", "empty diagnostics upload\n");
    }

    let run_id = format!("diag-{}", unix_now_millis());
    let run_dir = state_dir.join("runs").join(&run_id);
    fs::create_dir_all(&run_dir)?;

    let raw = String::from_utf8_lossy(&request.body);
    let redacted = redact_text(&raw);
    fs::write(run_dir.join("vapor.log"), redacted.as_bytes())?;
    fs::write(
        run_dir.join("metadata.toml"),
        format!(
            "schema_version = 1\nrun_id = \"{}\"\nreceived_at_unix = {}\noriginal_bytes = {}\nstored_bytes = {}\nhostname_collected = false\npersistent_machine_id_collected = false\n",
            run_id,
            unix_now(),
            request.body.len(),
            redacted.len()
        ),
    )?;
    fs::write(state_dir.join("latest.txt"), format!("{run_id}\n"))?;

    respond_text(
        stream,
        "201 Created",
        &format!("diagnostics: uploaded run {run_id}\n"),
    )
}

fn list_runs(stream: &mut TcpStream, state_dir: &Path, request: &Request) -> io::Result<()> {
    if !has_admin_token(request, "VAPOR_DIAGNOSTICS_ADMIN_TOKEN") {
        return respond_text(
            stream,
            "401 Unauthorized",
            "missing or invalid admin token\n",
        );
    }

    let mut run_ids = read_run_ids(&state_dir.join("runs"))?;
    run_ids.sort();
    respond_text(stream, "200 OK", &format!("{}\n", run_ids.join("\n")))
}

fn download_run(
    stream: &mut TcpStream,
    state_dir: &Path,
    request: &Request,
    run_id: &str,
) -> io::Result<()> {
    if !has_admin_token(request, "VAPOR_DIAGNOSTICS_ADMIN_TOKEN") {
        return respond_text(
            stream,
            "401 Unauthorized",
            "missing or invalid admin token\n",
        );
    }
    if !valid_run_id(run_id) {
        return respond_text(stream, "400 Bad Request", "invalid diagnostics run id\n");
    }

    let run_dir = state_dir.join("runs").join(run_id);
    let metadata = fs::read_to_string(run_dir.join("metadata.toml")).unwrap_or_default();
    let log = fs::read_to_string(run_dir.join("vapor.log")).unwrap_or_default();
    if metadata.is_empty() && log.is_empty() {
        return respond_text(stream, "404 Not Found", "diagnostics run not found\n");
    }

    let body = format!("# {run_id}\n\n--- metadata.toml ---\n{metadata}\n--- vapor.log ---\n{log}");
    respond_text(stream, "200 OK", &body)
}

fn export_runs(stream: &mut TcpStream, state_dir: &Path, request: &Request) -> io::Result<()> {
    if !has_admin_token(request, "VAPOR_DIAGNOSTICS_ADMIN_TOKEN") {
        return respond_text(
            stream,
            "401 Unauthorized",
            "missing or invalid admin token\n",
        );
    }

    let mut body = String::from("# Vapor diagnostics export scaffold\n\n");
    let mut run_ids = read_run_ids(&state_dir.join("runs"))?;
    run_ids.sort();
    for run_id in run_ids {
        let run_dir = state_dir.join("runs").join(&run_id);
        let metadata = fs::read_to_string(run_dir.join("metadata.toml")).unwrap_or_default();
        body.push_str(&format!("## {run_id}\n{metadata}\n"));
    }

    respond_text(stream, "200 OK", &body)
}

fn read_run_ids(runs_dir: &Path) -> io::Result<Vec<String>> {
    if !runs_dir.exists() {
        return Ok(Vec::new());
    }

    fs::read_dir(runs_dir)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if entry.file_type().ok()?.is_dir() {
                entry.file_name().into_string().ok()
            } else {
                None
            }
        })
        .filter(|run_id| valid_run_id(run_id))
        .collect::<Vec<_>>()
        .pipe(Ok)
}

fn has_admin_token(request: &Request, env_name: &str) -> bool {
    let Ok(expected) = env::var(env_name) else {
        return false;
    };
    if expected.is_empty() {
        return false;
    }

    request
        .headers
        .iter()
        .any(|(name, value)| name == "authorization" && value == &format!("Bearer {expected}"))
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

fn read_request(stream: &mut TcpStream, max_body: usize) -> io::Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut header_end = None;
    let mut content_length = 0_usize;

    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);

        if header_end.is_none() {
            header_end = find_header_end(&buffer);
            if let Some(end) = header_end {
                let headers = String::from_utf8_lossy(&buffer[..end]);
                content_length = parse_content_length(&headers).unwrap_or(0);
                if content_length > max_body {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "request body too large",
                    ));
                }
            }
        }

        if let Some(end) = header_end {
            if buffer.len() >= end + 4 + content_length {
                break;
            }
        }
    }

    let Some(end) = header_end else {
        return Ok(None);
    };

    let header_text = String::from_utf8_lossy(&buffer[..end]);
    let mut lines = header_text.lines();
    let Some(request_line) = lines.next() else {
        return Ok(None);
    };
    let mut request_parts = request_line.split_whitespace();
    let Some(method) = request_parts.next() else {
        return Ok(None);
    };
    let Some(path) = request_parts.next() else {
        return Ok(None);
    };

    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let body_start = end + 4;
    let body_end = body_start + content_length;
    let body = buffer
        .get(body_start..body_end)
        .map_or_else(Vec::new, ToOwned::to_owned);

    Ok(Some(Request {
        method: method.to_string(),
        path: path.to_string(),
        headers,
        body,
    }))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

fn respond_text(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
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

trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}

impl<T> Pipe for T {}
