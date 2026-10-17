use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use engine::{EngineConfig, default_state_file};
use protocol::parse_host_port;

/// indicontrol engine: controls INDI equipment and serves local/remote UIs.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Address to serve UIs on. Use 0.0.0.0:7700 to allow remote UIs.
    #[arg(long, default_value = "127.0.0.1:7700")]
    listen: SocketAddr,
    /// Directory containing the built web UI (see README).
    #[arg(long)]
    web_root: Option<PathBuf>,
    /// Settings file (default: ~/.config/indicontrol/engine.json).
    #[arg(long)]
    state_file: Option<PathBuf>,
    /// Connect to this INDI server on startup.
    #[arg(long, value_name = "HOST[:PORT]")]
    indi: Option<String>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let handle = engine::start(EngineConfig {
        listen: args.listen,
        web_root: args.web_root.clone(),
        state_file: args.state_file.or_else(default_state_file),
        indi: args
            .indi
            .map(|s| parse_host_port(&s, indi_link::DEFAULT_PORT)),
    })
    .await?;
    println!("UIs can connect to {}", handle.ws_url());
    if args.web_root.is_some() {
        println!("Web UI: http://{}/", handle.local_addr);
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("shutting down"),
        _ = handle.wait() => {}
    }
    Ok(())
}
