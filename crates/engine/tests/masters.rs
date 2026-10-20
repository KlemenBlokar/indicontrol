//! The masters folder as seen by a UI.

mod common;

use common::*;
use engine::EngineConfig;
use imaging::{Card, Fits, Header};
use protocol::{ImagingSettings, MasterKind, ToEngine};

fn write_master(path: &std::path::Path, kind: &str, exposure: f64) {
    let mut h = Header::default();
    for c in [
        Card::new("SIMPLE", true, ""),
        Card::new("BITPIX", -32i64, ""),
        Card::new("NAXIS", 2i64, ""),
        Card::new("NAXIS1", 4i64, ""),
        Card::new("NAXIS2", 2i64, ""),
        Card::new("IMAGETYP", kind, ""),
        Card::new("EXPTIME", exposure, ""),
    ] {
        h.set(c);
    }
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, Fits { header: h, data: vec![0; 32] }.to_bytes()).unwrap();
}

#[tokio::test]
async fn masters_folder_is_reported_and_rescanned() {
    let dir = std::env::temp_dir().join(format!("indicontrol-mastersdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine = engine::start(EngineConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        web_root: None,
        state_file: Some(dir.join("engine.json")),
        indi: None,
    })
    .await
    .unwrap();
    let mut ui = TestUi::connect(&engine.ws_url()).await;
    // Default: "masters" in the output folder, which does not exist yet.
    let out = dir.join("out");
    ui.send(ToEngine::SetImagingSettings(ImagingSettings {
        output_dir: out.display().to_string(),
        ..Default::default()
    }))
    .await;
    let folder = out.join("masters").display().to_string();
    ui.until("missing folder", |u| u.state.masters.folder == folder && u.state.masters.error.is_some())
        .await;

    write_master(&out.join("masters/dark_120.fits"), "Master Dark", 120.0);
    ui.send(ToEngine::RescanMasters).await;
    ui.until("one dark", |u| {
        let m = &u.state.masters;
        m.error.is_none() && m.masters.len() == 1 && m.masters[0].kind == MasterKind::Dark
    })
    .await;

    // Its own folder, set explicitly.
    let own = dir.join("calib");
    write_master(&own.join("flat.fits"), "Master Flat", 2.0);
    write_master(&own.join("bias.fits"), "Master Bias", 0.0);
    ui.send(ToEngine::SetImagingSettings(ImagingSettings {
        output_dir: out.display().to_string(),
        masters_dir: own.display().to_string(),
        ..Default::default()
    }))
    .await;
    ui.until("flat and bias", |u| {
        let mut kinds: Vec<_> = u.state.masters.masters.iter().map(|m| m.kind).collect();
        kinds.sort_by_key(|k| k.label());
        kinds == vec![MasterKind::Bias, MasterKind::Flat]
    })
    .await;

    engine.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}
