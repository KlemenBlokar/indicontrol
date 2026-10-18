//! End-to-end guiding: fake guide camera + pulse-guided mount → engine → UI.

mod common;

use common::*;
use engine::EngineConfig;
use protocol::guider::CalibrationSettings;
use protocol::{GuiderSettings, GuiderState, IndiStatus, Roles, ToEngine};
use tokio::net::TcpListener;

#[tokio::test]
async fn calibrates_and_guides() {
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = indi.local_addr().unwrap().port();
    tokio::spawn(fake_indi(indi, -10.0));
    let dir = std::env::temp_dir().join(format!("indicontrol-guide-{}", std::process::id()));
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
            && u.state.cache.property("Scope", "TELESCOPE_TIMED_GUIDE_WE").is_some()
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
    let settings = GuiderSettings {
        exposure: 0.01,
        settle_ms: 5.0,
        calibration: CalibrationSettings {
            step_ms: 100.0,
            distance_px: 20.0,
            ..Default::default()
        },
        ..Default::default()
    };
    ui.send(ToEngine::SetGuiderSettings(settings.clone())).await;
    ui.until("settings", |u| u.state.roles == roles && u.state.guider_settings == settings).await;

    // One button: selects a star, calibrates, then guides.
    ui.send(ToEngine::GuiderGuide).await;
    ui.until("calibration", |u| u.state.guider.calibration.is_some()).await;
    ui.until("30 guide steps", |u| {
        u.state.guider.state == GuiderState::Guiding && u.state.guide_history.len() >= 30
    })
    .await;

    let status = &ui.state.guider;
    let cal = status.calibration.as_ref().unwrap();
    let angle = |a: f64| a.to_degrees().rem_euclid(360.0);
    assert!((angle(cal.ra_angle) - GUIDE_THETA_DEG).abs() < 5.0, "RA angle {}", angle(cal.ra_angle));
    assert!((angle(cal.dec_angle) - GUIDE_THETA_DEG - 90.0).abs() < 5.0, "Dec angle {}", angle(cal.dec_angle));
    assert!((cal.ra_rate / GUIDE_RATE - 1.0).abs() < 0.2, "RA rate {}", cal.ra_rate);
    assert!((cal.dec_rate / GUIDE_RATE - 1.0).abs() < 0.2, "Dec rate {}", cal.dec_rate);

    // Pixel scale from the guide camera pixel size and guide scope focal length.
    let scale = status.pixel_scale.expect("pixel scale");
    assert!((scale - 206.265 * 3.75 / 240.0).abs() < 1e-6);

    // Guiding keeps the star near the lock position despite 0.3 px/frame drift
    // (30 frames unguided would be 9 px).
    let recent: Vec<_> = ui.state.guide_history.iter().rev().take(15).collect();
    assert!(recent.iter().all(|s| !s.star_lost));
    let rms = (recent.iter().map(|s| s.ra_px.powi(2) + s.dec_px.powi(2)).sum::<f64>() / recent.len() as f64).sqrt();
    eprintln!(
        "calibration: RA {:.1}° {:.4} px/ms, Dec {:.1}° {:.4} px/ms; guided RMS {rms:.3} px",
        angle(cal.ra_angle), cal.ra_rate, angle(cal.dec_angle), cal.dec_rate
    );
    assert!(rms < 0.8, "guide RMS {rms} px");
    assert!(recent.iter().any(|s| s.ra_pulse.is_some()), "corrections were sent");

    // Previews of guide frames reach the UI.
    let guide_previews = ui.previews.len();
    assert!(guide_previews > 0);

    ui.send(ToEngine::GuiderStop).await;
    ui.until("idle", |u| u.state.guider.state == GuiderState::Idle).await;

    // Calibration is kept for next time.
    let saved = engine::PersistedState::load(&dir.join("engine.json"));
    assert!(saved.guider_calibration.is_some());

    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

/// Several stars in the field: guiding uses all of them and survives losing
/// the primary star.
#[tokio::test]
async fn multi_star_guiding_survives_losing_the_primary() {
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = indi.local_addr().unwrap().port();
    tokio::spawn(fake_indi(indi, -10.0));
    let dir = std::env::temp_dir().join(format!("indicontrol-multistar-{}", std::process::id()));
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
            && u.state.cache.property("Scope", "TELESCOPE_TIMED_GUIDE_WE").is_some()
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
    let settings = GuiderSettings {
        exposure: 0.01,
        settle_ms: 5.0,
        calibration: CalibrationSettings {
            step_ms: 100.0,
            distance_px: 15.0,
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(settings.multi_star, "on by default");
    ui.send(ToEngine::SetGuiderSettings(settings.clone())).await;
    ui.until("settings", |u| u.state.roles == roles && u.state.guider_settings == settings).await;

    ui.send(ToEngine::GuiderGuide).await;
    ui.until("guiding on several stars", |u| {
        u.state.guider.state == GuiderState::Guiding && u.state.guide_history.len() >= 10 && u.state.guider.stars_used >= 3
    })
    .await;
    let primary = ui.state.guider.star.expect("primary star");
    assert!(ui.state.guider.guide_stars.len() >= 3, "{:?}", ui.state.guider.guide_stars);

    // The primary star vanishes; the others carry on.
    ui.send(ToEngine::SetNumber {
        device: "GuideCam".into(),
        name: "SIM_HIDE_PRIMARY".into(),
        values: vec![("HIDE".into(), 1.0)],
    })
    .await;
    let before = ui.state.guide_history.len();
    ui.until("20 more guide steps", |u| u.state.guide_history.len() >= before + 20).await;
    assert_eq!(ui.state.guider.state, GuiderState::Guiding);
    let recent: Vec<_> = ui.state.guide_history.iter().rev().take(15).collect();
    assert!(recent.iter().all(|s| !s.star_lost), "guide star counted as lost");
    let rms = (recent.iter().map(|s| s.ra_px.powi(2) + s.dec_px.powi(2)).sum::<f64>() / recent.len() as f64).sqrt();
    assert!(rms < 0.8, "guide RMS {rms} px after losing the primary");
    let now = ui.state.guider.star.expect("a new primary");
    assert!((now.x - primary.x).hypot(now.y - primary.y) > 10.0, "another star became primary");
    assert!(ui.state.guider.stars_used >= 2);

    ui.send(ToEngine::GuiderStop).await;
    ui.until("idle", |u| u.state.guider.state == GuiderState::Idle).await;
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
