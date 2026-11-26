use std::env;
use tokio::net::TcpListener;

const DEFAULT_BIND: &str = "127.0.0.1:7114";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind = env::var("VAPOR_DIAGNOSTICS_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let app = vapor_diagnostics_server::app_router_from_env().await?;
    let listener = TcpListener::bind(&bind).await?;
    eprintln!("vapor-diagnostics-server listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}
