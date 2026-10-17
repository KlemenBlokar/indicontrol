//! Mount status reaches UIs on startup, without any slew or role change.

mod common;

use common::*;
use engine::{EngineConfig, PersistedState};
use protocol::Roles;
use tokio::net::TcpListener;

#[tokio::test]
async fn mount_status_after_startup() {
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = indi.local_addr().unwrap().port();
    tokio::spawn(fake_indi(indi, -10.0));
    let dir = std::env::temp_dir().join(format!("indicontrol-mountstatus-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // Saved state from an earlier session: the mount role is already set.
    let state = PersistedState {
        roles: Roles {
            mount: Some("Scope".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    state.save(&dir.join("engine.json"));
    let engine = engine::start(EngineConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        web_root: None,
        state_file: Some(dir.join("engine.json")),
        indi: Some(("127.0.0.1".into(), port)),
    })
    .await
    .unwrap();
    let mut ui = TestUi::connect(&engine.ws_url()).await;
    ui.until("mount status", |u| u.state.mount.as_ref().is_some_and(|m| m.hour_angle.is_some())).await;
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
