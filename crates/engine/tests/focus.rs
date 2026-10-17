//! Autofocus end to end: fake focuser whose position sets the star size.

mod common;

use common::*;
use engine::EngineConfig;
use protocol::{
    AutofocusTriggers, Exposure, FocusSettings, ImagingSettings, IndiStatus, Roles, Sequence,
    SequenceStep, ToEngine,
};
use tokio::net::TcpListener;

async fn setup(name: &str) -> (TestUi, engine::EngineHandle, std::path::PathBuf) {
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = indi.local_addr().unwrap().port();
    tokio::spawn(fake_indi(indi, -10.0));
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
            && u.state.cache.property("Focuser", "ABS_FOCUS_POSITION").is_some()
            && u.state.cache.property("Wheel", "FILTER_NAME").is_some()
    })
    .await;
    let roles = Roles {
        main_camera: Some("Cam".into()),
        focuser: Some("Focuser".into()),
        filter_wheel: Some("Wheel".into()),
        ..Default::default()
    };
    let focus = FocusSettings {
        exposure: 0.01,
        backlash: 50,
        ..Default::default()
    };
    let imaging = ImagingSettings {
        output_dir: dir.join("out").display().to_string(),
        file_template: "{type}_{filter}_{seq:03}".into(),
    };
    ui.send(ToEngine::SetRoles(roles.clone())).await;
    ui.send(ToEngine::SetFocusSettings(focus.clone())).await;
    ui.send(ToEngine::SetImagingSettings(imaging.clone())).await;
    ui.until("settings", |u| u.state.roles == roles && u.state.focus_settings == focus && u.state.imaging == imaging)
        .await;
    (ui, engine, dir)
}

fn focuser_at(ui: &TestUi) -> f64 {
    ui.state.cache.property("Focuser", "ABS_FOCUS_POSITION").unwrap().number("FOCUS_ABSOLUTE_POSITION").unwrap()
}

fn log_has(ui: &TestUi, text: &str) -> bool {
    ui.state.log.iter().any(|l| l.text.contains(text))
}

#[tokio::test]
async fn autofocus_finds_best_focus() {
    let (mut ui, engine, dir) = setup("af").await;
    assert_eq!(focuser_at(&ui), FOCUS_START);
    // Frames carry star measurements: defocused at the start.
    ui.send(ToEngine::Autofocus).await;
    ui.until("autofocus done", |u| !u.state.autofocus.running && (log_has(u, "Focus at") || log_has(u, "failed"))).await;
    assert!(log_has(&ui, "Focus at"), "{:#?}", ui.state.log);
    // Started 400 steps inside focus: the range had to be extended.
    assert!(log_has(&ui, "extending around"));
    let best = ui.state.autofocus.best.unwrap();
    eprintln!("autofocus best {best:.1} (true {FOCUS_BEST}), {} samples", ui.state.autofocus.points.len());
    assert!((best - FOCUS_BEST).abs() < 15.0, "best {best}");
    ui.until("focuser at best", |u| (focuser_at(u) - best.round()).abs() < 0.5).await;
    assert!(!ui.state.autofocus.curve.is_empty());
    // HFR at best is close to the in-focus star size (sigma 1.2 → HFR ≈ 1.4).
    let min_hfr = ui.state.autofocus.points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    assert!(min_hfr < 1.8, "min HFR {min_hfr}");
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn sequence_focuses_at_start_and_on_filter_change() {
    let (mut ui, engine, dir) = setup("afseq").await;
    let light = |slot| SequenceStep {
        filter_slot: Some(slot),
        exposure: Exposure::Fixed { seconds: 0.01 },
        count: 1,
        ..Default::default()
    };
    let seq = Sequence {
        name: "AF".into(),
        steps: vec![light(1), light(2)],
        dither_every: 0,
        meridian_flip: false,
        autofocus: AutofocusTriggers {
            on_start: true,
            on_filter_change: true,
            ..Default::default()
        },
        ..Default::default()
    };
    ui.send(ToEngine::StartSequence(seq)).await;
    ui.until("done", |u| u.state.sequence.is_none() && log_has(u, "AF complete")).await;
    let log: Vec<&str> = ui.state.log.iter().map(|l| l.text.as_str()).collect();
    let all = |t: &str| log.iter().enumerate().filter(|(_, l)| l.contains(t)).map(|(i, _)| i).collect::<Vec<_>>();
    let (af, saved) = (all("Autofocus ("), all("Saved"));
    assert_eq!(af.len(), 2, "{log:#?}");
    assert!(log[af[0]].contains("start") && log[af[1]].contains("filter change"));
    assert!(af[0] < saved[0] && saved[0] < af[1] && af[1] < saved[1], "{log:#?}");
    assert!((focuser_at(&ui) - FOCUS_BEST).abs() < 15.0);
    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
