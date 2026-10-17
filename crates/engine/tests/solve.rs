//! Plate solving end to end with fake ASTAP / astrometry.net executables.

mod common;

use common::*;
use engine::EngineConfig;
use protocol::{
    Exposure, ImagingSettings, IndiStatus, Roles, Sequence, SequenceStep, SolverKind,
    SolverSettings, Target, ToEngine,
};
use tokio::net::TcpListener;

async fn setup(name: &str, hour_angle_min: f64, kind: SolverKind) -> (TestUi, engine::EngineHandle, std::path::PathBuf) {
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = indi.local_addr().unwrap().port();
    tokio::spawn(fake_indi_at(indi, -10.0, hour_angle_min));
    let dir = std::env::temp_dir().join(format!("indicontrol-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (astap, anet) = fake_solvers(&dir.join("bin"));
    let engine = engine::start(EngineConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        web_root: None,
        state_file: Some(dir.join("engine.json")),
        indi: Some(("127.0.0.1".into(), port)),
    })
    .await
    .unwrap();
    let mut ui = TestUi::connect(&engine.ws_url()).await;
    ui.until("devices", |u| {
        u.state.indi_status == IndiStatus::Connected && u.state.cache.property("Scope", "ON_COORD_SET").is_some()
    })
    .await;
    let roles = Roles {
        main_camera: Some("Cam".into()),
        mount: Some("Scope".into()),
        ..Default::default()
    };
    let solver = SolverSettings {
        kind,
        astap_path: astap.display().to_string(),
        astrometry_path: anet.display().to_string(),
        exposure: 0.01,
        timeout: 20.0,
        ..Default::default()
    };
    let imaging = ImagingSettings {
        output_dir: dir.join("out").display().to_string(),
        file_template: "{type}_{seq:03}".into(),
    };
    ui.send(ToEngine::SetRoles(roles.clone())).await;
    ui.send(ToEngine::SetSolverSettings(solver.clone())).await;
    ui.send(ToEngine::SetImagingSettings(imaging.clone())).await;
    ui.until("settings", |u| {
        u.state.roles == roles && u.state.solver_settings == solver && u.state.imaging == imaging && u.state.mount.is_some()
    })
    .await;
    (ui, engine, dir)
}

fn log_has(ui: &TestUi, text: &str) -> bool {
    ui.state.log.iter().any(|l| l.text.contains(text))
}

fn pos(ui: &TestUi, text: &str) -> usize {
    ui.state
        .log
        .iter()
        .position(|l| l.text.contains(text))
        .unwrap_or_else(|| panic!("no '{text}' in {:#?}", ui.state.log))
}

#[tokio::test]
async fn astap_centres_target() {
    let (mut ui, engine, dir) = setup("center", -120.0, SolverKind::Astap).await;
    let m = ui.state.mount.clone().unwrap();
    let target = (m.ra_j2000, m.dec_j2000 + 0.5);
    ui.send(ToEngine::CenterOn { ra_hours: target.0, dec_deg: target.1 }).await;
    ui.until("centred", |u| log_has(u, "Centred within") || log_has(u, "failed")).await;
    assert!(log_has(&ui, "Centred within"), "{:#?}", ui.state.log);
    // First solve saw the mount's pointing error, the second was on target.
    assert!(pos(&ui, "mount was off by") < pos(&ui, "slewing again"));
    let last = ui.state.solve.last.clone().unwrap();
    assert_eq!(last.solver, SolverKind::Astap);
    assert!((last.pixel_scale - 0.00086 * 3600.0).abs() < 0.01);
    ui.until("mount update", |u| {
        let m = u.state.mount.as_ref().unwrap();
        (m.dec_j2000 - target.1).abs() < 1.0 / 60.0 && !u.state.solve.busy
    })
    .await;
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn astrometry_net_solve_and_sync() {
    let (mut ui, engine, dir) = setup("sync", -120.0, SolverKind::AstrometryNet).await;
    let before = ui.state.mount.clone().unwrap();
    ui.send(ToEngine::SolveAndSync).await;
    ui.until("synced", |u| {
        (log_has(u, "Mount synced") && !u.state.solve.busy) || log_has(u, "failed")
    })
    .await;
    assert!(log_has(&ui, "Mount synced"), "{:#?}", ui.state.log);
    let last = ui.state.solve.last.clone().unwrap();
    assert_eq!(last.solver, SolverKind::AstrometryNet);
    // The mount was ~18' off in Dec; the solve found it.
    let err = last.pointing_error.unwrap();
    assert!((15.0..25.0).contains(&err), "pointing error {err}'");
    ui.until("mount moved to the solution", |u| {
        let m = u.state.mount.as_ref().unwrap();
        (m.dec_j2000 - before.dec_j2000 - POINTING_ERROR.1).abs() < 0.01
    })
    .await;
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn missing_solver_is_reported() {
    let (mut ui, engine, dir) = setup("nosolver", -120.0, SolverKind::Astap).await;
    let mut s = ui.state.solver_settings.clone();
    s.astap_path = "/nonexistent/astap".into();
    ui.send(ToEngine::SetSolverSettings(s)).await;
    ui.send(ToEngine::SolveAndSync).await;
    ui.until("error", |u| log_has(u, "ASTAP not found")).await;
    ui.until("idle", |u| !u.state.solve.busy).await;
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn sequence_centres_at_start_and_after_flip() {
    // Starts 10 min past the meridian on the west side: the first light triggers a flip.
    let (mut ui, engine, dir) = setup("seqcenter", 10.0, SolverKind::Astap).await;
    let m = ui.state.mount.clone().unwrap();
    let seq = Sequence {
        name: "Target".into(),
        steps: vec![SequenceStep {
            exposure: Exposure::Fixed { seconds: 0.05 },
            count: 1,
            ..Default::default()
        }],
        target: Some(Target { ra_hours: m.ra_j2000, dec_deg: m.dec_j2000 }),
        center_on_start: false,
        center_after_flip: true,
        meridian_flip: true,
        dither_every: 0,
        ..Default::default()
    };
    ui.send(ToEngine::StartSequence(seq)).await;
    ui.until("done", |u| u.state.sequence.is_none() && log_has(u, "Target complete")).await;
    assert!(pos(&ui, "Meridian flip: target") < pos(&ui, "Centred within"));
    assert!(pos(&ui, "Centred within") < pos(&ui, "Meridian flip complete"));
    assert!(pos(&ui, "Meridian flip complete") < pos(&ui, "Saved"));
    assert_eq!(ui.state.mount.as_ref().unwrap().pier_side.as_deref(), Some("EAST"));
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn sequence_centres_on_start() {
    let (mut ui, engine, dir) = setup("seqstart", -120.0, SolverKind::Astap).await;
    let m = ui.state.mount.clone().unwrap();
    let seq = Sequence {
        name: "Start".into(),
        steps: vec![SequenceStep {
            exposure: Exposure::Fixed { seconds: 0.05 },
            count: 1,
            ..Default::default()
        }],
        target: Some(Target { ra_hours: m.ra_j2000, dec_deg: m.dec_j2000 - 0.4 }),
        center_on_start: true,
        dither_every: 0,
        ..Default::default()
    };
    ui.send(ToEngine::StartSequence(seq)).await;
    ui.until("done", |u| u.state.sequence.is_none() && log_has(u, "Start complete")).await;
    assert!(pos(&ui, "Centred within") < pos(&ui, "Saved"));
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
