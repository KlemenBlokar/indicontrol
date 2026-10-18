//! Against real INDI simulators. Opt-in:
//!
//!   indiserver -p 7630 indi_simulator_telescope indi_simulator_ccd \
//!       indi_simulator_wheel indi_simulator_focus indi_simulator_guide
//!   INDI_SIM=127.0.0.1:7630 cargo test -p engine --test simulators -- --nocapture
//!
//! MULTI=0 guides on a single star (for comparison with multi-star guiding).

mod common;

use std::time::Duration;

use common::*;
use engine::EngineConfig;
use protocol::{CaptureRequest, CaptureState, ImagingSettings, IndiStatus, Roles, ToEngine};

pub const CAMERA: &str = "CCD Simulator";
pub const GUIDER: &str = "Guide Simulator";
pub const MOUNT: &str = "Telescope Simulator";
pub const WHEEL: &str = "Filter Simulator";
pub const FOCUSER: &str = "Focuser Simulator";

fn sim_addr() -> Option<(String, u16)> {
    let addr = std::env::var("INDI_SIM").ok()?;
    Some(protocol::parse_host_port(&addr, 7624))
}

async fn setup(name: &str) -> Option<(TestUi, engine::EngineHandle, std::path::PathBuf)> {
    let Some(addr) = sim_addr() else {
        eprintln!("INDI_SIM not set — skipping");
        return None;
    };
    let dir = std::env::temp_dir().join(format!("indicontrol-sim-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine = engine::start(EngineConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        web_root: None,
        state_file: Some(dir.join("engine.json")),
        indi: Some(addr),
    })
    .await
    .unwrap();
    let mut ui = TestUi::connect(&engine.ws_url()).await;
    let all = [CAMERA, GUIDER, MOUNT, WHEEL, FOCUSER];
    ui.until("devices defined", |u| {
        u.state.indi_status == IndiStatus::Connected && all.iter().all(|d| u.state.cache.property(d, "CONNECTION").is_some())
    })
    .await;
    for d in all {
        ui.send(ToEngine::ConnectDevice { device: d.into() }).await;
    }
    ui.until("devices connected", |u| all.iter().all(|d| u.state.cache.devices[*d].is_connected())).await;
    // Drivers define most properties only after connecting.
    ui.until("camera ready", |u| u.state.cache.property(CAMERA, "CCD_EXPOSURE").is_some()).await;
    let roles = Roles {
        main_camera: Some(CAMERA.into()),
        guide_camera: Some(GUIDER.into()),
        mount: Some(MOUNT.into()),
        filter_wheel: Some(WHEEL.into()),
        focuser: Some(FOCUSER.into()),
    };
    let imaging = ImagingSettings {
        output_dir: dir.join("out").display().to_string(),
        ..Default::default()
    };
    ui.send(ToEngine::SetRoles(roles.clone())).await;
    ui.send(ToEngine::SetImagingSettings(imaging.clone())).await;
    ui.until("settings", |u| u.state.roles == roles && u.state.imaging == imaging).await;
    Some((ui, engine, dir))
}

#[tokio::test]
async fn capture_from_ccd_simulator() {
    let Some((mut ui, engine, dir)) = setup("capture").await else {
        return;
    };
    ui.send(ToEngine::Capture(CaptureRequest {
        exposure: 1.0,
        filter_slot: Some(2),
        object: "Simulated".into(),
        ..Default::default()
    }))
    .await;
    ui.until("frame", |u| u.state.last_frame.is_some() || u.state.log.iter().any(|l| l.text.contains("failed"))).await;
    ui.until("idle", |u| u.state.capture == CaptureState::Idle).await;
    let f = ui.state.last_frame.clone().unwrap_or_else(|| panic!("{:#?}", ui.state.log));
    let a = &f.analysis;
    eprintln!("frame {}x{}, filter {:?}, adc_max {} step {}", f.width, f.height, f.filter, a.adc_max, a.step);
    for c in &a.channels {
        eprintln!("  {}: min {} max {} median {} sigma {:.1} zero {} sat {}", c.name, c.min, c.max, c.median, c.sigma(), c.zero, c.saturated);
    }
    eprintln!("stars: {:?}", f.stars);
    eprintln!("saved: {:?}", f.path);
    for (k, v) in &f.header {
        eprintln!("  {k:8} = {v}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    engine.shutdown();
    let _ = dir;
}

/// Put the mount somewhere useful: a real site, unparked, pointing at the Pleiades.
async fn point_at_pleiades(ui: &mut TestUi) -> (f64, f64) {
    ui.send(ToEngine::SetNumber {
        device: MOUNT.into(),
        name: "GEOGRAPHIC_COORD".into(),
        values: vec![("LAT".into(), 46.05), ("LONG".into(), 14.5), ("ELEV".into(), 300.0)],
    })
    .await;
    // INDI 2 keeps focal length on the cameras; the engine pushes these there.
    // Without it the simulator renders no stars.
    ui.send(ToEngine::SetOptics(protocol::Optics {
        focal_length: Some(500.0),
        aperture: Some(80.0),
        guide_focal_length: Some(240.0),
        guide_aperture: Some(50.0),
    }))
    .await;
    ui.until("optics on cameras", |u| {
        u.state.cache.property(CAMERA, "SCOPE_INFO").and_then(|p| p.number("FOCAL_LENGTH")) == Some(500.0)
            && u.state.cache.property(GUIDER, "SCOPE_INFO").and_then(|p| p.number("FOCAL_LENGTH")) == Some(240.0)
    })
    .await;
    if ui.state.mount.as_ref().and_then(|m| m.parked) == Some(true) {
        ui.send(ToEngine::MountUnpark).await;
        ui.until("unparked", |u| u.state.mount.as_ref().and_then(|m| m.parked) == Some(false)).await;
    }
    let target = (3.0 + 47.0 / 60.0, 24.0 + 7.0 / 60.0); // near Alcyone, J2000
    // From a previous run the mount may already be there: a goto would then
    // be a tiny slew that is easily missed (and would move the guide stars).
    let there = ui.state.mount.as_ref().is_some_and(|m| {
        !m.slewing && (m.ra_j2000 - target.0).abs() < 0.01 && (m.dec_j2000 - target.1).abs() < 0.1
    });
    if !there {
        ui.send(ToEngine::MountGoto { ra_hours: target.0, dec_deg: target.1 }).await;
        ui.until_for("slew started", 30, |u| u.state.mount.as_ref().is_some_and(|m| m.slewing)).await;
        ui.until_for("slew done", 120, |u| u.state.mount.as_ref().is_some_and(|m| !m.slewing)).await;
    }
    let m = ui.state.mount.clone().unwrap();
    eprintln!(
        "mount at {:.4}h {:.3}° (J2000), alt {:?}, pier {:?}",
        m.ra_j2000, m.dec_j2000, m.alt, m.pier_side
    );
    target
}

async fn capture(ui: &mut TestUi, exposure: f64) -> protocol::FrameInfo {
    let before = ui.state.last_frame.as_ref().map(|f| f.id);
    ui.send(ToEngine::Capture(CaptureRequest {
        exposure,
        object: "Pleiades".into(),
        save: false,
        ..Default::default()
    }))
    .await;
    ui.until("frame", |u| {
        u.state.last_frame.as_ref().map(|f| f.id) != before || u.state.log.iter().any(|l| l.text.contains("failed"))
    })
    .await;
    ui.until("idle", |u| u.state.capture == CaptureState::Idle).await;
    ui.state.last_frame.clone().unwrap()
}

#[tokio::test]
async fn stars_and_astap_on_simulator() {
    let Some((mut ui, engine, _dir)) = setup("astap").await else {
        return;
    };
    let target = point_at_pleiades(&mut ui).await;
    let f = capture(&mut ui, 2.0).await;
    let c = &f.analysis.channels[0];
    eprintln!(
        "frame {}x{}: median {} sigma {:.1} max {} sat {} zero {}; stars {:?}",
        f.width, f.height, c.median, c.sigma(), c.max, c.saturated, c.zero, f.stars
    );
    assert!(f.stars.is_some_and(|s| s.count >= 10), "no stars — is GSC available to the simulator?");

    let astap = std::env::var("ASTAP").unwrap_or_else(|_| {
        format!("{}/.local/opt/astap/astap_cli", std::env::var("HOME").unwrap())
    });
    let mut solver = ui.state.solver_settings.clone();
    solver.astap_path = astap;
    solver.exposure = 2.0;
    ui.send(ToEngine::SetSolverSettings(solver.clone())).await;
    ui.until("solver settings", |u| u.state.solver_settings == solver).await;
    ui.send(ToEngine::SolveAndSync).await;
    ui.until("solved", |u| {
        !u.state.solve.busy && u.state.log.iter().any(|l| l.text.contains("Mount synced") || l.text.contains("failed"))
    })
    .await;
    for l in ui.state.log.iter().filter(|l| l.text.contains("Solved") || l.text.contains("solv")) {
        eprintln!("log: {}", l.text);
    }
    let r = ui.state.solve.last.clone().unwrap_or_else(|| panic!("{:#?}", ui.state.log));
    eprintln!("solution: {r:#?}");
    let err = engine::solver::separation_arcmin(target.0, target.1, r.ra_hours, r.dec_deg);
    eprintln!("solution is {err:.2}' from the goto target");
    assert!(err < 10.0);
    engine.shutdown();
}

#[tokio::test]
async fn guiding_on_simulator() {
    let Some((mut ui, engine, _dir)) = setup("guide").await else {
        return;
    };
    point_at_pleiades(&mut ui).await;
    let mut g = ui.state.guider_settings.clone();
    g.exposure = 1.0;
    g.calibration.step_ms = 2000.0;
    g.calibration.distance_px = 15.0;
    g.settle_time = 3.0;
    g.multi_star = std::env::var("MULTI").map_or(true, |v| v != "0");
    ui.send(ToEngine::SetGuiderSettings(g.clone())).await;
    ui.until("guider settings", |u| u.state.guider_settings == g).await;
    ui.send(ToEngine::GuiderGuide).await;
    ui.until_for("calibrated", 300, |u| {
        u.state.guider.calibration.is_some() || u.state.log.iter().any(|l| l.text.contains("failed"))
    })
    .await;
    for l in ui.state.log.iter().filter(|l| l.text.contains("Guide star") || l.text.contains("Calibrat")) {
        eprintln!("log: {}", l.text);
    }
    let cal = ui.state.guider.calibration.clone().unwrap_or_else(|| panic!("{:#?}", ui.state.log));
    eprintln!("pixel scale {:?}\"/px", ui.state.guider.pixel_scale);
    ui.until_for("20 guide steps", 120, |u| u.state.guide_history.len() >= 20).await;
    let recent: Vec<_> = ui.state.guide_history.iter().rev().take(15).collect();
    let rms = (recent.iter().map(|s| s.ra_px.powi(2) + s.dec_px.powi(2)).sum::<f64>() / recent.len() as f64).sqrt();
    eprintln!("calibration {cal:?}\nguide RMS {rms:.3} px over {} steps", recent.len());
    let g = &ui.state.guider;
    eprintln!(
        "{} of {} tracked stars used, field rotation {:?}°",
        g.stars_used,
        g.guide_stars.len(),
        g.field_rotation
    );
    assert!(cal.orthogonality_error() < 10.0);
    assert!(rms < 1.0);
    ui.send(ToEngine::GuiderStop).await;
    engine.shutdown();
}

#[tokio::test]
async fn autofocus_on_simulator() {
    let Some((mut ui, engine, _dir)) = setup("focus").await else {
        return;
    };
    point_at_pleiades(&mut ui).await;
    let mut f = ui.state.focus_settings.clone();
    f.exposure = 1.0;
    f.step = std::env::var("AF_STEP").ok().and_then(|s| s.parse().ok()).unwrap_or(2500);
    ui.send(ToEngine::SetFocusSettings(f.clone())).await;
    ui.until("focus settings", |u| u.state.focus_settings == f).await;
    ui.send(ToEngine::Autofocus).await;
    ui.until_for("autofocus", 300, |u| {
        !u.state.autofocus.running && u.state.log.iter().any(|l| l.text.contains("Focus at") || l.text.contains("Autofocus failed"))
    })
    .await;
    eprintln!("points: {:?}", ui.state.autofocus.points);
    for l in ui.state.log.iter().filter(|l| l.text.contains("ocus")) {
        eprintln!("log: {}", l.text);
    }
    let sim_best = ui.state.cache.property(CAMERA, "SIM_FOCUSING").and_then(|p| p.number("SIM_FOCUS_POSITION"));
    eprintln!("simulator's best focus: {sim_best:?}, found: {:?}", ui.state.autofocus.best);
    assert!(ui.state.log.iter().any(|l| l.text.contains("Focus at")));
    // The simulator's stars stop shrinking below ~1 px HFR, so the bottom of
    // its V-curve is flat: allow 1.5 steps.
    let (best, sim) = (ui.state.autofocus.best.unwrap(), sim_best.unwrap());
    assert!((best - sim).abs() < 1.5 * f.step as f64, "found {best}, simulator {sim}");
    engine.shutdown();
}
