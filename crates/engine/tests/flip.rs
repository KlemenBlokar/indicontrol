//! Dithering with settling, and a meridian flip in the middle of a guided sequence.

mod common;

use common::*;
use engine::EngineConfig;
use protocol::guider::CalibrationSettings;
use protocol::{
    Exposure, GuiderSettings, GuiderState, ImagingSettings, IndiStatus, Roles, Sequence,
    SequenceStep, ToEngine,
};
use tokio::net::TcpListener;

async fn guided_setup(name: &str, hour_angle_min: f64) -> (TestUi, engine::EngineHandle, std::path::PathBuf) {
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = indi.local_addr().unwrap().port();
    tokio::spawn(fake_indi_at(indi, -10.0, hour_angle_min));
    let dir = std::env::temp_dir().join(format!("indicontrol-{name}-{}", std::process::id()));
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
    ui.until("devices", |u| {
        u.state.indi_status == IndiStatus::Connected
            && u.state.cache.property("Scope", "TELESCOPE_INFO").is_some()
            && u.state.cache.property("GuideCam", "CCD_INFO").is_some()
    })
    .await;
    let roles = Roles {
        main_camera: Some("Cam".into()),
        guide_camera: Some("GuideCam".into()),
        mount: Some("Scope".into()),
        ..Default::default()
    };
    ui.send(ToEngine::SetRoles(roles.clone())).await;
    let guider = GuiderSettings {
        exposure: 0.01,
        settle_ms: 5.0,
        calibration: CalibrationSettings {
            step_ms: 100.0,
            distance_px: 20.0,
            ..Default::default()
        },
        dither_px: 4.0,
        settle_px: 1.0,
        settle_time: 0.15,
        settle_timeout: 20.0,
        ..Default::default()
    };
    ui.send(ToEngine::SetGuiderSettings(guider.clone())).await;
    let imaging = ImagingSettings {
        output_dir: dir.join("out").display().to_string(),
        file_template: "{type}_{seq:03}".into(),
        ..Default::default()
    };
    ui.send(ToEngine::SetImagingSettings(imaging.clone())).await;
    ui.until("settings", |u| {
        u.state.roles == roles
            && u.state.guider_settings == guider
            && u.state.imaging == imaging
            && u.state.mount.is_some()
    })
    .await;
    ui.send(ToEngine::GuiderGuide).await;
    ui.until("guiding settled", |u| {
        u.state.guider.state == GuiderState::Guiding && !u.state.guider.settling
            && u.state.log.iter().any(|l| l.text == "Guiding settled")
    })
    .await;
    (ui, engine, dir)
}

fn count(ui: &TestUi, text: &str) -> usize {
    ui.state.log.iter().filter(|l| l.text.contains(text)).count()
}

#[tokio::test]
async fn manual_dither_settles() {
    let (mut ui, engine, dir) = guided_setup("dither", -120.0).await;
    let lock_before = ui.state.guider.lock.unwrap();
    ui.send(ToEngine::GuiderDither).await;
    ui.until("settling", |u| u.state.guider.settling).await;
    ui.until("settled again", |u| !u.state.guider.settling && count(u, "Guiding settled") >= 2).await;
    let lock_after = ui.state.guider.lock.unwrap();
    let moved = (lock_after.0 - lock_before.0).hypot(lock_after.1 - lock_before.1);
    assert!(moved > 0.0 && moved <= 4.0 * 2f64.sqrt() + 1e-9, "lock moved {moved} px");
    // The star followed the lock.
    let star = ui.state.guider.star.unwrap();
    assert!((star.x - lock_after.0).hypot(star.y - lock_after.1) < 1.5);
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn sequence_flips_at_meridian_and_dithers() {
    // Target already 10 minutes past the meridian, mount still on the west side.
    let (mut ui, engine, dir) = guided_setup("flip", 10.0).await;
    assert_eq!(ui.state.mount.as_ref().unwrap().pier_side.as_deref(), Some("WEST"));

    let seq = Sequence {
        name: "M 42".into(),
        object: "M 42".into(),
        steps: vec![SequenceStep {
            exposure: Exposure::Fixed { seconds: 0.1 },
            count: 3,
            ..Default::default()
        }],
        dither_every: 1,
        meridian_flip: true,
        flip_minutes_past: 5.0,
        ..Default::default()
    };
    ui.send(ToEngine::StartSequence(seq)).await;
    ui.until("sequence done", |u| u.state.sequence.is_none() && count(u, "M 42 complete") == 1).await;

    let log: Vec<&str> = ui.state.log.iter().map(|l| l.text.as_str()).collect();
    let pos = |t: &str| log.iter().position(|l| l.contains(t)).unwrap_or_else(|| panic!("no '{t}' in {log:#?}"));
    // Flip happened before the first frame, then guiding resumed flipped.
    assert!(pos("Meridian flip: target is") < pos("Meridian flip complete"));
    assert!(pos("using the flipped calibration") < pos("Meridian flip complete"));
    assert!(pos("Meridian flip complete") < pos("Saved"));
    assert_eq!(ui.state.mount.as_ref().unwrap().pier_side.as_deref(), Some("EAST"));
    assert_eq!(count(&ui, "Meridian flip complete"), 1, "only one flip");
    // Dither before frames 2 and 3.
    assert_eq!(count(&ui, "Dither by"), 2, "{log:#?}");
    assert_eq!(count(&ui, "Saved"), 3);

    // Guiding after the flip holds the star (flipped calibration works).
    // Measure after the last dither has been absorbed.
    let mark = ui.state.guide_history.back().unwrap().frame;
    ui.until("more guiding", |u| u.state.guide_history.back().unwrap().frame >= mark + 25).await;
    let recent: Vec<_> = ui.state.guide_history.iter().rev().take(10).collect();
    assert!(recent.iter().all(|s| !s.star_lost));
    let rms = (recent.iter().map(|s| s.ra_px.powi(2) + s.dec_px.powi(2)).sum::<f64>() / 10.0).sqrt();
    eprintln!("guide RMS after flip: {rms:.3} px");
    assert!(rms < 0.8, "guiding after flip: {rms} px");
    assert_eq!(ui.state.guider.state, GuiderState::Guiding);

    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
