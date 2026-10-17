//! HTTP/WebSocket front of the engine.

use std::path::PathBuf;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use protocol::ToEngine;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, oneshot};
use tower_http::services::ServeDir;

use crate::core::{CoreMsg, Session};

type CoreTx = mpsc::UnboundedSender<CoreMsg>;

pub(crate) fn router(core: CoreTx, web_root: Option<PathBuf>) -> Router {
    let router = Router::new()
        .route(protocol::WS_PATH, get(ws_handler))
        .with_state(core);
    match web_root {
        Some(root) => router.fallback_service(ServeDir::new(root)),
        None => router,
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(core): State<CoreTx>) -> Response {
    ws.on_upgrade(move |socket| session(socket, core))
}

async fn session(socket: WebSocket, core: CoreTx) {
    let (reply_tx, reply_rx) = oneshot::channel();
    if core.send(CoreMsg::NewSession(reply_tx)).is_err() {
        return;
    }
    let Ok(Session {
        initial,
        mut updates,
    }) = reply_rx.await
    else {
        return;
    };
    let (mut sink, mut stream) = socket.split();
    for msg in initial {
        if sink.send(Message::Text(msg.into())).await.is_err() {
            return;
        }
    }
    tracing::info!("UI connected");
    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Ok(json) => {
                    if sink.send(Message::Text(json.as_ref().into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // The UI reconnects and gets a fresh state dump.
                    tracing::warn!("UI fell {n} messages behind; dropping session");
                    break;
                }
                Err(RecvError::Closed) => break,
            },
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => match ToEngine::from_json(text.as_str()) {
                    Ok(cmd) => {
                        let _ = core.send(CoreMsg::Ui(cmd));
                    }
                    Err(e) => tracing::warn!("bad message from UI: {e}"),
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    tracing::info!("UI disconnected");
}
