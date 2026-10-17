//! End-to-end capture: fake INDI camera/mount/wheel → engine → WebSocket UI → FITS on disk.

use std::time::Duration;

use base64::Engine as _;
use engine::EngineConfig;
use futures_util::{SinkExt, StreamExt};
use imaging::fits::{Card, Fits, Header};
use protocol::{
    CaptureRequest, CaptureState, FrameType, ImagingSettings, IndiStatus, Roles, ToEngine, ToUi,
    UiState, preview_frame,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

const W: usize = 64;
const H: usize = 48;
const ZEROS: usize = 7;
const SATURATED: usize = 5;

/// Background ~1000 ADU with mild noise, a few black and saturated pixels.
fn test_frame() -> Vec<u8> {
    let pixels: Vec<u16> = (0..W * H)
        .map(|i| match i {
            i if i < ZEROS => 0,
            i if i >= W * H - SATURATED => 65535,
            i => 1000 + ((i * 7919) % 41) as u16,
        })
        .collect();
    let mut h = Header::default();
    for c in [
        Card::new("SIMPLE", true, ""),
        Card::new("BITPIX", 16i64, ""),
        Card::new("NAXIS", 2i64, ""),
        Card::new("NAXIS1", W as i64, ""),
        Card::new("NAXIS2", H as i64, ""),
        Card::new("BZERO", 32768i64, ""),
        Card::new("BSCALE", 1i64, ""),
        Card::new("CCD-TEMP", -10.0, "from driver"),
        Card::new("EXPTIME", 1.0, ""),
    ] {
        h.set(c);
    }
    let data = pixels
        .iter()
        .flat_map(|&p| ((p as i32 - 32768) as i16).to_be_bytes())
        .collect();
    Fits { header: h, data }.to_bytes()
}

const DEFS: &str = r#"
<defSwitchVector device="Cam" name="CONNECTION" group="Main" state="Ok" perm="rw" rule="OneOfMany"><defSwitch name="CONNECT">On</defSwitch><defSwitch name="DISCONNECT">Off</defSwitch></defSwitchVector>
<defTextVector device="Cam" name="DRIVER_INFO" group="Info" perm="ro"><defText name="DRIVER_INTERFACE">2</defText></defTextVector>
<defNumberVector device="Cam" name="CCD_EXPOSURE" group="Main" state="Idle" perm="rw"><defNumber name="CCD_EXPOSURE_VALUE" format="%.2f" min="0" max="3600" step="1">1</defNumber></defNumberVector>
<defSwitchVector device="Cam" name="CCD_FRAME_TYPE" group="Main" state="Idle" perm="rw" rule="OneOfMany"><defSwitch name="FRAME_LIGHT">On</defSwitch><defSwitch name="FRAME_DARK">Off</defSwitch></defSwitchVector>
<defNumberVector device="Cam" name="CCD_GAIN" group="Main" state="Idle" perm="rw"><defNumber name="GAIN" format="%.0f" min="0" max="500" step="1">0</defNumber></defNumberVector>
<defNumberVector device="Cam" name="CCD_TEMPERATURE" group="Main" state="Ok" perm="rw"><defNumber name="CCD_TEMPERATURE_VALUE" format="%.2f" min="-50" max="50" step="1">-10</defNumber></defNumberVector>
<defNumberVector device="Cam" name="CCD_INFO" group="Info" state="Ok" perm="ro"><defNumber name="CCD_BITSPERPIXEL" format="%.0f" min="8" max="16" step="1">16</defNumber></defNumberVector>
<defBLOBVector device="Cam" name="CCD1" group="Image" state="Idle" perm="ro"><defBLOB name="CCD1"/></defBLOBVector>
<defSwitchVector device="Scope" name="CONNECTION" group="Main" state="Ok" perm="rw" rule="OneOfMany"><defSwitch name="CONNECT">On</defSwitch><defSwitch name="DISCONNECT">Off</defSwitch></defSwitchVector>
<defNumberVector device="Scope" name="EQUATORIAL_EOD_COORD" group="Main" state="Ok" perm="rw"><defNumber name="RA" format="%010.6m" min="0" max="24" step="0">5.59</defNumber><defNumber name="DEC" format="%010.6m" min="-90" max="90" step="0">-5.39</defNumber></defNumberVector>
<defNumberVector device="Scope" name="GEOGRAPHIC_COORD" group="Site" state="Ok" perm="rw"><defNumber name="LAT" format="%g" min="-90" max="90" step="0">46.05</defNumber><defNumber name="LONG" format="%g" min="0" max="360" step="0">14.5</defNumber><defNumber name="ELEV" format="%g" min="-200" max="10000" step="0">300</defNumber></defNumberVector>
<defSwitchVector device="Scope" name="TELESCOPE_PIER_SIDE" group="Main" state="Ok" perm="ro" rule="AtMostOne"><defSwitch name="PIER_WEST">On</defSwitch><defSwitch name="PIER_EAST">Off</defSwitch></defSwitchVector>
<defSwitchVector device="Wheel" name="CONNECTION" group="Main" state="Ok" perm="rw" rule="OneOfMany"><defSwitch name="CONNECT">On</defSwitch><defSwitch name="DISCONNECT">Off</defSwitch></defSwitchVector>
<defNumberVector device="Wheel" name="FILTER_SLOT" group="Main" state="Ok" perm="rw"><defNumber name="FILTER_SLOT_VALUE" format="%.0f" min="1" max="2" step="1">1</defNumber></defNumberVector>
<defTextVector device="Wheel" name="FILTER_NAME" group="Main" state="Ok" perm="rw"><defText name="FILTER_SLOT_NAME_1">L</defText><defText name="FILTER_SLOT_NAME_2">Ha</defText></defTextVector>
"#;

/// Fake indiserver. Returns everything the client sent.
async fn fake_indi(listener: TcpListener) -> String {
    let (mut sock, _) = listener.accept().await.unwrap();
    let (mut seen, mut pending) = (String::new(), String::new());
    let mut buf = vec![0u8; 65536];
    let mut defined = false;
    loop {
        let n = match sock.read(&mut buf).await {
            Ok(0) | Err(_) => return seen,
            Ok(n) => n,
        };
        let text = std::str::from_utf8(&buf[..n]).unwrap();
        seen.push_str(text);
        pending.push_str(text);
        if !defined && pending.contains("<getProperties") {
            defined = true;
            sock.write_all(DEFS.as_bytes()).await.unwrap();
        }
        const END: &str = "</newNumberVector>";
        while let Some(i) = pending.find(END) {
            let chunk: String = pending.drain(..i + END.len()).collect();
            let msg = &chunk[chunk.rfind("<newNumberVector").unwrap()..];
            if msg.contains(r#"name="FILTER_SLOT""#) {
                sock.write_all(br#"<setNumberVector device="Wheel" name="FILTER_SLOT" state="Busy"><oneNumber name="FILTER_SLOT_VALUE">1</oneNumber></setNumberVector>"#).await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
                sock.write_all(br#"<setNumberVector device="Wheel" name="FILTER_SLOT" state="Ok"><oneNumber name="FILTER_SLOT_VALUE">2</oneNumber></setNumberVector>"#).await.unwrap();
            } else if msg.contains(r#"name="CCD_EXPOSURE""#) {
                sock.write_all(br#"<setNumberVector device="Cam" name="CCD_EXPOSURE" state="Busy"><oneNumber name="CCD_EXPOSURE_VALUE">1</oneNumber></setNumberVector>"#).await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
                sock.write_all(br#"<setNumberVector device="Cam" name="CCD_EXPOSURE" state="Ok"><oneNumber name="CCD_EXPOSURE_VALUE">0</oneNumber></setNumberVector>"#).await.unwrap();
                let frame = test_frame();
                let b64 = base64::engine::general_purpose::STANDARD.encode(&frame);
                let blob = format!(
                    r#"<setBLOBVector device="Cam" name="CCD1" state="Ok"><oneBLOB name="CCD1" size="{}" format=".fits">{b64}</oneBLOB></setBLOBVector>"#,
                    frame.len()
                );
                sock.write_all(blob.as_bytes()).await.unwrap();
            }
        }
    }
}

struct TestUi {
    ws: Ws,
    state: UiState,
    previews: Vec<(u64, Vec<u8>)>,
    capture_states: Vec<CaptureState>,
}

impl TestUi {
    async fn connect(url: &str) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        TestUi {
            ws,
            state: UiState::default(),
            previews: Vec::new(),
            capture_states: Vec::new(),
        }
    }

    async fn until(&mut self, what: &str, pred: impl Fn(&TestUi) -> bool) {
        while !pred(self) {
            let msg = timeout(Duration::from_secs(10), self.ws.next())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}; log: {:#?}", self.state.log))
                .expect("socket closed")
                .unwrap();
            match msg {
                Message::Text(t) => {
                    let msg = ToUi::from_json(t.as_str()).unwrap();
                    if let ToUi::Capture { state, .. } = &msg {
                        self.capture_states.push(state.clone());
                    }
                    self.state.apply(msg);
                }
                Message::Binary(b) => {
                    let (id, jpeg) = preview_frame::decode(&b).expect("preview frame");
                    self.previews.push((id, jpeg.to_vec()));
                }
                _ => {}
            }
        }
    }

    async fn send(&mut self, cmd: ToEngine) {
        self.ws.send(Message::Text(cmd.to_json().into())).await.unwrap();
    }
}

#[tokio::test]
async fn capture_saves_fits_with_metadata() {
    let indi = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let indi_port = indi.local_addr().unwrap().port();
    let server = tokio::spawn(fake_indi(indi));

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
    assert!((1000.0..1041.0).contains(&stats.median));
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
    assert_eq!(saved.data, Fits::parse(&test_frame()).unwrap().data);

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
