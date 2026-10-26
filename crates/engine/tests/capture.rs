//! End-to-end capture: fake INDI camera/mount/wheel → engine → WebSocket UI → FITS on disk.

mod common;

use std::time::Duration;

use common::*;
use engine::EngineConfig;
use imaging::fits::Fits;
use protocol::{CaptureRequest, CaptureState, FrameType, ImagingSettings, IndiStatus, Roles, ToEngine};
use tokio::net::TcpListener;
use tokio::time::timeout;

#[tokio::test]
async fn capture_saves_fits_with_metadata() {
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let indi_port = indi.local_addr().unwrap().port();
    let server = tokio::spawn(fake_indi(indi, -10.0));

    let dir = std::env::temp_dir().join(format!("indicontrol-capture-{}", std::process::id()));
    let engine = engine::start(EngineConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        web_root: None,
        state_file: Some(dir.join("engine.json")),
        indi: Some(("127.0.0.1".into(), indi_port)),
    })
    .await
    .unwrap();

    let mut ui = TestUi::connect(&engine.ws_url()).await;
    ui.until("INDI devices", |u| {
        u.state.indi_status == IndiStatus::Connected
            && u.state.cache.property("Wheel", "FILTER_NAME").is_some()
    })
    .await;

    let roles = Roles {
        main_camera: Some("Cam".into()),
        mount: Some("Scope".into()),
        filter_wheel: Some("Wheel".into()),
        ..Default::default()
    };
    ui.send(ToEngine::SetRoles(roles.clone())).await;
    let settings = ImagingSettings {
        output_dir: dir.join("out").display().to_string(),
        ..Default::default()
    };
    ui.send(ToEngine::SetImagingSettings(settings.clone())).await;
    ui.until("settings", |u| u.state.roles == roles && u.state.imaging == settings).await;

    ui.send(ToEngine::Capture(CaptureRequest {
        exposure: 1.0,
        frame_type: FrameType::Light,
        gain: Some(120.0),
        filter_slot: Some(2),
        object: "M 42".into(),
        save: true,
        ..Default::default()
    }))
    .await;
    ui.until("frame + preview", |u| u.state.last_frame.is_some() && !u.previews.is_empty()).await;
    ui.until("idle", |u| u.state.capture == CaptureState::Idle && u.state.frames_captured == 1).await;

    // State machine went through every stage.
    let stage = |f: fn(&CaptureState) -> bool| ui.capture_states.iter().any(f);
    assert!(stage(|s| matches!(s, CaptureState::Preparing { .. })), "{:?}", ui.capture_states);
    assert!(stage(|s| matches!(s, CaptureState::Exposing { .. })));
    assert!(stage(|s| matches!(s, CaptureState::Processing)));

    // Frame info: statistics see the clipped pixels.
    let frame = ui.state.last_frame.clone().unwrap();
    assert_eq!((frame.width, frame.height), (W, H));
    assert_eq!(frame.filter.as_deref(), Some("Ha"));
    let stats = &frame.analysis.channels[0];
    assert_eq!(stats.zero as usize, ZEROS);
    assert_eq!(stats.saturated as usize, SATURATED);
    assert_eq!(frame.analysis.adc_max, 65535);
    // The fake camera's gain scales the light.
    let level = (1000.0 + RATE * gain_factor(120.0)).min(60_000.0);
    assert!((level..level + 41.0).contains(&stats.median), "{}", stats.median);
    let (preview_id, jpeg) = &ui.previews[0];
    assert_eq!(*preview_id, frame.id);
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);

    // The FITS file on disk carries our metadata and the original pixels.
    let path = std::path::PathBuf::from(frame.path.as_deref().expect("saved"));
    assert!(path.starts_with(dir.join("out")));
    assert!(path.to_string_lossy().contains("/Light/Ha/M_42_Light_Ha_1s_G120_"), "{}", path.display());
    let saved = Fits::parse(&std::fs::read(&path).unwrap()).unwrap();
    let h = &saved.header;
    assert_eq!(h.get_str("IMAGETYP"), Some("Light Frame"));
    assert_eq!(h.get_str("FILTER"), Some("Ha"));
    assert_eq!(h.get_str("OBJECT"), Some("M 42"));
    assert_eq!(h.get_f64("GAIN"), Some(120.0));
    assert_eq!(h.get_f64("EXPTIME"), Some(1.0));
    assert_eq!(h.get_str("TELESCOP"), Some("Scope"));
    assert_eq!(h.get_str("PIERSIDE"), Some("WEST"));
    assert_eq!(h.get_f64("SITELAT"), Some(46.05));
    assert_eq!(h.get_str("INSTRUME"), Some("Cam"));
    assert!(h.get_str("DATE-OBS").is_some());
    assert!(h.get_str("OBJCTRA").is_some() && h.get_f64("RA").is_some());
    assert_eq!(h.get_f64("CCD-TEMP"), Some(-10.0), "driver value kept");
    assert_eq!(saved.data, Fits::parse(&test_frame_at_gain(1.0, gain_factor(120.0), FOCUS_START, &[])).unwrap().data);

    // A UI connecting later sees the last frame immediately.
    let mut late = TestUi::connect(&engine.ws_url()).await;
    late.until("replayed frame", |u| u.state.last_frame.is_some() && !u.previews.is_empty()).await;
    assert_eq!(late.state.last_frame.as_ref().unwrap().id, frame.id);

    // What the engine told the camera.
    engine.shutdown();
    let sent = timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
    assert!(sent.contains(r#"<enableBLOB device="Cam">Also</enableBLOB>"#));
    assert!(sent.contains(r#"<oneSwitch name="FRAME_LIGHT">On</oneSwitch>"#));
    assert!(sent.contains(r#"<oneNumber name="GAIN">120</oneNumber>"#));
    assert!(sent.contains(r#"<oneNumber name="FILTER_SLOT_VALUE">2</oneNumber>"#));
    let _ = std::fs::remove_dir_all(dir);
}
