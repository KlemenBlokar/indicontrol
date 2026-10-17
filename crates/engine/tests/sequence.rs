//! End-to-end sequences: temperature wait, darks, auto-exposed flats, dark flats.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use engine::EngineConfig;
use imaging::fits::Fits;
use protocol::sequence::presets;
use protocol::{
    Exposure, FrameType, ImagingSettings, IndiStatus, Roles, RunState, Sequence, SequenceStep,
    ToEngine,
};
use tokio::net::TcpListener;

struct Setup {
    ui: TestUi,
    engine: engine::EngineHandle,
    dir: PathBuf,
}

async fn setup(name: &str, initial_temp: f64) -> Setup {
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = indi.local_addr().unwrap().port();
    tokio::spawn(fake_indi(indi, initial_temp));
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
        file_template: "{type}/{filter}/{exposure}s_{seq:03}".into(),
    };
    ui.send(ToEngine::SetImagingSettings(settings.clone())).await;
    ui.until("settings", |u| u.state.roles == roles && u.state.imaging == settings).await;
    Setup { ui, engine, dir }
}

fn fits_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "fits") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn calibration_sequence_with_auto_flats() {
    let Setup { mut ui, engine, dir } = setup("calib", 0.0).await;

    let mut steps = presets::darks(&[0.5], 2, Some(-10.0));
    steps.extend(presets::flats(&[Some(2)], 2));
    steps.extend(presets::dark_flats(&[Some(2)], 1));
    let seq = Sequence {
        name: "Calibration".into(),
        steps,
        ..Default::default()
    };
    ui.send(ToEngine::StartSequence(seq)).await;
    ui.until("sequence start", |u| u.state.sequence.is_some()).await;
    ui.until("sequence end", |u| {
        u.state.sequence.is_none() && u.state.log.iter().any(|l| l.text.contains("complete"))
    })
    .await;
    assert!(
        ui.state.log.iter().any(|l| l.text == "Calibration complete: 5 frames"),
        "{:#?}",
        ui.state.log
    );

    let files = fits_files(&dir.join("out"));
    let rel: Vec<String> = files
        .iter()
        .map(|p| p.strip_prefix(dir.join("out")).unwrap().display().to_string())
        .collect();
    assert_eq!(rel.len(), 5, "{rel:?}");
    // Darks without a filter step record the filter left in the wheel (L).
    assert_eq!(rel.iter().filter(|r| r.starts_with("Dark/L/0.5s_")).count(), 2, "{rel:?}");
    let flats: Vec<_> = files.iter().filter(|p| p.to_string_lossy().contains("/Flat/Ha/")).collect();
    assert_eq!(flats.len(), 2, "{rel:?}");

    // Flats were exposed to ~45 % of full scale.
    let flat = Fits::parse(&std::fs::read(flats[0]).unwrap()).unwrap();
    assert_eq!(flat.header.get_str("IMAGETYP"), Some("Flat Frame"));
    let flat_exposure = flat.header.get_f64("EXPTIME").unwrap();
    let level = (1000.0 + RATE * flat_exposure) / 65535.0;
    assert!((level - 0.45).abs() <= 0.05, "flat level {level} at {flat_exposure}s");

    // The dark flat matches the flat exposure.
    let dark_flat = files
        .iter()
        .find(|p| p.to_string_lossy().contains("/Dark/Ha/"))
        .expect("dark flat");
    let df = Fits::parse(&std::fs::read(dark_flat).unwrap()).unwrap();
    assert_eq!(df.header.get_str("IMAGETYP"), Some("Dark Frame"));
    assert_eq!(df.header.get_f64("EXPTIME"), Some(flat_exposure));

    // The flat exposure is remembered across restarts.
    let saved = engine::PersistedState::load(&dir.join("engine.json"));
    assert_eq!(saved.flat_exposures.len(), 1);
    assert_eq!(saved.flat_exposures[0].filter.as_deref(), Some("Ha"));
    assert_eq!(saved.flat_exposures[0].exposure, flat_exposure);

    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn interleaved_lights_pause_resume_and_library() {
    let Setup { mut ui, engine, dir } = setup("lights", -10.0).await;

    let names = vec!["L".to_string(), "Ha".to_string()];
    let (mut steps, missing) = presets::filter_set(&names, &presets::SHO, 0.1, 2);
    assert_eq!(missing, vec!["SII", "OIII"]);
    steps.push(SequenceStep {
        filter_slot: Some(1),
        exposure: Exposure::Fixed { seconds: 0.2 },
        count: 2,
        ..Default::default()
    });
    let seq = Sequence {
        name: "Ha+L".into(),
        object: "M 42".into(),
        order: protocol::SequenceOrder::Interleaved,
        steps,
        dither_every: 0,
        meridian_flip: false,
        ..Default::default()
    };

    // Library round trip.
    ui.send(ToEngine::SaveSequence(seq.clone())).await;
    ui.until("library", |u| u.state.library == vec![seq.clone()]).await;

    ui.send(ToEngine::StartSequence(seq.clone())).await;
    ui.until("running", |u| u.state.sequence.is_some()).await;
    ui.send(ToEngine::PauseSequence).await;
    ui.until("paused", |u| {
        u.state.sequence.as_ref().is_some_and(|s| s.state == RunState::Paused)
    })
    .await;
    let done_at_pause = ui.state.sequence.as_ref().unwrap().frames_done();
    assert!(done_at_pause < 4);
    ui.send(ToEngine::ResumeSequence).await;
    ui.until("done", |u| u.state.sequence.is_none()).await;

    // Interleaved: Ha, L, Ha, L by capture time = file order within each folder.
    let files = fits_files(&dir.join("out"));
    assert_eq!(files.len(), 4, "{files:?}");
    let objects: Vec<_> = files
        .iter()
        .map(|p| {
            let h = Fits::parse(&std::fs::read(p).unwrap()).unwrap().header;
            (h.get_str("FILTER").unwrap().to_owned(), h.get_str("OBJECT").unwrap().to_owned())
        })
        .collect();
    assert_eq!(objects.iter().filter(|(f, _)| f == "Ha").count(), 2);
    assert!(objects.iter().all(|(_, o)| o == "M 42"));

    ui.send(ToEngine::DeleteSequence { name: "Ha+L".into() }).await;
    ui.until("library emptied", |u| u.state.library.is_empty()).await;

    // A step that needs a flat exposure nobody measured stops the sequence.
    let bad = Sequence {
        steps: vec![SequenceStep {
            frame_type: FrameType::Dark,
            filter_slot: Some(1),
            exposure: Exposure::MatchFlat,
            count: 1,
            ..Default::default()
        }],
        ..Default::default()
    };
    ui.send(ToEngine::StartSequence(bad)).await;
    ui.until("failure", |u| {
        u.state.log.iter().any(|l| l.text.contains("no flat exposure known for L"))
    })
    .await;
    ui.until("stopped", |u| u.state.sequence.is_none()).await;

    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
