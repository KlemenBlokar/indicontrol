//! Shared test fixtures: a scripted fake indiserver and a WebSocket test UI.
#![allow(dead_code)]

use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use imaging::fits::{Card, Fits, Header};
use protocol::{CaptureState, ToEngine, ToUi, UiState, preview_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

pub type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

pub const W: usize = 160;
pub const H: usize = 120;
pub const ZEROS: usize = 7;
pub const SATURATED: usize = 5;

/// ADU per second of exposure: the fake sky/flat panel brightness.
pub const RATE: f64 = 20_000.0;

/// Focuser position of best focus, and where the fake focuser starts.
pub const FOCUS_BEST: f64 = 5000.0;
pub const FOCUS_START: f64 = 4600.0;

/// Star size (Gaussian sigma, px) at a focuser position: a V-curve.
pub fn star_sigma(focus: f64) -> f64 {
    (1.2f64.powi(2) + ((focus - FOCUS_BEST) / 120.0).powi(2)).sqrt()
}

/// Background 1000 + RATE·exposure ADU with mild noise, 12 stars, a few black
/// and saturated pixels. Stars are sized for the focuser at [`FOCUS_START`].
pub fn test_frame(exposure: f64) -> Vec<u8> {
    test_frame_with(exposure, FOCUS_START, &[])
}

/// [`test_frame`] at a focuser position, with extra header cards.
pub fn test_frame_with(exposure: f64, focus: f64, extra: &[Card]) -> Vec<u8> {
    test_frame_at_gain(exposure, 1.0, focus, extra)
}

/// The signal a gain multiplies the light by: 0.1 dB per unit (ZWO-like).
pub fn gain_factor(gain: f64) -> f64 {
    10f64.powf(gain / 200.0)
}

/// [`test_frame_with`] with the light scaled by `factor` (the gain).
pub fn test_frame_at_gain(exposure: f64, factor: f64, focus: f64, extra: &[Card]) -> Vec<u8> {
    let level = (1000.0 + RATE * exposure * factor).min(60_000.0);
    let sigma = star_sigma(focus);
    // Constant flux: peak falls as stars spread out.
    let peak = 4000.0 * 1.44 / (sigma * sigma);
    let stars: Vec<(f64, f64)> = (0..12)
        .map(|i| (32.0 + (i % 4) as f64 * 32.0, 32.0 + (i / 4) as f64 * 28.0))
        .collect();
    let pixels: Vec<u16> = (0..W * H)
        .map(|i| match i {
            i if i < ZEROS => 0,
            i if i >= W * H - SATURATED => 65535,
            i => {
                let (x, y) = ((i % W) as f64, (i / W) as f64);
                let star: f64 = stars
                    .iter()
                    .map(|&(sx, sy)| peak * (-((x - sx).powi(2) + (y - sy).powi(2)) / (2.0 * sigma * sigma)).exp())
                    .sum();
                (level + ((i * 7919) % 41) as f64 + star).min(65000.0) as u16
            }
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
        Card::new("EXPTIME", exposure, ""),
    ] {
        h.set(c);
    }
    for c in extra {
        h.set(c.clone());
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
<defNumberVector device="Cam" name="CCD_TEMPERATURE" group="Main" state="Ok" perm="rw"><defNumber name="CCD_TEMPERATURE_VALUE" format="%.2f" min="-50" max="50" step="1">@TEMP@</defNumber></defNumberVector>
<defNumberVector device="Cam" name="CCD_INFO" group="Info" state="Ok" perm="ro"><defNumber name="CCD_BITSPERPIXEL" format="%.0f" min="8" max="16" step="1">16</defNumber><defNumber name="CCD_PIXEL_SIZE" format="%.2f" min="0" max="50" step="0">3.76</defNumber></defNumberVector>
<defBLOBVector device="Cam" name="CCD1" group="Image" state="Idle" perm="ro"><defBLOB name="CCD1"/></defBLOBVector>
<defSwitchVector device="Scope" name="CONNECTION" group="Main" state="Ok" perm="rw" rule="OneOfMany"><defSwitch name="CONNECT">On</defSwitch><defSwitch name="DISCONNECT">Off</defSwitch></defSwitchVector>
<defNumberVector device="Scope" name="EQUATORIAL_EOD_COORD" group="Main" state="Ok" perm="rw"><defNumber name="RA" format="%010.6m" min="0" max="24" step="0">@RA@</defNumber><defNumber name="DEC" format="%010.6m" min="-90" max="90" step="0">-5.39</defNumber></defNumberVector>
<defNumberVector device="Scope" name="GEOGRAPHIC_COORD" group="Site" state="Ok" perm="rw"><defNumber name="LAT" format="%g" min="-90" max="90" step="0">46.05</defNumber><defNumber name="LONG" format="%g" min="0" max="360" step="0">14.5</defNumber><defNumber name="ELEV" format="%g" min="-200" max="10000" step="0">300</defNumber></defNumberVector>
<defSwitchVector device="Scope" name="TELESCOPE_PIER_SIDE" group="Main" state="Ok" perm="ro" rule="AtMostOne"><defSwitch name="PIER_WEST">On</defSwitch><defSwitch name="PIER_EAST">Off</defSwitch></defSwitchVector>
<defSwitchVector device="Wheel" name="CONNECTION" group="Main" state="Ok" perm="rw" rule="OneOfMany"><defSwitch name="CONNECT">On</defSwitch><defSwitch name="DISCONNECT">Off</defSwitch></defSwitchVector>
<defNumberVector device="Wheel" name="FILTER_SLOT" group="Main" state="Ok" perm="rw"><defNumber name="FILTER_SLOT_VALUE" format="%.0f" min="1" max="2" step="1">1</defNumber></defNumberVector>
<defTextVector device="Wheel" name="FILTER_NAME" group="Main" state="Ok" perm="rw"><defText name="FILTER_SLOT_NAME_1">L</defText><defText name="FILTER_SLOT_NAME_2">Ha</defText></defTextVector>
<defSwitchVector device="GuideCam" name="CONNECTION" group="Main" state="Ok" perm="rw" rule="OneOfMany"><defSwitch name="CONNECT">On</defSwitch><defSwitch name="DISCONNECT">Off</defSwitch></defSwitchVector>
<defTextVector device="GuideCam" name="DRIVER_INFO" group="Info" perm="ro"><defText name="DRIVER_INTERFACE">6</defText></defTextVector>
<defNumberVector device="GuideCam" name="CCD_EXPOSURE" group="Main" state="Idle" perm="rw"><defNumber name="CCD_EXPOSURE_VALUE" format="%.2f" min="0" max="3600" step="1">1</defNumber></defNumberVector>
<defNumberVector device="GuideCam" name="CCD_INFO" group="Info" state="Ok" perm="ro"><defNumber name="CCD_PIXEL_SIZE" format="%.2f" min="0" max="50" step="0">3.75</defNumber></defNumberVector>
<defBLOBVector device="GuideCam" name="CCD1" group="Image" state="Idle" perm="ro"><defBLOB name="CCD1"/></defBLOBVector>
<defSwitchVector device="Focuser" name="CONNECTION" group="Main" state="Ok" perm="rw" rule="OneOfMany"><defSwitch name="CONNECT">On</defSwitch><defSwitch name="DISCONNECT">Off</defSwitch></defSwitchVector>
<defTextVector device="Focuser" name="DRIVER_INFO" group="Info" perm="ro"><defText name="DRIVER_INTERFACE">8</defText></defTextVector>
<defNumberVector device="Focuser" name="ABS_FOCUS_POSITION" group="Main" state="Ok" perm="rw"><defNumber name="FOCUS_ABSOLUTE_POSITION" format="%.0f" min="0" max="10000" step="1">@FOCUS@</defNumber></defNumberVector>
<defNumberVector device="Focuser" name="FOCUS_TEMPERATURE" group="Main" state="Ok" perm="ro"><defNumber name="TEMPERATURE" format="%.1f" min="-50" max="50" step="0">12.5</defNumber></defNumberVector>
<defNumberVector device="Scope" name="TELESCOPE_TIMED_GUIDE_NS" group="Guide" state="Idle" perm="rw"><defNumber name="TIMED_GUIDE_N" format="%.0f" min="0" max="60000" step="1">0</defNumber><defNumber name="TIMED_GUIDE_S" format="%.0f" min="0" max="60000" step="1">0</defNumber></defNumberVector>
<defNumberVector device="Scope" name="TELESCOPE_TIMED_GUIDE_WE" group="Guide" state="Idle" perm="rw"><defNumber name="TIMED_GUIDE_W" format="%.0f" min="0" max="60000" step="1">0</defNumber><defNumber name="TIMED_GUIDE_E" format="%.0f" min="0" max="60000" step="1">0</defNumber></defNumberVector>
<defNumberVector device="Scope" name="TELESCOPE_INFO" group="Info" state="Ok" perm="rw"><defNumber name="GUIDER_FOCAL_LENGTH" format="%g" min="0" max="10000" step="0">240</defNumber><defNumber name="TELESCOPE_FOCAL_LENGTH" format="%g" min="0" max="10000" step="0">500</defNumber></defNumberVector>
<defSwitchVector device="Scope" name="ON_COORD_SET" group="Main" state="Ok" perm="rw" rule="OneOfMany"><defSwitch name="TRACK">On</defSwitch><defSwitch name="SLEW">Off</defSwitch><defSwitch name="SYNC">Off</defSwitch></defSwitchVector>
"#;

/// Guide camera rotation relative to the mount axes, and pulse response.
pub const GUIDE_THETA_DEG: f64 = 30.0;
pub const GUIDE_RATE: f64 = 0.05; // px per ms of pulse
pub const GUIDE_DRIFT: f64 = 0.3; // px of RA drift per guide exposure
const GW: usize = 160;
const GH: usize = 120;

/// Initial pointing error of the fake mount (hours, degrees): ~18' in Dec.
pub const POINTING_ERROR: (f64, f64) = (0.01, 0.3);

/// Write fake `astap` and `solve-field` executables that "solve" frames by
/// reading the TRUERA/TRUEDEC cards the fake camera embeds.
pub fn fake_solvers(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let common = r#"
hdr=$(head -c 28800 "$f" | tr -c '[:print:]' ' ')
val() { echo "$hdr" | sed -n "s/.*$1 *= *\([-0-9.E+]*\).*/\1/p" | head -n 1; }
ra=$(val TRUERA); dec=$(val TRUEDEC)
"#;
    let astap = format!(
        r#"#!/bin/sh
f=""
while [ $# -gt 0 ]; do case "$1" in -f) f="$2"; shift;; esac; shift; done
{common}
printf 'PLTSOLVD=T
CRPIX1=80.5
CRPIX2=60.5
CRVAL1=%s
CRVAL2=%s
CD1_1=-0.00086
CD1_2=0
CD2_1=0
CD2_2=0.00086
' "$ra" "$dec" > "${{f%.*}}.ini"
"#
    );
    let anet = format!(
        r#"#!/bin/sh
f="$1"; shift; d=.
while [ $# -gt 0 ]; do case "$1" in --dir) d="$2"; shift;; esac; shift; done
{common}
card() {{ printf '%-80s' "$1"; }}
out="$d/$(basename "$f" .fits).wcs"
{{ card "SIMPLE  =                    T"; card "BITPIX  =                    8"; card "NAXIS   =                    0"
   card "CRVAL1  = $ra"; card "CRVAL2  = $dec"; card "CRPIX1  = 80.5"; card "CRPIX2  = 60.5"
   card "CD1_1   = -0.00086"; card "CD1_2   = 0.0"; card "CD2_1   = 0.0"; card "CD2_2   = 0.00086"; card "END"
   printf '%1920s' ''; }} > "$out"
"#
    );
    let write = |name: &str, body: &str| {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    };
    (write("astap", &astap), write("solve-field", &anet))
}

/// Site longitude of the fake mount (east positive), matching GEOGRAPHIC_COORD.
const SITE_LONG: f64 = 14.5;

/// Local sidereal time (hours) at the fake site, now.
pub fn lst_now() -> f64 {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let jd = imaging::astro::julian_day(unix);
    ((imaging::astro::gmst_deg(jd) + SITE_LONG) / 15.0).rem_euclid(24.0)
}

/// Simulated sky as seen by the guide camera.
struct GuideSky {
    x: f64,
    y: f64,
    frames: usize,
    /// After a meridian flip the camera is turned 180° relative to the sky.
    flipped: bool,
    /// The bright star at (x, y) is gone (a test sends SIM_HIDE_PRIMARY).
    primary_hidden: bool,
}

/// Fainter stars around the primary guide star: (dx, dy, peak).
const GUIDE_SECONDARIES: [(f64, f64, f64); 3] = [(-45.0, -25.0, 4000.0), (40.0, 30.0, 3000.0), (-30.0, 33.0, 2500.0)];

impl GuideSky {
    fn theta(&self) -> f64 {
        GUIDE_THETA_DEG + if self.flipped { 180.0 } else { 0.0 }
    }

    fn mv(&mut self, angle_deg: f64, px: f64) {
        let a = angle_deg.to_radians();
        self.x += a.cos() * px;
        self.y += a.sin() * px;
    }

    fn pulse(&mut self, n: f64, s: f64, w: f64, e: f64) {
        self.mv(self.theta(), GUIDE_RATE * (w - e));
        self.mv(self.theta() + 90.0, GUIDE_RATE * (n - s));
    }

    /// Render the next guide frame (drift is applied per exposure).
    fn frame(&mut self) -> Vec<u8> {
        self.frames += 1;
        self.mv(self.theta(), GUIDE_DRIFT);
        let pixels: Vec<u16> = (0..GW * GH)
            .map(|i| {
                let (x, y) = ((i % GW) as f64, (i / GW) as f64);
                let d2 = (x - self.x).powi(2) + (y - self.y).powi(2);
                let noise = ((i * 7919 + self.frames * 104_729) % 61) as f64;
                let star = |d2: f64, peak: f64| peak * (-d2 / (2.0 * 1.5 * 1.5)).exp();
                let mut v = 1000.0 + noise;
                if !self.primary_hidden {
                    v += star(d2, 6000.0);
                }
                for (dx, dy, peak) in GUIDE_SECONDARIES {
                    v += star((x - self.x - dx).powi(2) + (y - self.y - dy).powi(2), peak);
                }
                v.min(65535.0) as u16
            })
            .collect();
        let mut h = Header::default();
        for c in [
            Card::new("SIMPLE", true, ""),
            Card::new("BITPIX", 16i64, ""),
            Card::new("NAXIS", 2i64, ""),
            Card::new("NAXIS1", GW as i64, ""),
            Card::new("NAXIS2", GH as i64, ""),
            Card::new("BZERO", 32768i64, ""),
        ] {
            h.set(c);
        }
        let data = pixels
            .iter()
            .flat_map(|&p| ((p as i32 - 32768) as i16).to_be_bytes())
            .collect();
        Fits { header: h, data }.to_bytes()
    }
}

fn number_in(msg: &str, element: &str) -> f64 {
    let at = msg.find(&format!(r#"name="{element}">"#)).expect("element") + element.len() + 8;
    msg[at..msg[at..].find('<').unwrap() + at].trim().parse().unwrap()
}

/// Fake indiserver starting with the sensor at `initial_temp`, the mount two
/// hours east of the meridian. Returns everything the client sent.
pub async fn fake_indi(listener: TcpListener, initial_temp: f64) -> String {
    fake_indi_at(listener, initial_temp, -120.0).await
}

/// Like [`fake_indi`], with the mount `hour_angle_min` minutes from the
/// meridian (positive: past it) on the west side of the pier.
pub async fn fake_indi_at(listener: TcpListener, initial_temp: f64, hour_angle_min: f64) -> String {
    let (sock, _) = listener.accept().await.unwrap();
    serve(sock, initial_temp, hour_angle_min).await
}

/// Like [`fake_indi`], but accepts any number of (re)connections.
pub async fn fake_indi_multi(listener: TcpListener, initial_temp: f64) {
    loop {
        let (sock, _) = listener.accept().await.unwrap();
        tokio::spawn(serve(sock, initial_temp, -120.0));
    }
}

async fn serve(mut sock: TcpStream, initial_temp: f64, hour_angle_min: f64) -> String {
    let (mut seen, mut pending) = (String::new(), String::new());
    let mut buf = vec![0u8; 65536];
    let mut defined = false;
    let mut sky = GuideSky { x: 80.0, y: 60.0, frames: 0, flipped: false, primary_hidden: false };
    let ra0 = (lst_now() - hour_angle_min / 60.0).rem_euclid(24.0);
    // Mount model: reported position + pointing error = where it really points (JNow).
    let mut reported = (ra0, -5.39);
    let mut error = POINTING_ERROR;
    let mut sync_mode = false;
    // Darks see no light.
    let mut dark = false;
    let mut focus = FOCUS_START;
    let mut gain = 0.0;
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
            let defs = DEFS
                .replace("@TEMP@", &initial_temp.to_string())
                .replace("@RA@", &ra0.to_string())
                .replace("@FOCUS@", &focus.to_string());
            sock.write_all(defs.as_bytes()).await.unwrap();
        }
        // Handle complete messages in the order they were sent.
        const END: &str = "</newNumberVector>";
        const SW_END: &str = "</newSwitchVector>";
        loop {
            let next_sw = pending.find(SW_END).map(|i| i + SW_END.len());
            let next_num = pending.find(END).map(|i| i + END.len());
            let end = match (next_sw, next_num) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) | (None, Some(a)) => a,
                (None, None) => break,
            };
            let chunk: String = pending.drain(..end).collect();
            if chunk.ends_with(SW_END) {
                // Coordinate mode switches (sync vs. slew).
                if chunk.contains(r#"name="ON_COORD_SET""#) {
                    sync_mode = chunk.contains(r#"<oneSwitch name="SYNC">On"#);
                }
                if chunk.contains(r#"name="CCD_FRAME_TYPE""#) {
                    dark = chunk.contains(r#"<oneSwitch name="FRAME_DARK">On"#);
                }
                continue;
            }
            let msg = &chunk[chunk.rfind("<newNumberVector").unwrap()..];
            if msg.contains(r#"name="SIM_HIDE_PRIMARY""#) {
                sky.primary_hidden = true;
            } else if msg.contains(r#"name="CCD_GAIN""#) {
                gain = number_in(msg, "GAIN");
                let m = format!(r#"<setNumberVector device="Cam" name="CCD_GAIN" state="Ok"><oneNumber name="GAIN">{gain}</oneNumber></setNumberVector>"#);
                sock.write_all(m.as_bytes()).await.unwrap();
            } else if msg.contains(r#"name="FILTER_SLOT""#) {
                let slot = number_in(msg, "FILTER_SLOT_VALUE");
                sock.write_all(br#"<setNumberVector device="Wheel" name="FILTER_SLOT" state="Busy"><oneNumber name="FILTER_SLOT_VALUE">1</oneNumber></setNumberVector>"#).await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
                let done = format!(r#"<setNumberVector device="Wheel" name="FILTER_SLOT" state="Ok"><oneNumber name="FILTER_SLOT_VALUE">{slot}</oneNumber></setNumberVector>"#);
                sock.write_all(done.as_bytes()).await.unwrap();
            } else if msg.contains(r#"name="CCD_TEMPERATURE""#) {
                // Cool down in two steps.
                let target = number_in(msg, "CCD_TEMPERATURE_VALUE");
                for (t, state) in [((initial_temp + target) / 2.0, "Busy"), (target, "Ok")] {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    let m = format!(r#"<setNumberVector device="Cam" name="CCD_TEMPERATURE" state="{state}"><oneNumber name="CCD_TEMPERATURE_VALUE">{t}</oneNumber></setNumberVector>"#);
                    sock.write_all(m.as_bytes()).await.unwrap();
                }
            } else if msg.contains(r#"name="EQUATORIAL_EOD_COORD""#) && sync_mode {
                // Sync: the mount now believes it points at these coordinates.
                let (ra, dec) = (number_in(msg, "RA"), number_in(msg, "DEC"));
                let truth = (reported.0 + error.0, reported.1 + error.1);
                error = (truth.0 - ra, truth.1 - dec);
                reported = (ra, dec);
                let m = format!(r#"<setNumberVector device="Scope" name="EQUATORIAL_EOD_COORD" state="Ok"><oneNumber name="RA">{ra}</oneNumber><oneNumber name="DEC">{dec}</oneNumber></setNumberVector>"#);
                sock.write_all(m.as_bytes()).await.unwrap();
            } else if msg.contains(r#"name="EQUATORIAL_EOD_COORD""#) {
                // Goto: slew, then choose the pier side from the hour angle like a GEM.
                let (ra, dec) = (number_in(msg, "RA"), number_in(msg, "DEC"));
                reported = (ra, dec);
                sock.write_all(br#"<setNumberVector device="Scope" name="EQUATORIAL_EOD_COORD" state="Busy"><oneNumber name="RA">0</oneNumber><oneNumber name="DEC">0</oneNumber></setNumberVector>"#).await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
                let ha = (lst_now() - ra + 12.0).rem_euclid(24.0) - 12.0;
                let east = ha >= 0.0;
                sky.flipped = east;
                let (w, e) = if east { ("Off", "On") } else { ("On", "Off") };
                let m = format!(
                    r#"<setSwitchVector device="Scope" name="TELESCOPE_PIER_SIDE" state="Ok"><oneSwitch name="PIER_WEST">{w}</oneSwitch><oneSwitch name="PIER_EAST">{e}</oneSwitch></setSwitchVector><setNumberVector device="Scope" name="EQUATORIAL_EOD_COORD" state="Ok"><oneNumber name="RA">{ra}</oneNumber><oneNumber name="DEC">{dec}</oneNumber></setNumberVector>"#
                );
                sock.write_all(m.as_bytes()).await.unwrap();
            } else if msg.contains(r#"name="ABS_FOCUS_POSITION""#) {
                focus = number_in(msg, "FOCUS_ABSOLUTE_POSITION");
                sock.write_all(br#"<setNumberVector device="Focuser" name="ABS_FOCUS_POSITION" state="Busy"><oneNumber name="FOCUS_ABSOLUTE_POSITION">0</oneNumber></setNumberVector>"#).await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
                let m = format!(r#"<setNumberVector device="Focuser" name="ABS_FOCUS_POSITION" state="Ok"><oneNumber name="FOCUS_ABSOLUTE_POSITION">{focus}</oneNumber></setNumberVector>"#);
                sock.write_all(m.as_bytes()).await.unwrap();
            } else if msg.contains(r#"name="TELESCOPE_TIMED_GUIDE_NS""#) {
                sky.pulse(number_in(msg, "TIMED_GUIDE_N"), number_in(msg, "TIMED_GUIDE_S"), 0.0, 0.0);
            } else if msg.contains(r#"name="TELESCOPE_TIMED_GUIDE_WE""#) {
                sky.pulse(0.0, 0.0, number_in(msg, "TIMED_GUIDE_W"), number_in(msg, "TIMED_GUIDE_E"));
            } else if msg.contains(r#"device="GuideCam""#) && msg.contains(r#"name="CCD_EXPOSURE""#) {
                let frame = sky.frame();
                let b64 = base64::engine::general_purpose::STANDARD.encode(&frame);
                let blob = format!(
                    r#"<setNumberVector device="GuideCam" name="CCD_EXPOSURE" state="Ok"><oneNumber name="CCD_EXPOSURE_VALUE">0</oneNumber></setNumberVector><setBLOBVector device="GuideCam" name="CCD1" state="Ok"><oneBLOB name="CCD1" size="{}" format=".fits">{b64}</oneBLOB></setBLOBVector>"#,
                    frame.len()
                );
                sock.write_all(blob.as_bytes()).await.unwrap();
            } else if msg.contains(r#"name="CCD_EXPOSURE""#) {
                let exposure = number_in(msg, "CCD_EXPOSURE_VALUE");
                sock.write_all(br#"<setNumberVector device="Cam" name="CCD_EXPOSURE" state="Busy"><oneNumber name="CCD_EXPOSURE_VALUE">1</oneNumber></setNumberVector>"#).await.unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
                sock.write_all(br#"<setNumberVector device="Cam" name="CCD_EXPOSURE" state="Ok"><oneNumber name="CCD_EXPOSURE_VALUE">0</oneNumber></setNumberVector>"#).await.unwrap();
                // Where the camera really points, J2000, for the fake solvers.
                let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
                let jd = imaging::astro::julian_day(unix);
                let (tra, tdec) = imaging::astro::precess(reported.0 + error.0, reported.1 + error.1, jd, imaging::astro::J2000);
                let frame = test_frame_at_gain(
                    if dark { 0.0 } else { exposure },
                    gain_factor(gain),
                    focus,
                    &[Card::new("TRUERA", tra * 15.0, ""), Card::new("TRUEDEC", tdec, "")],
                );
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

pub struct TestUi {
    pub ws: Ws,
    pub state: UiState,
    pub previews: Vec<(u64, Vec<u8>)>,
    pub capture_states: Vec<CaptureState>,
}

impl TestUi {
    pub async fn connect(url: &str) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        TestUi {
            ws,
            state: UiState::default(),
            previews: Vec::new(),
            capture_states: Vec::new(),
        }
    }

    pub async fn until(&mut self, what: &str, pred: impl Fn(&TestUi) -> bool) {
        self.until_for(what, 10, pred).await
    }

    /// Like [`until`](Self::until) with a custom per-message timeout (seconds).
    pub async fn until_for(&mut self, what: &str, secs: u64, pred: impl Fn(&TestUi) -> bool) {
        // An overall deadline: a busy engine keeps sending messages, so a
        // per-message timeout alone would wait forever.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs.max(30));
        while !pred(self) {
            let msg = tokio::time::timeout_at(deadline.min(tokio::time::Instant::now() + Duration::from_secs(secs)), self.ws.next())
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
                    let (_kind, id, jpeg) = preview_frame::decode(&b).expect("preview frame");
                    self.previews.push((id, jpeg.to_vec()));
                }
                _ => {}
            }
        }
    }

    pub async fn send(&mut self, cmd: ToEngine) {
        self.ws.send(Message::Text(cmd.to_json().into())).await.unwrap();
    }
}

