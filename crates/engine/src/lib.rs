//! indicontrol engine: owns the INDI connection (and later guiding,
//! sequencing and file output) and serves UIs over a WebSocket.
//!
//! Runs standalone (`indicontrol-engine`, e.g. on the computer at the
//! telescope) or embedded in the native UI for local use.

mod core;
mod server;
mod state;

use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::task::JoinHandle;

pub use state::{IndiSettings, PersistedState, default_state_file};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Address for the WebSocket/HTTP server; port 0 picks a free port.
    pub listen: SocketAddr,
    /// Directory with the built web UI (index.html + wasm) to serve over HTTP.
    pub web_root: Option<PathBuf>,
    /// Where settings are persisted; `None` keeps them in memory only.
    pub state_file: Option<PathBuf>,
    /// Connect to this INDI server on start (overrides saved settings).
    pub indi: Option<(String, u16)>,
}

pub struct EngineHandle {
    pub local_addr: SocketAddr,
    tasks: [JoinHandle<()>; 2],
}

impl EngineHandle {
    /// WebSocket URL for UIs on this machine.
    pub fn ws_url(&self) -> String {
        format!("ws://{}{}", self.local_addr, protocol::WS_PATH)
    }

    /// Resolves when the engine stops (normally never).
    pub async fn wait(self) {
        let [server, core] = self.tasks;
        let _ = tokio::join!(server, core);
    }

    pub fn shutdown(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Start the engine on the current tokio runtime.
pub async fn start(config: EngineConfig) -> std::io::Result<EngineHandle> {
    let mut state = config
        .state_file
        .as_deref()
        .map(PersistedState::load)
        .unwrap_or_default();
    if let Some((host, port)) = config.indi {
        state.indi.host = host;
        state.indi.port = port;
        state.indi.autoconnect = true;
    }

    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let local_addr = listener.local_addr()?;

    let (core, core_tx) = core::Core::new(state, config.state_file);
    let core_task = tokio::spawn(core.run());
    let app = server::router(core_tx, config.web_root);
    let server_task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("engine server stopped: {e}");
        }
    });
    tracing::info!("engine listening on {local_addr}");
    Ok(EngineHandle {
        local_addr,
        tasks: [server_task, core_task],
    })
}
