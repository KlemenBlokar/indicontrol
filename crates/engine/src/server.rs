//! HTTP/WebSocket front of the engine.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{Query, State};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use protocol::{PROTOCOL_VERSION, ToEngine, ToUi};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, oneshot};
use tower_http::services::ServeDir;

use crate::core::{CoreMsg, Outgoing, Session};

type CoreTx = mpsc::UnboundedSender<CoreMsg>;

pub(crate) fn router(core: CoreTx, web_root: Option<PathBuf>) -> Router {
    let router = Router::new()
        .route(protocol::WS_PATH, get(ws_handler))
        .with_state(core);
    match web_root {
        // Release builds of the web UI come with .br/.gz copies (Trunk.toml).
        Some(root) => router.fallback_service(ServeDir::new(root).precompressed_br().precompressed_gzip()),
        None => router,
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(core): State<CoreTx>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let ui_protocol = query.get("protocol").and_then(|v| v.parse::<u32>().ok());
    if ui_protocol != Some(PROTOCOL_VERSION) {
        return ws.on_upgrade(move |socket| turn_away(socket, ui_protocol));
    }
    ws.on_upgrade(move |socket| session(socket, core))
}

/// A UI built for another protocol: tell it ours (a web UI reloads itself
/// on that), then close before it can send anything.
async fn turn_away(mut socket: WebSocket, ui_protocol: Option<u32>) {
    // Such a UI retries every few seconds: say so once a minute.
    static LAST_WARNING: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if now.saturating_sub(LAST_WARNING.load(Ordering::Relaxed)) >= 60 {
        LAST_WARNING.store(now, Ordering::Relaxed);
        let ui = ui_protocol.map_or("an unknown".into(), |v| format!("v{v}"));
        tracing::warn!("turned away a UI speaking {ui} protocol (engine: v{PROTOCOL_VERSION}); reload it");
    }
    let hello = ToUi::Hello {
        protocol: PROTOCOL_VERSION,
        engine_version: env!("CARGO_PKG_VERSION").into(),
        session: 0,
    };
    let _ = socket.send(Message::Text(hello.to_json().into())).await;
    let _ = socket.send(Message::Close(None)).await;
}

async fn session(socket: WebSocket, core: CoreTx) {
    let (reply_tx, reply_rx) = oneshot::channel();
    if core.send(CoreMsg::NewSession(reply_tx)).is_err() {
        return;
    }
    let Ok(Session {
        id,
        initial,
        mut updates,
    }) = reply_rx.await
    else {
        return;
    };
    let (mut sink, mut stream) = socket.split();
    for msg in initial {
        if sink.send(to_message(msg)).await.is_err() {
            return;
        }
    }
    tracing::info!("UI connected");
    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Ok(msg) => {
                    if sink.send(to_message(msg)).await.is_err() {
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
                    // Answered here: only this UI wants the result.
                    Ok(ToEngine::FindObjects { query }) => {
                        let objects = crate::catalog::search(&query, protocol::catalog::MAX_RESULTS);
                        let reply = ToUi::ObjectsFound { query, objects }.to_json();
                        if sink.send(Message::Text(reply.into())).await.is_err() {
                            break;
                        }
                    }
                    Ok(cmd) => {
                        let _ = core.send(CoreMsg::Ui { session: id, cmd });
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

fn to_message(msg: Outgoing) -> Message {
    match msg {
        Outgoing::Text(json) => Message::Text(json.as_ref().into()),
        Outgoing::Binary(bytes) => Message::Binary(bytes),
    }
}
