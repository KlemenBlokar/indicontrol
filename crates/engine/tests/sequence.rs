//! End-to-end sequences: temperature wait, darks, auto-exposed flats, dark flats.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use engine::EngineConfig;
use imaging::fits::Fits;
use protocol::sequence::presets;
use protocol::{
    DocId, Exposure, FrameType, ImagingSettings, IndiStatus, Roles, RunState, Sequence, SequenceDraft,
    SequenceStep, ToEngine,
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
        ..Default::default()
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

    // Covered and flat-panel frames do not share a sequence.
    let mut mixed = presets::darks(&[0.5], 2, Some(-10.0));
    mixed.extend(presets::flats(&[Some(2)], 2));
    ui.send(ToEngine::StartSequence(Sequence { steps: mixed, ..Default::default() })).await;
    ui.until("mixed refused", |u| u.state.log.iter().any(|l| l.text.contains("separate sequences"))).await;

    // Flats first: the dark flats take their exposure.
    run(&mut ui, "Darks", presets::darks(&[0.5], 2, Some(-10.0))).await;
    run(&mut ui, "Flats", presets::flats(&[Some(2)], 2)).await;
    run(&mut ui, "Dark flats", presets::dark_flats(&[Some(2)], 1)).await;

    let files: Vec<PathBuf> =
        fits_files(&dir.join("out")).into_iter().filter(|p| !p.starts_with(dir.join("out/masters"))).collect();
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

    // A master per step.
    let kinds: Vec<_> = ui.state.masters.masters.iter().map(|m| (m.kind, m.filter.clone(), m.frames)).collect();
    use protocol::MasterKind::*;
    for want in [(Dark, Some("L".to_string()), Some(2)), (Flat, Some("Ha".to_string()), Some(2)), (Dark, Some("Ha".to_string()), Some(1))] {
        assert!(kinds.contains(&want), "{want:?} in {kinds:?}");
    }

    // The flat exposure is remembered across restarts.
    let saved = engine::PersistedState::load(&dir.join("engine.json"));
    assert_eq!(saved.flat_exposures.len(), 1);
    assert_eq!(saved.flat_exposures[0].filter.as_deref(), Some("Ha"));
    assert_eq!(saved.flat_exposures[0].exposure, flat_exposure);

    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

/// Run `steps` as sequence `name` and wait for it and its masters.
async fn run(ui: &mut TestUi, name: &str, steps: Vec<SequenceStep>) {
    let masters = ui.state.masters.masters.len();
    let seq = Sequence { name: name.into(), steps, ..Default::default() };
    ui.send(ToEngine::StartSequence(seq)).await;
    let complete = format!("{name} complete");
    ui.until(&complete, |u| u.state.sequence.is_none() && u.state.log.iter().any(|l| l.text.starts_with(&complete)))
        .await;
    ui.until("its master", |u| u.state.masters.creating.is_none() && u.state.masters.masters.len() == masters + 1)
        .await;
}

#[tokio::test]
async fn interleaved_lights_pause_resume_and_library() {
    let Setup { mut ui, engine, dir } = setup("lights", -10.0).await;

    let steps = vec![
        SequenceStep {
            filter_slot: Some(2),
            exposure: Exposure::Fixed { seconds: 0.1 },
            count: 2,
            ..Default::default()
        },
        SequenceStep {
            filter_slot: Some(1),
            exposure: Exposure::Fixed { seconds: 0.2 },
            count: 2,
            ..Default::default()
        },
    ];
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

fn draft(ui: &TestUi) -> Sequence {
    let value = ui.state.docs.get(&DocId::Sequence).cloned().unwrap_or_default();
    serde_json::from_value::<SequenceDraft>(value).unwrap_or_default().sequence
}

fn done(ui: &TestUi) -> Vec<u32> {
    draft(ui).steps.iter().map(|s| s.done).collect()
}

#[tokio::test]
async fn progress_survives_abort_and_restart() {
    let Setup { mut ui, engine, dir } = setup("progress", -10.0).await;
    let step = |slot, count| SequenceStep {
        filter_slot: Some(slot),
        exposure: Exposure::Fixed { seconds: 0.1 },
        count,
        ..Default::default()
    };
    let seq = Sequence {
        steps: vec![step(1, 3), step(2, 2)],
        dither_every: 0,
        meridian_flip: false,
        ..Default::default()
    };
    let edit = ToEngine::EditDoc {
        doc: DocId::Sequence,
        seq: 1,
        ops: vec![("/sequence".into(), serde_json::to_value(&seq).unwrap())],
    };
    ui.send(edit).await;
    ui.until("ids", |u| {
        let d = draft(u);
        d.steps.len() == 2 && d.steps.iter().all(|s| s.id != 0) && d.steps[0].id != d.steps[1].id
    })
    .await;

    // Run part of it: the engine records each frame in the draft.
    ui.send(ToEngine::StartSequence(draft(&ui))).await;
    ui.until("two frames", |u| done(u)[0] >= 2).await;
    ui.send(ToEngine::AbortSequence).await;
    ui.until("aborted", |u| u.state.sequence.is_none()).await;
    let after_abort = done(&ui);
    assert!(after_abort[0] >= 2 && after_abort.iter().sum::<u32>() < 5, "{after_abort:?}");

    // Restart on the same state file once the draft is saved.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    engine.shutdown();
    drop(ui);
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = indi.local_addr().unwrap().port();
    tokio::spawn(fake_indi(indi, -10.0));
    let engine = engine::start(EngineConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        web_root: None,
        state_file: Some(dir.join("engine.json")),
        indi: Some(("127.0.0.1".into(), port)),
    })
    .await
    .unwrap();
    let mut ui = TestUi::connect(&engine.ws_url()).await;
    ui.until("progress restored", |u| {
        u.state.indi_status == IndiStatus::Connected && done(u) == after_abort
    })
    .await;

    // Starting again takes only what is missing.
    ui.send(ToEngine::StartSequence(draft(&ui))).await;
    let taken: u32 = after_abort.iter().sum();
    let continues = format!("continues: {taken} of 5 frames already taken");
    ui.until("continues", |u| u.state.log.iter().any(|l| l.text.contains(&continues))).await;
    ui.until("complete", |u| u.state.sequence.is_none() && done(u) == vec![3, 2]).await;
    assert_eq!(fits_files(&dir.join("out")).len(), 5, "no frame taken twice");

    ui.send(ToEngine::StartSequence(draft(&ui))).await;
    ui.until("nothing left", |u| {
        u.state.log.iter().any(|l| l.text.contains("every step is complete"))
    })
    .await;

    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_complete_dark_step_makes_a_master() {
    let Setup { mut ui, engine, dir } = setup("master", 0.0).await;
    let seq = Sequence {
        steps: vec![SequenceStep {
            frame_type: FrameType::Dark,
            exposure: Exposure::Fixed { seconds: 0.1 },
            count: 4,
            ..Default::default()
        }],
        ..Default::default()
    };
    let edit = ToEngine::EditDoc {
        doc: DocId::Sequence,
        seq: 1,
        ops: vec![("/sequence".into(), serde_json::to_value(&seq).unwrap())],
    };
    ui.send(edit).await;
    ui.until("id", |u| draft(u).steps.first().is_some_and(|s| s.id != 0)).await;

    // Interrupted: no master yet, but the frames taken are recorded.
    ui.send(ToEngine::StartSequence(draft(&ui))).await;
    ui.until("two frames", |u| done(u)[0] >= 2).await;
    ui.send(ToEngine::AbortSequence).await;
    ui.until("aborted", |u| u.state.sequence.is_none()).await;
    let taken = draft(&ui).steps[0].clone();
    assert_eq!(taken.frames.len() as u32, taken.done);
    assert!(ui.state.masters.masters.is_empty() && ui.state.masters.creating.is_none());

    // Continued: the step completes and all four frames are averaged.
    ui.send(ToEngine::StartSequence(draft(&ui))).await;
    ui.until("master in the folder", |u| {
        let m = &u.state.masters;
        m.creating.is_none() && m.masters.iter().any(|f| f.kind == protocol::MasterKind::Dark && f.frames == Some(4))
    })
    .await;
    let master = &ui.state.masters.masters[0];
    assert_eq!(master.exposure, Some(0.1));
    assert!(master.path.starts_with("master_dark_0.1s_"), "{}", master.path);
    // The raw frames stay where frames go, the master in the masters folder.
    assert_eq!(fits_files(&dir.join("out/Dark")).len(), 4);
    assert!(dir.join("out/masters").join(&master.path).is_file());
    assert_eq!(draft(&ui).steps[0].frames.len(), 4);

    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
