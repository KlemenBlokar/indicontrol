//! Fake indiserver → engine → WebSocket UI, end to end.

use std::time::Duration;

use engine::EngineConfig;
use futures_util::{SinkExt, StreamExt};
use indi_link::PropertyState;
use protocol::{IndiStatus, Roles, ToEngine, ToUi, UiState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn recv(ws: &mut Ws) -> ToUi {
    loop {
        let msg = timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for engine")
            .expect("socket closed")
            .unwrap();
        if let Message::Text(t) = msg {
            return ToUi::from_json(t.as_str()).unwrap();
        }
    }
}

/// Receive and mirror messages until `pred` holds.
async fn until(ws: &mut Ws, state: &mut UiState, pred: impl Fn(&UiState) -> bool) {
    while !pred(state) {
        let msg = recv(ws).await;
        state.apply(msg);
    }
}

async fn send(ws: &mut Ws, cmd: ToEngine) {
    ws.send(Message::Text(cmd.to_json().into())).await.unwrap();
}

#[tokio::test]
async fn ui_controls_indi_through_engine() {
    // Fake indiserver: defines a device, then acknowledges CONNECT.
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let indi_port = indi.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = indi.accept().await.unwrap();
        let mut seen = String::new();
        let mut buf = [0u8; 4096];
        let mut defined = false;
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 {
                return;
            }
            seen.push_str(std::str::from_utf8(&buf[..n]).unwrap());
            if !defined && seen.contains("<getProperties") {
                defined = true;
                sock.write_all(br#"<defSwitchVector device="Cam" name="CONNECTION" group="Main" state="Idle" perm="rw" rule="OneOfMany"><defSwitch name="CONNECT">Off</defSwitch><defSwitch name="DISCONNECT">On</defSwitch></defSwitchVector>"#).await.unwrap();
            }
            if seen.contains("</newSwitchVector>") {
                seen.clear();
                sock.write_all(br#"<setSwitchVector device="Cam" name="CONNECTION" state="Ok"><oneSwitch name="CONNECT">On</oneSwitch><oneSwitch name="DISCONNECT">Off</oneSwitch></setSwitchVector><message device="Cam" message="hello from Cam"/>"#).await.unwrap();
            }
        }
    });

    let dir = std::env::temp_dir().join(format!("indicontrol-test-{}", std::process::id()));
    let state_file = dir.join("engine.json");
    let engine = engine::start(EngineConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        web_root: None,
        state_file: Some(state_file.clone()),
        indi: None,
    })
    .await
    .unwrap();

    let (mut ws, _) = tokio_tungstenite::connect_async(engine.ws_url()).await.unwrap();
    let mut ui = UiState::default();
    assert!(matches!(recv(&mut ws).await, ToUi::Hello { .. }));
    until(&mut ws, &mut ui, |s| matches!(s.indi_status, IndiStatus::Disconnected { .. })).await;

    send(&mut ws, ToEngine::ConnectIndi { host: "127.0.0.1".into(), port: indi_port }).await;
    until(&mut ws, &mut ui, |s| s.cache.property("Cam", "CONNECTION").is_some()).await;
    assert_eq!(ui.indi_status, IndiStatus::Connected);

    send(&mut ws, ToEngine::ConnectDevice { device: "Cam".into() }).await;
    until(&mut ws, &mut ui, |s| s.cache.devices["Cam"].is_connected()).await;
    assert_eq!(ui.cache.property("Cam", "CONNECTION").unwrap().state, PropertyState::Ok);
    until(&mut ws, &mut ui, |s| s.log.iter().any(|l| l.text == "hello from Cam")).await;

    // A second UI gets the full current state on connect.
    let roles = Roles { main_camera: Some("Cam".into()), ..Default::default() };
    send(&mut ws, ToEngine::SetRoles(roles.clone())).await;
    until(&mut ws, &mut ui, |s| s.roles == roles).await;
    let (mut ws2, _) = tokio_tungstenite::connect_async(engine.ws_url()).await.unwrap();
    let mut ui2 = UiState::default();
    // Initial dump order: Hello, status, roles, devices, then log history.
    until(&mut ws2, &mut ui2, |s| s.log.iter().any(|l| l.text == "hello from Cam")).await;
    assert_eq!(ui2.roles, roles);
    assert!(ui2.cache.devices["Cam"].is_connected());

    // Settings were persisted.
    let saved = engine::PersistedState::load(&state_file);
    assert_eq!(saved.roles, roles);
    assert_eq!(saved.indi.port, indi_port);
    assert!(saved.indi.autoconnect);

    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
