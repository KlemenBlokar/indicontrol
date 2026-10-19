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

#[tokio::test]
async fn object_search_answers_only_the_asking_ui() {
    let engine = engine::start(EngineConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        web_root: None,
        state_file: None,
        indi: None,
    })
    .await
    .unwrap();
    let (mut asking, _) = tokio_tungstenite::connect_async(engine.ws_url()).await.unwrap();
    let (mut other, _) = tokio_tungstenite::connect_async(engine.ws_url()).await.unwrap();

    send(&mut asking, ToEngine::FindObjects { query: "m 110".into() }).await;
    let (query, objects) = loop {
        if let ToUi::ObjectsFound { query, objects } = recv(&mut asking).await {
            break (query, objects);
        }
    };
    assert_eq!(query, "m 110");
    assert_eq!(objects[0].name, "M 110");
    assert!(objects.len() <= protocol::catalog::MAX_RESULTS);

    // The other UI only gets its own state dump.
    while let Ok(Some(Ok(msg))) = timeout(Duration::from_millis(300), other.next()).await {
        if let Message::Text(t) = msg {
            assert!(!matches!(ToUi::from_json(t.as_str()).unwrap(), ToUi::ObjectsFound { .. }));
        }
    }
    engine.shutdown();
}

#[tokio::test]
async fn shared_documents_between_uis() {
    use protocol::{CaptureForm, DocId, GuiderSettings};
    use serde_json::json;

    let dir = std::env::temp_dir().join(format!("indicontrol-docs-{}", std::process::id()));
    let state_file = dir.join("engine.json");
    let config = EngineConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        web_root: None,
        state_file: Some(state_file.clone()),
        indi: None,
    };
    let engine = engine::start(config.clone()).await.unwrap();
    let connect = async || {
        let (mut ws, _) = tokio_tungstenite::connect_async(engine.ws_url()).await.unwrap();
        let mut state = UiState::default();
        until(&mut ws, &mut state, |s| s.docs.len() == DocId::ALL.len()).await;
        (ws, state)
    };
    let (mut a, mut sa) = connect().await;
    let (mut b, mut sb) = connect().await;
    assert_ne!(sa.session, sb.session);
    let capture = |s: &UiState| serde_json::from_value::<CaptureForm>(s.docs[&DocId::Capture].clone()).unwrap();

    // Both edit the capture form at once, different fields: both stick, everywhere.
    let edit = |seq, path: &str, value| ToEngine::EditDoc { doc: DocId::Capture, seq, ops: vec![(path.into(), value)] };
    send(&mut a, edit(1, "/exposure", json!(30.0))).await;
    send(&mut b, edit(1, "/object", json!("M 31"))).await;
    let both = |s: &UiState| {
        let c = capture(s);
        c.exposure == 30.0 && c.object == "M 31"
    };
    until(&mut a, &mut sa, both).await;
    until(&mut b, &mut sb, both).await;

    // A drafts a guider change; B sees the draft and applies it for everyone.
    let draft = |s: &UiState| serde_json::from_value::<GuiderSettings>(s.docs[&DocId::GuiderSettings].clone()).unwrap();
    assert_ne!(sb.guider_settings.exposure, 3.5);
    send(&mut a, ToEngine::EditDoc { doc: DocId::GuiderSettings, seq: 2, ops: vec![("/exposure".into(), json!(3.5))] })
        .await;
    until(&mut b, &mut sb, |s| draft(s).exposure == 3.5).await;
    assert_ne!(sb.guider_settings.exposure, 3.5, "a draft is not applied yet");
    send(&mut b, ToEngine::SetGuiderSettings(draft(&sb))).await;
    until(&mut a, &mut sa, |s| s.guider_settings.exposure == 3.5).await;

    // An edit that would break the document is ignored.
    send(&mut a, edit(3, "/exposure", json!("long"))).await;
    send(&mut a, edit(4, "/save", json!(false))).await;
    until(&mut a, &mut sa, |s| !capture(s).save).await;
    assert_eq!(capture(&sa).exposure, 30.0);

    // The form is saved on the engine (shortly after the last edit) and survives a restart.
    let saved = || {
        std::fs::read_to_string(&state_file)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .is_some_and(|v| v["capture"]["save"] == json!(false))
    };
    timeout(Duration::from_secs(5), async {
        while !saved() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("state file not written");
    engine.shutdown();
    drop((a, b));
    let engine = engine::start(config).await.unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(engine.ws_url()).await.unwrap();
    let mut state = UiState::default();
    until(&mut ws, &mut state, |s| s.docs.contains_key(&DocId::Capture)).await;
    assert_eq!(capture(&state).object, "M 31");
    assert!(!capture(&state).save);
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn ui_of_another_protocol_is_turned_away() {
    let engine = engine::start(EngineConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        web_root: None,
        state_file: None,
        indi: None,
    })
    .await
    .unwrap();
    let base = format!("ws://{}{}", engine.local_addr, protocol::WS_PATH);
    for url in [base.clone(), format!("{base}?protocol=1")] {
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        // Told the engine's version, so a web UI can reload…
        match recv(&mut ws).await {
            ToUi::Hello { protocol, .. } => assert_eq!(protocol, protocol::PROTOCOL_VERSION),
            other => panic!("expected Hello, got {other:?}"),
        }
        // …then closed: commands from it never reach the engine.
        send(&mut ws, ToEngine::DisconnectIndi).await;
        let rest = timeout(Duration::from_secs(5), async {
            let mut texts = 0;
            while let Some(Ok(msg)) = ws.next().await {
                texts += matches!(msg, Message::Text(_)) as usize;
            }
            texts
        })
        .await
        .expect("not closed");
        assert_eq!(rest, 0, "{url}");
    }
    // The current version gets the full state.
    let (mut ws, _) = tokio_tungstenite::connect_async(protocol::versioned_url(&base)).await.unwrap();
    let mut state = UiState::default();
    until(&mut ws, &mut state, |s| s.docs.len() == protocol::DocId::ALL.len()).await;
    engine.shutdown();
}
