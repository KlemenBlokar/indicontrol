//! Equipment profiles: save, switch (including the INDI server), delete.

mod common;

use common::*;
use engine::EngineConfig;
use protocol::{FocusSettings, IndiStatus, Roles, ToEngine};
use tokio::net::TcpListener;

#[tokio::test]
async fn profiles_switch_whole_rigs() {
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = indi.local_addr().unwrap().port();
    tokio::spawn(fake_indi_multi(indi, -10.0));
    let dir = std::env::temp_dir().join(format!("indicontrol-profiles-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine = engine::start(EngineConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        web_root: None,
        state_file: Some(dir.join("engine.json")),
        indi: Some(("127.0.0.1".into(), port)),
    })
    .await
    .unwrap();
    let mut ui = TestUi::connect(&engine.ws_url()).await;
    ui.until("connected", |u| u.state.indi_status == IndiStatus::Connected && u.state.cache.devices.contains_key("Cam"))
        .await;

    // Rig A: the refractor.
    let roles_a = Roles {
        main_camera: Some("Cam".into()),
        mount: Some("Scope".into()),
        ..Default::default()
    };
    let focus_a = FocusSettings { step: 40, ..Default::default() };
    ui.send(ToEngine::SetRoles(roles_a.clone())).await;
    ui.send(ToEngine::SetFocusSettings(focus_a.clone())).await;
    ui.send(ToEngine::SaveProfile { name: "Refractor".into() }).await;
    ui.until("saved A", |u| u.state.profiles == ["Refractor"]).await;

    // Rig B: another camera, focuser settings, and an INDI server that is down.
    let roles_b = Roles {
        main_camera: Some("GuideCam".into()),
        ..Default::default()
    };
    ui.send(ToEngine::SetRoles(roles_b.clone())).await;
    ui.send(ToEngine::SetFocusSettings(FocusSettings { step: 250, ..Default::default() })).await;
    ui.send(ToEngine::SaveProfile { name: "Newtonian".into() }).await;
    ui.until("saved B", |u| u.state.profiles.len() == 2 && u.state.active_profile.as_deref() == Some("Newtonian")).await;

    // Rig B uses a different INDI server, which is down.
    ui.send(ToEngine::ConnectIndi { host: "127.0.0.1".into(), port: 1 }).await;
    ui.until("B disconnected", |u| matches!(u.state.indi_status, IndiStatus::Disconnected { error: Some(_) })).await;
    ui.send(ToEngine::SaveProfile { name: "Newtonian".into() }).await;

    // Back to A: roles, settings and the INDI server all come back.
    ui.send(ToEngine::LoadProfile { name: "Refractor".into() }).await;
    ui.until("A loaded", |u| {
        u.state.roles == roles_a
            && u.state.focus_settings == focus_a
            && u.state.active_profile.as_deref() == Some("Refractor")
            && u.state.indi_status == IndiStatus::Connected
            && u.state.indi_port == port
    })
    .await;

    // And to B: different server (down), different settings.
    ui.send(ToEngine::LoadProfile { name: "Newtonian".into() }).await;
    ui.until("B loaded", |u| u.state.roles == roles_b && u.state.focus_settings.step == 250 && u.state.indi_port == 1)
        .await;

    ui.send(ToEngine::DeleteProfile { name: "Newtonian".into() }).await;
    ui.until("deleted", |u| u.state.profiles == ["Refractor"] && u.state.active_profile.is_none()).await;

    let saved = engine::PersistedState::load(&dir.join("engine.json"));
    assert_eq!(saved.profiles.len(), 1);
    assert_eq!(saved.profiles[0].roles, roles_a);
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
